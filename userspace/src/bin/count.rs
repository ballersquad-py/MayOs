//! Counts slowly, to show preemptive multitasking: the desktop stays
//! responsive while this runs.
#![no_std]
#![no_main]

use mstd::{args, println, process};

mstd::entry!(main);

fn main() -> i32 {
    let n: u32 = args().first().and_then(|a| a.parse().ok()).unwrap_or(10);
    for i in 1..=n {
        println!("{} / {}", i, n);
        process::sleep_ms(500);
    }
    println!("done");
    0
}
