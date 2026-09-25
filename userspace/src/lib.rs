//! mstd: the MayOS user-space standard library.
//!
//! Programs are `#![no_std]` + `#![no_main]` and declare their entry point
//! with `mstd::entry!(main)`, where `main` returns an exit code.

#![no_std]

extern crate alloc;

pub mod fs;
pub mod heap;
pub mod io;
pub mod process;
pub mod sys;

pub use alloc::{format, string::String, vec, vec::Vec};

static mut ARGS: &str = "";

/// Called by `entry!` before `main`.
#[doc(hidden)]
pub unsafe fn init(args: *const u8, len: usize) {
    let s = unsafe { core::slice::from_raw_parts(args, len) };
    unsafe { ARGS = core::str::from_utf8(s).unwrap_or("") };
}

/// Command-line arguments (without the program name), split on spaces.
pub fn args() -> Vec<&'static str> {
    raw_args().split_whitespace().collect()
}

/// The full argument string as typed.
pub fn raw_args() -> &'static str {
    unsafe { ARGS }
}

#[macro_export]
macro_rules! entry {
    ($main:path) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn _start(args: *const u8, len: usize) -> ! {
            unsafe { $crate::init(args, len) };
            let code: i32 = $main();
            $crate::process::exit(code)
        }
    };
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    io::eprint_fmt(format_args!("\npanic: {}\n", info));
    process::exit(101)
}
