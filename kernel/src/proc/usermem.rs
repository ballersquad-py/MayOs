//! Validated access to user-space memory from system calls.

use alloc::string::String;
use alloc::vec::Vec;

use crate::mem::paging::{self, USER, WRITABLE};

pub const USER_TOP: u64 = 0x0000_7fff_ffff_f000;

fn range_ok(pml4: u64, addr: u64, len: u64, write: bool) -> bool {
    if len == 0 {
        return true;
    }
    let Some(end) = addr.checked_add(len) else { return false };
    if end > USER_TOP {
        return false;
    }
    let mut page = addr & !0xfff;
    while page < end {
        match paging::translate(pml4, page) {
            Some((_, flags)) if flags & USER != 0 && (!write || flags & WRITABLE != 0) => {}
            // Linux programs' memory is handed out on first use.
            None if super::linux::fault_in(page, write) => {}
            _ => return false,
        }
        page += 0x1000;
    }
    true
}

pub fn read_bytes(pml4: u64, addr: u64, len: u64) -> Option<Vec<u8>> {
    if len > 64 * 1024 * 1024 || !range_ok(pml4, addr, len, false) {
        return None;
    }
    let mut v = alloc::vec![0u8; len as usize];
    unsafe { core::ptr::copy_nonoverlapping(addr as *const u8, v.as_mut_ptr(), len as usize) };
    Some(v)
}

pub fn read_str(pml4: u64, addr: u64, len: u64) -> Option<String> {
    if len > 4096 {
        return None;
    }
    String::from_utf8(read_bytes(pml4, addr, len)?).ok()
}

pub fn write_bytes(pml4: u64, addr: u64, data: &[u8]) -> bool {
    if !range_ok(pml4, addr, data.len() as u64, true) {
        return false;
    }
    unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), addr as *mut u8, data.len()) };
    true
}

/// A NUL-terminated string from user memory (at most `max` bytes).
pub fn read_cstr(pml4: u64, addr: u64, max: usize) -> Option<String> {
    let mut out = Vec::new();
    let mut a = addr;
    while out.len() < max {
        // Read up to the end of the current page at a time.
        let chunk = (0x1000 - (a & 0xfff)).min((max - out.len()) as u64);
        let b = read_bytes(pml4, a, chunk)?;
        if let Some(p) = b.iter().position(|&c| c == 0) {
            out.extend_from_slice(&b[..p]);
            return String::from_utf8(out).ok();
        }
        out.extend_from_slice(&b);
        a += chunk;
    }
    None
}

pub fn read_u64(pml4: u64, addr: u64) -> Option<u64> {
    let b = read_bytes(pml4, addr, 8)?;
    Some(u64::from_le_bytes(b.try_into().ok()?))
}

pub fn write_u32(pml4: u64, addr: u64, v: u32) -> bool {
    write_bytes(pml4, addr, &v.to_le_bytes())
}

pub fn write_u64(pml4: u64, addr: u64, v: u64) -> bool {
    write_bytes(pml4, addr, &v.to_le_bytes())
}
