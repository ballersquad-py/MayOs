//! The first MayOS user program.
#![no_std]
#![no_main]

use mstd::{args, println, process};

mstd::entry!(main);

fn main() -> i32 {
    println!("\x1b[92mHello from user space!\x1b[0m");
    println!("I am process {} and the system has been up for {} ms.", process::pid(), process::uptime_ms());
    let a = args();
    if a.is_empty() {
        println!("No arguments. Try: hello a b c");
    } else {
        println!("I got {} argument(s):", a.len());
        for (i, arg) in a.iter().enumerate() {
            println!("  {}: {}", i + 1, arg);
        }
    }
    0
}
