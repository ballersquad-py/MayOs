//! Kernel heap: a first-fit free-list allocator with coalescing, backed by
//! physically contiguous chunks from the frame allocator (accessed through
//! the direct map). Each allocation carries a 16-byte header recording the
//! chunk it came from, so frees never need the caller's layout to match an
//! exact split.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;

use super::{phys_to_virt, pmm, PAGE_SIZE};
use crate::sync::Spin;

const INITIAL_SIZE: usize = 32 * 1024 * 1024;
const GROW_MIN: usize = 8 * 1024 * 1024;
const HEADER: usize = 16;
const MIN_BLOCK: usize = 32;

#[repr(C)]
struct FreeBlock {
    size: usize,
    next: *mut FreeBlock,
}

/// Small blocks come from per-size free lists (O(1) allocate and free),
/// refilled in chunks from the main list; everything else uses the
/// address-ordered main list.
const CLASSES: [usize; 14] = [16, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 1536, 2048];
const CHUNK: usize = 64 * 1024;

struct Heap {
    head: *mut FreeBlock,
    total: usize,
    used: usize,
    /// Free small blocks per class (singly linked through their first word).
    small: [*mut usize; CLASSES.len()],
}

fn class_of(layout: Layout) -> Option<usize> {
    if layout.align() > 16 {
        return None;
    }
    let size = layout.size().max(1);
    CLASSES.iter().position(|&c| c >= size)
}

unsafe impl Send for Heap {}

static HEAP: Spin<Heap> = Spin::new(Heap { head: null_mut(), total: 0, used: 0, small: [null_mut(); CLASSES.len()] });

fn align_up(v: usize, a: usize) -> usize {
    (v + a - 1) & !(a - 1)
}

impl Heap {
    /// Insert a free region, keeping the list sorted by address and merging
    /// with neighbours.
    unsafe fn insert(&mut self, addr: usize, size: usize) {
        unsafe {
            let mut prev: *mut FreeBlock = null_mut();
            let mut cur = self.head;
            while !cur.is_null() && (cur as usize) < addr {
                prev = cur;
                cur = (*cur).next;
            }
            let block = addr as *mut FreeBlock;
            (*block).size = size;
            (*block).next = cur;
            if prev.is_null() {
                self.head = block;
            } else {
                (*prev).next = block;
            }
            // Merge with next.
            if !cur.is_null() && addr + size == cur as usize {
                (*block).size += (*cur).size;
                (*block).next = (*cur).next;
            }
            // Merge with previous.
            if !prev.is_null() && prev as usize + (*prev).size == addr {
                (*prev).size += (*block).size;
                (*prev).next = (*block).next;
            }
        }
    }

    unsafe fn try_alloc(&mut self, layout: Layout) -> *mut u8 {
        unsafe {
            let align = layout.align().max(16);
            let size = align_up(layout.size().max(1), 16);
            let mut prev: *mut FreeBlock = null_mut();
            let mut cur = self.head;
            while !cur.is_null() {
                let start = cur as usize;
                let bsize = (*cur).size;
                let user = align_up(start + HEADER, align);
                let end = user + size;
                if end <= start + bsize {
                    let next = (*cur).next;
                    let remain = start + bsize - end;
                    let chunk_size;
                    let new_next;
                    if remain >= MIN_BLOCK {
                        let rest = end as *mut FreeBlock;
                        (*rest).size = remain;
                        (*rest).next = next;
                        new_next = rest;
                        chunk_size = end - start;
                    } else {
                        new_next = next;
                        chunk_size = bsize;
                    }
                    if prev.is_null() {
                        self.head = new_next;
                    } else {
                        (*prev).next = new_next;
                    }
                    let hdr = (user - HEADER) as *mut usize;
                    *hdr = start;
                    *hdr.add(1) = chunk_size;
                    self.used += chunk_size;
                    return user as *mut u8;
                }
                prev = cur;
                cur = (*cur).next;
            }
            null_mut()
        }
    }

    unsafe fn alloc_small(&mut self, class: usize) -> *mut u8 {
        unsafe {
            if self.small[class].is_null() {
                // Carve a chunk into blocks of this class.
                let chunk = self.try_alloc(Layout::from_size_align_unchecked(CHUNK, 16));
                let chunk = if chunk.is_null() && self.grow(CHUNK + HEADER * 2) {
                    self.try_alloc(Layout::from_size_align_unchecked(CHUNK, 16))
                } else {
                    chunk
                };
                if chunk.is_null() {
                    return null_mut();
                }
                let size = CLASSES[class];
                let n = CHUNK / size;
                for i in (0..n).rev() {
                    let b = chunk.add(i * size) as *mut usize;
                    *b = self.small[class] as usize;
                    self.small[class] = b;
                }
            }
            let b = self.small[class];
            self.small[class] = *b as *mut usize;
            b as *mut u8
        }
    }

    fn grow(&mut self, at_least: usize) -> bool {
        let bytes = align_up(at_least.max(GROW_MIN), PAGE_SIZE as usize);
        let pages = bytes / PAGE_SIZE as usize;
        match pmm::alloc_contiguous(pages) {
            Some(phys) => {
                unsafe { self.insert(phys_to_virt(phys) as usize, bytes) };
                self.total += bytes;
                true
            }
            None => false,
        }
    }
}

pub struct KernelAllocator;

unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut h = HEAP.lock();
        unsafe {
            if let Some(c) = class_of(layout) {
                return h.alloc_small(c);
            }
            let p = h.try_alloc(layout);
            if !p.is_null() {
                return p;
            }
            if h.grow(layout.size() + layout.align() + HEADER * 2) {
                return h.try_alloc(layout);
            }
        }
        null_mut()
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let mut h = HEAP.lock();
        unsafe {
            if let Some(c) = class_of(layout) {
                let b = ptr as *mut usize;
                *b = h.small[c] as usize;
                h.small[c] = b;
                return;
            }
            let hdr = (ptr as usize - HEADER) as *const usize;
            let start = *hdr;
            let size = *hdr.add(1);
            h.used -= size;
            h.insert(start, size);
        }
    }
}

#[global_allocator]
static ALLOCATOR: KernelAllocator = KernelAllocator;

pub fn init() {
    let mut h = HEAP.lock();
    assert!(h.grow(INITIAL_SIZE), "cannot allocate the initial kernel heap");
}

/// (used bytes, total bytes)
pub fn stats() -> (usize, usize) {
    let h = HEAP.lock();
    (h.used, h.total)
}
