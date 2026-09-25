//! Process control and time.

use crate::sys;

pub fn exit(code: i32) -> ! {
    sys::syscall(sys::EXIT, code as i64 as u64, 0, 0);
    loop {
        core::hint::spin_loop();
    }
}

pub fn pid() -> u64 {
    sys::syscall(sys::GETPID, 0, 0, 0) as u64
}

pub fn sleep_ms(ms: u64) {
    sys::syscall(sys::SLEEP, ms, 0, 0);
}

pub fn yield_now() {
    sys::syscall(sys::YIELD, 0, 0, 0);
}

pub fn uptime_ms() -> u64 {
    sys::syscall(sys::UPTIME, 0, 0, 0) as u64
}

/// Start a program; returns its pid.
pub fn spawn(path: &str, args: &str) -> Result<u64, i64> {
    let r = unsafe {
        sys::syscall4(sys::SPAWN, path.as_ptr() as u64, path.len() as u64, args.as_ptr() as u64, args.len() as u64)
    };
    if r < 0 { Err(r) } else { Ok(r as u64) }
}

/// Wait for a process to exit and return its exit code.
pub fn wait(pid: u64) -> i64 {
    sys::syscall(sys::WAIT, pid, 0, 0)
}

#[derive(Clone, Copy, Debug)]
pub struct DateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

pub fn now() -> DateTime {
    let v = sys::syscall(sys::TIME, 0, 0, 0) as u64;
    DateTime {
        year: (v >> 40) as u16,
        month: (v >> 32) as u8,
        day: (v >> 24) as u8,
        hour: (v >> 16) as u8,
        minute: (v >> 8) as u8,
        second: v as u8,
    }
}
