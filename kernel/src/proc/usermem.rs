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
