//! The built-in command shell used by the terminal.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write;

use super::app::{Command, Ctx};
use super::editor::Editor;
use super::explorer::Explorer;
use super::terminal::{Job, Terminal};
use crate::proc::process::Console;
use crate::sync::Spin;
use crate::fs;

const C_DIR: &str = "\x1b[94m";
const C_EXE: &str = "\x1b[92m";
const C_DIM: &str = "\x1b[90m";
const C_ERR: &str = "\x1b[91m";
const C_HEAD: &str = "\x1b[96m";
const C_OFF: &str = "\x1b[0m";

const HELP: &[(&str, &str)] = &[
    ("help", "show this list"),
    ("ls [-l] [-a] [path]", "list a directory"),
    ("cd [path]", "change directory"),
    ("pwd", "print the current directory"),
    ("cat <file>...", "print files"),
    ("echo <text>", "print text (supports > and >> redirection)"),
    ("touch <file>", "create an empty file"),
    ("mkdir [-p] <dir>", "create a directory"),
    ("rm [-r] <path>", "remove a file (or a directory with -r)"),
    ("rmdir <dir>", "remove an empty directory"),
    ("mv <from> <to>", "move or rename"),
    ("cp [-r] <from> <to>", "copy files or directories"),
    ("tree [path]", "show a directory tree"),
    ("stat <path>", "show file details"),
    ("wc <file>", "count lines, words and bytes"),
    ("hexdump <file>", "show file bytes in hex"),
    ("df", "disk usage"),
    ("free", "memory usage"),
    ("ps", "list threads and processes"),
    ("kill <pid>", "stop a process"),
    ("uptime / date", "time since boot / current date"),
    ("lspci", "list PCI devices"),
    ("ifconfig", "network adapter and address"),
    ("ping <host> [count]", "send ICMP echo requests"),
    ("nslookup <name>", "resolve a host name with DNS"),
    ("dhcp", "request a new IP address"),
    ("play <file.wav>", "play a WAV file"),
    ("beep / volume [0-100]", "test sound / get or set the volume"),
    ("settings", "open the Settings app"),
    ("resolution [WxH]", "list or change the screen resolution"),
    ("dmesg", "kernel log"),
    ("uname", "system name"),
    ("history", "previous commands"),
    ("clear", "clear the screen"),
    ("explorer [path]", "open the file explorer"),
    ("edit <file>", "open the text editor"),
    ("open <path>", "open with the default app"),
    ("<program> [args]", "run a program from /bin (or a path)"),
    ("shutdown / reboot", "power off or restart"),
    ("exit", "close this terminal"),
];

/// Split a command line into words, honouring single and double quotes.
pub fn tokenize(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has = false;
    for ch in line.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => cur.push(ch),
            None if ch == '"' || ch == '\'' => {
                quote = Some(ch);
                has = true;
            }
            None if ch.is_whitespace() => {
                if has || !cur.is_empty() {
                    out.push(core::mem::take(&mut cur));
                    has = false;
                }
            }
            None => cur.push(ch),
        }
    }
    if has || !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut esc = false;
    for ch in s.chars() {
        if esc {
            if ch.is_ascii_alphabetic() {
                esc = false;
            }
            continue;
        }
        if ch == '\x1b' {
            esc = true;
            continue;
        }
        out.push(ch);
    }
    out
}

pub fn run(term: &mut Terminal, line: &str, ctx: &mut Ctx) {
    let mut words = tokenize(line);
    if words.is_empty() {
        return;
    }
    // Output redirection: `cmd ... > file` or `>> file`.
    let mut redirect: Option<(String, bool)> = None;
    if let Some(pos) = words.iter().position(|w| w == ">" || w == ">>") {
        let append = words[pos] == ">>";
        match words.get(pos + 1) {
            Some(file) => redirect = Some((fs::normalize(&term.cwd, file), append)),
            None => {
                term.print(&format!("{}syntax error: missing file after {}{}\n", C_ERR, words[pos], C_OFF));
                return;
            }
        }
        words.truncate(pos);
    }
    let mut out = String::new();
    let cmd = words[0].clone();
    let args: Vec<&str> = words[1..].iter().map(|s| s.as_str()).collect();
    let handled = builtin(term, &cmd, &args, &mut out, ctx);
    if !handled {
        run_program(term, &cmd, &args, &mut out);
    }
    if let Some((path, append)) = redirect {
        let text = strip_ansi(&out);
        let mut data = if append { fs::read_file(&path).unwrap_or_default() } else { Vec::new() };
        data.extend_from_slice(text.as_bytes());
        if let Err(e) = fs::write_file(&path, &data) {
            term.print(&format!("{}{}: {}{}\n", C_ERR, path, e, C_OFF));
        }
        return;
    }
    if !out.is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        term.print(&out);
    }
}

fn err(out: &mut String, msg: impl core::fmt::Display) {
    let _ = writeln!(out, "{}{}{}", C_ERR, msg, C_OFF);
}

fn flags<'a>(args: &[&'a str]) -> (Vec<char>, Vec<&'a str>) {
    let mut f = Vec::new();
    let mut rest = Vec::new();
    for a in args {
        if a.starts_with('-') && a.len() > 1 {
            f.extend(a[1..].chars());
        } else {
            rest.push(*a);
        }
    }
    (f, rest)
}

fn builtin(term: &mut Terminal, cmd: &str, args: &[&str], out: &mut String, ctx: &mut Ctx) -> bool {
    let cwd = term.cwd.clone();
    let abs = |p: &str| fs::normalize(&cwd, p);
    match cmd {
        "help" => {
            let _ = writeln!(out, "{}Built-in commands:{}", C_HEAD, C_OFF);
            for (c, d) in HELP {
                let _ = writeln!(out, "  {:<22}{}{}{}", c, C_DIM, d, C_OFF);
            }
            let _ = writeln!(out, "\nPrograms in /bin can be run by name. Tab completes file names.");
        }
        "pwd" => {
            let _ = writeln!(out, "{}", cwd);
        }
        "cd" => {
            let target = abs(args.first().copied().unwrap_or("/"));
            match fs::stat(&target) {
                Ok(e) if e.is_dir => term.cwd = target,
                Ok(_) => err(out, format!("cd: {}: not a directory", target)),
                Err(e) => err(out, format!("cd: {}: {}", target, e)),
            }
        }
        "ls" | "dir" => {
            let (f, rest) = flags(args);
            let long = f.contains(&'l');
            let all = f.contains(&'a');
            let paths: Vec<&str> = if rest.is_empty() { alloc::vec!["."] } else { rest };
            for (n, p) in paths.iter().enumerate() {
                let path = abs(p);
                match fs::stat(&path) {
                    Ok(e) if !e.is_dir => {
                        let _ = writeln!(out, "{}", e.name);
                        continue;
                    }
                    Err(e) => {
                        err(out, format!("ls: {}: {}", path, e));
                        continue;
                    }
                    _ => {}
                }
                if paths.len() > 1 {
                    let _ = writeln!(out, "{}{}:{}", C_HEAD, path, C_OFF);
                }
                match fs::read_dir(&path) {
                    Ok(entries) => {
                        let entries: Vec<_> = entries.into_iter().filter(|e| all || !e.is_hidden()).collect();
                        if long {
                            for e in &entries {
                                let size = if e.is_dir { String::from("-") } else { fs::format_size(e.size as u64) };
                                let color = if e.is_dir { C_DIR } else { C_OFF };
                                let _ = writeln!(
                                    out,
                                    "{} {:>9}  {}  {}{}{}{}",
                                    if e.is_dir { 'd' } else { '-' },
                                    size,
                                    fs::format_time(&e.modified),
                                    color,
                                    e.name,
                                    if e.is_dir { "/" } else { "" },
                                    C_OFF
                                );
                            }
                        } else {
                            let mut line = String::new();
                            for e in &entries {
                                let color = if e.is_dir {
                                    C_DIR
                                } else if path == "/bin" {
                                    C_EXE
                                } else {
                                    C_OFF
                                };
                                let name = if e.name.contains(' ') { format!("'{}'", e.name) } else { e.name.clone() };
                                let _ = write!(line, "{}{}{}  ", color, name, C_OFF);
                            }
                            if !line.is_empty() {
                                let _ = writeln!(out, "{}", line.trim_end());
                            }
                        }
                    }
                    Err(e) => err(out, format!("ls: {}: {}", path, e)),
                }
                if n + 1 < paths.len() {
                    out.push('\n');
                }
            }
        }
        "cat" => {
            if args.is_empty() {
                err(out, "usage: cat <file>...");
            }
            for p in args {
                match fs::read_file(&abs(p)) {
                    Ok(d) => out.push_str(&String::from_utf8_lossy(&d)),
                    Err(e) => err(out, format!("cat: {}: {}", p, e)),
                }
            }
        }
        "echo" => {
            let _ = writeln!(out, "{}", args.join(" "));
        }
        "touch" => {
            for p in args {
                let path = abs(p);
                if !fs::exists(&path)
                    && let Err(e) = fs::create_file(&path)
                {
                    err(out, format!("touch: {}: {}", p, e));
                }
            }
        }
        "mkdir" => {
            let (f, rest) = flags(args);
            if rest.is_empty() {
                err(out, "usage: mkdir [-p] <dir>");
            }
            for p in rest {
                let path = abs(p);
                let r = if f.contains(&'p') {
                    let mut cur = String::new();
                    let mut res = Ok(());
                    for part in path.split('/').filter(|s| !s.is_empty()) {
                        cur.push('/');
                        cur.push_str(part);
                        if !fs::exists(&cur)
                            && let Err(e) = fs::create_dir(&cur)
                        {
                            res = Err(e);
                            break;
                        }
                    }
                    res
                } else {
                    fs::create_dir(&path)
                };
                if let Err(e) = r {
                    err(out, format!("mkdir: {}: {}", p, e));
                }
            }
        }
        "rm" => {
            let (f, rest) = flags(args);
            if rest.is_empty() {
                err(out, "usage: rm [-r] <path>");
            }
            for p in rest {
                let path = abs(p);
                if path == "/" {
                    err(out, "rm: refusing to remove /");
                    continue;
                }
                let r = match fs::stat(&path) {
                    Ok(e) if e.is_dir && !f.contains(&'r') => {
                        err(out, format!("rm: {}: is a directory (use rm -r)", p));
                        continue;
                    }
                    Ok(e) if e.is_dir => fs::remove_all(&path),
                    Ok(_) => fs::remove(&path),
                    Err(e) => Err(e),
                };
                if let Err(e) = r {
                    err(out, format!("rm: {}: {}", p, e));
                }
            }
        }
        "rmdir" => {
            for p in args {
                let path = abs(p);
                match fs::stat(&path) {
                    Ok(e) if e.is_dir => {
                        if let Err(e) = fs::remove(&path) {
                            err(out, format!("rmdir: {}: {}", p, e));
                        }
                    }
                    Ok(_) => err(out, format!("rmdir: {}: not a directory", p)),
                    Err(e) => err(out, format!("rmdir: {}: {}", p, e)),
                }
            }
        }
        "mv" | "cp" => {
            let (f, rest) = flags(args);
            if rest.len() != 2 {
                err(out, format!("usage: {} <from> <to>", cmd));
                return true;
            }
            let from = abs(rest[0]);
            let mut to = abs(rest[1]);
            // Moving/copying into an existing directory keeps the name.
            if fs::is_dir(&to) {
                to = fs::join(&to, fs::file_name(&from));
            }
            let r = if cmd == "mv" {
                fs::rename(&from, &to)
            } else if fs::is_dir(&from) && !f.contains(&'r') {
                err(out, format!("cp: {}: is a directory (use cp -r)", rest[0]));
                return true;
            } else {
                fs::copy(&from, &to)
            };
            if let Err(e) = r {
                err(out, format!("{}: {}", cmd, e));
            }
        }
        "tree" => {
            let root = abs(args.first().copied().unwrap_or("."));
            let _ = writeln!(out, "{}{}{}", C_DIR, root, C_OFF);
            let mut counts = (0, 0);
            tree(&root, "", out, &mut counts, 0);
            let _ = writeln!(out, "\n{} directories, {} files", counts.0, counts.1);
        }
        "stat" => {
            for p in args {
                match fs::stat(&abs(p)) {
                    Ok(e) => {
                        let _ = writeln!(out, "  File: {}", abs(p));
                        let _ = writeln!(out, "  Type: {}", if e.is_dir { "directory" } else { "file" });
                        let _ = writeln!(out, "  Size: {} bytes", e.size);
                        let _ = writeln!(out, "Cluster: {}", e.first_cluster);
                        let _ = writeln!(out, "Created: {}", fs::format_time(&e.created));
                        let _ = writeln!(out, "Modified: {}", fs::format_time(&e.modified));
                    }
                    Err(e) => err(out, format!("stat: {}: {}", p, e)),
                }
            }
        }
        "wc" => {
            for p in args {
                match fs::read_file(&abs(p)) {
                    Ok(d) => {
                        let s = String::from_utf8_lossy(&d);
                        let _ = writeln!(
                            out,
                            "{:>7} {:>7} {:>7} {}",
                            s.lines().count(),
                            s.split_whitespace().count(),
                            d.len(),
                            p
                        );
                    }
                    Err(e) => err(out, format!("wc: {}: {}", p, e)),
                }
            }
        }
        "hexdump" | "xxd" => match args.first().map(|p| fs::read_file(&abs(p))) {
            Some(Ok(d)) => {
                for (i, chunk) in d.chunks(16).take(256).enumerate() {
                    let _ = write!(out, "{}{:08x}{}  ", C_DIM, i * 16, C_OFF);
                    for b in chunk {
                        let _ = write!(out, "{:02x} ", b);
                    }
                    for _ in chunk.len()..16 {
                        out.push_str("   ");
                    }
                    out.push(' ');
                    for &b in chunk {
                        out.push(if (0x20..0x7f).contains(&b) { b as char } else { '.' });
                    }
                    out.push('\n');
                }
                if d.len() > 4096 {
                    let _ = writeln!(out, "{}... ({} bytes total){}", C_DIM, d.len(), C_OFF);
                }
            }
            Some(Err(e)) => err(out, format!("hexdump: {}", e)),
            None => err(out, "usage: hexdump <file>"),
        },
        "df" => match fs::stats() {
            Ok(s) => {
                let used = s.total_bytes() - s.free_bytes();
                let pct = if s.total_bytes() > 0 { used * 100 / s.total_bytes() } else { 0 };
                let _ = writeln!(out, "{}Filesystem   Size      Used      Free      Use%  Mounted on{}", C_HEAD, C_OFF);
                let _ = writeln!(
                    out,
                    "disk         {:<9} {:<9} {:<9} {:>3}%  /  (FAT32, {} B clusters, {})",
                    fs::format_size(s.total_bytes()),
                    fs::format_size(used),
                    fs::format_size(s.free_bytes()),
                    pct,
                    s.cluster_size,
                    fs::backend()
                );
            }
            Err(e) => err(out, format!("df: {}", e)),
        },
        "free" | "mem" => {
            let (free, total) = crate::mem::pmm::stats();
            let (hu, ht) = crate::mem::heap::stats();
            let _ = writeln!(out, "{}            total       used       free{}", C_HEAD, C_OFF);
            let _ = writeln!(
                out,
                "Physical  {:>8} KB {:>8} KB {:>8} KB",
                total * 4,
                (total - free) * 4,
                free * 4
            );
            let _ = writeln!(out, "Heap      {:>8} KB {:>8} KB {:>8} KB", ht / 1024, hu / 1024, (ht - hu) / 1024);
        }
        "ps" => {
            let _ = writeln!(out, "{}  TID   PID  STATE      CPU(ms)  NAME{}", C_HEAD, C_OFF);
            for t in crate::proc::sched::list() {
                let state = match t.state {
                    crate::proc::sched::State::Ready => "ready",
                    crate::proc::sched::State::Running => "running",
                    crate::proc::sched::State::Sleeping(_) => "sleeping",
                    crate::proc::sched::State::Dead => "dead",
                };
                let pid = t.pid.map(|p| format!("{}", p)).unwrap_or_else(|| String::from("-"));
                let _ = writeln!(out, "{:>5} {:>5}  {:<9} {:>8}  {}", t.id, pid, state, t.cpu_ms, t.name);
            }
        }
        "kill" => match args.first().and_then(|a| a.parse::<u64>().ok()) {
            Some(pid) => match crate::proc::process::find(pid) {
                Some(p) => crate::proc::process::kill(&p),
                None => err(out, format!("kill: no process {}", pid)),
            },
            None => err(out, "usage: kill <pid>"),
        },
        "uptime" => {
            let s = crate::time::uptime_ms() / 1000;
            let _ = writeln!(out, "up {}h {:02}m {:02}s", s / 3600, (s / 60) % 60, s % 60);
        }
        "date" => {
            let t = crate::arch::rtc::now();
            let _ = writeln!(
                out,
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02} (RTC)",
                t.year, t.month, t.day, t.hour, t.minute, t.second
            );
        }
        "lspci" => {
            for d in crate::drivers::pci::devices() {
                let _ = writeln!(
                    out,
                    "{:02x}:{:02x}.{}  {:04x}:{:04x}  {}",
                    d.bus,
                    d.slot,
                    d.func,
                    d.vendor,
                    d.device,
                    d.class_name()
                );
            }
        }
        "dmesg" => out.push_str(&crate::log::contents()),
        "uname" => {
            let _ = writeln!(out, "MayOS {} x86_64", crate::VERSION);
        }
        "history" => {
            for (i, h) in term.history().iter().enumerate() {
                let _ = writeln!(out, "{:>4}  {}", i + 1, h);
            }
        }
        "clear" | "cls" => term.clear(),
        "explorer" | "files" => {
            let path = abs(args.first().copied().unwrap_or("."));
            ctx.open(Box::new(Explorer::new(&path)));
        }
        "edit" | "notepad" => match args.first() {
            Some(p) => ctx.open(Box::new(Editor::open(&abs(p)))),
            None => ctx.open(Box::new(Editor::new_empty())),
        },
        "open" => match args.first() {
            Some(p) => {
                let path = abs(p);
                if let Err(e) = super::open_path(&path, ctx) {
                    err(out, format!("open: {}: {}", p, e));
                }
            }
            None => err(out, "usage: open <path>"),
        },
        "ifconfig" | "ip" => match crate::network::status() {
            Some(st) => {
                let _ = writeln!(out, "{}eth0{}  {}", C_HEAD, C_OFF, st.adapter);
                let _ = writeln!(out, "      link {}  {} Mb/s  mac {}", if st.link_up { "up" } else { "down" }, st.speed_mbps, st.mac);
                let _ = writeln!(out, "      inet {}/{}  gateway {}", st.ip, st.mask.prefix_len(), st.gateway);
                let dns: Vec<String> = st.dns.iter().map(|d| format!("{}", d)).collect();
                let _ = writeln!(out, "      dns {}  config {:?}", if dns.is_empty() { String::from("-") } else { dns.join(", ") }, st.dhcp);
                let _ = writeln!(out, "      rx {} packets ({} bytes)  tx {} packets ({} bytes)", st.rx_packets, st.rx_bytes, st.tx_packets, st.tx_bytes);
            }
            None => err(out, "no network adapter (MayOS supports Intel e1000 NICs)"),
        },
        "dhcp" => {
            if crate::network::is_present() {
                crate::settings::update(|s| s.dhcp = true);
                crate::network::use_dhcp();
                let _ = writeln!(out, "requesting a new address; check with ifconfig");
            } else {
                err(out, "no network adapter");
            }
        }
        "ping" => match args.first() {
            Some(_) => term.start_job("ping", args.join(" "), job_ping),
            None => err(out, "usage: ping <host> [count]  (or ping -c <count> <host>)"),
        },
        "nslookup" | "host" => match args.first() {
            Some(name) => term.start_job("nslookup", String::from(*name), job_nslookup),
            None => err(out, "usage: nslookup <name>"),
        },
        "play" => match args.first() {
            Some(p) => match fs::read_file(&abs(p)) {
                Ok(data) => match crate::audio::wav::decode(&data) {
                    Ok((info, samples)) => {
                        if !crate::audio::is_present() {
                            err(out, "no sound card");
                        } else {
                            let secs = samples.len() / 2 / 48000;
                            let _ = writeln!(
                                out,
                                "playing {} ({} Hz, {}-bit, {} ch, {}:{:02})",
                                p,
                                info.rate,
                                info.bits,
                                info.channels,
                                secs / 60,
                                secs % 60
                            );
                            crate::audio::play(alloc::sync::Arc::new(samples));
                        }
                    }
                    Err(e) => err(out, format!("play: {}", e)),
                },
                Err(e) => err(out, format!("play: {}: {}", p, e)),
            },
            None => err(out, "usage: play <file.wav>"),
        },
        "beep" => {
            if crate::audio::is_present() {
                crate::audio::play_system(crate::audio::SystemSound::Test);
            } else {
                err(out, "no sound card");
            }
        }
        "volume" => match args.first().and_then(|v| v.parse::<u8>().ok()) {
            Some(v) => {
                crate::settings::update(|s| {
                    s.volume = v.min(100);
                    s.muted = false;
                });
                let _ = writeln!(out, "volume {}%", v.min(100));
            }
            None => {
                let s = crate::settings::get();
                let _ = writeln!(out, "volume {}%{}  ({})", s.volume, if s.muted { " (muted)" } else { "" }, crate::audio::device_name());
            }
        },
        "settings" => ctx.open(super::settings_app::boxed()),
        "resolution" | "res" => {
            let (cw, ch) = super::display_mode();
            match args.first().and_then(|a| a.split_once('x')).and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?))) {
                Some((w, h)) => {
                    if super::display_modes().contains(&(w, h)) {
                        ctx.commands.push(Command::SetResolution(w, h));
                        let _ = writeln!(out, "switching to {}x{}", w, h);
                    } else {
                        err(out, format!("{}x{} is not available; run 'resolution' for the list", w, h));
                    }
                }
                None => {
                    let _ = writeln!(out, "{} (current {}x{})", super::display_description(), cw, ch);
                    for (w, h) in super::display_modes() {
                        let mark = if (w, h) == (cw, ch) { "*" } else { " " };
                        let _ = writeln!(out, " {} {}x{}", mark, w, h);
                    }
                }
            }
        }
        "shutdown" | "poweroff" => ctx.commands.push(Command::Shutdown),
        "reboot" | "restart" => ctx.commands.push(Command::Reboot),
        "exit" => ctx.close(),
        _ => return false,
    }
    true
}

fn tree(path: &str, prefix: &str, out: &mut String, counts: &mut (usize, usize), depth: usize) {
    let Ok(entries) = fs::read_dir(path) else { return };
    if depth > 16 {
        return;
    }
    let n = entries.len();
    for (i, e) in entries.iter().enumerate() {
        let last = i + 1 == n;
        let branch = if last { "\u{2514}\u{2500}\u{2500} " } else { "\u{251c}\u{2500}\u{2500} " };
        if e.is_dir {
            counts.0 += 1;
            let _ = writeln!(out, "{}{}{}{}{}", prefix, branch, C_DIR, e.name, C_OFF);
            let child_prefix = format!("{}{}", prefix, if last { "    " } else { "\u{2502}   " });
            tree(&fs::join(path, &e.name), &child_prefix, out, counts, depth + 1);
        } else {
            counts.1 += 1;
            let _ = writeln!(out, "{}{}{}", prefix, branch, e.name);
        }
    }
}

/// Locate an executable: a path, or a name in /bin.
pub fn find_program(cwd: &str, name: &str) -> Option<String> {
    let candidates = if name.contains('/') {
        alloc::vec![fs::normalize(cwd, name)]
    } else {
        alloc::vec![fs::join("/bin", name), fs::normalize(cwd, name)]
    };
    candidates.into_iter().find(|p| fs::stat(p).map(|e| !e.is_dir).unwrap_or(false))
}

fn run_program(term: &mut Terminal, cmd: &str, args: &[&str], out: &mut String) {
    let cwd = term.cwd.clone();
    let Some(path) = find_program(&cwd, cmd) else {
        err(out, format!("{}: command not found (try 'help')", cmd));
        return;
    };
    let joined: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    if let Err(e) = term.start_program(&path, &joined.join(" ")) {
        err(out, e);
    }
}

// ---------------------------------------------------------------------------
// Background jobs
// ---------------------------------------------------------------------------

pub type JobFn = fn(&str, &Console, &AtomicBool) -> i64;

struct JobStart {
    f: JobFn,
    args: String,
    console: Arc<Console>,
    result: Arc<Spin<Option<i64>>>,
    cancel: Arc<AtomicBool>,
}

extern "C" fn job_entry(ptr: usize) {
    let start = unsafe { Box::from_raw(ptr as *mut JobStart) };
    let code = (start.f)(&start.args, &start.console, &start.cancel);
    *start.result.lock() = Some(code);
}

pub fn spawn_job(name: &str, args: String, console: Arc<Console>, f: JobFn) -> Job {
    let result = Arc::new(Spin::new(None));
    let cancel = Arc::new(AtomicBool::new(false));
    let start = Box::new(JobStart { f, args, console, result: result.clone(), cancel: cancel.clone() });
    crate::proc::sched::spawn_kernel(name, job_entry, Box::into_raw(start) as usize);
    Job { name: String::from(name), result, cancel }
}

fn say(c: &Console, s: &str) {
    c.write(s.as_bytes());
}

fn job_ping(args: &str, c: &Console, cancel: &AtomicBool) -> i64 {
    // Accept `ping host [count]`, `ping -c N host` (Unix) and `ping -n N host` (Windows).
    let words: Vec<&str> = args.split_whitespace().collect();
    let mut host = "";
    let mut count: u16 = 4;
    let mut i = 0;
    while i < words.len() {
        match words[i] {
            "-c" | "-n" if i + 1 < words.len() => {
                count = words[i + 1].parse().unwrap_or(4);
                i += 1;
            }
            w if host.is_empty() => host = w,
            w => count = w.parse().unwrap_or(count),
        }
        i += 1;
    }
    let count = count.clamp(1, 1000);
    let ip = match crate::network::resolve(host) {
        Ok(ip) => ip,
        Err(e) => {
            say(c, &format!("{}ping: {}: {}{}\n", C_ERR, host, e, C_OFF));
            return 1;
        }
    };
    say(c, &format!("PING {} ({}): 56 data bytes\n", host, ip));
    let (mut ok, mut min, mut max, mut sum) = (0u32, u64::MAX, 0u64, 0u64);
    let mut sent = 0;
    for seq in 0..count {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        sent += 1;
        let t0 = crate::time::uptime_ms();
        match crate::network::ping(ip, seq, 2000) {
            Ok(us) => {
                ok += 1;
                min = min.min(us);
                max = max.max(us);
                sum += us;
                say(c, &format!("64 bytes from {}: icmp_seq={} time={}.{:03} ms\n", ip, seq, us / 1000, us % 1000));
            }
            Err(e) => say(c, &format!("{}icmp_seq={}: {}{}\n", C_DIM, seq, e, C_OFF)),
        }
        // One request per second.
        while crate::time::uptime_ms() - t0 < 1000 && seq + 1 < count && !cancel.load(Ordering::Relaxed) {
            crate::proc::sched::sleep_ms(20);
        }
    }
    say(c, &format!("--- {} ping statistics ---\n", host));
    let loss = if sent > 0 { (sent - ok) * 100 / sent } else { 0 };
    say(c, &format!("{} packets transmitted, {} received, {}% packet loss\n", sent, ok, loss));
    if ok > 0 {
        let avg = sum / ok as u64;
        say(c, &format!(
            "round-trip min/avg/max = {}.{:03}/{}.{:03}/{}.{:03} ms\n",
            min / 1000, min % 1000, avg / 1000, avg % 1000, max / 1000, max % 1000
        ));
    }
    if ok > 0 { 0 } else { 1 }
}

fn job_nslookup(name: &str, c: &Console, _cancel: &AtomicBool) -> i64 {
    let dns = crate::network::status().map(|s| s.dns).unwrap_or_default();
    if let Some(d) = dns.first() {
        say(c, &format!("Server:  {}\n", d));
    }
    match crate::network::resolve(name) {
        Ok(ip) => {
            say(c, &format!("Name:    {}\nAddress: {}\n", name, ip));
            0
        }
        Err(e) => {
            say(c, &format!("{}nslookup: {}: {}{}\n", C_ERR, name, e, C_OFF));
            1
        }
    }
}
