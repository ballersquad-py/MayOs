//! Heap for user programs: a first-fit free list on memory obtained with
//! `sbrk`, with coalescing of adjacent free blocks.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;

use crate::sys;

const HEADER: usize = 16;
const CHUNK: usize = 64 * 1024;

struct Free {
    size: usize,
    next: *mut Free,
}

struct Heap {
    head: *mut Free,
}

static mut HEAP: Heap = Heap { head: null_mut() };

fn align_up(v: usize, a: usize) -> usize {
    (v + a - 1) & !(a - 1)
}

unsafe fn insert(h: &mut Heap, addr: usize, size: usize) {
    unsafe {
        let mut prev: *mut Free = null_mut();
        let mut cur = h.head;
        while !cur.is_null() && (cur as usize) < addr {
            prev = cur;
            cur = (*cur).next;
        }
        let b = addr as *mut Free;
        (*b).size = size;
        (*b).next = cur;
        if prev.is_null() {
            h.head = b;
        } else {
            (*prev).next = b;
        }
        if !cur.is_null() && addr + size == cur as usize {
            (*b).size += (*cur).size;
            (*b).next = (*cur).next;
        }
        if !prev.is_null() && prev as usize + (*prev).size == addr {
            (*prev).size += (*b).size;
            (*prev).next = (*b).next;
        }
    }
}

unsafe fn try_alloc(h: &mut Heap, layout: Layout) -> *mut u8 {
    unsafe {
        let align = layout.align().max(16);
        let size = align_up(layout.size().max(1), 16);
        let mut prev: *mut Free = null_mut();
        let mut cur = h.head;
        while !cur.is_null() {
            let start = cur as usize;
            let bsize = (*cur).size;
            let user = align_up(start + HEADER, align);
            let end = user + size;
            if end <= start + bsize {
                let remain = start + bsize - end;
                let (chunk, next) = if remain >= 32 {
                    let rest = end as *mut Free;
                    (*rest).size = remain;
                    (*rest).next = (*cur).next;
                    (end - start, rest)
                } else {
                    (bsize, (*cur).next)
                };
                if prev.is_null() {
                    h.head = next;
                } else {
                    (*prev).next = next;
                }
                let hdr = (user - HEADER) as *mut usize;
                *hdr = start;
                *hdr.add(1) = chunk;
                return user as *mut u8;
            }
            prev = cur;
            cur = (*cur).next;
        }
        null_mut()
    }
}

pub struct UserAlloc;

unsafe impl GlobalAlloc for UserAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe {
            let h = &mut *(&raw mut HEAP);
            let p = try_alloc(h, layout);
            if !p.is_null() {
                return p;
            }
            let need = align_up(layout.size() + layout.align() + HEADER * 2, 4096).max(CHUNK);
            let old = sys::syscall(sys::SBRK, need as u64, 0, 0);
            if old < 0 {
                return null_mut();
            }
            insert(h, old as usize, need);
            try_alloc(h, layout)
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        unsafe {
            let h = &mut *(&raw mut HEAP);
            let hdr = (ptr as usize - HEADER) as *const usize;
            insert(h, *hdr, *hdr.add(1));
        }
    }
}

#[global_allocator]
static ALLOC: UserAlloc = UserAlloc;
