//! List a directory through the readdir system call.
#![no_std]
#![no_main]

use mstd::{args, eprintln, fs, println, sys};

mstd::entry!(main);

fn main() -> i32 {
    let a = args();
    let path = a.first().copied().unwrap_or(".");
    match fs::read_dir(path) {
        Ok(entries) => {
            for e in entries {
                if e.is_dir {
                    println!("\x1b[94m{}/\x1b[0m", e.name);
                } else {
                    println!("{:<32} {:>8} bytes", e.name, e.size);
                }
            }
            0
        }
        Err(e) => {
            eprintln!("ls: {}: {}", path, sys::error_name(e));
            1
        }
    }
}
