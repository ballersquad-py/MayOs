//! Physical frame allocator: one bit per 4 KiB frame (1 = used).

use super::{phys_to_virt, PAGE_SIZE};
use crate::boot::{MemmapResponse, MEMMAP_USABLE};
use crate::sync::Spin;

struct Bitmap {
    bits: *mut u64,
    frames: usize,
    free: usize,
    hint: usize,
}

unsafe impl Send for Bitmap {}

static PMM: Spin<Bitmap> = Spin::new(Bitmap { bits: core::ptr::null_mut(), frames: 0, free: 0, hint: 0 });

impl Bitmap {
    fn is_used(&self, f: usize) -> bool {
        unsafe { *self.bits.add(f / 64) & (1 << (f % 64)) != 0 }
    }
    fn set(&mut self, f: usize) {
        unsafe { *self.bits.add(f / 64) |= 1 << (f % 64) }
    }
    fn clear(&mut self, f: usize) {
        unsafe { *self.bits.add(f / 64) &= !(1 << (f % 64)) }
    }
}

pub fn init(memmap: &MemmapResponse) {
    let top = memmap
        .entries()
        .filter(|e| e.kind == MEMMAP_USABLE)
        .map(|e| e.base + e.length)
        .max()
        .unwrap_or(0);
    let frames = (top / PAGE_SIZE) as usize;
    let bytes = frames.div_ceil(64) * 8;

    let region = memmap
        .entries()
        .find(|e| e.kind == MEMMAP_USABLE && e.length >= bytes as u64 && e.base >= 0x100000)
        .expect("no room for the frame bitmap");
    let bits = phys_to_virt(region.base) as *mut u64;
    unsafe { core::ptr::write_bytes(bits as *mut u8, 0xff, bytes) };

    let mut bm = PMM.lock();
    bm.bits = bits;
    bm.frames = frames;
    for e in memmap.entries().filter(|e| e.kind == MEMMAP_USABLE) {
        let start = e.base.div_ceil(PAGE_SIZE) as usize;
        let end = ((e.base + e.length) / PAGE_SIZE) as usize;
        for f in start..end {
            bm.clear(f);
            bm.free += 1;
        }
    }
    let bitmap_start = (region.base / PAGE_SIZE) as usize;
    for f in bitmap_start..bitmap_start + bytes.div_ceil(PAGE_SIZE as usize) {
        if !bm.is_used(f) {
            bm.set(f);
            bm.free -= 1;
        }
    }
    // Never hand out the first megabyte (legacy BIOS areas, null page).
    for f in 0..256.min(frames) {
        if !bm.is_used(f) {
            bm.set(f);
            bm.free -= 1;
        }
    }
    bm.hint = 256;
}

pub fn alloc_frame() -> Option<u64> {
    alloc_contiguous(1)
}

pub fn alloc_frame_zeroed() -> Option<u64> {
    let f = alloc_frame()?;
    unsafe { core::ptr::write_bytes(phys_to_virt(f) as *mut u8, 0, PAGE_SIZE as usize) };
    Some(f)
}

/// First-fit search for `n` consecutive free frames.
pub fn alloc_contiguous(n: usize) -> Option<u64> {
    let mut bm = PMM.lock();
    if n == 0 || bm.free < n {
        return None;
    }
    let frames = bm.frames;
    let mut tries = 0;
    let mut start = bm.hint;
    while tries < 2 {
        let mut f = start;
        while f + n <= frames {
            // Skip whole used words quickly.
            if f % 64 == 0 && unsafe { *bm.bits.add(f / 64) } == u64::MAX {
                f += 64;
                continue;
            }
            if bm.is_used(f) {
                f += 1;
                continue;
            }
            let mut len = 0;
            while len < n && !bm.is_used(f + len) {
                len += 1;
            }
            if len == n {
                for i in f..f + n {
                    bm.set(i);
                }
                bm.free -= n;
                if n == 1 {
                    bm.hint = f + 1;
                }
                return Some(f as u64 * PAGE_SIZE);
            }
            f += len + 1;
        }
        start = 256;
        tries += 1;
    }
    None
}

pub fn free_frame(phys: u64) {
    free_contiguous(phys, 1)
}

pub fn free_contiguous(phys: u64, n: usize) {
    let mut bm = PMM.lock();
    let f0 = (phys / PAGE_SIZE) as usize;
    for f in f0..f0 + n {
        assert!(bm.is_used(f), "double free of frame {:#x}", f as u64 * PAGE_SIZE);
        bm.clear(f);
    }
    bm.free += n;
    if f0 < bm.hint {
        bm.hint = f0;
    }
}

/// (free frames, total frames)
pub fn stats() -> (usize, usize) {
    let bm = PMM.lock();
    (bm.free, bm.frames)
}
