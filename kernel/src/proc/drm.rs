//! /dev/dri/card0 and /dev/dri/renderD128: the Linux `vmwgfx` DRM
//! interface on VirtualBox/VMware's SVGA adapter, so Mesa's `svga`
//! (vmwgfx_dri.so) driver can use the host GPU.
//!
//! Stage 1: identification and capabilities (VERSION, GET_CAP,
//! vmwgfx GET_PARAM and GET_3D_CAP). Objects and command submission come
//! next. Turned on only when /etc/gpu3d exists, so the working software
//! path is untouched until the rest is in place.

use super::usermem;
use crate::drivers::vmware_svga;

const EINVAL: i64 = 22;
const EFAULT: i64 = 14;
const ENOSYS: i64 = 38;
const ENOTTY: i64 = 25;
const ENOMEM: i64 = 12;
const ENOENT: i64 = 2;

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::drivers::svga3d::{self, Cmds, Mob};
use crate::sync::Mutex;
use super::unix::Shm;

const INVALID: u32 = 0xffff_ffff;

/// A GPU buffer: shared memory programs map, defined on the device as a
/// MOB whose id is the buffer's handle (so command streams need no
/// translation).
struct Bo {
    shm: Arc<Shm>,
    _mob: Mob,
    size: u64,
}

struct Surface {
    create: Vec<u8>, // the create request (returned by REF)
    backup: u32,     // buffer handle
    backup_size: u32,
    own_backup: bool,
}

struct Context {
    mob: u32,
    cotables: Vec<u32>,
}

struct Objects {
    bos: BTreeMap<u32, Bo>,
    surfaces: BTreeMap<u32, Surface>,
    contexts: BTreeMap<u32, Context>,
}

static OBJ: Mutex<Objects> = Mutex::new(Objects { bos: BTreeMap::new(), surfaces: BTreeMap::new(), contexts: BTreeMap::new() });
/// Device ids are table slots (the object tables have a fixed number of
/// entries), so freed ids are handed out again.
struct Ids {
    next: u32,
    free: Vec<u32>,
}

impl Ids {
    const fn new(first: u32) -> Ids {
        Ids { next: first, free: Vec::new() }
    }
    fn get(&mut self) -> u32 {
        self.free.pop().unwrap_or_else(|| {
            self.next += 1;
            self.next - 1
        })
    }
    fn put(&mut self, id: u32) {
        self.free.push(id);
    }
}

static BO_IDS: crate::sync::Spin<Ids> = crate::sync::Spin::new(Ids::new(1));
static SURFACE_IDS: crate::sync::Spin<Ids> = crate::sync::Spin::new(Ids::new(1));
static CONTEXT_IDS: crate::sync::Spin<Ids> = crate::sync::Spin::new(Ids::new(0));
static FENCE_SEQ: AtomicU32 = AtomicU32::new(1);

fn gpu_log(what: &str, e: &str) {
    static N: AtomicU32 = AtomicU32::new(0);
    if N.fetch_add(1, Ordering::Relaxed) < 32 {
        crate::kprintln!("drm: {}: {}", what, e);
    }
}

/// mmap offset of a buffer (what ALLOC_BO returns as map_handle).
pub fn map_offset(handle: u32) -> u64 {
    (handle as u64) << 32
}

/// The shared memory behind a buffer, for mmap of the DRM device.
pub fn bo_for_offset(off: u64) -> Option<(Arc<Shm>, u64)> {
    let o = OBJ.lock();
    o.bos.get(&((off >> 32) as u32)).map(|b| (b.shm.clone(), b.size))
}

fn bo_new(size: u64) -> Option<u32> {
    let size = size.max(1).div_ceil(4096) * 4096;
    let shm = Shm::new();
    if !shm.resize(size) {
        return None;
    }
    let mob = Mob::over(shm.clone(), size as usize)?;
    let h = BO_IDS.lock().get();
    if let Err(e) = svga3d::define_mob(h, &mob) {
        gpu_log("define buffer", &e);
        BO_IDS.lock().put(h);
        return None;
    }
    OBJ.lock().bos.insert(h, Bo { shm, _mob: mob, size });
    Some(h)
}

fn bo_close(h: u32) {
    if OBJ.lock().bos.remove(&h).is_some() {
        let _ = svga3d::destroy_mob(h);
        BO_IDS.lock().put(h);
    }
}

fn bo_alloc(pml4: u64, arg: u64) -> i64 {
    let Some(size) = rd32(pml4, arg) else { return -EFAULT };
    let Some(h) = bo_new(size as u64) else { return -ENOMEM };
    let ok = usermem::write_u64(pml4, arg, map_offset(h))
        && usermem::write_u32(pml4, arg + 8, h)
        && usermem::write_u32(pml4, arg + 12, h)
        && usermem::write_u32(pml4, arg + 16, 0);
    if ok { 0 } else { -EFAULT }
}

/// Bytes a surface's backing memory needs: every mip level of every
/// array slice, in the format's blocks (the device's guest-backed layout).
fn surface_bytes(format: u32, w: u32, h: u32, d: u32, mips: u32, array: u32, samples: u32) -> u64 {
    // Unknown formats: assume the largest (16 bytes a pixel).
    let (bw, bh, bd, bpb) = crate::drivers::svga3d_formats::format_block(format).unwrap_or((1, 1, 1, 16));
    let mut total = 0u64;
    for m in 0..mips.max(1) {
        let (mw, mh, md) = ((w >> m).max(1), (h >> m).max(1), (d >> m).max(1));
        total += mw.div_ceil(bw) as u64 * mh.div_ceil(bh) as u64 * md.div_ceil(bd) as u64 * bpb as u64;
    }
    // Multisampled surfaces hold every sample (as vmwgfx sizes them).
    total * array.max(1) as u64 * samples.max(1) as u64
}

/// GB_SURFACE_CREATE(_EXT): `req` holds the request words.
fn surface_define(pml4: u64, arg: u64, req: &[u32], ext: bool) -> i64 {
    let (flags, format, mips, drm_flags, msaa, filter, buf, array) = (req[0], req[1], req[2], req[3], req[4], req[5], req[6], req[7]);
    let (w, h, d) = (req[8], req[9], req[10]);
    let (flags_hi, ms_pattern, quality, stride) = if ext { (req[12], req[13], req[14], req[15]) } else { (0, 0, 0, 0) };
    let sid = SURFACE_IDS.lock().get();
    let size = surface_bytes(format, w, h, d, mips, if flags & (1 << 0) != 0 && array == 0 { 6 } else { array }, msaa);
    let (backup, own) = if buf != INVALID && buf != 0 && OBJ.lock().bos.contains_key(&buf) {
        (buf, false)
    } else if drm_flags & 0x4 != 0 || buf == INVALID || buf == 0 {
        match bo_new(size) {
            Some(b) => (b, true),
            None => return -ENOMEM,
        }
    } else {
        return -ENOENT;
    };
    let mut c = Cmds::default();
    c.cmd(svga3d::CMD_DEFINE_GB_SURFACE_V4, &[sid, flags, flags_hi, format, mips.max(1), msaa, ms_pattern, quality, filter, w, h, d.max(1), array, stride]);
    c.cmd(svga3d::CMD_BIND_GB_SURFACE, &[sid, backup]);
    if let Err(e) = svga3d::submit(&c) {
        gpu_log("define surface", &e);
        if own {
            bo_close(backup);
        }
        SURFACE_IDS.lock().put(sid);
        return -EINVAL;
    }
    let bsize = OBJ.lock().bos.get(&backup).map(|b| b.size).unwrap_or(size);
    let create: Vec<u8> = req.iter().flat_map(|w| w.to_le_bytes()).collect();
    OBJ.lock().surfaces.insert(sid, Surface { create, backup, backup_size: bsize as u32, own_backup: own });
    // rep: handle, backup_size, buffer_handle, buffer_size, buffer_map_handle
    let ok = usermem::write_u32(pml4, arg, sid)
        && usermem::write_u32(pml4, arg + 4, bsize as u32)
        && usermem::write_u32(pml4, arg + 8, backup)
        && usermem::write_u32(pml4, arg + 12, bsize as u32)
        && usermem::write_u64(pml4, arg + 16, map_offset(backup));
    if ok { 0 } else { -EFAULT }
}

fn read_words(pml4: u64, arg: u64, n: usize) -> Option<Vec<u32>> {
    let b = usermem::read_bytes(pml4, arg, n as u64 * 4)?;
    Some(b.chunks(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect())
}

fn surface_create(pml4: u64, arg: u64) -> i64 {
    // base request (11 words) + version, flags_hi, pattern, quality, stride, mbz
    let Some(req) = read_words(pml4, arg, 17) else { return -EFAULT };
    surface_define(pml4, arg, &req, true)
}

fn surface_create_legacy(pml4: u64, arg: u64) -> i64 {
    let Some(req) = read_words(pml4, arg, 11) else { return -EFAULT };
    surface_define(pml4, arg, &req, false)
}

fn surface_ref(pml4: u64, arg: u64, ext: bool) -> i64 {
    let Some(sid) = rd32(pml4, arg) else { return -EFAULT };
    let o = OBJ.lock();
    let Some(s) = o.surfaces.get(&sid) else { return -ENOENT };
    let req_len = if ext { 17 * 4 } else { 11 * 4 };
    let mut out = s.create.clone();
    out.resize(req_len, 0);
    for w in [sid, s.backup_size, s.backup, s.backup_size] {
        out.extend_from_slice(&w.to_le_bytes());
    }
    out.extend_from_slice(&map_offset(s.backup).to_le_bytes());
    if usermem::write_bytes(pml4, arg, &out) { 0 } else { -EFAULT }
}

fn surface_destroy(sid: u32) {
    let s = OBJ.lock().surfaces.remove(&sid);
    if let Some(s) = s {
        let mut c = Cmds::default();
        c.cmd(svga3d::CMD_BIND_GB_SURFACE, &[sid, INVALID]);
        c.cmd(svga3d::CMD_DESTROY_GB_SURFACE, &[sid]);
        let _ = svga3d::submit(&c);
        if s.own_backup {
            bo_close(s.backup);
        }
        SURFACE_IDS.lock().put(sid);
    }
}

/// Entries per DX object table (fixed; Linux grows them on demand) and
/// the size of one entry.
const COTABLES: [(u32, u32); 12] = [
    (1024, 32),  // render target views
    (256, 32),   // depth/stencil views
    (4096, 32),  // shader resource views
    (256, 1024), // element layouts
    (512, 128),  // blend states
    (512, 16),   // depth/stencil states
    (512, 32),   // rasterizer states
    (1024, 64),  // samplers
    (16, 2048),  // stream output
    (1024, 16),  // queries
    (4096, 32),  // shaders
    (256, 64),   // unordered access views
];

fn dx_context_new() -> Option<u32> {
    let cid = CONTEXT_IDS.lock().get();
    let Some(mob) = bo_new(8192) else {
        CONTEXT_IDS.lock().put(cid);
        return None;
    };
    let mut c = Cmds::default();
    c.cmd(svga3d::CMD_DX_DEFINE_CONTEXT, &[cid]);
    c.cmd(svga3d::CMD_DX_BIND_CONTEXT, &[cid, mob, 0]);
    let mut cotables = Vec::new();
    for (ty, &(n, sz)) in COTABLES.iter().enumerate() {
        let Some(t) = bo_new((n * sz) as u64) else {
            for t in cotables {
                bo_close(t);
            }
            bo_close(mob);
            CONTEXT_IDS.lock().put(cid);
            return None;
        };
        c.cmd(svga3d::CMD_DX_SET_COTABLE, &[cid, t, ty as u32, 0]);
        cotables.push(t);
    }
    if let Err(e) = svga3d::submit(&c) {
        gpu_log("define DX context", &e);
        for t in cotables {
            bo_close(t);
        }
        bo_close(mob);
        CONTEXT_IDS.lock().put(cid);
        return None;
    }
    OBJ.lock().contexts.insert(cid, Context { mob, cotables });
    Some(cid)
}

fn context_create(pml4: u64, arg: u64) -> i64 {
    let Some(kind) = rd32(pml4, arg) else { return -EFAULT };
    if kind != 1 {
        return -EINVAL; // only DX contexts
    }
    let Some(cid) = dx_context_new() else { return -ENOMEM };
    if usermem::write_u32(pml4, arg, cid) && usermem::write_u32(pml4, arg + 4, 0) { 0 } else { -EFAULT }
}

fn context_destroy(cid: u32) {
    let ctx = OBJ.lock().contexts.remove(&cid);
    if let Some(ctx) = ctx {
        let mut c = Cmds::default();
        c.cmd(svga3d::CMD_DX_BIND_CONTEXT, &[cid, INVALID, 0]);
        c.cmd(svga3d::CMD_DX_DESTROY_CONTEXT, &[cid]);
        let _ = svga3d::submit(&c);
        for t in ctx.cotables {
            bo_close(t);
        }
        bo_close(ctx.mob);
        CONTEXT_IDS.lock().put(cid);
    }
}

/// EXECBUF: run the command stream (ids are the handles, so it goes to
/// the device as is) and report a fence that has already passed.
fn execbuf(pml4: u64, arg: u64) -> i64 {
    let Some(w) = read_words(pml4, arg, 10) else { return -EFAULT };
    let cmds = (w[0] as u64) | ((w[1] as u64) << 32);
    let size = w[2];
    let fence_rep = (w[4] as u64) | ((w[5] as u64) << 32);
    let cid = w[8];
    let Some(bytes) = usermem::read_bytes(pml4, cmds, size as u64) else { return -EFAULT };
    let bytes = if cid != INVALID { rewrite_cb_offsets(cid, bytes) } else { bytes };
    let c = Cmds(bytes);
    let r = if cid != INVALID && OBJ.lock().contexts.contains_key(&cid) { svga3d::submit_dx(&c, cid) } else { svga3d::submit(&c) };
    let seq = FENCE_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    let err = match r {
        Ok(()) => 0,
        Err(e) => {
            gpu_log("command stream", &e);
            -EINVAL as i32
        }
    };
    if fence_rep != 0 {
        // handle, mask, seqno, passed_seqno, fd, error
        let mut rep = Vec::new();
        for v in [seq, 1, seq, seq, u32::MAX, err as u32] {
            rep.extend_from_slice(&v.to_le_bytes());
        }
        let _ = usermem::write_bytes(pml4, fence_rep, &rep);
    }
    if err != 0 { err as i64 } else { 0 }
}

/// Constant buffer bound per (DX context, shader type, slot): surface and size.
static CB_BINDINGS: crate::sync::Spin<BTreeMap<(u32, u32, u32), (u32, u32)>> = crate::sync::Spin::new(BTreeMap::new());

/// VirtualBox mishandles DX_SET_{VS..CS}_CONSTANT_BUFFER_OFFSET (1220..1225,
/// which Mesa uses to move within one big constant upload buffer): shaders
/// then read stale matrices. Rewrite each into a full
/// DX_SET_SINGLE_CONSTANT_BUFFER with the binding's surface and size.
fn rewrite_cb_offsets(cid: u32, bytes: Vec<u8>) -> Vec<u8> {
    const SET_SINGLE_CB: u32 = 1148;
    let word = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    // Only streams made purely of 3D commands (id, size, body) are parsed.
    let mut off = 0;
    while off + 8 <= bytes.len() {
        let id = word(&bytes, off);
        if !(1040..2000).contains(&id) {
            return bytes;
        }
        off += 8 + word(&bytes, off + 4) as usize;
    }
    if off != bytes.len() {
        return bytes;
    }
    let mut map = CB_BINDINGS.lock();
    let mut out = Vec::with_capacity(bytes.len() + 64);
    let mut off = 0;
    while off < bytes.len() {
        let (id, size) = (word(&bytes, off), word(&bytes, off + 4) as usize);
        let body = &bytes[off + 8..off + 8 + size];
        if id == SET_SINGLE_CB && size >= 20 {
            let (slot, ty, sid, _o, sz) = (word(body, 0), word(body, 4), word(body, 8), word(body, 12), word(body, 16));
            map.insert((cid, ty, slot), (sid, sz));
        } else if (1220..=1225).contains(&id) && size >= 8 {
            let ty = id - 1220 + 1; // VS=1, PS, GS, HS, DS, CS
            let (slot, offset) = (word(body, 0), word(body, 4));
            if let Some(&(sid, sz)) = map.get(&(cid, ty, slot)) {
                for v in [SET_SINGLE_CB, 20, slot, ty, sid, offset, sz] {
                    out.extend_from_slice(&v.to_le_bytes());
                }
                off += 8 + size;
                continue;
            }
        }
        out.extend_from_slice(&bytes[off..off + 8 + size]);
        off += 8 + size;
    }
    out
}

pub const MAJOR: u64 = 226;

/// Device names under /dev/dri with their minor numbers.
pub const NODES: [(&str, u64); 2] = [("card0", 0), ("renderD128", 128)];

pub fn enabled() -> bool {
    crate::fs::exists("/etc/gpu3d") && vmware_svga::gpu_info().is_some_and(|g| g.caps & 0x0800_0000 != 0)
}

/// Firefox on the GPU (experimental): /etc/firefox-gpu as well as the
/// GPU device.
pub fn firefox_gpu() -> bool {
    enabled() && crate::fs::exists("/etc/firefox-gpu")
}

pub fn node(path: &str) -> Option<u64> {
    let name = path.strip_prefix("/dev/dri/")?;
    NODES.iter().find(|(n, _)| *n == name).map(|&(_, m)| m).filter(|_| enabled())
}

/// st_rdev for a node (Linux's new_encode_dev for small numbers).
pub fn rdev(minor: u64) -> u64 {
    (MAJOR << 8) | minor
}

fn rd(pml4: u64, a: u64) -> Option<u64> {
    usermem::read_u64(pml4, a)
}

fn rd32(pml4: u64, a: u64) -> Option<u32> {
    usermem::read_bytes(pml4, a, 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
}

/// Copy a string into a user buffer of `len_at`/`ptr_at` (drm_version
/// style) and store its full length.
fn put_str(pml4: u64, arg: u64, len_at: u64, ptr_at: u64, s: &str) -> bool {
    let (Some(len), Some(ptr)) = (rd(pml4, arg + len_at), rd(pml4, arg + ptr_at)) else { return false };
    if ptr != 0 && len != 0 {
        let n = (len as usize).min(s.len());
        if !usermem::write_bytes(pml4, ptr, &s.as_bytes()[..n]) {
            return false;
        }
    }
    usermem::write_u64(pml4, arg + len_at, s.len() as u64)
}

pub fn ioctl(pml4: u64, cmd: u64, arg: u64) -> i64 {
    if (cmd >> 8) & 0xff != 0x64 {
        return -ENOTTY;
    }
    let Some(g) = vmware_svga::gpu_info() else { return -ENOSYS };
    let nr = cmd & 0xff;
    match nr {
        0x00 => {
            // VERSION: vmwgfx 2.20 (what current Mesa expects for DX/GB).
            let ok = usermem::write_u32(pml4, arg, 2)
                && usermem::write_u32(pml4, arg + 4, 20)
                && usermem::write_u32(pml4, arg + 8, 0)
                && put_str(pml4, arg, 16, 24, "vmwgfx")
                && put_str(pml4, arg, 32, 40, "20211206")
                && put_str(pml4, arg, 48, 56, "Linux drm driver for VMware graphics devices");
            if ok { 0 } else { -EFAULT }
        }
        0x01 => {
            // GET_UNIQUE
            if put_str(pml4, arg, 0, 8, "pci:0000:00:02.0") { 0 } else { -EFAULT }
        }
        0x02 => if usermem::write_u32(pml4, arg, 1) { 0 } else { -EFAULT }, // GET_MAGIC
        0x11 => 0, // AUTH_MAGIC
        0x07 => 0, // SET_VERSION: keep what the client asked for
        0x0c => {
            // GET_CAP
            let Some(cap) = rd(pml4, arg) else { return -EFAULT };
            let v = match cap {
                0x1 => 1,  // DUMB_BUFFER
                0x5 => 3,  // PRIME import | export
                0x6 => 1,  // TIMESTAMP_MONOTONIC
                0x8 | 0x9 => 64, // CURSOR_WIDTH/HEIGHT
                _ => 0,
            };
            if usermem::write_u64(pml4, arg + 8, v) { 0 } else { -EFAULT }
        }
        0x0d => 0, // SET_CLIENT_CAP
        0x40 => {
            // vmwgfx GET_PARAM { u64 value; u32 param; u32 pad }
            let Some(param) = rd32(pml4, arg + 8) else { return -EFAULT };
            // Shader model as Linux's vmwgfx decides it.
            let dc = |i: usize| g.dev_caps.get(i).copied().unwrap_or(0) != 0;
            let dx = g.caps & 0x1000_0000 != 0 && dc(95);
            let sm41 = dx && g.cap2 & 0x4 != 0 && dc(244);
            let sm5 = sm41 && g.cap2 & 0x400 != 0 && dc(258);
            let gl43 = sm5 && dc(261);
            let v = match param {
                0 | 1 => 0,                        // video overlay streams
                2 => 1,                            // 3D
                3 => g.caps as u64,                // HW_CAPS
                4 => g.fifo_caps as u64,           // FIFO_CAPS
                5 => g.vram,                       // MAX_FB_SIZE
                6 => g.fifo_hw_version as u64,     // FIFO_HW_VERSION
                7 => g.max_surface_kib * 1024,     // MAX_SURF_MEMORY
                8 => g.dev_caps.len() as u64 * 4,  // 3D_CAPS_SIZE (GB objects: flat devcap array)
                9 => g.mob_memory_kib * 1024,      // MAX_MOB_MEMORY
                10 => g.max_mob_bytes,             // MAX_MOB_SIZE
                11 => 1,                           // SCREEN_TARGET
                12 => dx as u64,                   // DX
                13 => g.cap2 as u64,               // HW_CAPS2
                14 => sm41 as u64,                 // SM4_1
                15 => sm5 as u64,                  // SM5
                16 => gl43 as u64,                 // GL43
                17 => 0x0405,                      // DEVICE_ID
                _ => return -EINVAL,
            };
            if usermem::write_u64(pml4, arg, v) { 0 } else { -EFAULT }
        }
        0x4d => {
            // vmwgfx GET_3D_CAP { u64 buffer; u32 max_size; u32 pad }
            let (Some(buf), Some(max)) = (rd(pml4, arg), rd32(pml4, arg + 8)) else { return -EFAULT };
            let bytes: alloc::vec::Vec<u8> = g.dev_caps.iter().flat_map(|v| v.to_le_bytes()).collect();
            let n = (max as usize).min(bytes.len());
            if usermem::write_bytes(pml4, buf, &bytes[..n]) { 0 } else { -EFAULT }
        }
        0x41 => bo_alloc(pml4, arg),                      // ALLOC_BO
        0x42 => {
            // HANDLE_CLOSE (buffers)
            let Some(h) = rd32(pml4, arg) else { return -EFAULT };
            bo_close(h);
            0
        }
        0x09 => {
            // GEM_CLOSE
            let Some(h) = rd32(pml4, arg) else { return -EFAULT };
            bo_close(h);
            0
        }
        0x5b => surface_create(pml4, arg),                // GB_SURFACE_CREATE_EXT
        0x57 => surface_create_legacy(pml4, arg),         // GB_SURFACE_CREATE
        0x5c | 0x58 => surface_ref(pml4, arg, nr == 0x5c), // GB_SURFACE_REF(_EXT)
        0x4a => {
            // UNREF_SURFACE
            let Some(sid) = rd32(pml4, arg) else { return -EFAULT };
            surface_destroy(sid);
            0
        }
        0x5a => context_create(pml4, arg),                // CREATE_EXTENDED_CONTEXT
        0x47 => {
            // CREATE_CONTEXT (legacy): make a DX one, Mesa uses DX here
            let Some(cid) = dx_context_new() else { return -ENOMEM };
            if usermem::write_u32(pml4, arg, cid) { 0 } else { -EFAULT }
        }
        0x48 => {
            // UNREF_CONTEXT
            let Some(cid) = rd32(pml4, arg) else { return -EFAULT };
            context_destroy(cid);
            0
        }
        0x4c => execbuf(pml4, arg),                       // EXECBUF
        0x4e => 0,                                        // FENCE_WAIT: work is done at submit
        0x4f => {
            // FENCE_SIGNALED: everything submitted has finished
            let ok = usermem::write_u32(pml4, arg + 8, 1)
                && usermem::write_u32(pml4, arg + 12, FENCE_SEQ.load(Ordering::Relaxed))
                && usermem::write_u32(pml4, arg + 16, 1);
            if ok { 0 } else { -EFAULT }
        }
        0x50 => 0, // FENCE_UNREF
        0x59 => 0, // SYNCCPU: the CPU and GPU never overlap here
        0x5d => {
            // MSG (messages to the host's log): accepted, no reply
            let _ = usermem::write_u32(pml4, arg + 20, 0);
            0
        }
        _ => {
            // Logged a few times only: the serial port is slow and a
            // probing loop would stall everything.
            static LOGGED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
            if LOGGED.fetch_add(1, core::sync::atomic::Ordering::Relaxed) < 16 {
                crate::kprintln!("drm: ioctl nr {:#x} (cmd {:#x}) not implemented yet", nr, cmd);
            }
            -EINVAL
        }
    }
}

/// The PCI device directory of the GPU as sysfs paths name it, and the
/// file inside it.
fn sys_device_file(path: &str) -> Option<&str> {
    for prefix in ["/sys/dev/char/226:0/device/", "/sys/dev/char/226:128/device/", "/sys/class/drm/card0/device/", "/sys/class/drm/renderD128/device/", "/sys/bus/pci/devices/0000:00:02.0/"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            return Some(rest);
        }
    }
    None
}

/// sysfs files libdrm reads to identify the GPU (drmGetDevice2), which is
/// how Mesa picks the vmwgfx driver.
pub fn sys_file(path: &str) -> Option<Vec<u8>> {
    use alloc::string::String;
    if !enabled() {
        return None;
    }
    for (name, minor) in NODES {
        for dir in ["/sys/dev/char/226:", "/sys/class/drm/"] {
            let base = if dir.ends_with(':') { alloc::format!("{}{}", dir, minor) } else { alloc::format!("{}{}", dir, name) };
            if path == alloc::format!("{}/uevent", base) {
                return Some(alloc::format!("MAJOR=226\nMINOR={}\nDEVNAME=dri/{}\n", minor, name).into_bytes());
            }
            if path == alloc::format!("{}/dev", base) {
                return Some(alloc::format!("226:{}\n", minor).into_bytes());
            }
        }
    }
    let f = sys_device_file(path)?;
    Some(match f {
        "vendor" | "subsystem_vendor" => String::from("0x15ad\n"),
        "device" | "subsystem_device" => String::from("0x0405\n"),
        "revision" => String::from("0x00\n"),
        "class" => String::from("0x030000\n"),
        "boot_vga" => String::from("1\n"),
        "uevent" => String::from("DRIVER=vmwgfx\nPCI_CLASS=30000\nPCI_ID=15AD:0405\nPCI_SUBSYS_ID=15AD:0405\nPCI_SLOT_NAME=0000:00:02.0\nMODALIAS=pci:v000015ADd00000405sv000015ADsd00000405bc03sc00i00\n"),
        "config" => {
            // The first 64 bytes of PCI configuration space.
            let mut c = [0u8; 64];
            c[0..2].copy_from_slice(&0x15adu16.to_le_bytes());
            c[2..4].copy_from_slice(&0x0405u16.to_le_bytes());
            c[0x0a] = 0x00;
            c[0x0b] = 0x03;
            c[0x2c..0x2e].copy_from_slice(&0x15adu16.to_le_bytes());
            c[0x2e..0x30].copy_from_slice(&0x0405u16.to_le_bytes());
            return Some(c.to_vec());
        }
        _ => return None,
    }.into_bytes())
}

/// readlink on sysfs: the PCI subsystem link, and plain directories
/// (EINVAL = "not a link", which realpath needs to walk them).
pub fn sys_readlink(path: &str) -> Option<Result<alloc::string::String, i64>> {
    if !enabled() || !path.starts_with("/sys") {
        return None;
    }
    if let Some(f) = sys_device_file(path) {
        return Some(match f {
            "subsystem" => Ok(alloc::string::String::from("/sys/bus/pci")),
            "driver" => Ok(alloc::string::String::from("/sys/bus/pci/drivers/vmwgfx")),
            _ => Err(-EINVAL),
        });
    }
    let dirs = ["/sys", "/sys/bus", "/sys/bus/pci", "/sys/bus/pci/devices", "/sys/bus/pci/drivers", "/sys/bus/pci/drivers/vmwgfx", "/sys/dev", "/sys/dev/char", "/sys/class", "/sys/class/drm"];
    let t = path.trim_end_matches('/');
    let dev = t.strip_prefix("/sys/dev/char/").is_some_and(|n| n == "226:0" || n == "226:128")
        || t.strip_prefix("/sys/class/drm/").is_some_and(|n| n == "card0" || n == "renderD128")
        || t.ends_with("/device") || t == "/sys/bus/pci/devices/0000:00:02.0";
    if dirs.contains(&t) || dev {
        return Some(Err(-EINVAL));
    }
    None
}

/// sysfs directories libdrm lists: the device's `drm` folder (its nodes).
pub fn sys_dir(path: &str) -> Option<Vec<(alloc::string::String, bool)>> {
    if !enabled() {
        return None;
    }
    let t = path.trim_end_matches('/');
    let f = sys_device_file(&alloc::format!("{}/", t)).map(|_| "")
        .or_else(|| sys_device_file(t));
    if f == Some("drm") {
        return Some(NODES.iter().map(|(n, _)| (alloc::string::String::from(*n), true)).collect());
    }
    if t == "/sys/class/drm" {
        return Some(NODES.iter().map(|(n, _)| (alloc::string::String::from(*n), true)).collect());
    }
    None
}

pub fn surface_exists(sid: u32) -> bool {
    OBJ.lock().surfaces.contains_key(&sid)
}

/// The memory behind a GPU surface after asking the GPU to copy its
/// current contents there (for showing a GPU-drawn window).
pub fn surface_readback(sid: u32) -> Option<Arc<Shm>> {
    let backup = OBJ.lock().surfaces.get(&sid).map(|s| s.backup)?;
    let mut c = Cmds::default();
    c.cmd(svga3d::CMD_READBACK_GB_SURFACE, &[sid]);
    if let Err(e) = svga3d::submit(&c) {
        gpu_log("readback for display", &e);
    }
    OBJ.lock().bos.get(&backup).map(|b| b.shm.clone())
}

/// The memory behind a GPU surface (without fetching its contents).
pub fn surface_memory(sid: u32) -> Option<Arc<Shm>> {
    let o = OBJ.lock();
    let backup = o.surfaces.get(&sid)?.backup;
    o.bos.get(&backup).map(|b| b.shm.clone())
}
