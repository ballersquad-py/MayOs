//! Boot-time self-test (kernel command line contains `selftest`).
//!
//! Exercises the file system, the shell, the explorer, user processes and
//! the desktop against the real virtio disk, then exits QEMU with a status
//! code. `tools/run-selftest.sh` runs it and afterwards checks the disk
//! image with `fsck.fat`.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::fs;
use crate::gui::app::{AppEvent, Ctx, Msg};
use crate::gui::explorer::Explorer;
use crate::gui::terminal::Terminal;
use crate::gui::{app::App, shell};
use crate::proc::process::{self, Console};
use crate::proc::sched;

type TestResult = Result<(), String>;

macro_rules! ensure {
    ($cond:expr, $($arg:tt)*) => {
        if !$cond {
            return Err(format!($($arg)*));
        }
    };
}

fn fs_roundtrip() -> TestResult {
    let _ = fs::remove_all("/selftest");
    fs::create_dir("/selftest").map_err(|e| format!("mkdir: {}", e))?;
    let name = "/selftest/A file with a long name.txt";
    fs::write_file(name, b"hello disk").map_err(|e| format!("write: {}", e))?;
    ensure!(fs::read_file(name).map_err(|e| format!("{}", e))? == b"hello disk", "content mismatch");
    let big: Vec<u8> = (0..300_000u32).map(|i| (i * 31 % 251) as u8).collect();
    fs::write_file("/selftest/big.bin", &big).map_err(|e| format!("write big: {}", e))?;
    ensure!(fs::read_file("/selftest/big.bin").map_err(|e| format!("{}", e))? == big, "big file mismatch");
    fs::create_dir("/selftest/sub").map_err(|e| format!("{}", e))?;
    fs::rename(name, "/selftest/sub/renamed.txt").map_err(|e| format!("rename: {}", e))?;
    ensure!(!fs::exists(name), "old name still exists");
    fs::copy("/selftest/sub", "/selftest/sub copy").map_err(|e| format!("copy: {}", e))?;
    ensure!(fs::read_file("/selftest/sub copy/renamed.txt").map_err(|e| format!("{}", e))? == b"hello disk", "copy mismatch");
    let names: Vec<String> = fs::read_dir("/selftest").map_err(|e| format!("{}", e))?.into_iter().map(|e| e.name).collect();
    ensure!(names == ["sub", "sub copy", "big.bin"], "unexpected listing {:?}", names);
    fs::remove_all("/selftest/sub copy").map_err(|e| format!("remove_all: {}", e))?;
    ensure!(!fs::exists("/selftest/sub copy"), "remove_all left files");
    Ok(())
}

fn terminal_text(t: &Terminal) -> String {
    t.plain_text()
}

fn shell_commands() -> TestResult {
    let mut t = Terminal::new();
    let mut ctx = Ctx::new(0);
    for cmd in [
        "mkdir -p /selftest/shell/deep",
        "cd /selftest/shell",
        "echo first line > notes.txt",
        "echo second line >> notes.txt",
        "cp notes.txt deep",
        "mv deep/notes.txt deep/moved.txt",
        "cat deep/moved.txt",
        "ls",
    ] {
        shell::run(&mut t, cmd, &mut ctx);
    }
    let text = terminal_text(&t);
    ensure!(text.contains("first line\nsecond line"), "cat output missing:\n{}", text);
    ensure!(text.contains("notes.txt"), "ls output missing:\n{}", text);
    ensure!(fs::exists("/selftest/shell/deep/moved.txt"), "mv did not happen");
    shell::run(&mut t, "rm -r /selftest/shell", &mut ctx);
    ensure!(!fs::exists("/selftest/shell"), "rm -r failed");
    shell::run(&mut t, "nonexistent-command", &mut ctx);
    ensure!(terminal_text(&t).contains("command not found"), "missing error");
    Ok(())
}

fn explorer_actions() -> TestResult {
    fs::create_dir("/selftest/ex").map_err(|e| format!("{}", e))?;
    let mut ex = Explorer::new("/selftest/ex");
    ex.test_dialog_result(1, Some("My Folder".into()));
    ex.test_dialog_result(2, Some("readme.md".into()));
    ensure!(fs::is_dir("/selftest/ex/My Folder"), "explorer did not create folder");
    ensure!(fs::exists("/selftest/ex/readme.md"), "explorer did not create file");
    ex.test_select("readme.md");
    ex.test_rename("README.md");
    ensure!(fs::exists("/selftest/ex/README.md"), "rename failed");
    ex.test_select("My Folder");
    ex.test_delete();
    ensure!(!fs::exists("/selftest/ex/My Folder"), "delete failed");
    // Events must not panic.
    let mut ctx = Ctx::new(0);
    ex.event(&AppEvent::Message(Msg::DialogResult { tag: 99, value: None }), &mut ctx);
    Ok(())
}

fn run_program(path: &str, args: &str, timeout_ms: u64) -> Result<(i64, String), String> {
    let console = Console::new();
    let p = process::spawn(path, args, "/", console.clone())?;
    let start = crate::time::uptime_ms();
    let mut out = Vec::new();
    loop {
        out.extend(console.take_output());
        if let Some(code) = p.has_exited() {
            out.extend(console.take_output());
            process::reap(p.pid);
            return Ok((code, String::from_utf8_lossy(&out).into_owned()));
        }
        if crate::time::uptime_ms() - start > timeout_ms {
            process::kill(&p);
            return Err(format!("{} timed out; output so far: {}", path, String::from_utf8_lossy(&out)));
        }
        sched::sleep_ms(10);
    }
}

fn user_programs() -> TestResult {
    let before = crate::mem::pmm::stats().0;
    let (code, out) = run_program("/bin/hello", "alpha beta", 5000)?;
    ensure!(code == 0, "hello exited with {}", code);
    ensure!(out.contains("Hello from user space") && out.contains("2: beta"), "hello output: {}", out);

    let (code, out) = run_program("/bin/write", "/selftest/from-user.txt written by a process", 5000)?;
    ensure!(code == 0, "write exited with {}: {}", code, out);
    let data = fs::read_file("/selftest/from-user.txt").map_err(|e| format!("{}", e))?;
    ensure!(data == b"written by a process\n", "file content {:?}", String::from_utf8_lossy(&data));

    let (code, out) = run_program("/bin/cat", "/selftest/from-user.txt", 5000)?;
    ensure!(code == 0 && out.contains("written by a process"), "cat: {} {}", code, out);

    let (code, out) = run_program("/bin/ls", "/selftest", 5000)?;
    ensure!(code == 0 && out.contains("from-user.txt"), "ls: {} {}", code, out);

    let (code, out) = run_program("/bin/sysinfo", "", 8000)?;
    ensure!(code == 0 && out.contains("exited with 0"), "sysinfo: {} {}", code, out);

    // Give the scheduler a moment to reap threads, then check for leaks.
    sched::sleep_ms(100);
    let after = crate::mem::pmm::stats().0;
    ensure!(before.abs_diff(after) < 64, "frame leak: {} free before, {} after", before, after);
    Ok(())
}

fn desktop_is_drawing() -> TestResult {
    let start = crate::time::uptime_ms();
    while crate::gui::FRAMES.load(core::sync::atomic::Ordering::Relaxed) < 3 {
        ensure!(crate::time::uptime_ms() - start < 20_000, "desktop did not draw any frames");
        sched::sleep_ms(50);
    }
    Ok(())
}

pub extern "C" fn run(_: usize) {
    crate::kprintln!("selftest: starting");
    let tests: &[(&str, fn() -> TestResult)] = &[
        ("fs_roundtrip", fs_roundtrip),
        ("shell_commands", shell_commands),
        ("explorer_actions", explorer_actions),
        ("user_programs", user_programs),
        ("desktop_is_drawing", desktop_is_drawing),
    ];
    let mut failed = 0;
    if !fs::is_mounted() {
        crate::kprintln!("selftest: FAIL no disk mounted");
        crate::power::qemu_exit(false);
    }
    for (name, test) in tests {
        match test() {
            Ok(()) => crate::kprintln!("selftest: ok   {}", name),
            Err(e) => {
                failed += 1;
                crate::kprintln!("selftest: FAIL {}: {}", name, e);
            }
        }
    }
    // Leave the disk tidy apart from the directory fsck will inspect.
    let _ = fs::remove_all("/selftest/ex");
    if failed == 0 {
        crate::kprintln!("selftest: PASS");
    } else {
        crate::kprintln!("selftest: {} test(s) FAILED", failed);
    }
    crate::power::qemu_exit(failed == 0);
}
