//! ELF64 executable loader.

use alloc::string::String;

use crate::mem::paging::{self, NO_EXECUTE, USER, WRITABLE};
use crate::mem::{phys_to_virt, pmm, PAGE_SIZE};

use super::usermem::USER_TOP;

pub struct LoadedImage {
    pub entry: u64,
    pub brk: u64,
    /// Where the program headers are in memory (Linux AT_PHDR).
    pub phdr: u64,
    pub phnum: u64,
    pub phent: u64,
    /// Load bias (non-zero for position-independent executables).
    pub base: u64,
    /// Built for Linux (has thread-local storage, or is a static PIE).
    pub linux: bool,
}

/// Where static position-independent Linux executables are placed.
const PIE_BASE: u64 = 0x0000_5555_5555_0000;

fn u16at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

pub fn is_elf(data: &[u8]) -> bool {
    data.len() >= 64 && &data[..4] == b"\x7fELF"
}

/// Map every PT_LOAD segment of `data` into the address space `pml4`.
pub fn load(pml4: u64, data: &[u8]) -> Result<LoadedImage, String> {
    if !is_elf(data) {
        return Err("not an ELF file".into());
    }
    if data[4] != 2 || data[5] != 1 || u16at(data, 18) != 0x3e {
        return Err("not a 64-bit little-endian x86_64 ELF".into());
    }
    let etype = u16at(data, 16);
    if etype != 2 && etype != 3 {
        return Err("not an executable".into());
    }
    let phoff = u64at(data, 32) as usize;
    let phentsize = u16at(data, 54) as usize;
    let phnum = u16at(data, 56) as usize;
    let mut has_tls = false;
    let mut first_load = None;
    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        if ph + 56 > data.len() {
            return Err("truncated program header".into());
        }
        match u32at(data, ph) {
            3 => return Err("dynamically linked programs are not supported (build it static, e.g. with musl)".into()),
            7 => has_tls = true,
            1 if first_load.is_none() => first_load = Some((u64at(data, ph + 16), u64at(data, ph + 8))),
            _ => {}
        }
    }
    let base = if etype == 3 { PIE_BASE } else { 0 };
    let entry = u64at(data, 24) + base;
    let mut brk = 0u64;
    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        if u32at(data, ph) != 1 {
            continue; // not PT_LOAD
        }
        let flags = u32at(data, ph + 4);
        let offset = u64at(data, ph + 8) as usize;
        let vaddr = u64at(data, ph + 16) + base;
        let filesz = u64at(data, ph + 32) as usize;
        let memsz = u64at(data, ph + 40);
        if memsz == 0 {
            continue;
        }
        let end = vaddr.checked_add(memsz).ok_or("segment overflow")?;
        if vaddr < 0x1000 || end > USER_TOP || offset + filesz > data.len() || filesz as u64 > memsz {
            return Err("segment outside user space".into());
        }
        let mut pflags = USER;
        if flags & 2 != 0 {
            pflags |= WRITABLE;
        }
        if flags & 1 == 0 {
            pflags |= NO_EXECUTE;
        }
        let mut page = vaddr & !(PAGE_SIZE - 1);
        while page < end {
            let frame = match paging::translate(pml4, page) {
                Some((phys, f)) => {
                    // Page shared with a previous segment: widen permissions.
                    let merged = USER | ((f | pflags) & WRITABLE) | (f & pflags & NO_EXECUTE);
                    paging::map(pml4, page, phys, merged).map_err(|_| "map failed")?;
                    phys & !(PAGE_SIZE - 1)
                }
                None => {
                    let f = pmm::alloc_frame_zeroed().ok_or("out of memory")?;
                    paging::map(pml4, page, f, pflags).map_err(|_| "map failed")?;
                    f
                }
            };
            // Copy the part of the file that lands in this page.
            let page_start = page.max(vaddr);
            let page_end = (page + PAGE_SIZE).min(vaddr + filesz as u64);
            if page_end > page_start {
                let src = offset + (page_start - vaddr) as usize;
                let n = (page_end - page_start) as usize;
                let dst = phys_to_virt(frame) + (page_start - page);
                unsafe { core::ptr::copy_nonoverlapping(data[src..].as_ptr(), dst as *mut u8, n) };
            }
            page += PAGE_SIZE;
        }
        brk = brk.max(end);
    }
    if entry == 0 || brk == 0 {
        return Err("no loadable segments".into());
    }
    let phdr = first_load.map(|(v, o)| v + base - o + phoff as u64).unwrap_or(0);
    Ok(LoadedImage {
        entry,
        brk: brk.div_ceil(PAGE_SIZE) * PAGE_SIZE,
        phdr,
        phnum: phnum as u64,
        phent: phentsize as u64,
        base,
        linux: has_tls || etype == 3,
    })
}
