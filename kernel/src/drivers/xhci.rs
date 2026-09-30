//! xHCI (USB 3) host controller driver with USB keyboards and mice.
//!
//! Every PC from the last ten years has its USB ports on an xHCI
//! controller. This driver takes the controller over from the firmware,
//! enumerates the devices on the root ports (hot plug too) and drives HID
//! keyboards and mice through the boot protocol. It runs polled in its
//! own kernel thread; hubs are not supported yet (devices behind a hub are
//! skipped). Other USB drivers (phone tethering, ...) hook in through
//! `Device` and the class code.
//!
//! "nousb" on the kernel command line leaves the controller to the
//! firmware (its PS/2 emulation keeps the keyboard working).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};

use super::pci::PciDevice;
use super::usbnet::{self, Proto, UsbNet};
use net::Mac;
use crate::input::{self, InputEvent};
use crate::mem::{paging, DmaBuf};
use crate::time::uptime_ms;

const RING_TRBS: usize = 256;
/// PORTSC bits that are safe to write back unchanged (no RW1C, no PED).
const PORT_NEUTRAL: u32 = 0x4e00_ffe9;

// TRB types.
const TRB_NORMAL: u32 = 1;
const TRB_SETUP: u32 = 2;
const TRB_DATA: u32 = 3;
const TRB_STATUS: u32 = 4;
const TRB_LINK: u32 = 6;
const TRB_ENABLE_SLOT: u32 = 9;
const TRB_DISABLE_SLOT: u32 = 10;
const TRB_ADDRESS_DEVICE: u32 = 11;
const TRB_CONFIGURE_EP: u32 = 12;
const TRB_EVALUATE_CTX: u32 = 13;
const EV_TRANSFER: u32 = 32;
const EV_COMMAND: u32 = 33;
const EV_PORT: u32 = 34;

fn r32(a: usize) -> u32 {
    unsafe { read_volatile(a as *const u32) }
}
fn w32(a: usize, v: u32) {
    unsafe { write_volatile(a as *mut u32, v) }
}
fn w64(a: usize, v: u64) {
    w32(a, v as u32);
    w32(a + 4, (v >> 32) as u32);
}

#[derive(Clone, Copy, Default)]
struct Trb {
    param: u64,
    status: u32,
    control: u32,
}

impl Trb {
    fn kind(&self) -> u32 {
        (self.control >> 10) & 0x3f
    }
}

/// A producer ring (command ring or a transfer ring) ending in a link TRB.
struct Ring {
    mem: DmaBuf,
    idx: usize,
    cycle: bool,
}

impl Ring {
    fn new() -> Option<Ring> {
        let mem = DmaBuf::try_new(RING_TRBS * 16)?;
        let mut r = Ring { mem, idx: 0, cycle: true };
        // Link back to the start, toggling the cycle bit.
        let phys = r.mem.phys;
        r.write(RING_TRBS - 1, Trb { param: phys, status: 0, control: TRB_LINK << 10 | 1 << 1 });
        Some(r)
    }

    fn write(&mut self, i: usize, t: Trb) {
        let p = self.mem.virt() as usize + i * 16;
        unsafe {
            write_volatile(p as *mut u64, t.param);
            write_volatile((p + 8) as *mut u32, t.status);
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            write_volatile((p + 12) as *mut u32, t.control);
        }
    }

    /// Queue a TRB (cycle bit filled in); returns its physical address.
    fn push(&mut self, mut t: Trb) -> u64 {
        t.control = (t.control & !1) | self.cycle as u32;
        let at = self.mem.phys + self.idx as u64 * 16;
        self.write(self.idx, t);
        self.idx += 1;
        if self.idx == RING_TRBS - 1 {
            // Hand the link TRB over and wrap.
            let link = self.mem.virt() as usize + (RING_TRBS - 1) * 16 + 12;
            let c = unsafe { read_volatile(link as *const u32) };
            unsafe { write_volatile(link as *mut u32, (c & !1) | self.cycle as u32) };
            self.idx = 0;
            self.cycle = !self.cycle;
        }
        at
    }

    fn phys(&self) -> u64 {
        self.mem.phys
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Keyboard,
    Mouse,
}

/// An interrupt IN endpoint we read reports from.
struct Hid {
    kind: Kind,
    dci: u8,
    ring: Ring,
    buf: DmaBuf,
    len: usize,
    last: [u8; 8],
    buttons: u8,
    /// Report-protocol mouse layout (None: boot protocol).
    layout: Option<MouseLayout>,
    reports: u32,
}

/// Where a mouse's buttons and axes are in its reports (from its HID
/// report descriptor): bit offsets and sizes.
#[derive(Clone, Copy, Debug, Default)]
struct MouseLayout {
    id: u8,
    buttons: (u32, u32),
    x: (u32, u32),
    y: (u32, u32),
    wheel: (u32, u32),
    /// Absolute X/Y (tablets, touch screens) with their largest value.
    absolute: bool,
    max: u32,
}

/// Parse a HID report descriptor for a mouse (X, Y, buttons, wheel).
fn parse_mouse(desc: &[u8]) -> Option<MouseLayout> {
    let (mut page, mut size, mut count, mut id, mut lmax) = (0u32, 0u32, 0u32, 0u8, 0u32);
    let mut usages: Vec<u32> = Vec::new();
    let (mut umin, mut umax) = (0u32, 0u32);
    let mut bits: alloc::collections::BTreeMap<u8, u32> = alloc::collections::BTreeMap::new();
    let mut found: Vec<MouseLayout> = Vec::new();
    let mut cur = MouseLayout::default();
    let mut i = 0;
    while i < desc.len() {
        let b = desc[i];
        if b == 0xfe {
            i += 3 + *desc.get(i + 1)? as usize;
            continue;
        }
        let n = [0, 1, 2, 4][(b & 3) as usize];
        let mut v = 0u32;
        for k in 0..n {
            v |= (*desc.get(i + 1 + k)? as u32) << (8 * k);
        }
        let (kind, tag) = ((b >> 2) & 3, b >> 4);
        match (kind, tag) {
            (1, 0) => page = v,
            (1, 7) => size = v,
            (1, 2) => lmax = v,
            (1, 9) => count = v,
            (1, 8) => {
                if cur.x.1 != 0 && cur.y.1 != 0 {
                    found.push(cur);
                }
                id = v as u8;
                cur = MouseLayout { id, ..Default::default() };
            }
            (2, 0) => usages.push(if n == 4 { v } else { page << 16 | v }),
            (2, 1) => umin = if n == 4 { v } else { page << 16 | v },
            (2, 2) => umax = if n == 4 { v } else { page << 16 | v },
            (0, 8) => {
                // Input: `count` fields of `size` bits.
                let at = *bits.get(&id).unwrap_or(&0);
                let constant = v & 1 != 0;
                if !constant {
                    for f in 0..count {
                        let usage = if !usages.is_empty() { usages[(f as usize).min(usages.len() - 1)] } else { umin + f };
                        let off = at + f * size;
                        match usage {
                            0x0001_0030 => {
                                cur.x = (off, size);
                                cur.absolute = v & 4 == 0;
                                cur.max = lmax.max(1);
                            }
                            0x0001_0031 => cur.y = (off, size),
                            0x0001_0038 => cur.wheel = (off, size),
                            u if u >> 16 == 9 && cur.buttons.1 == 0 => cur.buttons = (off, count.min(8)),
                            _ => {}
                        }
                    }
                    let _ = umax;
                }
                bits.insert(id, at + size * count);
                usages.clear();
                umin = 0;
                umax = 0;
            }
            (0, _) => {
                usages.clear();
                umin = 0;
                umax = 0;
            }
            _ => {}
        }
        i += 1 + n;
    }
    if cur.x.1 != 0 && cur.y.1 != 0 {
        found.push(cur);
    }
    found.into_iter().next()
}

/// A signed field of a report.
fn field(r: &[u8], (off, size): (u32, u32)) -> i32 {
    if size == 0 || size > 32 {
        return 0;
    }
    let mut v: u64 = 0;
    for k in 0..size {
        let bit = off + k;
        let byte = (bit / 8) as usize;
        if byte < r.len() && r[byte] >> (bit % 8) & 1 != 0 {
            v |= 1 << k;
        }
    }
    if size < 32 && v >> (size - 1) & 1 != 0 {
        v |= !0u64 << size;
    }
    v as i64 as i32
}

fn mouse_layout_report(l: &MouseLayout, prev: &mut u8, r: &[u8]) {
    let r = if l.id != 0 {
        if r.first() != Some(&l.id) {
            return;
        }
        &r[1..]
    } else {
        r
    };
    if l.absolute {
        let get = |f: (u32, u32)| field(r, (f.0, f.1.min(31))) as u32 & ((1u64 << f.1.min(31)) - 1) as u32;
        let (x, y) = (get(l.x), get(l.y));
        let scale = |v: u32| (v.min(l.max) as u64 * 65535 / l.max as u64) as u32;
        input::push(InputEvent::MouseAbsolute { x: Some(scale(x)), y: Some(scale(y)) });
    }
    let (dx, dy) = if l.absolute { (0, 0) } else { (field(r, l.x), field(r, l.y)) };
    if dx != 0 || dy != 0 {
        input::push(InputEvent::MouseMove { dx, dy });
    }
    let mut now = 0u8;
    for b in 0..l.buttons.1.min(3) {
        if field(r, (l.buttons.0 + b, 1)) != 0 {
            now |= 1 << b;
        }
    }
    for (bit, button) in [(1u8, 0u8), (2, 1), (4, 2)] {
        if (now ^ *prev) & bit != 0 {
            input::push(InputEvent::MouseButton { button, pressed: now & bit != 0 });
        }
    }
    *prev = now;
    let w = field(r, l.wheel);
    if w != 0 {
        input::push(InputEvent::Wheel(-w));
    }
}

struct Device {
    slot: u8,
    port: u8,
    ep0: Ring,
    /// Output device context (kept alive for the controller).
    _out: DmaBuf,
    hids: Vec<Hid>,
    nets: Vec<NetDev>,
}

/// A USB network function (phone tethering).
struct NetDev {
    proto: Proto,
    usb: Arc<UsbNet>,
    in_dci: u8,
    out_dci: u8,
    in_ring: Ring,
    out_ring: Ring,
    in_bufs: Vec<DmaBuf>,
    in_next: usize,
    in_len: usize,
    out_bufs: Vec<DmaBuf>,
    out_next: usize,
    inflight: usize,
    seq: u16,
    carrier_at: u64,
    out_mps: u16,
}

#[derive(Clone, Copy)]
struct Ep {
    addr: u8,
    attr: u8,
    mps: u16,
    interval: u8,
}

struct Iface {
    num: u8,
    alt: u8,
    class: u8,
    sub: u8,
    proto: u8,
    eps: Vec<Ep>,
    /// iMACAddress from a CDC Ethernet functional descriptor.
    mac_string: u8,
    /// Length of the HID report descriptor (HID interfaces).
    hid_len: u16,
}

fn parse_config(cfg: &[u8]) -> Vec<Iface> {
    let mut out: Vec<Iface> = Vec::new();
    let mut i = 0;
    while i + 2 <= cfg.len() {
        let len = cfg[i] as usize;
        if len < 2 || i + len > cfg.len() {
            break;
        }
        let d = &cfg[i..i + len];
        match d[1] {
            4 if len >= 9 => out.push(Iface { num: d[2], alt: d[3], class: d[5], sub: d[6], proto: d[7], eps: Vec::new(), mac_string: 0, hid_len: 0 }),
            5 if len >= 7 => {
                if let Some(f) = out.last_mut() {
                    f.eps.push(Ep { addr: d[2], attr: d[3], mps: u16::from_le_bytes([d[4], d[5]]) & 0x7ff, interval: d[6] });
                }
            }
            0x21 if len >= 9 => {
                if let Some(f) = out.last_mut() {
                    f.hid_len = u16::from_le_bytes([d[7], d[8]]);
                }
            }
            0x24 if len >= 4 && d[2] == 0x0f => {
                if let Some(f) = out.last_mut() {
                    f.mac_string = d[3];
                }
            }
            _ => {}
        }
        i += len;
    }
    out
}

struct NetPlan {
    proto: Proto,
    ctrl_if: u8,
    data_if: u8,
    alt: u8,
    bulk_in: Ep,
    bulk_out: Ep,
    mac_string: u8,
}

fn bulk(f: &Iface) -> Option<(Ep, Ep)> {
    let i = f.eps.iter().find(|e| e.attr & 3 == 2 && e.addr & 0x80 != 0)?;
    let o = f.eps.iter().find(|e| e.attr & 3 == 2 && e.addr & 0x80 == 0)?;
    Some((*i, *o))
}

/// The iPhone's usbmux interface (255/254/2).
fn find_mux(ifaces: &[Iface]) -> Option<NetPlan> {
    let f = ifaces.iter().find(|f| (f.class, f.sub, f.proto) == (0xff, 0xfe, 2) && bulk(f).is_some())?;
    let (i, o) = bulk(f)?;
    Some(NetPlan { proto: Proto::Raw, ctrl_if: f.num, data_if: f.num, alt: f.alt, bulk_in: i, bulk_out: o, mac_string: 0 })
}

/// The best network function among the interfaces of one configuration.
fn find_net(ifaces: &[Iface]) -> Option<NetPlan> {
    // iPhone: vendor interface 255/253/1 (any alternate setting with bulk).
    for f in ifaces {
        if (f.class, f.sub, f.proto) == (0xff, 0xfd, 1)
            && let Some((i, o)) = bulk(f)
        {
            return Some(NetPlan { proto: Proto::Ipheth, ctrl_if: f.num, data_if: f.num, alt: f.alt, bulk_in: i, bulk_out: o, mac_string: 0 });
        }
    }
    // CDC NCM / ECM / RNDIS: a control interface and the data interface after it.
    for (kind, test) in [
        (Proto::Ncm, (|f: &Iface| (f.class, f.sub) == (2, 0x0d)) as fn(&Iface) -> bool),
        (Proto::Ecm, |f: &Iface| (f.class, f.sub) == (2, 6)),
        (Proto::Rndis, |f: &Iface| matches!((f.class, f.sub, f.proto), (0xe0, 1, 3) | (2, 2, 0xff) | (0xef, 4, 1))),
    ] {
        for (k, c) in ifaces.iter().enumerate() {
            if !test(c) {
                continue;
            }
            for d in &ifaces[k + 1..] {
                if d.class == 0x0a
                    && let Some((i, o)) = bulk(d)
                {
                    return Some(NetPlan { proto: kind, ctrl_if: c.num, data_if: d.num, alt: d.alt, bulk_in: i, bulk_out: o, mac_string: c.mac_string });
                }
            }
        }
    }
    None
}

pub struct Xhci {
    op: usize,
    db: usize,
    ir: usize,
    ports: u8,
    csz: usize,
    dcbaa: DmaBuf,
    _scratch: Vec<DmaBuf>,
    cmd: Ring,
    events: DmaBuf,
    _erst: DmaBuf,
    ev_idx: usize,
    ev_cycle: bool,
    devices: Vec<Device>,
    /// Events that arrived while waiting for something else.
    backlog: Vec<Trb>,
}

unsafe impl Send for Xhci {}

fn wait(mut f: impl FnMut() -> bool, ms: u64) -> bool {
    let start = uptime_ms();
    while !f() {
        if uptime_ms() - start > ms {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

impl Xhci {
    pub fn new(pci: &PciDevice) -> Option<Xhci> {
        pci.enable();
        let bar = pci.bar(0);
        if bar == 0 {
            return None;
        }
        let base = paging::map_mmio(bar, 0x10000) as usize;
        let caplen = (r32(base) & 0xff) as usize;
        let hcs1 = r32(base + 4);
        let hcs2 = r32(base + 8);
        let hcc1 = r32(base + 0x10);
        let op = base + caplen;
        let db = base + (r32(base + 0x14) & !3) as usize;
        let rt = base + (r32(base + 0x18) & !0x1f) as usize;
        let slots = (hcs1 & 0xff).min(64);
        let ports = (hcs1 >> 24) as u8;
        let csz = if hcc1 & 4 != 0 { 64 } else { 32 };

        // Take the controller from the firmware (USB legacy support).
        let mut ext = ((hcc1 >> 16) & 0xffff) as usize * 4;
        while ext != 0 {
            let p = base + ext;
            let v = r32(p);
            if v & 0xff == 1 {
                w32(p, v | 1 << 24);
                wait(|| r32(p) & (1 << 16) == 0, 1000);
                // SMI on USB events off.
                w32(p + 4, r32(p + 4) & 0x000e_1fee);
            }
            let next = ((v >> 8) & 0xff) as usize * 4;
            if next == 0 {
                break;
            }
            ext += next;
        }

        // Stop and reset.
        w32(op, r32(op) & !1);
        if !wait(|| r32(op + 4) & 1 != 0, 100) {
            return None;
        }
        w32(op, r32(op) | 2);
        if !wait(|| r32(op) & 2 == 0 && r32(op + 4) & (1 << 11) == 0, 1000) {
            return None;
        }
        w32(op + 0x38, slots);

        let mut dcbaa = DmaBuf::try_new(4096)?;
        // Scratchpad buffers the controller asks for.
        let nscratch = (((hcs2 >> 21) & 0x1f) << 5 | (hcs2 >> 27)) as usize;
        let mut scratch = Vec::new();
        if nscratch > 0 {
            let mut arr = DmaBuf::try_new(nscratch * 8)?;
            for i in 0..nscratch {
                let pg = DmaBuf::try_new(4096)?;
                arr.as_mut_slice()[i * 8..i * 8 + 8].copy_from_slice(&pg.phys.to_le_bytes());
                scratch.push(pg);
            }
            dcbaa.as_mut_slice()[0..8].copy_from_slice(&arr.phys.to_le_bytes());
            scratch.push(arr);
        }
        w64(op + 0x30, dcbaa.phys);

        let cmd = Ring::new()?;
        w64(op + 0x18, cmd.phys() | 1);

        // Event ring for interrupter 0 (polled).
        let events = DmaBuf::try_new(RING_TRBS * 16)?;
        let mut erst = DmaBuf::try_new(64)?;
        erst.as_mut_slice()[0..8].copy_from_slice(&events.phys.to_le_bytes());
        erst.as_mut_slice()[8..12].copy_from_slice(&(RING_TRBS as u32).to_le_bytes());
        let ir = rt + 0x20;
        w32(ir + 8, 1);
        w64(ir + 0x18, events.phys);
        w64(ir + 0x10, erst.phys);
        w32(ir, 0); // no interrupts

        w32(op, r32(op) | 1); // run
        if !wait(|| r32(op + 4) & 1 == 0, 100) {
            return None;
        }
        Some(Xhci {
            op,
            db,
            ir,
            ports,
            csz,
            dcbaa,
            _scratch: scratch,
            cmd,
            events,
            _erst: erst,
            ev_idx: 0,
            ev_cycle: true,
            devices: Vec::new(),
            backlog: Vec::new(),
        })
    }

    fn portsc(&self, port: u8) -> usize {
        self.op + 0x400 + (port as usize - 1) * 0x10
    }

    /// Next event from the event ring, if any.
    fn next_event(&mut self) -> Option<Trb> {
        let p = self.events.virt() as usize + self.ev_idx * 16;
        let control = unsafe { read_volatile((p + 12) as *const u32) };
        if (control & 1 != 0) != self.ev_cycle {
            return None;
        }
        let t = unsafe { Trb { param: read_volatile(p as *const u64), status: read_volatile((p + 8) as *const u32), control } };
        self.ev_idx += 1;
        if self.ev_idx == RING_TRBS {
            self.ev_idx = 0;
            self.ev_cycle = !self.ev_cycle;
        }
        w64(self.ir + 0x18, (self.events.phys + self.ev_idx as u64 * 16) | 8);
        Some(t)
    }

    /// Wait for an event matching `want`; others are kept for `poll`.
    fn wait_event(&mut self, ms: u64, mut want: impl FnMut(&Trb) -> bool) -> Option<Trb> {
        let start = uptime_ms();
        loop {
            while let Some(t) = self.next_event() {
                if want(&t) {
                    return Some(t);
                }
                self.backlog.push(t);
            }
            if uptime_ms() - start > ms {
                return None;
            }
            core::hint::spin_loop();
        }
    }

    fn command(&mut self, t: Trb) -> Option<Trb> {
        let at = self.cmd.push(t);
        w32(self.db, 0);
        let ev = self.wait_event(2000, |e| e.kind() == EV_COMMAND && e.param == at)?;
        if (ev.status >> 24) == 1 { Some(ev) } else { None }
    }

    /// A control transfer on endpoint 0; `data` is read into or written from.
    fn control(&mut self, dev: usize, req_type: u8, req: u8, value: u16, index: u16, data: Option<(&mut DmaBuf, usize)>) -> bool {
        let slot = self.devices[dev].slot;
        let len = data.as_ref().map(|d| d.1).unwrap_or(0);
        let inp = req_type & 0x80 != 0;
        let setup = req_type as u64 | (req as u64) << 8 | (value as u64) << 16 | (index as u64) << 32 | (len as u64) << 48;
        let trt = if len == 0 { 0 } else if inp { 3 } else { 2 };
        let ring = &mut self.devices[dev].ep0;
        ring.push(Trb { param: setup, status: 8, control: TRB_SETUP << 10 | 1 << 6 | trt << 16 });
        if let Some((buf, n)) = data {
            ring.push(Trb { param: buf.phys, status: n as u32, control: TRB_DATA << 10 | (inp as u32) << 16 });
        }
        let dir_in = len == 0 || !inp;
        let last = ring.push(Trb { param: 0, status: 0, control: TRB_STATUS << 10 | 1 << 5 | (dir_in as u32) << 16 });
        w32(self.db + slot as usize * 4, 1);
        match self.wait_event(2000, |e| e.kind() == EV_TRANSFER && (e.control >> 24) as u8 == slot && e.param == last) {
            Some(e) => matches!(e.status >> 24, 1 | 13),
            None => false,
        }
    }

    /// Input context helper: pointer to context `i` (0 = input control).
    fn ctx(&self, buf: &DmaBuf, i: usize) -> usize {
        buf.virt() as usize + i * self.csz
    }

    /// Set up the device on a root port.
    fn attach(&mut self, port: u8) {
        let sc = self.portsc(port);
        let v = r32(sc);
        if v & 1 == 0 {
            return;
        }
        // USB 2 ports need a reset to enable (USB 3 ones come up enabled).
        if v & 2 == 0 {
            let neutral = v & PORT_NEUTRAL;
            w32(sc, neutral | 1 << 4);
            if !wait(|| r32(sc) & (1 << 21) != 0, 500) {
                return;
            }
            w32(sc, (r32(sc) & PORT_NEUTRAL) | 1 << 21);
            crate::proc::sched::sleep_ms(20);
        }
        let v = r32(sc);
        if v & 2 == 0 {
            return;
        }
        let speed = (v >> 10) & 0xf;
        let Some(ev) = self.command(Trb { param: 0, status: 0, control: TRB_ENABLE_SLOT << 10 }) else { return };
        let slot = (ev.control >> 24) as u8;
        let (Some(out), Some(inctx), Some(ep0)) = (DmaBuf::try_new(4096), DmaBuf::try_new(4096), Ring::new()) else { return };
        self.dcbaa.as_mut_slice()[slot as usize * 8..slot as usize * 8 + 8].copy_from_slice(&out.phys.to_le_bytes());
        let mps: u32 = match speed {
            1 | 2 => 8,
            3 => 64,
            _ => 512,
        };
        unsafe {
            write_volatile((self.ctx(&inctx, 0) + 4) as *mut u32, 3); // add slot + ep0
            let sl = self.ctx(&inctx, 1);
            write_volatile(sl as *mut u32, speed << 20 | 1 << 27);
            write_volatile((sl + 4) as *mut u32, (port as u32) << 16);
            let e0 = self.ctx(&inctx, 2);
            write_volatile((e0 + 4) as *mut u32, 3 << 1 | 4 << 3 | mps << 16);
            write_volatile((e0 + 8) as *mut u64, ep0.phys() | 1);
            write_volatile((e0 + 16) as *mut u32, 8);
        }
        if self.command(Trb { param: inctx.phys, status: 0, control: TRB_ADDRESS_DEVICE << 10 | (slot as u32) << 24 }).is_none() {
            let _ = self.command(Trb { param: 0, status: 0, control: TRB_DISABLE_SLOT << 10 | (slot as u32) << 24 });
            return;
        }
        self.devices.push(Device { slot, port, ep0, _out: out, hids: Vec::new(), nets: Vec::new() });
        let dev = self.devices.len() - 1;
        let Some(mut buf) = DmaBuf::try_new(4096) else { return };
        // Full-speed devices have 8, 16, 32 or 64 byte control packets:
        // read the real size first (8 bytes always fit), then tell the
        // controller, or longer replies fail (many gaming mice use 64).
        if speed == 1 && self.control(dev, 0x80, 6, 0x0100, 0, Some((&mut buf, 8))) {
            let real = buf.as_slice()[7] as u32;
            if matches!(real, 16 | 32 | 64) && real != mps {
                unsafe {
                    core::ptr::write_bytes(inctx.virt() as *mut u8, 0, 4096);
                    write_volatile((self.ctx(&inctx, 0) + 4) as *mut u32, 2); // ep0
                    let e0 = self.ctx(&inctx, 2);
                    write_volatile((e0 + 4) as *mut u32, 3 << 1 | 4 << 3 | real << 16);
                }
                if self.command(Trb { param: inctx.phys, status: 0, control: TRB_EVALUATE_CTX << 10 | (slot as u32) << 24 }).is_none() {
                    crate::kprintln!("usb: port {}: evaluate context failed", port);
                }
            }
        }
        if !self.control(dev, 0x80, 6, 0x0100, 0, Some((&mut buf, 18))) {
            crate::kprintln!("usb: port {}: no device descriptor", port);
            return;
        }
        let d = buf.as_slice();
        let (class, vid, pid) = (d[4], u16::from_le_bytes([d[8], d[9]]), u16::from_le_bytes([d[10], d[11]]));
        let nconf = d[17].max(1);
        let serial_idx = d[16];
        crate::kprintln!("usb: port {} device {:04x}:{:04x} class {:#x} ({})", port, vid, pid, class, speed_name(speed));
        if class == 9 {
            crate::kprintln!("usb: hubs are not supported yet: plug devices straight into the PC");
            return;
        }
        // Every configuration: phones put tethering in a later one.
        let mut configs: Vec<Vec<u8>> = Vec::new();
        for ci in 0..nconf.min(8) {
            if !self.control(dev, 0x80, 6, 0x0200 | ci as u16, 0, Some((&mut buf, 9))) {
                break;
            }
            let total = (u16::from_le_bytes([buf.as_slice()[2], buf.as_slice()[3]]) as usize).clamp(9, 4096);
            if !self.control(dev, 0x80, 6, 0x0200 | ci as u16, 0, Some((&mut buf, total))) {
                break;
            }
            configs.push(buf.as_slice()[..total].to_vec());
        }
        if configs.is_empty() {
            return;
        }
        let parsed: Vec<Vec<Iface>> = configs.iter().map(|c| parse_config(c)).collect();
        // Phones: list what each configuration offers (for diagnosing tethering).
        if matches!(vid, 0x05ac | 0x18d1 | 0x04e8 | 0x22b8 | 0x2717 | 0x12d1) {
            for (ci, ifaces) in parsed.iter().enumerate() {
                for f in ifaces {
                    crate::kprintln!(
                        "usb: port {} config {} (value {}) interface {} alt {}: class {:02x}/{:02x}/{:02x}, {} endpoints",
                        port, ci, configs[ci][5], f.num, f.alt, f.class, f.sub, f.proto, f.eps.len()
                    );
                }
            }
        }
        // A network function (phone tethering) in any configuration.
        for (ci, ifaces) in parsed.iter().enumerate() {
            if let Some(plan) = find_net(ifaces) {
                let value = configs[ci][5];
                if !self.control(dev, 0x00, 9, value as u16, 0, None) {
                    return;
                }
                let iphone = plan.proto == Proto::Ipheth;
                self.setup_net(dev, speed, plan, vid, pid);
                // iPhone: also the "Apple Mobile Device" interface, to pair.
                if iphone && let Some(mux) = find_mux(ifaces) {
                    let udid = self.string(dev, serial_idx).unwrap_or_default();
                    self.setup_net(dev, speed, mux, vid, pid);
                    if let Some(n) = self.devices[dev].nets.iter().find(|n| n.proto == Proto::Raw) {
                        crate::iphone::start(n.usb.clone(), udid);
                    }
                }
                return;
            }
        }
        // An iPhone without Personal Hotspot: pair now (the hotspot works
        // once it is turned on and the phone is plugged in again).
        if vid == 0x05ac {
            for (ci, ifaces) in parsed.iter().enumerate().rev() {
                if let Some(mux) = find_mux(ifaces) {
                    if !self.control(dev, 0x00, 9, configs[ci][5] as u16, 0, None) {
                        return;
                    }
                    let udid = self.string(dev, serial_idx).unwrap_or_default();
                    self.setup_net(dev, speed, mux, vid, pid);
                    if let Some(n) = self.devices[dev].nets.iter().find(|n| n.proto == Proto::Raw) {
                        crate::iphone::start(n.usb.clone(), udid);
                    }
                    return;
                }
            }
        }
        // Keyboards and mice in the first configuration: boot keyboards,
        // and any HID interface whose report descriptor describes a mouse.
        let mut found: Vec<(Kind, u8, Ep, u16)> = Vec::new();
        for i in &parsed[0] {
            if i.class != 3 || i.alt != 0 {
                continue;
            }
            let kind = match (i.sub, i.proto) {
                (1, 1) => Kind::Keyboard,
                _ => Kind::Mouse,
            };
            if let Some(ep) = i.eps.iter().find(|e| e.addr & 0x80 != 0 && e.attr & 3 == 3) {
                found.push((kind, i.num, *ep, i.hid_len));
            }
        }
        if found.is_empty() {
            return;
        }
        if !self.control(dev, 0x00, 9, configs[0][5] as u16, 0, None) {
            return;
        }
        for (kind, ifn, ep, hid_len) in found {
            let mut layout = None;
            if kind == Kind::Keyboard {
                // Boot protocol, and only report changes.
                self.control(dev, 0x21, 0x0b, 0, ifn as u16, None);
                self.control(dev, 0x21, 0x0a, 0, ifn as u16, None);
            } else {
                // Mice: read the report layout (gaming and wireless mice
                // often have no boot mode, or 16-bit movement).
                let n = (hid_len as usize).clamp(1, 4096);
                let mut desc = DmaBuf::try_new(4096);
                if let Some(d) = desc.as_mut()
                    && self.control(dev, 0x81, 6, 0x2200, ifn as u16, Some((d, n)))
                {
                    layout = parse_mouse(&d.as_slice()[..n]);
                }
                match layout {
                    Some(l) => crate::kprintln!("usb: port {} interface {}: mouse layout {:?}", port, ifn, l),
                    None => {
                        // Not a mouse (media keys, a second keyboard part...):
                        // only real boot mice are used without a layout.
                        let boot = parsed[0].iter().any(|i| i.num == ifn && i.sub == 1 && i.proto == 2);
                        if !boot {
                            continue;
                        }
                        self.control(dev, 0x21, 0x0b, 0, ifn as u16, None);
                    }
                }
            }
            let dci = (ep.addr & 0xf) * 2 + 1;
            let (Some(ring), Some(buf)) = (Ring::new(), DmaBuf::try_new(64)) else { return };
            // Interval: exponent of 125 us units.
            let exp = if speed >= 3 {
                (ep.interval.clamp(1, 16) - 1) as u32
            } else {
                let frames = (ep.interval.max(1) as u32) * 8;
                31 - frames.leading_zeros()
            };
            if !self.configure_ep(dev, dci, 7, ep.mps, exp, &ring) {
                crate::kprintln!("usb: port {}: configuring the endpoint failed", port);
                continue;
            }
            let len = (ep.mps as usize).min(64);
            let mut h = Hid { kind, dci, ring, buf, len, last: [0; 8], buttons: 0, layout, reports: 0 };
            h.ring.push(Trb { param: h.buf.phys, status: len as u32, control: TRB_NORMAL << 10 | 1 << 5 | 1 << 2 });
            w32(self.db + slot as usize * 4, dci as u32);
            crate::kprintln!("usb: port {}: {}", port, if kind == Kind::Keyboard { "keyboard" } else { "mouse" });
            self.devices[dev].hids.push(h);
        }
    }

    /// Add one endpoint to the device (Configure Endpoint command).
    /// Types: 2 bulk OUT, 6 bulk IN, 7 interrupt IN.
    fn configure_ep(&mut self, dev: usize, dci: u8, ep_type: u32, mps: u16, interval_exp: u32, ring: &Ring) -> bool {
        let Some(inctx) = DmaBuf::try_new(4096) else { return false };
        let slot = self.devices[dev].slot;
        unsafe {
            write_volatile((self.ctx(&inctx, 0) + 4) as *mut u32, 1 | 1 << dci);
            // Slot context: the current one with enough entries.
            let src = self.devices[dev]._out.virt() as usize;
            let sl = self.ctx(&inctx, 1);
            for k in 0..4 {
                write_volatile((sl + k * 4) as *mut u32, read_volatile((src + k * 4) as *const u32));
            }
            let d0 = read_volatile(sl as *const u32);
            let entries = ((d0 >> 27) & 0x1f).max(dci as u32);
            write_volatile(sl as *mut u32, (d0 & !(0x1f << 27)) | entries << 27);
            let e = self.ctx(&inctx, dci as usize + 1);
            write_volatile(e as *mut u32, interval_exp << 16);
            write_volatile((e + 4) as *mut u32, 3 << 1 | ep_type << 3 | (mps as u32) << 16);
            write_volatile((e + 8) as *mut u64, ring.phys() | 1);
            let avg = if ep_type == 7 { mps as u32 | (mps as u32) << 16 } else { 1024 };
            write_volatile((e + 16) as *mut u32, avg);
        }
        self.command(Trb { param: inctx.phys, status: 0, control: TRB_CONFIGURE_EP << 10 | (slot as u32) << 24 }).is_some()
    }

    /// A string descriptor (US English) as text.
    fn string(&mut self, dev: usize, idx: u8) -> Option<String> {
        if idx == 0 {
            return None;
        }
        let mut buf = DmaBuf::try_new(256)?;
        if !self.control(dev, 0x80, 6, 0x0300 | idx as u16, 0x0409, Some((&mut buf, 255))) {
            return None;
        }
        let n = (buf.as_slice()[0] as usize).min(255);
        let s: String = buf.as_slice()[2..n].chunks(2).map(|c| c[0] as char).filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
        Some(s)
    }

    fn setup_net(&mut self, dev: usize, speed: u32, plan: NetPlan, vid: u16, pid: u16) {
        let port = self.devices[dev].port;
        let slot = self.devices[dev].slot;
        let Some(mut buf) = DmaBuf::try_new(4096) else { return };
        if plan.alt != 0 || plan.proto == Proto::Ipheth {
            self.control(dev, 0x01, 0x0b, plan.alt as u16, plan.data_if as u16, None);
        }
        let (Some(in_ring), Some(out_ring)) = (Ring::new(), Ring::new()) else { return };
        let (in_dci, out_dci) = ((plan.bulk_in.addr & 0xf) * 2 + 1, (plan.bulk_out.addr & 0xf) * 2);
        let _ = speed;
        if !self.configure_ep(dev, in_dci, 6, plan.bulk_in.mps, 0, &in_ring) || !self.configure_ep(dev, out_dci, 2, plan.bulk_out.mps, 0, &out_ring) {
            crate::kprintln!("usb: port {}: network endpoints failed", port);
            return;
        }
        if plan.proto == Proto::Raw {
            let mut n = NetDev {
                proto: Proto::Raw,
                usb: Arc::new(UsbNet::new(Mac::ZERO, "Apple Mobile Device")),
                in_dci,
                out_dci,
                in_ring,
                out_ring,
                in_bufs: Vec::new(),
                in_next: 0,
                in_len: 16384,
                out_bufs: Vec::new(),
                out_next: 0,
                inflight: 0,
                seq: 0,
                carrier_at: 0,
                out_mps: plan.bulk_out.mps,
            };
            for _ in 0..4 {
                let (Some(b), Some(o)) = (DmaBuf::try_new(16384), DmaBuf::try_new(16384 + 64)) else { return };
                n.in_ring.push(Trb { param: b.phys, status: 16384, control: TRB_NORMAL << 10 | 1 << 5 | 1 << 2 });
                n.in_bufs.push(b);
                n.out_bufs.push(o);
            }
            w32(self.db + slot as usize * 4, in_dci as u32);
            self.devices[dev].nets.push(n);
            return;
        }
        // The adapter's MAC address.
        let mut mac = None;
        let ctrl = plan.ctrl_if as u16;
        match plan.proto {
            Proto::Ipheth => {
                if self.control(dev, 0xc0, 0x00, 0, 2, Some((&mut buf, 6))) {
                    mac = Some(Mac(buf.as_slice()[..6].try_into().unwrap()));
                }
            }
            Proto::Rndis => {
                let mut id = 1;
                let ask = |me: &mut Self, msg: Vec<u8>, buf: &mut DmaBuf| -> Option<Vec<u8>> {
                    buf.as_mut_slice()[..msg.len()].copy_from_slice(&msg);
                    if !me.control(dev, 0x21, 0x00, 0, ctrl, Some((buf, msg.len()))) {
                        return None;
                    }
                    for _ in 0..50 {
                        buf.as_mut_slice()[..8].fill(0);
                        if me.control(dev, 0xa1, 0x01, 0, ctrl, Some((buf, 1024))) && buf.as_slice()[0..4] != [0, 0, 0, 0] {
                            return Some(buf.as_slice()[..1024].to_vec());
                        }
                        crate::proc::sched::sleep_ms(10);
                    }
                    None
                };
                if ask(self, usbnet::rndis_msg(usbnet::RNDIS_INIT, id, 0, None), &mut buf).is_none() {
                    crate::kprintln!("usb: port {}: RNDIS did not start", port);
                    return;
                }
                id += 1;
                if let Some(r) = ask(self, usbnet::rndis_msg(usbnet::RNDIS_QUERY, id, usbnet::OID_PERMANENT_ADDRESS, None), &mut buf)
                    && let Some(m) = usbnet::rndis_query_result(&r)
                    && m.len() >= 6
                {
                    mac = Some(Mac(m[..6].try_into().unwrap()));
                }
                id += 1;
                ask(self, usbnet::rndis_msg(usbnet::RNDIS_SET, id, usbnet::OID_PACKET_FILTER, Some(0x0000_000f)), &mut buf);
            }
            Proto::Raw => {}
            Proto::Ecm | Proto::Ncm => {
                if plan.mac_string != 0 && self.control(dev, 0x80, 6, 0x0300 | plan.mac_string as u16, 0x0409, Some((&mut buf, 64))) {
                    let n = (buf.as_slice()[0] as usize).min(64);
                    mac = usbnet::mac_from_string(&buf.as_slice()[..n]);
                }
                // Directed, broadcast and multicast frames.
                self.control(dev, 0x21, 0x43, 0x0e, ctrl, None);
            }
        }
        let mac = mac.unwrap_or_else(|| usbnet::local_mac(pid ^ vid));
        let model = match plan.proto {
            Proto::Ipheth => "iPhone (USB tethering)",
            Proto::Rndis => "Android phone (USB tethering)",
            Proto::Ncm => "USB tethering (NCM)",
            Proto::Ecm => "USB Ethernet",
            Proto::Raw => "USB device",
        };
        let big = matches!(plan.proto, Proto::Rndis | Proto::Ncm);
        let in_len = if big { 16384 } else { 2048 };
        let mut n = NetDev {
            proto: plan.proto,
            usb: Arc::new(UsbNet::new(mac, model)),
            in_dci,
            out_dci,
            in_ring,
            out_ring,
            in_bufs: Vec::new(),
            in_next: 0,
            in_len,
            out_bufs: Vec::new(),
            out_next: 0,
            inflight: 0,
            seq: 0,
            carrier_at: 0,
            out_mps: plan.bulk_out.mps,
        };
        for _ in 0..8 {
            let (Some(b), Some(o)) = (DmaBuf::try_new(in_len), DmaBuf::try_new(16384)) else { return };
            n.in_ring.push(Trb { param: b.phys, status: in_len as u32, control: TRB_NORMAL << 10 | 1 << 5 | 1 << 2 });
            n.in_bufs.push(b);
            n.out_bufs.push(o);
        }
        w32(self.db + slot as usize * 4, in_dci as u32);
        crate::kprintln!("usb: port {}: {} mac {}", port, model, mac);
        crate::network::add_usb(n.usb.clone());
        self.devices[dev].nets.push(n);
    }

    fn detach(&mut self, port: u8) {
        if let Some(i) = self.devices.iter().position(|d| d.port == port) {
            let d = self.devices.remove(i);
            for n in &d.nets {
                n.usb.gone.store(true, core::sync::atomic::Ordering::Relaxed);
            }
            let _ = self.command(Trb { param: 0, status: 0, control: TRB_DISABLE_SLOT << 10 | (d.slot as u32) << 24 });
            self.dcbaa.as_mut_slice()[d.slot as usize * 8..d.slot as usize * 8 + 8].fill(0);
            crate::kprintln!("usb: port {} unplugged", port);
        }
    }

    fn handle(&mut self, t: Trb) {
        match t.kind() {
            EV_TRANSFER => {
                let slot = (t.control >> 24) as u8;
                let dci = ((t.control >> 16) & 0x1f) as u8;
                let code = t.status >> 24;
                let db = self.db;
                let Some(d) = self.devices.iter_mut().find(|d| d.slot == slot) else { return };
                if let Some(n) = d.nets.iter_mut().find(|n| n.in_dci == dci || n.out_dci == dci) {
                    if dci == n.in_dci {
                        let i = n.in_next;
                        n.in_next = (n.in_next + 1) % n.in_bufs.len();
                        if matches!(code, 1 | 13) {
                            let got = n.in_len.saturating_sub((t.status & 0xff_ffff) as usize);
                            let mut frames = Vec::new();
                            usbnet::unwrap(n.proto, &n.in_bufs[i].as_slice()[..got], &mut frames);
                            for f in frames {
                                n.usb.deliver(f);
                            }
                        }
                        let phys = n.in_bufs[i].phys;
                        n.in_ring.push(Trb { param: phys, status: n.in_len as u32, control: TRB_NORMAL << 10 | 1 << 5 | 1 << 2 });
                        w32(db + slot as usize * 4, n.in_dci as u32);
                        return;
                    }
                    if dci == n.out_dci {
                        n.inflight = n.inflight.saturating_sub(1);
                        return;
                    }
                }
                let Some(h) = d.hids.iter_mut().find(|h| h.dci == dci) else { return };
                if matches!(code, 1 | 13) {
                    let got = h.len - (t.status & 0xff_ffff) as usize;
                    let mut rep = [0u8; 8];
                    let n = got.min(8);
                    rep[..n].copy_from_slice(&h.buf.as_slice()[..n]);
                    h.reports += 1;
                    if h.reports <= 3 {
                        crate::kprintln!("usb: report {:02x?}", &h.buf.as_slice()[..got.min(16)]);
                    }
                    match (h.kind, &h.layout) {
                        (Kind::Keyboard, _) => {
                            keyboard_report(&h.last, &rep);
                            let now = uptime_ms();
                            let held = rep[2..].iter().rev().find(|&&k| k > 3).copied();
                            let mut r = REPEAT.lock();
                            match held {
                                Some(k) if !h.last[2..].contains(&k) => *r = Some((k, now + 500)),
                                Some(_) => {}
                                None => *r = None,
                            }
                        }
                        (Kind::Mouse, Some(l)) => {
                            let l = *l;
                            mouse_layout_report(&l, &mut h.buttons, &h.buf.as_slice()[..got]);
                        }
                        (Kind::Mouse, None) => mouse_report(&mut h.buttons, &rep[..n]),
                    }
                    h.last = rep;
                }
                h.ring.push(Trb { param: h.buf.phys, status: h.len as u32, control: TRB_NORMAL << 10 | 1 << 5 | 1 << 2 });
                w32(db + slot as usize * 4, dci as u32);
            }
            EV_PORT => {
                let port = (t.param >> 24) as u8;
                let sc = self.portsc(port);
                let v = r32(sc);
                // Acknowledge the change bits.
                w32(sc, (v & PORT_NEUTRAL) | (v & 0x00fe_0000));
                if v & (1 << 17) != 0 {
                    if v & 1 != 0 {
                        if !self.devices.iter().any(|d| d.port == port) {
                            crate::proc::sched::sleep_ms(100);
                            self.attach(port);
                        }
                    } else {
                        self.detach(port);
                    }
                }
            }
            _ => {}
        }
    }

    pub fn poll(&mut self) {
        // Held key: repeat it (USB keyboards do not; PS/2 ones do).
        {
            let mut r = REPEAT.lock();
            if let Some((k, at)) = *r
                && uptime_ms() >= at
            {
                if let Some((c, e)) = usage_to_set1(k) {
                    super::ps2::scancode(c, e, true);
                }
                *r = Some((k, at + 33));
            }
        }
        let backlog = core::mem::take(&mut self.backlog);
        for t in backlog {
            self.handle(t);
        }
        while let Some(t) = self.next_event() {
            self.handle(t);
        }
        let db = self.db;
        let mut carrier: Vec<usize> = Vec::new();
        let now = uptime_ms();
        for (k, d) in self.devices.iter_mut().enumerate() {
            let slot = d.slot;
            for n in d.nets.iter_mut() {
            let mut sent = false;
            while n.inflight < n.out_bufs.len() {
                let Some(frame) = n.usb.tx.lock().pop_front() else { break };
                let data = usbnet::wrap(n.proto, &frame, &mut n.seq);
                let b = &mut n.out_bufs[n.out_next];
                let len = data.len().min(b.len());
                b.as_mut_slice()[..len].copy_from_slice(&data[..len]);
                let phys = b.phys;
                n.out_next = (n.out_next + 1) % n.out_bufs.len();
                n.out_ring.push(Trb { param: phys, status: len as u32, control: TRB_NORMAL << 10 | 1 << 5 });
                n.inflight += 1;
                // A stream ends a transfer that fills whole packets with an
                // empty one (the iPhone waits for it).
                if n.proto == Proto::Raw && len % n.out_mps.max(1) as usize == 0 {
                    n.out_ring.push(Trb { param: phys, status: 0, control: TRB_NORMAL << 10 | 1 << 5 });
                    n.inflight += 1;
                }
                sent = true;
            }
            if sent {
                w32(db + slot as usize * 4, n.out_dci as u32);
            }
            if n.proto == Proto::Ipheth && now - n.carrier_at > 1000 {
                n.carrier_at = now;
                carrier.push(k);
            }
            }
        }
        // iPhone: is Personal Hotspot on?
        for k in carrier {
            let Some(mut b) = DmaBuf::try_new(64) else { break };
            let on = self.control(k, 0xc0, 0x45, 0, 2, Some((&mut b, 1))) && b.as_slice()[0] == 4;
            if let Some(n) = self.devices[k].nets.iter().find(|n| n.proto == Proto::Ipheth) {
                n.usb.link.store(on, core::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    pub fn attach_all(&mut self) {
        for p in 1..=self.ports {
            let sc = self.portsc(p);
            // Clear the connect change so it is not handled twice.
            let v = r32(sc);
            w32(sc, (v & PORT_NEUTRAL) | (v & 0x00fe_0000));
            self.attach(p);
        }
    }
}

fn speed_name(s: u32) -> &'static str {
    match s {
        1 => "full speed",
        2 => "low speed",
        3 => "high speed",
        4 => "SuperSpeed",
        _ => "SuperSpeed+",
    }
}

/// USB HID usage → PS/2 set 1 scan code (extended flag).
fn usage_to_set1(u: u8) -> Option<(u8, bool)> {
    const LETTERS: [u8; 26] = [
        0x1e, 0x30, 0x2e, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31, 0x18, 0x19, 0x10, 0x13, 0x1f,
        0x14, 0x16, 0x2f, 0x11, 0x2d, 0x15, 0x2c,
    ];
    Some(match u {
        0x04..=0x1d => (LETTERS[(u - 0x04) as usize], false),
        0x1e..=0x26 => (u - 0x1e + 0x02, false), // 1..9
        0x27 => (0x0b, false),                   // 0
        0x28 => (0x1c, false),                   // Enter
        0x29 => (0x01, false),                   // Esc
        0x2a => (0x0e, false),                   // Backspace
        0x2b => (0x0f, false),                   // Tab
        0x2c => (0x39, false),                   // Space
        0x2d => (0x0c, false),
        0x2e => (0x0d, false),
        0x2f => (0x1a, false),
        0x30 => (0x1b, false),
        0x31 => (0x2b, false),
        0x32 => (0x2b, false),
        0x33 => (0x27, false),
        0x34 => (0x28, false),
        0x35 => (0x29, false),
        0x36 => (0x33, false),
        0x37 => (0x34, false),
        0x38 => (0x35, false),
        0x39 => (0x3a, false), // Caps Lock
        0x3a..=0x43 => (u - 0x3a + 0x3b, false), // F1..F10
        0x44 => (0x57, false),
        0x45 => (0x58, false),
        0x46 => (0x37, true), // Print Screen
        0x47 => (0x46, false),
        0x49 => (0x52, true),
        0x4a => (0x47, true),
        0x4b => (0x49, true),
        0x4c => (0x53, true),
        0x4d => (0x4f, true),
        0x4e => (0x51, true),
        0x4f => (0x4d, true), // Right
        0x50 => (0x4b, true), // Left
        0x51 => (0x50, true), // Down
        0x52 => (0x48, true), // Up
        0x53 => (0x45, false),
        0x54 => (0x35, true),
        0x55 => (0x37, false),
        0x56 => (0x4a, false),
        0x57 => (0x4e, false),
        0x58 => (0x1c, true),
        0x59 => (0x4f, false),
        0x5a => (0x50, false),
        0x5b => (0x51, false),
        0x5c => (0x4b, false),
        0x5d => (0x4c, false),
        0x5e => (0x4d, false),
        0x5f => (0x47, false),
        0x60 => (0x48, false),
        0x61 => (0x49, false),
        0x62 => (0x52, false),
        0x63 => (0x53, false),
        0x64 => (0x56, false),
        0x65 => (0x5d, true), // Menu
        _ => return None,
    })
}

const MODIFIERS: [(u8, bool); 8] = [(0x1d, false), (0x2a, false), (0x38, false), (0x5b, true), (0x1d, true), (0x36, false), (0x38, true), (0x5c, true)];

fn keyboard_report(old: &[u8; 8], new: &[u8; 8]) {
    // Rollover error: all keys 1.
    if new[2..].iter().all(|&k| k == 1) {
        return;
    }
    for (bit, &(code, ext)) in MODIFIERS.iter().enumerate() {
        let (was, is) = (old[0] >> bit & 1, new[0] >> bit & 1);
        if was != is {
            super::ps2::scancode(code, ext, is == 1);
        }
    }
    for &k in &old[2..] {
        if k > 3 && !new[2..].contains(&k)
            && let Some((c, e)) = usage_to_set1(k)
        {
            super::ps2::scancode(c, e, false);
        }
    }
    for &k in &new[2..] {
        if k > 3 && !old[2..].contains(&k)
            && let Some((c, e)) = usage_to_set1(k)
        {
            super::ps2::scancode(c, e, true);
        }
    }
}

fn mouse_report(prev: &mut u8, r: &[u8]) {
    if r.len() < 3 {
        return;
    }
    let (dx, dy) = (r[1] as i8 as i32, r[2] as i8 as i32);
    if dx != 0 || dy != 0 {
        input::push(InputEvent::MouseMove { dx, dy });
    }
    let now = r[0] & 7;
    for (bit, button) in [(1u8, 0u8), (2, 1), (4, 2)] {
        if (now ^ *prev) & bit != 0 {
            input::push(InputEvent::MouseButton { button, pressed: now & bit != 0 });
        }
    }
    *prev = now;
    if r.len() >= 4 && r[3] != 0 {
        input::push(InputEvent::Wheel(-(r[3] as i8 as i32)));
    }
}

/// The key being held on a USB keyboard and when it next repeats.
static REPEAT: crate::sync::Spin<Option<(u8, u64)>> = crate::sync::Spin::new(None);

static CONTROLLERS: crate::sync::Spin<Vec<PciDevice>> = crate::sync::Spin::new(Vec::new());

extern "C" fn usb_thread(_: usize) {
    let pcis = CONTROLLERS.lock().clone();
    let mut hcs: Vec<Xhci> = Vec::new();
    for d in pcis {
        match Xhci::new(&d) {
            Some(mut hc) => {
                crate::kprintln!("usb: xHCI controller {:04x}:{:04x}, {} ports", d.vendor, d.device, hc.ports);
                hc.attach_all();
                hcs.push(hc);
            }
            None => crate::kprintln!("usb: xHCI controller {:04x}:{:04x} did not start", d.vendor, d.device),
        }
    }
    if hcs.is_empty() {
        return;
    }
    loop {
        for hc in hcs.iter_mut() {
            hc.poll();
        }
        crate::proc::sched::sleep_ms(2);
    }
}

/// Start the USB thread for every xHCI controller.
pub fn init() {
    if crate::boot::cmdline().contains("nousb") {
        return;
    }
    let list: Vec<PciDevice> = super::pci::devices().into_iter().filter(|d| d.class == 0x0c && d.subclass == 0x03 && d.prog_if == 0x30).collect();
    if list.is_empty() {
        return;
    }
    *CONTROLLERS.lock() = list;
    crate::proc::sched::spawn_kernel("usb", usb_thread, 0);
}
