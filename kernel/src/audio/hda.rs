//! Intel High Definition Audio: the sound on every PC since about 2006
//! (Realtek, Conexant, IDT... codecs behind an Intel/AMD controller).
//!
//! Commands go through the CORB/RIRB rings. On the first codec with an
//! analog output we route every output pin (speakers, headphones, line
//! out) to one DAC and play 48 kHz 16-bit stereo from a cyclic buffer of
//! `BUFFERS` pieces, the same interface as the AC'97 driver.

use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};

use crate::drivers::pci::PciDevice;
use crate::mem::{paging, DmaBuf};
use crate::time::uptime_ms;

pub const BUFFERS: usize = 32;
pub const FRAMES: usize = 1024;

const GCAP: usize = 0x00;
const GCTL: usize = 0x08;
const STATESTS: usize = 0x0e;
const CORBLBASE: usize = 0x40;
const CORBWP: usize = 0x48;
const CORBRP: usize = 0x4a;
const CORBCTL: usize = 0x4c;
const CORBSIZE: usize = 0x4e;
const RIRBLBASE: usize = 0x50;
const RIRBWP: usize = 0x58;
const RINTCNT: usize = 0x5a;
const RIRBCTL: usize = 0x5c;
const RIRBSIZE: usize = 0x5e;

const STREAM_TAG: u32 = 1;

pub struct Hda {
    mmio: usize,
    corb: DmaBuf,
    rirb: DmaBuf,
    corb_wp: u16,
    rirb_rp: u16,
    codec: u32,
    sd: usize,
    bdl: DmaBuf,
    pub buffers: DmaBuf,
    /// DAC and output pins on the path (for volume).
    dac: u32,
    dac_steps: u32,
    pins: Vec<u32>,
    lvi: u8,
    last_civ: u8,
    running: bool,
}

unsafe impl Send for Hda {}

impl Hda {
    fn r8(&self, o: usize) -> u8 {
        unsafe { read_volatile((self.mmio + o) as *const u8) }
    }
    fn w8(&self, o: usize, v: u8) {
        unsafe { write_volatile((self.mmio + o) as *mut u8, v) }
    }
    fn r16(&self, o: usize) -> u16 {
        unsafe { read_volatile((self.mmio + o) as *const u16) }
    }
    fn w16(&self, o: usize, v: u16) {
        unsafe { write_volatile((self.mmio + o) as *mut u16, v) }
    }
    fn r32(&self, o: usize) -> u32 {
        unsafe { read_volatile((self.mmio + o) as *const u32) }
    }
    fn w32(&self, o: usize, v: u32) {
        unsafe { write_volatile((self.mmio + o) as *mut u32, v) }
    }

    pub fn new(pci: &PciDevice) -> Option<Hda> {
        pci.enable();
        let bar = pci.bar(0);
        if bar == 0 {
            return None;
        }
        let mmio = paging::map_mmio(bar, 0x4000) as usize;
        let mut h = Hda {
            mmio,
            corb: DmaBuf::try_new(1024)?,
            rirb: DmaBuf::try_new(2048)?,
            corb_wp: 0,
            rirb_rp: 0,
            codec: 0,
            sd: 0,
            bdl: DmaBuf::try_new(BUFFERS * 16)?,
            buffers: DmaBuf::try_new(BUFFERS * FRAMES * 4)?,
            dac: 0,
            dac_steps: 0,
            pins: Vec::new(),
            lvi: 0,
            last_civ: 0,
            running: false,
        };
        // Controller reset.
        h.w32(GCTL, h.r32(GCTL) & !1);
        if !wait(|| h.r32(GCTL) & 1 == 0, 100) {
            return None;
        }
        crate::proc::sched::sleep_ms(1);
        h.w32(GCTL, h.r32(GCTL) | 1);
        if !wait(|| h.r32(GCTL) & 1 != 0, 100) {
            return None;
        }
        // Codecs need 521 us after reset to announce themselves.
        crate::proc::sched::sleep_ms(2);
        let present = h.r16(STATESTS);

        // CORB (256 commands) and RIRB (256 responses).
        h.w8(CORBCTL, 0);
        h.w8(RIRBCTL, 0);
        wait(|| h.r8(CORBCTL) & 2 == 0 && h.r8(RIRBCTL) & 2 == 0, 50);
        h.w32(CORBLBASE, h.corb.phys as u32);
        h.w32(CORBLBASE + 4, (h.corb.phys >> 32) as u32);
        h.w8(CORBSIZE, 2);
        h.w16(CORBRP, 1 << 15);
        wait(|| h.r16(CORBRP) & (1 << 15) != 0, 20);
        h.w16(CORBRP, 0);
        wait(|| h.r16(CORBRP) & (1 << 15) == 0, 20);
        h.w16(CORBWP, 0);
        h.w32(RIRBLBASE, h.rirb.phys as u32);
        h.w32(RIRBLBASE + 4, (h.rirb.phys >> 32) as u32);
        h.w8(RIRBSIZE, 2);
        h.w16(RIRBWP, 1 << 15);
        h.w16(RINTCNT, 0xff);
        h.w8(CORBCTL, 2);
        h.w8(RIRBCTL, 2);

        // First codec with an analog output path.
        let mut ok = false;
        for cad in 0..15 {
            if present & (1 << cad) != 0 {
                h.codec = cad;
                if h.setup_codec() {
                    ok = true;
                    break;
                }
            }
        }
        if !ok {
            return None;
        }

        // First output stream descriptor.
        let gcap = h.r16(GCAP);
        let iss = ((gcap >> 8) & 0xf) as usize;
        if (gcap >> 12) & 0xf == 0 {
            return None;
        }
        h.sd = 0x80 + iss * 0x20;
        h.stream_reset();
        Some(h)
    }

    fn stream_reset(&mut self) {
        let sd = self.sd;
        self.w8(sd, 0);
        self.w8(sd, 1);
        wait(|| self.r8(sd) & 1 != 0, 20);
        self.w8(sd, 0);
        wait(|| self.r8(sd) & 1 == 0, 20);
        for i in 0..BUFFERS {
            let e = self.bdl.virt() as usize + i * 16;
            unsafe {
                write_volatile(e as *mut u64, self.buffers.phys + (i * FRAMES * 4) as u64);
                write_volatile((e + 8) as *mut u32, (FRAMES * 4) as u32);
                write_volatile((e + 12) as *mut u32, 0);
            }
        }
        self.w32(sd + 0x18, self.bdl.phys as u32);
        self.w32(sd + 0x1c, (self.bdl.phys >> 32) as u32);
        self.w32(sd + 0x08, (BUFFERS * FRAMES * 4) as u32);
        self.w16(sd + 0x0c, (BUFFERS - 1) as u16);
        self.w16(sd + 0x12, 0x0011); // 48 kHz, 16 bit, 2 channels
        self.w8(sd + 2, (STREAM_TAG << 4) as u8);
        self.w8(sd + 3, 0x1c); // clear status
        self.last_civ = 0;
    }

    /// Send one verb, wait for the response.
    fn cmd(&mut self, nid: u32, verb: u32) -> Option<u32> {
        let v = self.codec << 28 | (nid & 0x7f) << 20 | (verb & 0xf_ffff);
        self.corb_wp = (self.corb_wp + 1) % 256;
        unsafe { write_volatile((self.corb.virt() as usize + self.corb_wp as usize * 4) as *mut u32, v) };
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        self.w16(CORBWP, self.corb_wp);
        let want = (self.rirb_rp + 1) % 256;
        if !wait(|| self.r16(RIRBWP) & 0xff == want, 50) {
            return None;
        }
        self.rirb_rp = want;
        // Acknowledge (some controllers stop after RINTCNT responses).
        self.w8(RIRBCTL + 1, 0x05);
        Some(unsafe { read_volatile((self.rirb.virt() as usize + want as usize * 8) as *const u32) })
    }

    fn param(&mut self, nid: u32, p: u32) -> u32 {
        self.cmd(nid, 0xf0000 | p).unwrap_or(0)
    }

    fn connections(&mut self, nid: u32) -> Vec<u32> {
        let len = self.param(nid, 0x0e);
        let long = len & 0x80 != 0;
        let n = len & 0x7f;
        let mut out = Vec::new();
        let per = if long { 2 } else { 4 };
        let mut i = 0;
        while i < n {
            let r = self.cmd(nid, 0xf0200 | i).unwrap_or(0);
            for k in 0..per {
                if i + k >= n {
                    break;
                }
                let e = if long { (r >> (16 * k)) & 0xffff } else { (r >> (8 * k)) & 0xff };
                out.push(e & 0x7f);
            }
            i += per;
        }
        out
    }

    fn unmute(&mut self, nid: u32, inputs: usize) {
        let caps = self.param(nid, 0x12);
        let gain = caps & 0x7f;
        let _ = self.cmd(nid, 0x3b000 | gain); // output, left+right
        for i in 0..inputs.max(1) {
            let _ = self.cmd(nid, 0x37000 | (i as u32) << 8 | gain);
        }
    }

    /// A path from `nid` down to an output converter, through mixers and
    /// selectors (depth-first, a few levels).
    fn path_to_dac(&mut self, nid: u32, depth: u32, seen: &mut Vec<u32>) -> Option<Vec<u32>> {
        if depth > 5 || seen.contains(&nid) {
            return None;
        }
        seen.push(nid);
        let caps = self.param(nid, 0x09);
        let kind = (caps >> 20) & 0xf;
        if kind == 0 {
            return Some(alloc::vec![nid]);
        }
        let conns = self.connections(nid);
        for (i, c) in conns.iter().enumerate() {
            if let Some(mut p) = self.path_to_dac(*c, depth + 1, seen) {
                if conns.len() > 1 && kind != 2 {
                    let _ = self.cmd(nid, 0x70100 | i as u32); // connection select
                }
                p.insert(0, nid);
                return Some(p);
            }
        }
        None
    }

    fn setup_codec(&mut self) -> bool {
        let sub = self.param(0, 0x04);
        let (start, count) = ((sub >> 16) & 0xff, sub & 0xff);
        let mut afg = None;
        for n in start..start + count {
            if self.param(n, 0x05) & 0xff == 1 {
                afg = Some(n);
                break;
            }
        }
        let Some(afg) = afg else { return false };
        let _ = self.cmd(afg, 0x70500); // power D0
        crate::proc::sched::sleep_ms(10);
        let sub = self.param(afg, 0x04);
        let (start, count) = ((sub >> 16) & 0xff, sub & 0xff);
        // Output pins, preferring speakers and headphones.
        let mut pins = Vec::new();
        for n in start..start + count {
            let caps = self.param(n, 0x09);
            if (caps >> 20) & 0xf != 4 {
                continue;
            }
            let cfg = self.cmd(n, 0xf1c00).unwrap_or(0);
            let device = (cfg >> 20) & 0xf;
            let connectivity = cfg >> 30;
            let digital = caps & (1 << 9) != 0;
            if connectivity == 1 || digital || !matches!(device, 0..=2) {
                continue;
            }
            if self.param(n, 0x0c) & (1 << 4) == 0 {
                continue; // not output capable
            }
            pins.push((n, device));
        }
        if pins.is_empty() {
            return false;
        }
        let mut dac = None;
        for &(pin, device) in &pins {
            let mut seen = Vec::new();
            let Some(path) = self.path_to_dac(pin, 0, &mut seen) else { continue };
            let d = *path.last().unwrap();
            // All pins on one DAC: skip pins that only reach another one.
            if dac.is_some_and(|x| x != d) {
                continue;
            }
            dac = Some(d);
            for &n in &path {
                let _ = self.cmd(n, 0x70500);
                let conns = self.param(n, 0x0e) & 0x7f;
                self.unmute(n, conns as usize);
            }
            // Pin: output on (headphone amp for headphones), external amp on.
            let ctl = 0x40 | if device == 2 { 0x80 } else { 0 };
            let _ = self.cmd(pin, 0x70700 | ctl);
            let _ = self.cmd(pin, 0x70c02);
            self.pins.push(pin);
        }
        let Some(dac) = dac else { return false };
        self.dac = dac;
        self.dac_steps = (self.param(dac, 0x12) & 0x7f).max(1);
        let _ = self.cmd(dac, 0x70600 | STREAM_TAG << 4); // stream 1, channel 0
        let _ = self.cmd(dac, 0x20011); // format: 48 kHz, 16 bit, stereo
        true
    }

    pub fn buffer(&mut self, i: usize) -> &mut [i16] {
        unsafe { core::slice::from_raw_parts_mut((self.buffers.virt() as usize + i * FRAMES * 4) as *mut i16, FRAMES * 2) }
    }

    pub fn current(&self) -> u8 {
        ((self.r32(self.sd + 4) as usize / (FRAMES * 4)) % BUFFERS) as u8
    }

    pub fn last_valid(&self) -> u8 {
        self.lvi
    }

    pub fn set_last_valid(&mut self, i: u8) {
        self.lvi = i & 31;
    }

    /// The DMA engine loops over the whole ring: clear what it has played
    /// so a pause is silent instead of repeating old sound.
    pub fn kick(&mut self) {
        if !self.running {
            self.w8(self.sd, self.r8(self.sd) | 2);
            self.running = true;
            self.last_civ = self.current();
            return;
        }
        let civ = self.current();
        let mut i = self.last_civ;
        while i != civ {
            // Keep buffers the mixer has already filled for the future.
            if (self.lvi.wrapping_sub(i) & 31) >= super::AHEAD + 1 {
                self.buffer(i as usize).fill(0);
            }
            i = (i + 1) & 31;
        }
        self.last_civ = civ;
    }

    pub fn stop(&mut self) {
        self.w8(self.sd, self.r8(self.sd) & !2);
        self.running = false;
        for i in 0..BUFFERS {
            self.buffer(i).fill(0);
        }
        self.stream_reset();
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn set_volume(&mut self, percent: u8, mute: bool) {
        let g = self.dac_steps * percent.min(100) as u32 / 100;
        let m = if mute || percent == 0 { 0x80 } else { 0 };
        let dac = self.dac;
        let _ = self.cmd(dac, 0x3b000 | m | g);
    }
}

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
