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

pub const MAJOR: u64 = 226;

/// Device names under /dev/dri with their minor numbers.
pub const NODES: [(&str, u64); 2] = [("card0", 0), ("renderD128", 128)];

pub fn enabled() -> bool {
    crate::fs::exists("/etc/gpu3d") && vmware_svga::gpu_info().is_some_and(|g| g.caps & 0x0800_0000 != 0)
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
