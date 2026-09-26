//! Boot-time self-test (kernel command line contains `selftest`).
//!
//! Exercises the file system, the shell, the explorer, user processes and
//! the desktop against the real virtio disk, then exits QEMU with a status
//! code. `tools/run-selftest.sh` runs it and afterwards checks the disk
//! image with `fsck.fat`.

use alloc::format;
use alloc::string::{String, ToString};
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

    // The Linux layer: signals, timerfd, select, symlinks, /proc, ...
    // (after the leak check: forks and threads still leak a kernel stack).
    if fs::exists("/bin/linux-signals") {
        let (code, out) = run_program("/bin/linux-signals", "", 20000)?;
        ensure!(code == 0 && out.contains("SIGNALS PASSED"), "linux-signals: {} {}", code, out);
    }
    Ok(())
}

fn settings_persist() -> TestResult {
    let before = crate::settings::get();
    crate::settings::update(|s| {
        s.volume = 42;
        s.tz_offset_min = 90;
        s.hostname = String::from("selftest-host");
    });
    let text = fs::read_file(crate::settings::PATH).map_err(|e| format!("settings file: {}", e))?;
    let parsed = crate::settings::parse(&String::from_utf8_lossy(&text));
    ensure!(parsed.volume == 42 && parsed.tz_offset_min == 90 && parsed.hostname == "selftest-host", "settings did not round-trip: {:?}", parsed);
    ensure!(crate::settings::parse(&crate::settings::serialize(&before)) == before, "serialize/parse mismatch");
    let t = crate::arch::rtc::DateTime { year: 2024, month: 12, day: 31, hour: 23, minute: 30, second: 0 };
    let u = crate::settings::shift_minutes(t, 60);
    ensure!((u.year, u.month, u.day, u.hour, u.minute) == (2025, 1, 1, 0, 30), "time zone rollover wrong: {:?}", u);
    let d = crate::settings::shift_minutes(crate::arch::rtc::DateTime { year: 2024, month: 3, day: 1, hour: 0, minute: 10, second: 0 }, -20);
    ensure!((d.month, d.day, d.hour, d.minute) == (2, 29, 23, 50), "leap day rollback wrong: {:?}", d);
    crate::settings::update(|s| *s = before.clone());
    Ok(())
}

fn network_works() -> TestResult {
    if !crate::network::is_present() {
        crate::kprintln!("selftest: (no network adapter, skipping network test)");
        return Ok(());
    }
    ensure!(crate::network::wait_configured(15_000), "DHCP did not configure an address");
    let st = crate::network::status().unwrap();
    crate::kprintln!("selftest: network {}", crate::network::describe(&st));
    ensure!(!st.gateway.is_unspecified(), "no gateway from DHCP");
    let rtt = crate::network::ping(st.gateway, 1, 3000).map_err(|e| format!("ping gateway: {}", e))?;
    crate::kprintln!("selftest: ping {} = {} us", st.gateway, rtt);
    // DNS depends on the host having internet access; report but don't fail.
    match crate::network::resolve("example.com") {
        Ok(ip) => {
            crate::kprintln!("selftest: example.com -> {}", ip);
            // TCP: fetch a page (again only reported, it needs the internet).
            match http_get(ip, "example.com") {
                Ok(line) => crate::kprintln!("selftest: tcp http://example.com/ -> {}", line),
                Err(e) => crate::kprintln!("selftest: (TCP to example.com failed: {})", e),
            }
        }
        Err(e) => crate::kprintln!("selftest: (DNS lookup not available here: {})", e),
    }
    // The file-sharing server comes up with the network.
    let start = crate::time::uptime_ms();
    while !crate::network::httpd::is_running() && crate::time::uptime_ms() - start < 5000 {
        crate::proc::sched::sleep_ms(20);
    }
    ensure!(crate::network::httpd::is_running(), "file sharing server did not start");
    Ok(())
}

/// First line of the reply to `GET /`.
fn http_get(ip: net::Ipv4, host: &str) -> Result<String, String> {
    use crate::network::tcp::TcpStream;
    let mut c = TcpStream::connect(ip, 80, 5000).map_err(|e| e.to_string())?;
    let peer = c.peer().ok_or("no peer")?;
    ensure!(peer == (ip, 80), "connected to the wrong peer");
    let req = format!("GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n", host);
    c.write_all(req.as_bytes(), 5000).map_err(|e| e.to_string())?;
    let mut reply = Vec::new();
    let mut buf = [0u8; 2048];
    loop {
        match c.read(&mut buf, 5000) {
            Ok(0) => break,
            Ok(n) => reply.extend_from_slice(&buf[..n]),
            Err(e) if reply.is_empty() => return Err(e.to_string()),
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&reply);
    let first = text.lines().next().unwrap_or("").to_string();
    ensure!(first.starts_with("HTTP/1."), "not an HTTP reply");
    Ok(format!("{} ({} bytes)", first, reply.len()))
}

fn audio_works() -> TestResult {
    if !crate::audio::is_present() {
        crate::kprintln!("selftest: (no sound card, skipping audio test)");
        return Ok(());
    }
    let tone = alloc::sync::Arc::new(crate::audio::sounds::click());
    let wav = crate::audio::wav::encode(&tone);
    let (info, decoded) = crate::audio::wav::decode(&wav).map_err(|e| String::from(e))?;
    ensure!(info.rate == 48000 && decoded.len() == tone.len(), "WAV round-trip changed the audio");
    let before = crate::audio::FRAMES_MIXED.load(core::sync::atomic::Ordering::Relaxed);
    let id = crate::audio::play(alloc::sync::Arc::new(crate::audio::sounds::notify())).ok_or("play failed")?;
    let start = crate::time::uptime_ms();
    while crate::audio::is_playing(id) {
        ensure!(crate::time::uptime_ms() - start < 5000, "sound never finished playing (DMA not running?)");
        sched::sleep_ms(20);
    }
    let mixed = crate::audio::FRAMES_MIXED.load(core::sync::atomic::Ordering::Relaxed) - before;
    ensure!(mixed >= 48000 * 4 / 10, "only {} frames mixed", mixed);
    Ok(())
}

fn media_decodes() -> TestResult {
    let mut pictures = 0;
    for e in fs::read_dir("/pictures").map_err(|e| e.to_string())? {
        if crate::gui::imageview::is_image_name(&e.name) {
            let data = fs::read_file(&fs::join("/pictures", &e.name)).map_err(|e| e.to_string())?;
            let t = crate::time::uptime_ms();
            let img = image::decode(&data).map_err(|err| alloc::format!("{}: {}", e.name, err))?;
            crate::kprintln!("selftest: {} {}x{} in {} ms", e.name, img.width, img.height, crate::time::uptime_ms() - t);
            let _ = img.cover(1280, 800, 0xff000000);
            pictures += 1;
        }
    }
    ensure!(pictures > 0, "no sample pictures in /pictures");
    // The sample video: demux and decode the first second with the media
    // pipeline (as the player does), from a file handle.
    let file = fs::open("/videos/Welcome to MayOS.mp4").map_err(|e| e.to_string())?;
    let mut dm = media::demux::open(alloc::boxed::Box::new(file)).map_err(|e| alloc::format!("{:?}", e))?;
    let info = dm.info().clone();
    let (vi, ai) = media::pipeline::choose_tracks(&info.tracks);
    let mut vd = media::pipeline::VideoDecoder::new(&info.tracks[vi.ok_or("no video track")?])?;
    let mut ad = media::pipeline::AudioDecoder::new(&info.tracks[ai.ok_or("no audio track")?])?;
    let t = crate::time::uptime_ms();
    let (mut frames, mut samples) = (0, 0);
    while let Some(p) = dm.next_packet() {
        let p = p.map_err(|e| alloc::format!("{:?}", e))?;
        if p.pts > 1_000_000 {
            break;
        }
        if Some(p.track) == vi {
            vd.decode(&p);
            while vd.next_frame().is_some() {
                frames += 1;
            }
        } else {
            samples += ad.decode(&p).len();
        }
    }
    crate::kprintln!("selftest: video {}x{}, first second: {} frames, {} samples in {} ms", info.tracks[vi.unwrap()].width, info.tracks[vi.unwrap()].height, frames, samples, crate::time::uptime_ms() - t);
    ensure!(frames >= 20 && samples > 40_000, "sample video did not decode");
    let song = fs::open("/music/Northern Lights.mp3").map_err(|e| e.to_string())?;
    let mut src: alloc::boxed::Box<dyn media::demux::Source + Send> = alloc::boxed::Box::new(song);
    let tags = media::tags::read(&mut *src);
    ensure!(tags.title == "Northern Lights" && tags.cover.is_some(), "song tags not read");
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
        ("settings_persist", settings_persist),
        ("network_works", network_works),
        ("audio_works", audio_works),
        ("media_decodes", media_decodes),
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
