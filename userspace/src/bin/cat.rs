//! Print files, using the file system calls.
#![no_std]
#![no_main]

use mstd::{args, eprintln, fs, io, sys};

mstd::entry!(main);

fn main() -> i32 {
    let files = args();
    if files.is_empty() {
        eprintln!("usage: cat <file>...");
        return 1;
    }
    let mut status = 0;
    for f in files {
        match fs::read(f) {
            Ok(data) => {
                io::write(1, &data);
            }
            Err(e) => {
                eprintln!("cat: {}: {}", f, sys::error_name(e));
                status = 1;
            }
        }
    }
    status
}
