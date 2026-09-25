//! Raw system calls. See kernel/src/proc/syscall.rs for the numbers.

use core::arch::asm;

pub const EXIT: u64 = 0;
pub const WRITE: u64 = 1;
pub const READ: u64 = 2;
pub const OPEN: u64 = 3;
pub const CLOSE: u64 = 4;
pub const SBRK: u64 = 5;
pub const SLEEP: u64 = 6;
pub const UPTIME: u64 = 7;
pub const GETPID: u64 = 8;
pub const READDIR: u64 = 9;
pub const MKDIR: u64 = 10;
pub const UNLINK: u64 = 11;
pub const RENAME: u64 = 12;
pub const YIELD: u64 = 13;
pub const TIME: u64 = 14;
pub const SEEK: u64 = 15;
pub const SPAWN: u64 = 16;
pub const WAIT: u64 = 17;

#[inline]
pub unsafe fn syscall4(n: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let ret: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") n as i64 => ret,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            in("r10") a3,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack)
        );
    }
    ret
}

#[inline]
pub fn syscall(n: u64, a0: u64, a1: u64, a2: u64) -> i64 {
    unsafe { syscall4(n, a0, a1, a2, 0) }
}

pub fn error_name(code: i64) -> &'static str {
    match code {
        -2 => "no such file or directory",
        -5 => "I/O error",
        -9 => "bad file descriptor",
        -12 => "out of memory",
        -14 => "bad address",
        -17 => "already exists",
        -20 => "not a directory",
        -21 => "is a directory",
        -22 => "invalid argument",
        -28 => "no space left on device",
        -38 => "not implemented",
        -39 => "directory not empty",
        _ => "error",
    }
}
