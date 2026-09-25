//! Write text to a file: `write <file> <text...>` (use `-a` to append).
#![no_std]
#![no_main]

use mstd::{format, fs, println, raw_args, sys, eprintln};

mstd::entry!(main);

fn main() -> i32 {
    let raw = raw_args().trim();
    let (append, rest) = match raw.strip_prefix("-a ") {
        Some(r) => (true, r.trim_start()),
        None => (false, raw),
    };
    let Some((file, text)) = rest.split_once(' ') else {
        eprintln!("usage: write [-a] <file> <text...>");
        return 1;
    };
    let line = format!("{}\n", text);
    let r = if append { fs::append(file, line.as_bytes()) } else { fs::write(file, line.as_bytes()) };
    match r {
        Ok(()) => {
            println!("{} {} bytes to {}", if append { "appended" } else { "wrote" }, line.len(), file);
            0
        }
        Err(e) => {
            eprintln!("write: {}: {}", file, sys::error_name(e));
            1
        }
    }
}
