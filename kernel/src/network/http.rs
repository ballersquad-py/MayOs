//! HTTP/1.1 client with HTTPS (TLS 1.3 from the `embedded-tls` crate).
//!
//! Note: server certificates are not verified yet (there is no root
//! certificate store), so HTTPS here protects against eavesdropping but
//! not against an active man-in-the-middle.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use embedded_tls::blocking::{TlsConnection, TlsContext};
use embedded_tls::{Aes128GcmSha256, TlsConfig, UnsecureProvider};
use rand_core::SeedableRng;
use web::url::Url;

use super::tcp::TcpStream;

const MAX_BODY: usize = 24 * 1024 * 1024;
const TIMEOUT_MS: u64 = 15_000;
pub const USER_AGENT: &str = "Mozilla/5.0 (MayOS; x86_64) MayOSBrowser/0.1";

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Final URL after redirects.
    pub url: Url,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// GET a URL, following up to 8 redirects. `file://` reads from disk.
pub fn get(url: &Url) -> Result<Response, String> {
    let mut url = url.clone();
    for _ in 0..8 {
        let r = get_once(&url)?;
        if matches!(r.status, 301 | 302 | 303 | 307 | 308)
            && let Some(loc) = r.header("location")
        {
            url = url.join(loc).ok_or_else(|| format!("bad redirect to {}", loc))?;
            continue;
        }
        return Ok(r);
    }
    Err(String::from("too many redirects"))
}

fn get_once(url: &Url) -> Result<Response, String> {
    if url.scheme == "file" {
        let path = url.path.split('?').next().unwrap_or("/");
        let body = crate::fs::read_file(&percent_decode(path)).map_err(|e| format!("{}: {}", path, e))?;
        return Ok(Response { status: 200, headers: Vec::new(), body, url: url.clone() });
    }
    let ip = super::resolve(&url.host).map_err(|e| format!("{}: {}", url.host, e))?;
    let stream = TcpStream::connect(ip, url.port, TIMEOUT_MS).map_err(|e| format!("{}: {}", url.host, e))?;
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {}\r\nAccept: text/html,application/xhtml+xml,*/*;q=0.8\r\nAccept-Encoding: identity\r\nAccept-Language: en\r\nConnection: close\r\n\r\n",
        url.path, url.host, USER_AGENT
    );
    let raw = if url.scheme == "https" { tls_exchange(stream, &url.host, request.as_bytes())? } else { plain_exchange(stream, request.as_bytes())? };
    parse_response(raw, url)
}

fn plain_exchange(mut s: TcpStream, req: &[u8]) -> Result<Vec<u8>, String> {
    s.write_all(req, TIMEOUT_MS).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match s.read(&mut buf, TIMEOUT_MS) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if out.is_empty() => return Err(e.to_string()),
            Err(_) => break,
        }
        if out.len() > MAX_BODY {
            break;
        }
    }
    Ok(out)
}

/// The TCP stream as the byte pipe `embedded-tls` expects.
struct Io(TcpStream);

#[derive(Debug)]
struct IoError;

impl core::fmt::Display for IoError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("connection error")
    }
}

impl core::error::Error for IoError {}

impl embedded_io::Error for IoError {
    fn kind(&self) -> embedded_io::ErrorKind {
        embedded_io::ErrorKind::Other
    }
}

impl embedded_io::ErrorType for Io {
    type Error = IoError;
}

impl embedded_io::Read for Io {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, IoError> {
        self.0.read(buf, TIMEOUT_MS).map_err(|_| IoError)
    }
}

impl embedded_io::Write for Io {
    fn write(&mut self, buf: &[u8]) -> Result<usize, IoError> {
        self.0.write_all(buf, TIMEOUT_MS).map_err(|_| IoError)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> Result<(), IoError> {
        Ok(())
    }
}

/// Seed for the TLS random number generator.
fn seed() -> [u8; 32] {
    let mut s = [0u8; 32];
    for (i, chunk) in s.chunks_mut(8).enumerate() {
        let mut v = crate::arch::cpu::rdtsc().rotate_left(i as u32 * 13);
        // RDRAND when the CPU has it.
        let mut r = 0u64;
        let ok: u8;
        unsafe { core::arch::asm!("rdrand {0}", "setc {1}", out(reg) r, out(reg_byte) ok) };
        if ok != 0 {
            v ^= r;
        }
        v ^= crate::time::uptime_us().wrapping_mul(0x9e37_79b9_7f4a_7c15);
        chunk.copy_from_slice(&v.to_le_bytes());
    }
    s
}

fn tls_exchange(s: TcpStream, host: &str, req: &[u8]) -> Result<Vec<u8>, String> {
    let has_rdrand = core::arch::x86_64::__cpuid(1).ecx & (1 << 30) != 0;
    let seed = if has_rdrand {
        seed()
    } else {
        let mut s = [0u8; 32];
        for (i, c) in s.chunks_mut(8).enumerate() {
            c.copy_from_slice(&(crate::arch::cpu::rdtsc() ^ (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ crate::time::uptime_us()).to_le_bytes());
        }
        s
    };
    let rng = rand_chacha::ChaCha20Rng::from_seed(seed);
    let mut read_buf = vec![0u8; 16 * 1024 + 256];
    let mut write_buf = vec![0u8; 16 * 1024 + 256];
    let config = TlsConfig::new().with_server_name(host).enable_rsa_signatures();
    let mut tls: TlsConnection<Io, Aes128GcmSha256> = TlsConnection::new(Io(s), &mut read_buf, &mut write_buf);
    tls.open(TlsContext::new(&config, UnsecureProvider::new::<Aes128GcmSha256>(rng)))
        .map_err(|e| format!("secure connection to {} failed ({:?})", host, e))?;
    let mut sent = 0;
    while sent < req.len() {
        sent += tls.write(&req[sent..]).map_err(|e| format!("TLS write: {:?}", e))?;
    }
    tls.flush().map_err(|e| format!("TLS flush: {:?}", e))?;
    let mut out = Vec::new();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match tls.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            // Many servers just drop the connection after the body.
            Err(_) => break,
        }
        if out.len() > MAX_BODY {
            break;
        }
    }
    if out.is_empty() {
        return Err(format!("{} sent nothing back", host));
    }
    Ok(out)
}

fn parse_response(raw: Vec<u8>, url: &Url) -> Result<Response, String> {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or("the server's reply was cut off")?;
    let head = String::from_utf8_lossy(&raw[..end]).into_owned();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status = status_line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or("not an HTTP reply")?;
    let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
    let mut body = raw[end + 4..].to_vec();
    let chunked = headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked"));
    if chunked {
        body = dechunk(&body);
    } else if let Some(len) = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.parse::<usize>().ok()) {
        body.truncate(len);
    }
    Ok(Response { status, headers, body, url: url.clone() })
}

fn dechunk(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let Some(nl) = b[i..].windows(2).position(|w| w == b"\r\n") else { break };
        let size_str = String::from_utf8_lossy(&b[i..i + nl]);
        let size = usize::from_str_radix(size_str.split(';').next().unwrap_or("").trim(), 16).unwrap_or(0);
        i += nl + 2;
        if size == 0 {
            break;
        }
        let end = (i + size).min(b.len());
        out.extend_from_slice(&b[i..end]);
        i = end + 2;
    }
    out
}

pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
