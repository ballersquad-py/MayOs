//! Programs started from the menu or a desktop entry: no terminal window,
//! their output goes to the kernel log (`dmesg`).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::proc::process::{self, Console, Process};
use crate::sync::Spin;

static RUNNING: Spin<Vec<(Arc<Process>, Arc<Console>, String)>> = Spin::new(Vec::new());

/// Start `cmd` ("program args...") in the background.
pub fn run(cwd: &str, cmd: &str) -> Result<(), String> {
    let cmd = cmd.trim();
    let (name, args) = cmd.split_once(char::is_whitespace).unwrap_or((cmd, ""));
    let path = super::shell::find_program(cwd, name).ok_or_else(|| alloc::format!("{}: not installed", name))?;
    let console = Console::new();
    let p = process::spawn(&path, args.trim(), cwd, console.clone())?;
    RUNNING.lock().push((p, console, String::from(name)));
    Ok(())
}

/// Drain output and reap finished programs (called by the window manager).
pub fn poll() {
    let mut list = RUNNING.lock();
    list.retain(|(p, c, name)| {
        let out = c.take_output();
        if !out.is_empty() {
            for line in String::from_utf8_lossy(&out).lines().filter(|l| !l.trim().is_empty()).take(20) {
                crate::kprintln!("{}: {}", name, line);
            }
        }
        if let Some(code) = p.has_exited() {
            crate::kprintln!("{}: exited ({})", name, code);
            process::reap(p.pid);
            return false;
        }
        true
    });
}
