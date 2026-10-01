//! Freeze detector: if the desktop stops drawing frames for 5 seconds
//! while the kernel still runs, write what every thread is doing and the
//! kernel log to /hang.txt (once per boot), so a frozen screen still
//! leaves something to diagnose after a restart.

use alloc::string::String;
use core::fmt::Write;
use core::sync::atomic::Ordering;

use crate::proc::sched;

pub extern "C" fn run(_: usize) {
    let mut last = crate::gui::FRAMES.load(Ordering::Relaxed);
    let mut stuck_since = crate::time::uptime_ms();
    loop {
        sched::sleep_ms(1000);
        let now = crate::gui::FRAMES.load(Ordering::Relaxed);
        let t = crate::time::uptime_ms();
        if now != last || last == 0 {
            last = now;
            stuck_since = t;
            continue;
        }
        if t - stuck_since >= 5000 {
            dump(t - stuck_since);
            return;
        }
    }
}

fn dump(ms: u64) {
    let mut r = String::new();
    let _ = writeln!(r, "MayOS freeze report: the desktop drew no frame for {} ms (uptime {} s)\n", ms, crate::time::uptime_ms() / 1000);
    let _ = writeln!(r, "== threads");
    for t in sched::list() {
        let [nr, a0, a1] = t.syscall;
        let what = if nr == sched::NO_SYSCALL { String::from("-") } else { alloc::format!("{}({:#x}, {:#x})", crate::proc::linux::syscall_name(nr), a0, a1) };
        let name = crate::proc::linux::thread_name(t.id).unwrap_or_default();
        let _ = writeln!(r, "{:>5} pid {:>4?} {:?} cpu {}ms {} {} {}", t.id, t.pid, t.state, t.cpu_ms, t.name, name, what);
    }
    let threads_end = r.len();
    let log = crate::log::contents();
    let mut from = log.len().saturating_sub(30000);
    while !log.is_char_boundary(from) {
        from += 1;
    }
    let _ = writeln!(r, "\n== kernel log (end)\n{}", &log[from..]);
    // The thread list on the serial port first: writing the file needs
    // the disk, which may be what is stuck.
    for line in r[..threads_end].lines().take(400) {
        crate::kprintln!("hang: {}", line);
    }
    crate::kprintln!("watchdog: desktop frozen for {} ms, writing /hang.txt", ms);
    let _ = crate::fs::write_file("/hang.txt", r.as_bytes());
    crate::kprintln!("watchdog: /hang.txt written");
}
