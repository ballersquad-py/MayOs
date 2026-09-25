//! Show the date, uptime and a few process facts; also spawns a child to
//! exercise spawn/wait.
#![no_std]
#![no_main]

use mstd::{println, process};

mstd::entry!(main);

fn main() -> i32 {
    let t = process::now();
    println!("Date:    {:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.year, t.month, t.day, t.hour, t.minute, t.second);
    let up = process::uptime_ms() / 1000;
    println!("Uptime:  {}h {:02}m {:02}s", up / 3600, (up / 60) % 60, up % 60);
    println!("PID:     {}", process::pid());
    let mut v = mstd::Vec::new();
    for i in 0..10_000u32 {
        v.push(i);
    }
    println!("Heap:    allocated a {}-element vector (sum {})", v.len(), v.iter().map(|&x| x as u64).sum::<u64>());
    match process::spawn("/bin/hello", "from sysinfo") {
        Ok(pid) => {
            let code = process::wait(pid);
            println!("Child:   /bin/hello (pid {}) exited with {}", pid, code);
        }
        Err(e) => println!("Child:   spawn failed ({})", e),
    }
    0
}
