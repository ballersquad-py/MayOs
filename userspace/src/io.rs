//! Console input and output.

use alloc::string::String;
use core::fmt::{self, Write};

use crate::sys;

pub fn write(fd: u64, data: &[u8]) -> i64 {
    sys::syscall(sys::WRITE, fd, data.as_ptr() as u64, data.len() as u64)
}

struct Stdout(u64);

impl Write for Stdout {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write(self.0, s.as_bytes());
        Ok(())
    }
}

pub fn print_fmt(args: fmt::Arguments) {
    let _ = Stdout(1).write_fmt(args);
}

pub fn eprint_fmt(args: fmt::Arguments) {
    let _ = Stdout(2).write_fmt(args);
}

/// Read one line from the terminal (without the newline). `None` at EOF.
pub fn read_line() -> Option<String> {
    let mut out = alloc::vec::Vec::new();
    let mut buf = [0u8; 64];
    loop {
        let n = sys::syscall(sys::READ, 0, buf.as_mut_ptr() as u64, buf.len() as u64);
        if n <= 0 {
            return if out.is_empty() { None } else { Some(String::from_utf8_lossy(&out).into_owned()) };
        }
        for &b in &buf[..n as usize] {
            if b == b'\n' {
                return Some(String::from_utf8_lossy(&out).into_owned());
            }
            out.push(b);
        }
    }
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { $crate::io::print_fmt(format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => { $crate::io::print_fmt(format_args!("{}\n", format_args!($($arg)*))) };
}

#[macro_export]
macro_rules! eprintln {
    ($($arg:tt)*) => { $crate::io::eprint_fmt(format_args!("{}\n", format_args!($($arg)*))) };
}
