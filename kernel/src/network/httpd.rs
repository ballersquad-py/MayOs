//! "Share files": a small web server so files can be moved between MayOS
//! and the computer it runs on with any web browser — upload by dragging
//! files onto the page, download by clicking them.
//!
//! It listens on port 80. Under VirtualBox (NAT) add a port-forwarding
//! rule host 8080 -> guest 80 and open http://localhost:8080.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use super::tcp::{TcpListener, TcpStream};
use crate::fs;
use crate::proc::sched;

pub const PORT: u16 = 80;
const PAGE: &str = include_str!("share.html");
const MAX_CONNECTIONS: usize = 8;
const IO_TIMEOUT_MS: u64 = 20_000;

static RUNNING: AtomicBool = AtomicBool::new(false);
static WANTED: AtomicBool = AtomicBool::new(false);
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
static REQUESTS: AtomicU64 = AtomicU64::new(0);
static UPLOADED: AtomicU64 = AtomicU64::new(0);

/// Start or stop the server (follows the "file sharing" setting).
pub fn set_enabled(on: bool) {
    WANTED.store(on, Ordering::Release);
    if on && !RUNNING.swap(true, Ordering::AcqRel) {
        sched::spawn_kernel("httpd", server_thread, 0);
    }
}

pub fn is_running() -> bool {
    RUNNING.load(Ordering::Acquire) && WANTED.load(Ordering::Acquire)
}

/// (requests served, bytes uploaded) since boot.
pub fn stats() -> (u64, u64) {
    (REQUESTS.load(Ordering::Relaxed), UPLOADED.load(Ordering::Relaxed))
}

/// Human-readable ways to reach the server.
pub fn addresses() -> Vec<String> {
    let mut v = Vec::new();
    if let Some(s) = super::status()
        && !s.ip.is_unspecified()
    {
        if s.ip.0[..3] == [10, 0, 2] {
            // The usual VirtualBox/QEMU NAT address: the host can't reach it
            // directly, only through a port-forwarding rule.
            v.push(String::from("http://localhost:8080  (needs port forwarding, see below)"));
        }
        v.push(format!("http://{}/", s.ip));
    }
    v
}

extern "C" fn server_thread(_: usize) {
    // Wait for the network to come up.
    while WANTED.load(Ordering::Acquire) && !super::wait_configured(1000) {}
    let listener = match TcpListener::bind(PORT) {
        Ok(l) => l,
        Err(e) => {
            crate::kprintln!("share: cannot listen on port {}: {}", PORT, e);
            RUNNING.store(false, Ordering::Release);
            return;
        }
    };
    crate::kprintln!("share: file sharing on port {}", PORT);
    while WANTED.load(Ordering::Acquire) {
        let Some(conn) = listener.accept(250) else { continue };
        if ACTIVE.load(Ordering::Acquire) >= MAX_CONNECTIONS {
            let mut c = conn;
            let _ = respond(&mut c, 503, "text/plain", b"busy, try again");
            continue;
        }
        ACTIVE.fetch_add(1, Ordering::AcqRel);
        sched::spawn_kernel("httpd-conn", conn_thread, Box::into_raw(Box::new(conn)) as usize);
    }
    drop(listener);
    crate::kprintln!("share: file sharing stopped");
    RUNNING.store(false, Ordering::Release);
    // Re-enabled while shutting down?
    if WANTED.load(Ordering::Acquire) {
        set_enabled(true);
    }
}

extern "C" fn conn_thread(arg: usize) {
    let mut conn = unsafe { Box::from_raw(arg as *mut TcpStream) };
    // Serve requests until the browser closes the connection.
    while let Some(()) = handle(&mut conn) {}
    let _ = conn.flush(2000);
    drop(conn);
    ACTIVE.fetch_sub(1, Ordering::AcqRel);
}

struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    content_length: u64,
    keep_alive: bool,
    expect_continue: bool,
    /// Body bytes that arrived together with the headers.
    body_start: Vec<u8>,
}

impl Request {
    fn param(&self, name: &str) -> Option<String> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push(h * 16 + l);
                        i += 3;
                        continue;
                    }
                    _ => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn url_encode_path(p: &str) -> String {
    let mut s = String::new();
    for &b in p.as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{:02X}", b));
        }
    }
    s
}

/// Read the request line and headers.
fn read_request(conn: &mut TcpStream) -> Option<Request> {
    let mut head: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 4096];
    let end = loop {
        if let Some(p) = head.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
        if head.len() > 32 * 1024 {
            return None;
        }
        // Idle keep-alive connections are dropped after a while.
        let n = conn.read(&mut buf, if head.is_empty() { 15_000 } else { IO_TIMEOUT_MS }).ok()?;
        if n == 0 {
            return None;
        }
        head.extend_from_slice(&buf[..n]);
    };
    let text = String::from_utf8_lossy(&head[..end]).into_owned();
    let mut lines = text.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let target = first.next()?.to_string();
    let version = first.next().unwrap_or("HTTP/1.0");
    let (raw_path, raw_query) = target.split_once('?').unwrap_or((&target, ""));
    let query = raw_query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (url_decode(k), url_decode(v))
        })
        .collect();
    let mut req = Request {
        method,
        path: url_decode(raw_path),
        query,
        content_length: 0,
        keep_alive: version == "HTTP/1.1",
        expect_continue: false,
        body_start: head[end + 4..].to_vec(),
    };
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        match k.trim().to_ascii_lowercase().as_str() {
            "content-length" => req.content_length = v.parse().unwrap_or(0),
            "connection" => req.keep_alive = !v.eq_ignore_ascii_case("close"),
            "expect" => req.expect_continue = v.eq_ignore_ascii_case("100-continue"),
            _ => {}
        }
    }
    Some(req)
}

fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

fn header(code: u16, ctype: &str, len: u64, extra: &str) -> String {
    format!(
        "HTTP/1.1 {} {}\r\nServer: MayOS\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n{}\r\n",
        code,
        status_text(code),
        ctype,
        len,
        extra
    )
}

fn respond(conn: &mut TcpStream, code: u16, ctype: &str, body: &[u8]) -> Option<()> {
    let mut out = header(code, ctype, body.len() as u64, "").into_bytes();
    out.extend_from_slice(body);
    conn.write_all(&out, IO_TIMEOUT_MS).ok()
}

fn json_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn json_ok(conn: &mut TcpStream) -> Option<()> {
    respond(conn, 200, "application/json", b"{\"ok\":true}")
}

fn json_err(conn: &mut TcpStream, code: u16, msg: &str) -> Option<()> {
    respond(conn, code, "application/json", format!("{{\"ok\":false,\"error\":{}}}", json_str(msg)).as_bytes())
}

fn content_type(name: &str) -> &'static str {
    match fs::extension(name).as_deref() {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("txt" | "md" | "log" | "cfg" | "ini" | "conf" | "rs" | "c" | "h" | "toml" | "sh") => "text/plain; charset=utf-8",
        Some("css") => "text/css",
        Some("js") => "text/javascript",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("jpg" | "jpeg" | "jfif") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("bmp") => "image/bmp",
        Some("webp") => "image/webp",
        Some("mp4" | "m4v") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("mkv") => "video/x-matroska",
        Some("webm") => "video/webm",
        Some("avi") => "video/x-msvideo",
        Some("mp3") => "audio/mpeg",
        Some("m4a" | "aac") => "audio/mp4",
        Some("wav") => "audio/wav",
        Some("pdf") => "application/pdf",
        Some("zip") => "application/zip",
        _ => "application/octet-stream",
    }
}

/// Serve one request. `None` ends the connection.
fn handle(conn: &mut TcpStream) -> Option<()> {
    let req = read_request(conn)?;
    REQUESTS.fetch_add(1, Ordering::Relaxed);
    let keep = req.keep_alive;
    let r = match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => respond(conn, 200, "text/html; charset=utf-8", PAGE.as_bytes()),
        ("GET", "/api/list") => list(conn, &req),
        ("PUT" | "POST", "/api/upload") => return upload(conn, &req).filter(|_| keep),
        ("POST", "/api/mkdir") => simple(conn, &req, |p, _| fs::create_dir(p)),
        ("POST", "/api/delete") => simple(conn, &req, |p, _| if fs::is_dir(p) { fs::remove_all(p) } else { fs::remove(p) }),
        ("POST", "/api/rename") => simple(conn, &req, |p, to| fs::rename(p, &to)),
        ("GET", p) if p.starts_with("/files/") => download(conn, &req),
        ("GET", "/favicon.ico") => respond(conn, 404, "text/plain", b""),
        (_, _) => respond(conn, 404, "text/plain", b"not found"),
    };
    // Any body we did not read would corrupt the next request.
    if req.method != "GET" && req.content_length > req.body_start.len() as u64 {
        return None;
    }
    r.filter(|_| keep)
}

fn clean(path: &str) -> String {
    fs::normalize("/", path)
}

fn simple(conn: &mut TcpStream, req: &Request, f: impl FnOnce(&str, String) -> fs::Result<()>) -> Option<()> {
    let Some(path) = req.param("path") else { return json_err(conn, 400, "missing path") };
    let path = clean(&path);
    if path == "/" || fs::is_mount_point(&path) {
        return json_err(conn, 400, "that folder can't be changed");
    }
    let to = req.param("to").map(|t| clean(&t)).unwrap_or_default();
    match f(&path, to) {
        Ok(()) => json_ok(conn),
        Err(e) => json_err(conn, 409, &e.to_string()),
    }
}

fn list(conn: &mut TcpStream, req: &Request) -> Option<()> {
    let path = clean(&req.param("path").unwrap_or_else(|| String::from("/")));
    let entries = match fs::read_dir(&path) {
        Ok(e) => e,
        Err(e) => return json_err(conn, 404, &e.to_string()),
    };
    let mut out = format!("{{\"ok\":true,\"path\":{},\"entries\":[", json_str(&path));
    for (i, e) in entries.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"name\":{},\"dir\":{},\"size\":{},\"modified\":{}}}",
            json_str(&e.name),
            e.is_dir,
            e.size,
            json_str(&fs::format_time(&e.modified))
        ));
    }
    out.push(']');
    if let Ok(s) = fs::stats() {
        out.push_str(&format!(",\"free\":{},\"total\":{}", s.free_bytes(), s.total_bytes()));
    }
    out.push('}');
    respond(conn, 200, "application/json", out.as_bytes())
}

fn download(conn: &mut TcpStream, req: &Request) -> Option<()> {
    let path = clean(&req.path["/files".len()..]);
    let file = match fs::open(&path) {
        Ok(f) if !fs::is_dir(&path) => f,
        _ => return respond(conn, 404, "text/plain", b"file not found"),
    };
    let name = fs::file_name(&path);
    let disposition = if req.param("download").is_some() {
        format!("Content-Disposition: attachment; filename=\"{}\"\r\n", name.replace('"', "'"))
    } else {
        String::new()
    };
    conn.write_all(header(200, content_type(name), file.size, &disposition).as_bytes(), IO_TIMEOUT_MS).ok()?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut off = 0u64;
    while off < file.size {
        let n = file.read_at(off, &mut buf).ok()?;
        if n == 0 {
            return None;
        }
        conn.write_all(&buf[..n], IO_TIMEOUT_MS).ok()?;
        off += n as u64;
    }
    Some(())
}

fn upload(conn: &mut TcpStream, req: &Request) -> Option<()> {
    let Some(path) = req.param("path") else { return json_err(conn, 400, "missing path") };
    let path = clean(&path);
    if fs::is_dir(&path) {
        return json_err(conn, 409, "a folder with that name exists");
    }
    if let Ok(s) = fs::stats()
        && req.content_length > s.free_bytes()
    {
        json_err(conn, 409, "not enough free space on the MayOS disk");
        return None;
    }
    if req.expect_continue {
        conn.write_all(b"HTTP/1.1 100 Continue\r\n\r\n", IO_TIMEOUT_MS).ok()?;
    }
    let mut w = match fs::create_writer(&path) {
        Ok(w) => w,
        Err(e) => {
            json_err(conn, 409, &e.to_string());
            return None;
        }
    };
    let total = req.content_length;
    let mut got = 0u64;
    // Collect the body in large pieces: fewer, bigger disk writes.
    let mut pending: Vec<u8> = req.body_start.clone();
    pending.truncate(total as usize);
    let mut buf = vec![0u8; 32 * 1024];
    let mut failed = None;
    loop {
        let done = got + pending.len() as u64 >= total;
        if pending.len() >= 256 * 1024 || (done && !pending.is_empty()) {
            if let Err(e) = w.write(&pending) {
                failed = Some(e.to_string());
                break;
            }
            got += pending.len() as u64;
            pending.clear();
        }
        if got >= total {
            break;
        }
        match conn.read(&mut buf, IO_TIMEOUT_MS) {
            Ok(0) | Err(_) => {
                failed = Some(String::from("connection lost"));
                break;
            }
            Ok(n) => {
                let want = (total - got - pending.len() as u64).min(n as u64) as usize;
                pending.extend_from_slice(&buf[..want]);
            }
        }
    }
    let finished = w.finish();
    if let Some(e) = failed {
        let _ = fs::remove(&path);
        crate::kprintln!("share: upload of {} failed: {}", path, e);
        json_err(conn, 500, &e);
        return None;
    }
    if let Err(e) = finished {
        return json_err(conn, 500, &e.to_string());
    }
    UPLOADED.fetch_add(total, Ordering::Relaxed);
    crate::kprintln!("share: received {} ({})", path, fs::format_size(total));
    respond(conn, 200, "application/json", format!("{{\"ok\":true,\"path\":{},\"url\":{}}}", json_str(&path), json_str(&format!("/files{}", url_encode_path(&path)))).as_bytes())
}
