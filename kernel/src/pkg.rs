//! `pkg`: install Linux software from Alpine Linux packages.
//!
//! MayOS runs Alpine's (musl, dynamically linked) programs through its
//! Linux layer, so their packages can be used as they are: `pkg` reads
//! Alpine's package index, resolves dependencies, downloads the `.apk`
//! files (gzip'd tar archives), unpacks them onto the disk and runs the
//! usual post-install steps (GSettings schemas, image loaders, MIME and
//! font caches) with Alpine's own tools.
//!
//! FAT32 has no symbolic links: links are replaced by copies of their
//! targets once everything is unpacked.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::fs;
use crate::proc::process::Console;

const MIRROR: &str = "https://dl-cdn.alpinelinux.org/alpine/v3.22";
const REPOS: [&str; 2] = ["main", "community"];
const DB_DIR: &str = "/var/lib/pkg";

/// Large optional dependencies left out unless asked for (OpenGL stacks
/// MayOS cannot use: programs fall back to software drawing).
const SKIP: &[&str] = &[
    "mesa", "mesa-egl", "mesa-gbm", "mesa-gl", "mesa-gles", "mesa-dri-gallium", "mesa-glapi", "mesa-vulkan-swrast", "llvm20-libs",
    "llvm19-libs", "spirv-tools", "vulkan-loader",
];

struct Pkg {
    name: String,
    version: String,
    repo: &'static str,
    deps: Vec<String>,
    size: u64,
    desc: String,
}

struct Index {
    pkgs: BTreeMap<String, Pkg>,
    /// "so:libfoo.so.1", "cmd:foo", package names → package.
    provides: BTreeMap<String, String>,
}

fn say(c: &Console, s: &str) {
    c.write(s.as_bytes());
}

pub fn job(args: &str, c: &Console, cancel: &AtomicBool) -> i64 {
    let words: Vec<&str> = args.split_whitespace().collect();
    let r = match words.first().copied() {
        Some("install" | "add") if words.len() > 1 => install(&words[1..], c, cancel),
        Some("update") => load_index(c, true).map(|i| {
            say(c, &format!("{} packages available\n", i.pkgs.len()));
        }),
        Some("search") if words.len() > 1 => search(words[1], c),
        Some("list") => {
            for (n, v) in installed() {
                say(c, &format!("{} {}\n", n, v));
            }
            Ok(())
        }
        _ => {
            say(
                c,
                "usage: pkg install <name>...   install Alpine Linux packages\n       pkg search <word>         find packages\n       pkg list                  installed packages\n       pkg update                refresh the package index\nexample: pkg install netsurf   (a web browser)\n",
            );
            Ok(())
        }
    };
    match r {
        Ok(()) => 0,
        Err(e) => {
            say(c, &format!("pkg: {}\n", e));
            1
        }
    }
}

// --- index ---------------------------------------------------------------

/// Defaults that suit MayOS: one process, software drawing, no sandbox,
/// no telemetry or updates.
fn firefox_prefs(dir: &str) {
    let prefs = r#"// MayOS defaults (written by pkg)
pref("fission.autostart", false);
pref("browser.tabs.remote.autostart", false);
pref("dom.ipc.processCount", 1);
pref("dom.ipc.processPrelaunch.enabled", false);
pref("network.process.enabled", false);
pref("media.rdd-process.enabled", false);
pref("media.utility-process.enabled", false);
pref("gfx.webrender.software", true);
pref("layers.acceleration.disabled", true);
pref("media.hardware-video-decoding.enabled", false);
pref("widget.dmabuf.force-enabled", false);
pref("security.sandbox.content.level", 0);
pref("browser.shell.checkDefaultBrowser", false);
pref("browser.startup.homepage_override.mstone", "ignore");
pref("app.update.enabled", false);
pref("toolkit.telemetry.enabled", false);
pref("datareporting.policy.dataSubmissionEnabled", false);
pref("browser.sessionstore.resume_from_crash", false);
"#;
    let pdir = format!("{}/defaults/pref", dir);
    let _ = mkdirs(&pdir);
    let _ = fs::write_file(&format!("{}/mayos.js", pdir), prefs.as_bytes());
}

fn download(url: &str) -> Result<Vec<u8>, String> {
    let u = web::url::Url::parse(url).ok_or("bad url")?;
    // Big packages (Firefox is ~90 MB): allow up to 1 GB, and retry a
    // download that was cut off.
    let mut tries = 0;
    let r = loop {
        match crate::network::http::get_limit(&u, 1 << 30) {
            Ok(r) => break r,
            Err(e) if tries < 3 && e.contains("cut off") => tries += 1,
            Err(e) => return Err(e),
        }
    };
    if r.status != 200 {
        return Err(format!("{}: HTTP {}", url, r.status));
    }
    Ok(r.body)
}

fn load_index(c: &Console, refresh: bool) -> Result<Index, String> {
    let _ = mkdirs(DB_DIR);
    let mut idx = Index { pkgs: BTreeMap::new(), provides: BTreeMap::new() };
    for repo in REPOS {
        let cache = format!("{}/{}.index", DB_DIR, repo);
        let text = match fs::read_file(&cache) {
            Ok(t) if !refresh => t,
            _ => {
                say(c, &format!("Downloading the {} package index...\n", repo));
                let gz = download(&format!("{}/{}/x86_64/APKINDEX.tar.gz", MIRROR, repo))?;
                let tar = gunzip_all(&gz)?;
                let mut found = None;
                tar_entries(&tar, |e| {
                    if e.name == "APKINDEX" {
                        found = Some(e.data.to_vec());
                    }
                });
                let t = found.ok_or("index has no APKINDEX")?;
                fs::write_file(&cache, &t).map_err(|e| format!("{}: {}", cache, e))?;
                t
            }
        };
        parse_index(&String::from_utf8_lossy(&text), repo, &mut idx);
    }
    Ok(idx)
}

fn parse_index(text: &str, repo: &'static str, idx: &mut Index) {
    let mut cur: BTreeMap<char, String> = BTreeMap::new();
    let mut flush = |cur: &mut BTreeMap<char, String>, idx: &mut Index| {
        if let Some(name) = cur.get(&'P').cloned() {
            if idx.pkgs.contains_key(&name) {
                cur.clear();
                return; // main wins over community
            }
            for p in cur.get(&'p').map(|s| s.as_str()).unwrap_or("").split_whitespace() {
                let key = p.split('=').next().unwrap_or(p).to_string();
                idx.provides.entry(key).or_insert_with(|| name.clone());
            }
            idx.provides.entry(name.clone()).or_insert_with(|| name.clone());
            let deps = cur.get(&'D').map(|d| d.split_whitespace().map(String::from).collect()).unwrap_or_default();
            idx.pkgs.insert(
                name.clone(),
                Pkg {
                    name,
                    version: cur.get(&'V').cloned().unwrap_or_default(),
                    repo,
                    deps,
                    size: cur.get(&'S').and_then(|s| s.parse().ok()).unwrap_or(0),
                    desc: cur.get(&'T').cloned().unwrap_or_default(),
                },
            );
        }
        cur.clear();
    };
    for line in text.lines() {
        if line.is_empty() {
            flush(&mut cur, idx);
            continue;
        }
        let mut ch = line.chars();
        if let (Some(k), Some(':')) = (ch.next(), ch.next()) {
            cur.insert(k, line[2..].to_string());
        }
    }
    flush(&mut cur, idx);
}

fn search(word: &str, c: &Console) -> Result<(), String> {
    let idx = load_index(c, false)?;
    let w = word.to_ascii_lowercase();
    let mut n = 0;
    for p in idx.pkgs.values() {
        if p.name.contains(&w) || p.desc.to_ascii_lowercase().contains(&w) {
            say(c, &format!("{:<28} {}\n", p.name, p.desc));
            n += 1;
            if n >= 60 {
                say(c, "...\n");
                break;
            }
        }
    }
    Ok(())
}

fn installed() -> BTreeMap<String, String> {
    let text = fs::read_file(&format!("{}/installed", DB_DIR)).unwrap_or_default();
    String::from_utf8_lossy(&text)
        .lines()
        .filter_map(|l| l.split_once(' ').map(|(a, b)| (a.to_string(), b.to_string())))
        .collect()
}

fn save_installed(m: &BTreeMap<String, String>) {
    let mut t = String::new();
    for (k, v) in m {
        t.push_str(&format!("{} {}\n", k, v));
    }
    let _ = fs::write_file(&format!("{}/installed", DB_DIR), t.as_bytes());
}

/// Everything `names` needs, in the order found.
fn resolve(idx: &Index, names: &[&str], c: &Console) -> Result<Vec<String>, String> {
    let mut want: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    // Package scripts and wrappers need /bin/sh.
    want.push(String::from("busybox"));
    want.push(String::from("busybox-binsh"));
    let mut seen = BTreeSet::new();
    let mut order = Vec::new();
    // Things GTK programs need at run time but do not list.
    let extras = [("fontconfig", "font-dejavu"), ("gdk-pixbuf", "shared-mime-info"), ("gtk+3.0", "gsettings-desktop-schemas"), ("gtk+3.0", "hicolor-icon-theme")];
    while let Some(n) = want.pop() {
        let dep = n.trim_start_matches('!');
        if n.starts_with('!') {
            continue;
        }
        let key = dep.split(['>', '<', '=', '~']).next().unwrap_or(dep);
        let Some(pkg) = idx.provides.get(key).cloned() else {
            if names.contains(&key) {
                return Err(format!("no package called {}", key));
            }
            say(c, &format!("(no package provides {}, skipped)\n", key));
            continue;
        };
        if SKIP.contains(&pkg.as_str()) && !names.contains(&pkg.as_str()) {
            continue;
        }
        if !seen.insert(pkg.clone()) {
            continue;
        }
        let p = &idx.pkgs[&pkg];
        want.extend(p.deps.iter().cloned());
        for (trigger, extra) in extras {
            if pkg == trigger {
                want.push(extra.to_string());
            }
        }
        order.push(pkg);
    }
    Ok(order)
}

// --- install ---------------------------------------------------------------

fn install(names: &[&str], c: &Console, cancel: &AtomicBool) -> Result<(), String> {
    let idx = load_index(c, false)?;
    let all = resolve(&idx, names, c)?;
    let mut have = installed();
    let todo: Vec<&Pkg> = all.iter().map(|n| &idx.pkgs[n]).filter(|p| have.get(&p.name) != Some(&p.version)).collect();
    if todo.is_empty() {
        say(c, "Everything is already installed.\n");
        return Ok(());
    }
    let total: u64 = todo.iter().map(|p| p.size).sum();
    say(c, &format!("Installing {} packages ({}):\n", todo.len(), fs::format_size(total)));
    let mut links: Vec<(String, String)> = load_links();
    let mut touched = BTreeSet::new();
    for (i, p) in todo.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(String::from("cancelled"));
        }
        say(c, &format!("[{}/{}] {} {}\n", i + 1, todo.len(), p.name, p.version));
        let url = format!("{}/{}/x86_64/{}-{}.apk", MIRROR, p.repo, p.name, p.version);
        let apk = download(&url)?;
        let tar = gunzip_all(&apk)?;
        let mut failed = 0;
        tar_entries(&tar, |e| {
            if e.name.starts_with('.') || e.name.is_empty() {
                return; // .PKGINFO, .SIGN.*, install scripts
            }
            let path = format!("/{}", e.name.trim_end_matches('/'));
            for area in ["usr/share/glib-2.0/schemas", "usr/share/mime", "gdk-pixbuf-2.0", "usr/share/fonts", "gtk-3.0/3.0.0/immodules"] {
                if e.name.contains(area) {
                    touched.insert(area);
                }
            }
            let ok = match e.kind {
                b'5' => mkdirs(&path).is_ok(),
                b'2' | b'1' => {
                    let target = if e.kind == b'1' { format!("/{}", e.link) } else { e.link.clone() };
                    links.retain(|(l, _)| *l != path);
                    links.push((path, target));
                    true
                }
                b'0' | 0 | b'7' => {
                    let parent = parent_of(&path);
                    let _ = mkdirs(&parent);
                    fs::write_file(&path, e.data).is_ok()
                }
                _ => true,
            };
            if !ok {
                failed += 1;
            }
        });
        if failed > 0 {
            say(c, &format!("  ({} files could not be written)\n", failed));
        }
        have.insert(p.name.clone(), p.version.clone());
        save_installed(&have);
    }
    resolve_links(&links, c);
    save_links(&links);
    busybox_links(c);
    for d in ["/run", "/tmp", "/home", "/usr/share/X11/xkb", "/var/cache"] {
        let _ = mkdirs(d);
    }
    // Post-install steps, done by Alpine's own tools.
    let steps: [(&str, &str, &str); 5] = [
        ("usr/share/glib-2.0/schemas", "/usr/bin/glib-compile-schemas", "/usr/share/glib-2.0/schemas"),
        ("usr/share/mime", "/usr/bin/update-mime-database", "/usr/share/mime"),
        ("gdk-pixbuf-2.0", "/usr/bin/gdk-pixbuf-query-loaders", "--update-cache"),
        ("gtk-3.0/3.0.0/immodules", "/usr/bin/gtk-query-immodules-3.0", "--update-cache"),
        ("usr/share/fonts", "/usr/bin/fc-cache", ""),
    ];
    for (area, tool, args) in steps {
        if touched.contains(area) && fs::exists(tool) {
            say(c, &format!("Running {}...\n", crate::fs::file_name(tool)));
            run(tool, args, c);
        }
    }
    say(c, "Done.\n");
    let hint: Vec<String> = names.iter().filter_map(|n| idx.pkgs.get(*n)).map(|p| p.name.clone()).collect();
    for dir in ["/usr/lib/firefox", "/usr/lib/firefox-esr"] {
        if fs::exists(dir) {
            firefox_prefs(dir);
        }
    }
    if hint.iter().any(|n| n.starts_with("firefox")) {
        say(c, "Start Firefox with: firefox  (first start takes a while)\n");
    }
    if hint.iter().any(|n| n == "netsurf") {
        say(c, "Start the browser with: netsurf\n");
    }
    Ok(())
}

fn run(tool: &str, args: &str, c: &Console) {
    let console = Console::new();
    match crate::proc::process::spawn(tool, args, "/", console.clone()) {
        Ok(p) => {
            while p.has_exited().is_none() {
                let out = console.take_output();
                if !out.is_empty() {
                    c.write(&out);
                }
                crate::proc::sched::sleep_ms(50);
            }
            c.write(&console.take_output());
            crate::proc::process::reap(p.pid);
        }
        Err(e) => say(c, &format!("  {}\n", e)),
    }
}

fn parent_of(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => String::from("/"),
        Some(i) => path[..i].to_string(),
    }
}

fn mkdirs(path: &str) -> Result<(), fs::FsError> {
    if path == "/" || fs::is_dir(path) {
        return Ok(());
    }
    mkdirs(&parent_of(path))?;
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(_) if fs::is_dir(path) => Ok(()),
        Err(e) => Err(e),
    }
}

fn load_links() -> Vec<(String, String)> {
    let t = fs::read_file(&format!("{}/links", DB_DIR)).unwrap_or_default();
    String::from_utf8_lossy(&t).lines().filter_map(|l| l.split_once(" -> ").map(|(a, b)| (a.to_string(), b.to_string()))).collect()
}

fn save_links(links: &[(String, String)]) {
    let mut t = String::new();
    for (a, b) in links {
        t.push_str(&format!("{} -> {}\n", a, b));
    }
    let _ = fs::write_file(&format!("{}/links", DB_DIR), t.as_bytes());
}

/// Absolute path a link points to.
fn link_target(link: &str, target: &str) -> String {
    if target.starts_with('/') {
        fs::normalize("/", target)
    } else {
        fs::normalize(&parent_of(link), target)
    }
}

/// Replace links with copies of what they point to (links to links are
/// followed; links to directories are copied recursively, once).
fn resolve_links(links: &[(String, String)], c: &Console) {
    let map: BTreeMap<&str, &str> = links.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let real = |start: &str| -> Option<String> {
        let mut p = start.to_string();
        for _ in 0..16 {
            match map.get(p.as_str()) {
                Some(t) => p = link_target(&p, t),
                None => return Some(p),
            }
        }
        None
    };
    let mut done = 0;
    for (link, _) in links {
        if fs::exists(link) {
            continue;
        }
        let Some(target) = real(link) else { continue };
        if fs::is_dir(&target) {
            if copy_tree(&target, link, 0) {
                done += 1;
            }
        } else if fs::stat(&target).map(|e| e.size > 64 * 1024).unwrap_or(false) {
            // Big file (a program or library): a link stub, not a copy.
            let _ = mkdirs(&parent_of(link));
            if write_stub(link, &target) {
                done += 1;
            }
        } else if let Ok(data) = fs::read_file(&target) {
            let _ = mkdirs(&parent_of(link));
            if fs::write_file(link, &data).is_ok() {
                done += 1;
            }
        }
    }
    if done > 0 {
        say(c, &format!("Linked {} files.\n", done));
    }
}

fn write_stub(link: &str, target: &str) -> bool {
    let mut d = fs::LINK_MAGIC.to_vec();
    d.extend_from_slice(target.as_bytes());
    d.push(b'\n');
    fs::write_file(link, &d).is_ok()
}

/// BusyBox's tools are links to one program; its install script normally
/// makes them. Missing ones become link stubs.
fn busybox_links(c: &Console) {
    if !fs::exists("/bin/busybox") {
        return;
    }
    let out = capture("/bin/busybox", "--list-full");
    let mut n = 0;
    for line in out.lines() {
        let path = format!("/{}", line.trim());
        if line.trim().is_empty() || fs::exists(&path) {
            continue;
        }
        let _ = mkdirs(&parent_of(&path));
        if write_stub(&path, "/bin/busybox") {
            n += 1;
        }
    }
    if n > 0 {
        say(c, &format!("Set up {} BusyBox tools.\n", n));
    }
}

fn capture(tool: &str, args: &str) -> String {
    let console = Console::new();
    let mut out = Vec::new();
    if let Ok(p) = crate::proc::process::spawn(tool, args, "/", console.clone()) {
        while p.has_exited().is_none() {
            out.extend(console.take_output());
            crate::proc::sched::sleep_ms(20);
        }
        out.extend(console.take_output());
        crate::proc::process::reap(p.pid);
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn copy_tree(from: &str, to: &str, depth: u32) -> bool {
    if depth > 6 || mkdirs(to).is_err() {
        return false;
    }
    let Ok(entries) = fs::read_dir(from) else { return false };
    for e in entries {
        let (a, b) = (fs::join(from, &e.name), fs::join(to, &e.name));
        if e.is_dir {
            copy_tree(&a, &b, depth + 1);
        } else if let Ok(d) = fs::read_file(&a) {
            let _ = fs::write_file(&b, &d);
        }
    }
    true
}

// --- archives --------------------------------------------------------------

/// Decompress every gzip member of `data` and concatenate the results.
pub fn gunzip_all(data: &[u8]) -> Result<Vec<u8>, String> {
    use miniz_oxide::inflate::stream::{inflate, InflateState};
    use miniz_oxide::{DataFormat, MZFlush, MZStatus};
    let mut out = Vec::new();
    let mut pos = 0;
    let mut buf = alloc::vec![0u8; 64 * 1024];
    while pos + 18 <= data.len() && data[pos] == 0x1f && data[pos + 1] == 0x8b {
        let flags = data[pos + 3];
        let mut p = pos + 10;
        if flags & 4 != 0 {
            let xlen = u16::from_le_bytes([data[p], data[p + 1]]) as usize;
            p += 2 + xlen;
        }
        for bit in [8u8, 16] {
            if flags & bit != 0 {
                while p < data.len() && data[p] != 0 {
                    p += 1;
                }
                p += 1;
            }
        }
        if flags & 2 != 0 {
            p += 2;
        }
        let mut st = InflateState::new_boxed(DataFormat::Raw);
        loop {
            if p > data.len() {
                return Err(String::from("truncated archive"));
            }
            let r = inflate(&mut st, &data[p..], &mut buf, MZFlush::None);
            p += r.bytes_consumed;
            out.extend_from_slice(&buf[..r.bytes_written]);
            match r.status {
                Ok(MZStatus::StreamEnd) => break,
                Ok(_) if r.bytes_consumed == 0 && r.bytes_written == 0 => return Err(String::from("corrupt archive")),
                Ok(_) => {}
                Err(_) => return Err(String::from("corrupt archive")),
            }
        }
        pos = p + 8; // CRC32 + size
    }
    if out.is_empty() {
        return Err(String::from("not a gzip archive"));
    }
    Ok(out)
}

pub struct TarEntry<'a> {
    pub name: String,
    pub kind: u8,
    pub link: String,
    pub data: &'a [u8],
}

fn octal(b: &[u8]) -> u64 {
    b.iter().filter(|c| (b'0'..=b'7').contains(c)).fold(0, |a, &c| a * 8 + (c - b'0') as u64)
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

/// Walk a tar stream (ustar, pax and GNU long names). Concatenated
/// archives work: end-of-archive blocks are skipped.
pub fn tar_entries<'a>(t: &'a [u8], mut f: impl FnMut(TarEntry<'a>)) {
    let mut o = 0;
    let (mut long_name, mut long_link): (Option<String>, Option<String>) = (None, None);
    while o + 512 <= t.len() {
        let h = &t[o..o + 512];
        if h.iter().all(|&b| b == 0) {
            o += 512;
            continue;
        }
        let size = octal(&h[124..136]) as usize;
        let kind = h[156];
        let start = o + 512;
        let end = (start + size).min(t.len());
        let data = &t[start..end];
        o = start + size.div_ceil(512) * 512;
        match kind {
            b'x' => {
                // pax: "len key=value\n"
                let text = String::from_utf8_lossy(data);
                for rec in text.split('\n') {
                    if let Some((_, kv)) = rec.split_once(' ')
                        && let Some((k, v)) = kv.split_once('=')
                    {
                        match k {
                            "path" => long_name = Some(v.to_string()),
                            "linkpath" => long_link = Some(v.to_string()),
                            _ => {}
                        }
                    }
                }
                continue;
            }
            b'L' => {
                long_name = Some(cstr(data));
                continue;
            }
            b'K' => {
                long_link = Some(cstr(data));
                continue;
            }
            b'g' => continue,
            _ => {}
        }
        let mut name = cstr(&h[0..100]);
        if &h[257..262] == b"ustar" {
            let prefix = cstr(&h[345..500]);
            if !prefix.is_empty() {
                name = format!("{}/{}", prefix, name);
            }
        }
        let name = long_name.take().unwrap_or(name);
        let link = long_link.take().unwrap_or_else(|| cstr(&h[157..257]));
        f(TarEntry { name, kind, link, data });
    }
}
