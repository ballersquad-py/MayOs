//! 3D side of the VMware SVGA adapter (VirtualBox VMSVGA with 3D on):
//! command buffers, the device's object tables (OTables) and guest-backed
//! memory objects (MOBs). This is what the vmwgfx DRM device (proc::drm)
//! drives on behalf of Mesa's svga driver.
//!
//! Commands go through SVGA command buffers: a 64-byte header plus the
//! commands, both in guest memory, submitted by physical address through
//! SVGA_REG_COMMAND_HIGH/LOW and completed when the device writes the
//! header's status.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use crate::drivers::vmware_svga::{reg_read, reg_write, IO_BASE};
use crate::mem::{phys_to_virt, pmm};

const REG_COMMAND_LOW: u32 = 48;
const REG_COMMAND_HIGH: u32 = 49;

const CB_CONTEXT_DEVICE: u32 = 0x3f;
const CB_CONTEXT_0: u32 = 0;
const CB_STATUS_NONE: u32 = 0;
const CB_STATUS_COMPLETED: u32 = 1;
const CB_FLAG_NO_IRQ: u32 = 1;

const DC_CMD_START_STOP_CONTEXT: u32 = 1;

pub const CMD_SURFACE_COPY: u32 = 1042;
pub const CMD_DESTROY_GB_MOB: u32 = 1094;
pub const CMD_DEFINE_GB_SURFACE: u32 = 1097;
pub const CMD_DESTROY_GB_SURFACE: u32 = 1098;
pub const CMD_BIND_GB_SURFACE: u32 = 1099;
pub const CMD_UPDATE_GB_IMAGE: u32 = 1101;
pub const CMD_READBACK_GB_IMAGE: u32 = 1103;
pub const CMD_SET_OTABLE_BASE64: u32 = 1115;
pub const CMD_DEFINE_GB_MOB64: u32 = 1135;
pub const CMD_DX_DEFINE_CONTEXT: u32 = 1143;
pub const CMD_DX_DESTROY_CONTEXT: u32 = 1144;
pub const CMD_DX_BIND_CONTEXT: u32 = 1145;
pub const CMD_DX_SET_COTABLE: u32 = 1207;
pub const CMD_DEFINE_GB_SURFACE_V4: u32 = 1267;

const MOBFMT_PT64_0: u32 = 4;
const MOBFMT_PT64_1: u32 = 5;
const MOBFMT_PT64_2: u32 = 6;

const PAGE: usize = 4096;

/// Guest memory handed to the device: pages plus the page table that
/// describes them (depth chosen by size).
pub struct Mob {
    /// Pages owned by this Mob (freed with it) unless `shared` holds them.
    shared: Option<alloc::sync::Arc<crate::proc::unix::Shm>>,
    pub pages: Vec<u64>,
    tables: Vec<u64>,
    pub depth: u32,
    pub base: u64,
    pub size: usize,
}

impl Mob {
    pub fn new(size: usize) -> Option<Mob> {
        let n = size.div_ceil(PAGE).max(1);
        let mut pages = Vec::with_capacity(n);
        for _ in 0..n {
            match pmm::alloc_frame_zeroed() {
                Some(f) => pages.push(f),
                None => {
                    for f in pages {
                        pmm::free_frame(f);
                    }
                    return None;
                }
            }
        }
        Mob::from_pages(pages, size)
    }

    /// Describe existing pages (their ownership moves to the Mob).
    pub fn from_pages(pages: Vec<u64>, size: usize) -> Option<Mob> {
        let mut tables = Vec::new();
        let (depth, base) = if pages.len() == 1 {
            (MOBFMT_PT64_0, pages[0] >> 12)
        } else if pages.len() <= 512 {
            let t = pmm::alloc_frame_zeroed()?;
            tables.push(t);
            let e = phys_to_virt(t) as *mut u64;
            for (i, p) in pages.iter().enumerate() {
                unsafe { e.add(i).write(p >> 12) };
            }
            (MOBFMT_PT64_1, t >> 12)
        } else if pages.len() <= 512 * 512 {
            let top = pmm::alloc_frame_zeroed()?;
            tables.push(top);
            for (k, chunk) in pages.chunks(512).enumerate() {
                let t = pmm::alloc_frame_zeroed()?;
                tables.push(t);
                let e = phys_to_virt(t) as *mut u64;
                for (i, p) in chunk.iter().enumerate() {
                    unsafe { e.add(i).write(p >> 12) };
                }
                unsafe { (phys_to_virt(top) as *mut u64).add(k).write(t >> 12) };
            }
            (MOBFMT_PT64_2, top >> 12)
        } else {
            return None;
        };
        Some(Mob { shared: None, pages, tables, depth, base, size })
    }

    pub fn write(&self, off: usize, data: &[u8]) {
        let mut done = 0;
        while done < data.len() {
            let pos = off + done;
            let (pg, within) = (pos / PAGE, pos % PAGE);
            let k = (PAGE - within).min(data.len() - done);
            unsafe {
                core::ptr::copy_nonoverlapping(data[done..].as_ptr(), (phys_to_virt(self.pages[pg]) as *mut u8).add(within), k);
            }
            done += k;
        }
    }

    pub fn read(&self, off: usize, out: &mut [u8]) {
        let mut done = 0;
        while done < out.len() {
            let pos = off + done;
            let (pg, within) = (pos / PAGE, pos % PAGE);
            let k = (PAGE - within).min(out.len() - done);
            unsafe {
                core::ptr::copy_nonoverlapping((phys_to_virt(self.pages[pg]) as *const u8).add(within), out[done..].as_mut_ptr(), k);
            }
            done += k;
        }
    }
}

impl Mob {
    /// A Mob over shared memory (a GPU buffer programs can map); the
    /// pages live as long as the Shm.
    pub fn over(shm: alloc::sync::Arc<crate::proc::unix::Shm>, size: usize) -> Option<Mob> {
        let n = size.div_ceil(PAGE).max(1);
        let mut pages = Vec::with_capacity(n);
        for i in 0..n {
            pages.push(shm.page(i)?);
        }
        let mut m = Mob::from_pages(pages, size)?;
        m.shared = Some(shm);
        Some(m)
    }
}

impl Drop for Mob {
    fn drop(&mut self) {
        for &f in self.tables.iter() {
            pmm::free_frame(f);
        }
        if self.shared.is_none() {
            for &f in self.pages.iter() {
                pmm::free_frame(f);
            }
        }
    }
}

/// A command stream being built: 3D commands are `id, size, body`.
#[derive(Default)]
pub struct Cmds(pub Vec<u8>);

impl Cmds {
    pub fn cmd(&mut self, id: u32, body: &[u32]) -> &mut Self {
        self.0.extend_from_slice(&id.to_le_bytes());
        self.0.extend_from_slice(&((body.len() * 4) as u32).to_le_bytes());
        for w in body {
            self.0.extend_from_slice(&w.to_le_bytes());
        }
        self
    }
}

struct State {
    started: bool,
    /// Object tables stay allocated for the device's lifetime.
    otables: Vec<Mob>,
    /// Header page + command pages for synchronous submission.
    header: u64,
    buf: u64,
    buf_pages: usize,
    next_id: u64,
}

/// A sleeping lock: submissions wait for the device, which must not
/// happen with interrupts off.
static STATE: crate::sync::Mutex<Option<State>> = crate::sync::Mutex::new(None);

const BUF_PAGES: usize = 64; // 256 KiB of commands per submission

fn io() -> Option<u16> {
    let io = IO_BASE.load(Ordering::Acquire);
    (io != 0).then_some(io)
}

/// Submit raw bytes on command-buffer context `ctx` and wait for the
/// device. Returns the final status and the error offset.
fn submit_raw(st: &mut State, ctx: u32, bytes: &[u8], dx_context: Option<u32>) -> Result<(), String> {
    if crate::drivers::vmware_svga::fake_gpu() {
        let _ = (st, ctx, bytes, dx_context);
        return Ok(());
    }
    let io = io().ok_or("no SVGA adapter")?;
    if bytes.len() > st.buf_pages * PAGE {
        return Err(alloc::format!("command stream of {} bytes is too long", bytes.len()));
    }
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), phys_to_virt(st.buf) as *mut u8, bytes.len());
    }
    let h = phys_to_virt(st.header) as *mut u32;
    st.next_id += 1;
    unsafe {
        core::ptr::write_bytes(h as *mut u8, 0, 64);
        h.add(0).write_volatile(CB_STATUS_NONE);
        (h.add(2) as *mut u64).write_volatile(st.next_id);
        h.add(4).write_volatile(CB_FLAG_NO_IRQ | if dx_context.is_some() { 2 } else { 0 });
        h.add(5).write_volatile(bytes.len() as u32);
        (h.add(6) as *mut u64).write_volatile(st.buf);
        h.add(8).write_volatile(0);
        h.add(9).write_volatile(dx_context.unwrap_or(0));
    }
    core::sync::atomic::fence(Ordering::SeqCst);
    reg_write(io, REG_COMMAND_HIGH, (st.header >> 32) as u32);
    reg_write(io, REG_COMMAND_LOW, (st.header as u32) | ctx);
    let start = crate::time::uptime_ms();
    loop {
        let s = unsafe { h.read_volatile() };
        if s != CB_STATUS_NONE {
            if s == CB_STATUS_COMPLETED {
                return Ok(());
            }
            let off = unsafe { h.add(1).read_volatile() };
            return Err(alloc::format!("device status {} at command offset {}", s, off));
        }
        let waited = crate::time::uptime_ms() - start;
        if waited > 2000 {
            return Err(String::from("device did not answer within 2 s"));
        }
        // Short jobs finish while spinning; longer ones let others run.
        if waited > 0 {
            crate::proc::sched::yield_now();
        } else {
            core::hint::spin_loop();
        }
    }
}

/// Run 3D commands on context 0 and wait for completion.
pub fn submit(c: &Cmds) -> Result<(), String> {
    init()?;
    let mut s = STATE.lock();
    let st = s.as_mut().ok_or("3D not initialised")?;
    submit_raw(st, CB_CONTEXT_0, &c.0, None)
}

/// Run commands for a DX context.
pub fn submit_dx(c: &Cmds, dx_context: u32) -> Result<(), String> {
    init()?;
    let mut s = STATE.lock();
    let st = s.as_mut().ok_or("3D not initialised")?;
    submit_raw(st, CB_CONTEXT_0, &c.0, Some(dx_context))
}

/// Entries per object table (size / entry size is the device's limit).
const OTABLE_BYTES: [usize; 6] = [
    1 << 20,   // MOB: 65536 ids x 16 bytes
    2 << 20,   // SURFACE: 32768 ids x 64 bytes
    16 << 10,  // CONTEXT
    64 << 10,  // SHADER
    16 << 10,  // SCREENTARGET
    16 << 10,  // DXCONTEXT
];

/// Start command buffers and hand the device its object tables (once).
pub fn init() -> Result<(), String> {
    let mut s = STATE.lock();
    if s.as_ref().is_some_and(|s| s.started) {
        return Ok(());
    }
    let g = crate::drivers::vmware_svga::gpu_info().ok_or("no VMSVGA adapter with 3D")?;
    if g.caps & 0x0100_0000 == 0 || g.caps & 0x0800_0000 == 0 {
        return Err(String::from("the adapter has no command buffers / GB objects (turn on 3D acceleration)"));
    }
    let header = pmm::alloc_frame_zeroed().ok_or("out of memory")?;
    let buf = pmm::alloc_contiguous(BUF_PAGES).ok_or("out of memory")?;
    let mut st = State { started: false, otables: Vec::new(), header, buf, buf_pages: BUF_PAGES, next_id: 0 };
    // Device context: enable command-buffer context 0.
    let mut dc = Vec::new();
    for w in [DC_CMD_START_STOP_CONTEXT, 1, CB_CONTEXT_0] {
        dc.extend_from_slice(&w.to_le_bytes());
    }
    submit_raw(&mut st, CB_CONTEXT_DEVICE, &dc, None).map_err(|e| alloc::format!("starting command buffers: {}", e))?;
    // Object tables.
    let tables = if g.caps & 0x1000_0000 != 0 { 6 } else { 5 };
    let mut c = Cmds::default();
    for (ty, &bytes) in OTABLE_BYTES.iter().enumerate().take(tables) {
        let m = Mob::new(bytes).ok_or("out of memory")?;
        c.cmd(CMD_SET_OTABLE_BASE64, &[ty as u32, m.base as u32, (m.base >> 32) as u32, bytes as u32, 0, m.depth]);
        st.otables.push(m);
    }
    submit_raw(&mut st, CB_CONTEXT_0, &c.0, None).map_err(|e| alloc::format!("setting object tables: {}", e))?;
    st.started = true;
    *s = Some(st);
    crate::kprintln!("svga3d: command buffers and object tables ready");
    Ok(())
}

/// Define `mob` on the device under `id`.
pub fn define_mob(id: u32, mob: &Mob) -> Result<(), String> {
    let mut c = Cmds::default();
    c.cmd(CMD_DEFINE_GB_MOB64, &[id, mob.depth, mob.base as u32, (mob.base >> 32) as u32, mob.size as u32]);
    submit(&c)
}

pub fn destroy_mob(id: u32) -> Result<(), String> {
    let mut c = Cmds::default();
    c.cmd(CMD_DESTROY_GB_MOB, &[id]);
    submit(&c)
}

/// The GPU round trip `gpu3d test` runs, step by step so a failure says
/// which part of the path does not work yet.
pub fn self_test() -> Result<String, String> {
    use core::fmt::Write;
    init()?;
    const W: u32 = 64;
    const H: u32 = 64;
    const FMT_A8R8G8B8: u32 = 2;
    let bytes = (W * H * 4) as usize;
    let src = Mob::new(bytes).ok_or("out of memory")?;
    let dst = Mob::new(bytes).ok_or("out of memory")?;
    let pattern: Vec<u8> = (0..bytes).map(|i| (i * 7 + 3) as u8).collect();
    let (mob_a, mob_b, sid_a, sid_b) = (0xfff0u32, 0xfff1u32, 0xfff0u32, 0xfff1u32);
    let mut log = String::new();
    let count = |m: &Mob| {
        let mut back = alloc::vec![0u8; bytes];
        m.read(0, &mut back);
        let same = back.iter().zip(&pattern).filter(|(a, b)| a == b).count();
        let zero = back.iter().filter(|b| **b == 0).count();
        (same, zero, back[0..8].to_vec())
    };
    let mut run = |log: &mut String, what: &str, c: &Cmds| -> bool {
        match submit(c) {
            Ok(()) => { let _ = writeln!(log, "ok   {}", what); true }
            Err(e) => { let _ = writeln!(log, "FAIL {}: {}", what, e); false }
        }
    };
    src.write(0, &pattern);
    let mut ok = true;
    let mut c = Cmds::default();
    c.cmd(CMD_DEFINE_GB_MOB64, &[mob_a, src.depth, src.base as u32, (src.base >> 32) as u32, src.size as u32]);
    c.cmd(CMD_DEFINE_GB_MOB64, &[mob_b, dst.depth, dst.base as u32, (dst.base >> 32) as u32, dst.size as u32]);
    ok &= run(&mut log, "define 2 memory objects", &c);
    let mut c = Cmds::default();
    for sid in [sid_a, sid_b] {
        c.cmd(CMD_DEFINE_GB_SURFACE, &[sid, 1 << 5, FMT_A8R8G8B8, 1, 0, 0, W, H, 1]);
    }
    c.cmd(CMD_BIND_GB_SURFACE, &[sid_a, mob_a]);
    c.cmd(CMD_BIND_GB_SURFACE, &[sid_b, mob_b]);
    ok &= ok && run(&mut log, "define 2 surfaces and bind them", &c);
    // 1: upload into A, wipe A's memory, read A back.
    let mut c = Cmds::default();
    c.cmd(CMD_UPDATE_GB_IMAGE, &[sid_a, 0, 0, 0, 0, 0, W, H, 1]);
    ok &= ok && run(&mut log, "upload pattern to surface A", &c);
    src.write(0, &alloc::vec![0u8; bytes]);
    let mut c = Cmds::default();
    c.cmd(CMD_READBACK_GB_IMAGE, &[sid_a, 0, 0]);
    ok &= ok && run(&mut log, "read surface A back", &c);
    let (same, zero, first) = count(&src);
    let _ = writeln!(log, "{}  A round trip: {} of {} bytes match ({} zero), first bytes {:?}", if same == bytes { "ok  " } else { "FAIL" }, same, bytes, zero, first);
    let upload_ok = same == bytes;
    // 2: GPU copy A -> B, read B back.
    let mut c = Cmds::default();
    c.cmd(CMD_SURFACE_COPY, &[sid_a, 0, 0, sid_b, 0, 0, 0, 0, 0, W, H, 1, 0, 0, 0]);
    c.cmd(CMD_READBACK_GB_IMAGE, &[sid_b, 0, 0]);
    ok &= ok && run(&mut log, "copy A to B on the GPU, read B back", &c);
    let (same_b, zero_b, first_b) = count(&dst);
    // VirtualBox's DX backend ignores this pre-DX copy; Mesa copies with
    // DX commands, so this line is information only.
    let _ = writeln!(log, "info legacy SURFACE_COPY: {} of {} bytes arrived ({} zero){}", same_b, bytes, zero_b, if same_b == bytes { "" } else { " (not supported by this host; Mesa does not use it)" });
    let _ = first_b;
    let mut cleanup = Cmds::default();
    cleanup.cmd(CMD_BIND_GB_SURFACE, &[sid_a, 0xffff_ffff]);
    cleanup.cmd(CMD_BIND_GB_SURFACE, &[sid_b, 0xffff_ffff]);
    cleanup.cmd(CMD_DESTROY_GB_SURFACE, &[sid_a]);
    cleanup.cmd(CMD_DESTROY_GB_SURFACE, &[sid_b]);
    cleanup.cmd(CMD_DESTROY_GB_MOB, &[mob_a]);
    cleanup.cmd(CMD_DESTROY_GB_MOB, &[mob_b]);
    let _ = submit(&cleanup);
    if ok && upload_ok { Ok(log) } else { Err(log) }
}
