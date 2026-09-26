//! 4-level page tables.
//!
//! We keep running on the tables Limine built (they live in
//! bootloader-reclaimable memory, which we never reuse) and extend them. All
//! 256 upper-half PML4 slots are populated at boot so that every process
//! address space can share the kernel half by copying those slots.

use core::sync::atomic::{AtomicU64, Ordering};

use super::{phys_to_virt, pmm, PAGE_SIZE};
use crate::arch::cpu;
use crate::sync::Spin;

pub const PRESENT: u64 = 1;
pub const WRITABLE: u64 = 1 << 1;
pub const USER: u64 = 1 << 2;
pub const WRITE_THROUGH: u64 = 1 << 3;
pub const NO_CACHE: u64 = 1 << 4;
pub const HUGE: u64 = 1 << 7;
pub const NO_EXECUTE: u64 = 1 << 63;
/// Software bit: the page belongs to someone else (a shared buffer) and
/// must not be freed with the address space.
pub const BORROWED: u64 = 1 << 9;

const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

static KERNEL_PML4: AtomicU64 = AtomicU64::new(0);
/// Serialises page-table edits.
static LOCK: Spin<()> = Spin::new(());

pub fn kernel_pml4() -> u64 {
    KERNEL_PML4.load(Ordering::Relaxed)
}

fn table(phys: u64) -> &'static mut [u64; 512] {
    unsafe { &mut *(phys_to_virt(phys) as *mut [u64; 512]) }
}

pub fn init() {
    crate::arch::idt::enable_nx();
    let pml4 = cpu::read_cr3() & ADDR_MASK;
    KERNEL_PML4.store(pml4, Ordering::Relaxed);
    let t = table(pml4);
    for e in t.iter_mut().skip(256) {
        if *e & PRESENT == 0 {
            let f = pmm::alloc_frame_zeroed().expect("oom");
            *e = f | PRESENT | WRITABLE;
        }
    }
}

#[derive(Debug)]
pub enum MapError {
    OutOfMemory,
    HugePage,
}

fn index(virt: u64, level: u32) -> usize {
    ((virt >> (12 + 9 * (level - 1))) & 0x1ff) as usize
}

pub fn map(pml4: u64, virt: u64, phys: u64, flags: u64) -> Result<(), MapError> {
    let _g = LOCK.lock();
    let mut t = pml4;
    for level in (2..=4).rev() {
        let e = &mut table(t)[index(virt, level)];
        if *e & PRESENT == 0 {
            let f = pmm::alloc_frame_zeroed().ok_or(MapError::OutOfMemory)?;
            *e = f | PRESENT | WRITABLE | (flags & USER);
        } else if *e & HUGE != 0 {
            return Err(MapError::HugePage);
        } else if flags & USER != 0 {
            *e |= USER;
        }
        t = *e & ADDR_MASK;
    }
    table(t)[index(virt, 1)] = (phys & ADDR_MASK) | flags | PRESENT;
    cpu::invlpg(virt);
    Ok(())
}

/// Returns the physical address and flags of the page mapping `virt`.
pub fn translate(pml4: u64, virt: u64) -> Option<(u64, u64)> {
    let mut t = pml4;
    for level in (1..=4).rev() {
        let e = table(t)[index(virt, level)];
        if e & PRESENT == 0 {
            return None;
        }
        if level == 1 || (level <= 3 && e & HUGE != 0) {
            let page_size = 1u64 << (12 + 9 * (level - 1));
            return Some(((e & ADDR_MASK & !(page_size - 1)) + (virt & (page_size - 1)), e));
        }
        t = e & ADDR_MASK;
    }
    None
}

pub fn unmap(pml4: u64, virt: u64) -> Option<u64> {
    let _g = LOCK.lock();
    let mut t = pml4;
    for level in (2..=4).rev() {
        let e = table(t)[index(virt, level)];
        if e & PRESENT == 0 || e & HUGE != 0 {
            return None;
        }
        t = e & ADDR_MASK;
    }
    let e = &mut table(t)[index(virt, 1)];
    if *e & PRESENT == 0 {
        return None;
    }
    let phys = *e & ADDR_MASK;
    *e = 0;
    cpu::invlpg(virt);
    Some(phys)
}

/// Map device memory into the direct-map window (uncached) and return its
/// virtual address. Ranges that are already mapped are left untouched.
pub fn map_mmio(phys: u64, len: usize) -> u64 {
    let start = phys & !(PAGE_SIZE - 1);
    let end = (phys + len as u64).div_ceil(PAGE_SIZE) * PAGE_SIZE;
    let pml4 = kernel_pml4();
    let mut p = start;
    while p < end {
        let v = phys_to_virt(p);
        if translate(pml4, v).is_none() {
            map(pml4, v, p, WRITABLE | NO_CACHE | WRITE_THROUGH | NO_EXECUTE).expect("map_mmio failed");
        }
        p += PAGE_SIZE;
    }
    phys_to_virt(phys)
}

/// PTE flags selecting a write-combining memory type through the PAT.
/// Uses an existing WC entry if the bootloader set one up, otherwise
/// reprograms PAT entry 7 (normally UC, unused by us) as WC.
fn wc_flags() -> u64 {
    static FLAGS: Spin<Option<u64>> = Spin::new(None);
    let mut g = FLAGS.lock();
    if let Some(f) = *g {
        return f;
    }
    const IA32_PAT: u32 = 0x277;
    let pat = unsafe { cpu::rdmsr(IA32_PAT) };
    let idx = match (0..8).find(|i| (pat >> (i * 8)) & 0x7 == 0x01) {
        Some(i) => i,
        None => {
            let new = (pat & !(0xffu64 << 56)) | (0x01u64 << 56);
            unsafe {
                cpu::wrmsr(IA32_PAT, new);
                core::arch::asm!("wbinvd");
                cpu::write_cr3(cpu::read_cr3());
            }
            7
        }
    };
    let f = (if idx & 1 != 0 { WRITE_THROUGH } else { 0 })
        | (if idx & 2 != 0 { NO_CACHE } else { 0 })
        | (if idx & 4 != 0 { HUGE } else { 0 }); // bit 7 is the PAT bit in a 4 KiB PTE
    *g = Some(f);
    f
}

/// Map a framebuffer write-combining (fast sequential writes) into the
/// direct map. Pages that are already mapped keep their attributes.
/// Returns `None` unless every page of the range ends up mapped.
pub fn map_framebuffer(phys: u64, len: usize) -> Option<u64> {
    let flags = wc_flags();
    let start = phys & !(PAGE_SIZE - 1);
    let end = (phys + len as u64).div_ceil(PAGE_SIZE) * PAGE_SIZE;
    let pml4 = kernel_pml4();
    let mut p = start;
    while p < end {
        let v = phys_to_virt(p);
        if translate(pml4, v).is_none() {
            match map(pml4, v, p, WRITABLE | NO_EXECUTE | flags) {
                Ok(()) => {}
                Err(e) => {
                    crate::kprintln!("paging: cannot map framebuffer page {:#x}: {:?}", p, e);
                    return None;
                }
            }
        }
        p += PAGE_SIZE;
    }
    // Verify: a missing page here would be a page fault in the compositor.
    let mut p = start;
    while p < end {
        match translate(pml4, phys_to_virt(p)) {
            Some((mapped, _)) if mapped & !(PAGE_SIZE - 1) == p => {}
            other => {
                crate::kprintln!("paging: framebuffer page {:#x} maps to {:?}", p, other.map(|o| o.0));
                return None;
            }
        }
        p += PAGE_SIZE;
    }
    Some(phys_to_virt(phys))
}

/// A fresh address space sharing the kernel half.
pub fn new_address_space() -> Option<u64> {
    let pml4 = pmm::alloc_frame_zeroed()?;
    let k = table(kernel_pml4());
    let t = table(pml4);
    t[256..].copy_from_slice(&k[256..]);
    Some(pml4)
}

/// Free every user page and page table of an address space, then the PML4.
pub fn destroy_address_space(pml4: u64) {
    let _g = LOCK.lock();
    fn free_level(t: u64, level: u32) {
        for &e in table(t).iter() {
            if e & PRESENT == 0 {
                continue;
            }
            let child = e & ADDR_MASK;
            if level > 1 {
                free_level(child, level - 1);
            } else if e & BORROWED != 0 {
                continue;
            }
            pmm::free_frame(child);
        }
    }
    let top = table(pml4);
    for i in 0..256 {
        let e = top[i];
        if e & PRESENT != 0 {
            free_level(e & ADDR_MASK, 3);
            pmm::free_frame(e & ADDR_MASK);
        }
    }
    pmm::free_frame(pml4);
}
