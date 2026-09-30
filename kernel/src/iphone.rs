//! iPhone pairing: what iTunes/Apple Mobile Device Support does when an
//! iPhone is plugged in, so the phone asks "Trust This Computer?" and then
//! allows its Personal Hotspot over USB.
//!
//! The phone's "Apple Mobile Device" USB interface carries usbmux: TCP-like
//! connections to services on the phone. We connect to lockdownd (port
//! 62078) and send it a Pair request (XML property lists) with a pair
//! record: a root certificate, a host certificate and a certificate for
//! the phone's own public key, all RSA-2048 and made here. The host keys
//! are made once and kept in /config/iphone/host.key.
//!
//! Protocol details follow libimobiledevice/usbmuxd.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use crypto_bigint::modular::runtime_mod::{DynResidue, DynResidueParams};
use crypto_bigint::{Encoding, NonZero, U1024, U2048, U4096};
use sha2::{Digest, Sha256};

use crate::drivers::usbnet::UsbNet;
use crate::fs;
use crate::sync::Spin;
use crate::time::uptime_ms;

const DIR: &str = "/config/iphone";

/// What the pairing is doing (for Settings and the log).
static STATUS: Spin<String> = Spin::new(String::new());

pub fn status() -> String {
    STATUS.lock().clone()
}

fn set_status(s: &str) {
    crate::kprintln!("iphone: {}", s);
    *STATUS.lock() = String::from(s);
}

// --- random numbers and RSA ---------------------------------------------

fn random(buf: &mut [u8]) {
    crate::proc::linux::fill_random(buf);
}

const SMALL_PRIMES: [u32; 54] = [
    3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47, 53, 59, 61, 67, 71, 73, 79, 83, 89, 97, 101, 103, 107, 109, 113, 127, 131, 137, 139, 149, 151,
    157, 163, 167, 173, 179, 181, 191, 193, 197, 199, 211, 223, 227, 229, 233, 239, 241, 251, 257,
];

fn rem_small(x: &U1024, m: u32) -> u32 {
    let mut r: u64 = 0;
    for b in x.to_be_bytes() {
        r = ((r << 8) | b as u64) % m as u64;
    }
    r as u32
}

/// Miller-Rabin with random bases.
fn probably_prime(n: &U1024, rounds: usize) -> bool {
    let one = U1024::ONE;
    let n1 = n.wrapping_sub(&one);
    let s = n1.trailing_zeros();
    let d = n1.shr_vartime(s);
    let params = DynResidueParams::new(n);
    let one_r = DynResidue::new(&one, params);
    let minus_one = DynResidue::new(&n1, params);
    'outer: for _ in 0..rounds {
        let mut b = [0u8; 128];
        random(&mut b);
        b[0] &= 0x3f;
        let a = U1024::from_be_slice(&b).wrapping_add(&U1024::from_u8(2));
        let mut x = DynResidue::new(&a, params).pow(&d);
        if x == one_r || x == minus_one {
            continue;
        }
        for _ in 1..s {
            x = x.square();
            if x == minus_one {
                continue 'outer;
            }
            if x == one_r {
                return false;
            }
        }
        return false;
    }
    true
}

fn random_prime() -> U1024 {
    loop {
        let mut b = [0u8; 128];
        random(&mut b);
        b[0] |= 0xc0; // two top bits: p*q has 2048 bits
        b[127] |= 1;
        let mut p = U1024::from_be_slice(&b);
        // Walk up from the random start past small factors.
        for _ in 0..2000 {
            if SMALL_PRIMES.iter().all(|&sp| rem_small(&p, sp) != 0) && rem_small(&p, 65537) != 1 && probably_prime(&p, 24) {
                return p;
            }
            p = p.wrapping_add(&U1024::from_u8(2));
        }
    }
}

struct RsaKey {
    n: U2048,
    d: U2048,
}

const E: u32 = 65537;

fn widen(x: &U1024) -> U2048 {
    let mut b = [0u8; 256];
    b[128..].copy_from_slice(&x.to_be_bytes());
    U2048::from_be_slice(&b)
}

fn rsa_generate() -> RsaKey {
    loop {
        let (p, q) = (random_prime(), random_prime());
        if p == q {
            continue;
        }
        let n = widen(&p).wrapping_mul(&widen(&q));
        let phi = widen(&p.wrapping_sub(&U1024::ONE)).wrapping_mul(&widen(&q.wrapping_sub(&U1024::ONE)));
        // d = (1 + k*phi) / e with k = -phi^-1 mod e.
        let phi_mod_e = {
            let mut r: u64 = 0;
            for b in phi.to_be_bytes() {
                r = ((r << 8) | b as u64) % E as u64;
            }
            r
        };
        if phi_mod_e == 0 {
            continue;
        }
        let Some(k) = (1..E as u64).find(|k| (1 + k * phi_mod_e) % E as u64 == 0) else { continue };
        let mut wide = [0u8; 512];
        wide[256..].copy_from_slice(&phi.to_be_bytes());
        let phi4 = U4096::from_be_slice(&wide);
        let num = phi4.wrapping_mul(&U4096::from_u64(k)).wrapping_add(&U4096::ONE);
        let (dq, _) = num.div_rem(&NonZero::new(U4096::from_u32(E)).unwrap());
        let db = dq.to_be_bytes();
        let d = U2048::from_be_slice(&db[256..]);
        return RsaKey { n, d };
    }
}

/// PKCS#1 v1.5 signature with SHA-256.
fn rsa_sign(k: &RsaKey, msg: &[u8]) -> Vec<u8> {
    const PREFIX: [u8; 19] = [0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05, 0x00, 0x04, 0x20];
    let hash = Sha256::digest(msg);
    let mut em = [0xffu8; 256];
    em[0] = 0;
    em[1] = 1;
    let t = 256 - PREFIX.len() - 32;
    em[t - 1] = 0;
    em[t..t + PREFIX.len()].copy_from_slice(&PREFIX);
    em[t + PREFIX.len()..].copy_from_slice(&hash);
    let m = U2048::from_be_slice(&em);
    let params = DynResidueParams::new(&k.n);
    let s = DynResidue::new(&m, params).pow(&k.d).retrieve();
    s.to_be_bytes().to_vec()
}

// --- DER, base64, PEM ---------------------------------------------------

fn der(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut v = alloc::vec![tag];
    let n = body.len();
    if n < 0x80 {
        v.push(n as u8);
    } else if n < 0x100 {
        v.extend_from_slice(&[0x81, n as u8]);
    } else {
        v.extend_from_slice(&[0x82, (n >> 8) as u8, n as u8]);
    }
    v.extend_from_slice(body);
    v
}

fn seq(parts: &[&[u8]]) -> Vec<u8> {
    der(0x30, &parts.concat())
}

fn der_uint(be: &[u8]) -> Vec<u8> {
    let mut b: Vec<u8> = be.iter().copied().skip_while(|&x| x == 0).collect();
    if b.is_empty() || b[0] & 0x80 != 0 {
        b.insert(0, 0);
    }
    der(0x02, &b)
}

const OID_RSA: &[u8] = &[0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
const OID_SHA256_RSA: &[u8] = &[0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b];
const NULL: &[u8] = &[0x05, 0x00];

/// RSAPublicKey (PKCS#1).
fn rsa_public_der(n: &U2048) -> Vec<u8> {
    seq(&[&der_uint(&n.to_be_bytes()), &der_uint(&E.to_be_bytes())])
}

fn spki(pkcs1: &[u8]) -> Vec<u8> {
    let mut bits = alloc::vec![0u8];
    bits.extend_from_slice(pkcs1);
    seq(&[&seq(&[OID_RSA, NULL]), &der(0x03, &bits)])
}

/// An X.509 v3 certificate for `pkcs1_pub`, signed by `issuer`.
fn certificate(pkcs1_pub: &[u8], issuer: &RsaKey, ca: bool, serial: u8) -> Vec<u8> {
    let version = der(0xa0, &der(0x02, &[2]));
    let alg = seq(&[OID_SHA256_RSA, NULL]);
    let name = seq(&[]);
    let validity = seq(&[&der(0x17, b"250101000000Z"), &der(0x17, b"450101000000Z")]);
    let basic = if ca { seq(&[&der(0x01, &[0xff])]) } else { seq(&[]) };
    let mut exts = Vec::new();
    exts.extend_from_slice(&seq(&[&[0x06, 0x03, 0x55, 0x1d, 0x13], &der(0x01, &[0xff]), &der(0x04, &basic)]));
    if !ca {
        // keyUsage: digitalSignature, keyEncipherment.
        exts.extend_from_slice(&seq(&[&[0x06, 0x03, 0x55, 0x1d, 0x0f], &der(0x01, &[0xff]), &der(0x04, &[0x03, 0x02, 0x05, 0xa0])]));
    }
    let extensions = der(0xa3, &der(0x30, &exts));
    let tbs = seq(&[&version, &der(0x02, &[serial]), &alg, &name, &validity, &name, &spki(pkcs1_pub), &extensions]);
    let mut sig = alloc::vec![0u8];
    sig.extend_from_slice(&rsa_sign(issuer, &tbs));
    seq(&[&tbs, &alg, &der(0x03, &sig)])
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(data: &[u8]) -> String {
    let mut s = String::new();
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= c.len() {
                s.push(B64[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

fn unbase64(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes() {
        let v = match B64.iter().position(|&x| x == c) {
            Some(v) => v as u32,
            None => continue,
        };
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

fn pem(kind: &str, der: &[u8]) -> String {
    let b = base64(der);
    let mut s = format!("-----BEGIN {}-----\n", kind);
    for line in b.as_bytes().chunks(64) {
        s.push_str(core::str::from_utf8(line).unwrap_or(""));
        s.push('\n');
    }
    s.push_str(&format!("-----END {}-----\n", kind));
    s
}

fn unpem(text: &str) -> Vec<u8> {
    let body: String = text.lines().filter(|l| !l.starts_with("-----")).collect();
    unbase64(&body)
}

// --- property lists -----------------------------------------------------

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

enum Pv<'a> {
    S(&'a str),
    D(&'a [u8]),
    B(bool),
    Dict(Vec<(&'a str, Pv<'a>)>),
}

fn plist_value(v: &Pv, out: &mut String) {
    match v {
        Pv::S(s) => out.push_str(&format!("<string>{}</string>", xml_escape(s))),
        Pv::D(d) => out.push_str(&format!("<data>{}</data>", base64(d))),
        Pv::B(b) => out.push_str(if *b { "<true/>" } else { "<false/>" }),
        Pv::Dict(items) => {
            out.push_str("<dict>");
            for (k, v) in items {
                out.push_str(&format!("<key>{}</key>", xml_escape(k)));
                plist_value(v, out);
            }
            out.push_str("</dict>");
        }
    }
}

fn plist(items: Vec<(&str, Pv)>) -> Vec<u8> {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">",
    );
    plist_value(&Pv::Dict(items), &mut s);
    s.push_str("</plist>\n");
    s.into_bytes()
}

/// Text of the value after `<key>key</key>` (string or data), if any.
fn plist_get(xml: &str, key: &str) -> Option<String> {
    let k = format!("<key>{}</key>", key);
    let at = xml.find(&k)? + k.len();
    let rest = xml[at..].trim_start();
    for tag in ["string", "data", "integer"] {
        let open = format!("<{}>", tag);
        if let Some(r) = rest.strip_prefix(&open) {
            let end = r.find(&format!("</{}>", tag))?;
            return Some(String::from(&r[..end]));
        }
    }
    None
}

// --- usbmux -------------------------------------------------------------

const MUX_VERSION: u32 = 0;
const MUX_SETUP: u32 = 2;
const MUX_TCP: u32 = 6;
const LOCKDOWN_PORT: u16 = 62078;
const SPORT: u16 = 0x0a01;

struct Mux {
    ch: Arc<UsbNet>,
    version: u32,
    tx_seq: u16,
    rx_seq: u16,
    inbuf: Vec<u8>,
    // TCP state of our one connection.
    seq: u32,
    ack: u32,
    stream: Vec<u8>,
}

impl Mux {
    fn send(&mut self, proto: u32, payload: &[u8]) {
        let hlen = if self.version >= 2 { 16 } else { 8 };
        let mut p = Vec::with_capacity(hlen + payload.len());
        p.extend_from_slice(&proto.to_be_bytes());
        p.extend_from_slice(&((hlen + payload.len()) as u32).to_be_bytes());
        if self.version >= 2 {
            if proto == MUX_SETUP {
                self.tx_seq = 0;
                self.rx_seq = 0xffff;
            }
            p.extend_from_slice(&0xfeed_faceu32.to_be_bytes());
            p.extend_from_slice(&self.tx_seq.to_be_bytes());
            p.extend_from_slice(&self.rx_seq.to_be_bytes());
            self.tx_seq = self.tx_seq.wrapping_add(1);
        }
        p.extend_from_slice(payload);
        self.ch.send(&p);
    }

    /// Next whole mux packet: (protocol, payload).
    fn recv(&mut self, ms: u64) -> Option<(u32, Vec<u8>)> {
        let start = uptime_ms();
        loop {
            if self.inbuf.len() >= 8 {
                let proto = u32::from_be_bytes(self.inbuf[0..4].try_into().unwrap());
                let len = u32::from_be_bytes(self.inbuf[4..8].try_into().unwrap()) as usize;
                let hlen = if self.version >= 2 && proto != MUX_VERSION { 16 } else { 8 };
                if len < hlen || len > 1 << 20 {
                    self.inbuf.clear();
                    continue;
                }
                if self.inbuf.len() >= len {
                    if hlen == 16 {
                        self.rx_seq = u16::from_be_bytes([self.inbuf[12], self.inbuf[13]]);
                    }
                    let payload = self.inbuf[hlen..len].to_vec();
                    self.inbuf.drain(..len);
                    return Some((proto, payload));
                }
            }
            if self.ch.gone.load(Ordering::Relaxed) || uptime_ms() - start > ms {
                return None;
            }
            match self.ch.recv() {
                Some(d) => self.inbuf.extend_from_slice(&d),
                None => crate::proc::sched::sleep_ms(2),
            }
        }
    }

    fn tcp(&mut self, flags: u8, data: &[u8]) {
        let mut h = [0u8; 20];
        h[0..2].copy_from_slice(&SPORT.to_be_bytes());
        h[2..4].copy_from_slice(&LOCKDOWN_PORT.to_be_bytes());
        h[4..8].copy_from_slice(&self.seq.to_be_bytes());
        h[8..12].copy_from_slice(&self.ack.to_be_bytes());
        h[12] = 5 << 4;
        h[13] = flags;
        h[14..16].copy_from_slice(&((131072u32 >> 8) as u16).to_be_bytes());
        let mut p = h.to_vec();
        p.extend_from_slice(data);
        self.send(MUX_TCP, &p);
        self.seq = self.seq.wrapping_add(data.len() as u32);
    }

    fn handshake(&mut self) -> bool {
        // Version: we speak 2.0.
        let mut v = Vec::new();
        for x in [2u32, 0, 0] {
            v.extend_from_slice(&x.to_be_bytes());
        }
        self.send(MUX_VERSION, &v);
        let Some((MUX_VERSION, r)) = self.recv(3000) else { return false };
        let major = u32::from_be_bytes(r.get(0..4).and_then(|s| s.try_into().ok()).unwrap_or([0; 4]));
        self.version = major;
        if major >= 2 {
            self.send(MUX_SETUP, &[0x07]);
        }
        // Connect to lockdownd.
        self.seq = 0;
        self.ack = 0;
        self.tcp(0x02, &[]); // SYN
        let start = uptime_ms();
        while uptime_ms() - start < 3000 {
            let Some((proto, p)) = self.recv(3000) else { return false };
            if proto != MUX_TCP || p.len() < 20 {
                continue;
            }
            let flags = p[13];
            if flags & 0x04 != 0 {
                return false; // RST: refused
            }
            if flags & 0x12 == 0x12 {
                let their = u32::from_be_bytes(p[4..8].try_into().unwrap());
                self.seq = 1;
                self.ack = their.wrapping_add(1);
                self.tcp(0x10, &[]); // ACK
                return true;
            }
        }
        false
    }

    fn write(&mut self, data: &[u8]) {
        for chunk in data.chunks(16 * 1024) {
            self.tcp(0x10, chunk); // ACK (as usbmuxd)
        }
    }

    /// Read exactly `n` bytes of the stream.
    fn read(&mut self, n: usize, ms: u64) -> Option<Vec<u8>> {
        let start = uptime_ms();
        while self.stream.len() < n {
            let left = ms.saturating_sub(uptime_ms() - start);
            let (proto, p) = self.recv(left.max(1))?;
            if proto != MUX_TCP || p.len() < 20 {
                continue;
            }
            let off = ((p[12] >> 4) as usize * 4).max(20);
            if p[13] & 0x05 != 0 {
                return None; // RST or FIN
            }
            if p.len() > off {
                self.stream.extend_from_slice(&p[off..]);
                self.ack = self.ack.wrapping_add((p.len() - off) as u32);
                self.tcp(0x10, &[]);
            }
        }
        Some(self.stream.drain(..n).collect())
    }

    /// One lockdown request and its reply (length-prefixed plists).
    fn request(&mut self, body: &[u8]) -> Option<String> {
        let mut m = (body.len() as u32).to_be_bytes().to_vec();
        m.extend_from_slice(body);
        self.write(&m);
        let len = u32::from_be_bytes(self.read(4, 10_000)?.try_into().ok()?) as usize;
        let reply = self.read(len.min(1 << 20), 10_000)?;
        Some(String::from_utf8_lossy(&reply).into_owned())
    }
}

// --- host identity ------------------------------------------------------

struct Host {
    root: RsaKey,
    host: RsaKey,
    host_id: String,
    buid: String,
}

fn uuid() -> String {
    let mut b = [0u8; 16];
    random(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{:02X}", x)).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

fn load_host() -> Host {
    let path = format!("{}/host.key", DIR);
    if let Ok(d) = fs::read_file(&path)
        && d.len() == 256 * 4 + 72
    {
        let n = |i: usize| U2048::from_be_slice(&d[i * 256..(i + 1) * 256]);
        let text = String::from_utf8_lossy(&d[1024..]).into_owned();
        let (host_id, buid) = (String::from(&text[0..36]), String::from(&text[36..72]));
        return Host { root: RsaKey { n: n(0), d: n(1) }, host: RsaKey { n: n(2), d: n(3) }, host_id, buid };
    }
    set_status("making this computer's pairing keys (once, a few seconds)");
    let t0 = uptime_ms();
    let h = Host { root: rsa_generate(), host: rsa_generate(), host_id: uuid(), buid: uuid() };
    crate::kprintln!("iphone: keys made in {} ms", uptime_ms() - t0);
    let mut d = Vec::new();
    for x in [&h.root.n, &h.root.d, &h.host.n, &h.host.d] {
        d.extend_from_slice(&x.to_be_bytes());
    }
    d.extend_from_slice(h.host_id.as_bytes());
    d.extend_from_slice(h.buid.as_bytes());
    let _ = fs::create_dir("/config");
    let _ = fs::create_dir(DIR);
    let _ = fs::write_file(&path, &d);
    h
}

// --- pairing ------------------------------------------------------------

struct Job {
    ch: Arc<UsbNet>,
    udid: String,
}

extern "C" fn pair_thread(arg: usize) {
    let job = unsafe { alloc::boxed::Box::from_raw(arg as *mut Job) };
    let marker = format!("{}/{}.paired", DIR, job.udid);
    let mut mux = Mux { ch: job.ch.clone(), version: 0, tx_seq: 0, rx_seq: 0, inbuf: Vec::new(), seq: 0, ack: 0, stream: Vec::new() };
    if !mux.handshake() {
        set_status("could not talk to the iPhone (usbmux)");
        return;
    }
    let q = plist(alloc::vec![("Label", Pv::S("MayOS")), ("Request", Pv::S("QueryType"))]);
    match mux.request(&q) {
        Some(r) if r.contains("com.apple.mobile.lockdown") => {}
        _ => {
            set_status("the iPhone's lockdown service did not answer");
            return;
        }
    }
    if fs::exists(&marker) {
        set_status("iPhone already trusts this computer");
        return;
    }
    let get = plist(alloc::vec![("Label", Pv::S("MayOS")), ("Key", Pv::S("DevicePublicKey")), ("Request", Pv::S("GetValue"))]);
    let Some(dev_key) = mux.request(&get).and_then(|r| plist_get(&r, "Value")) else {
        set_status("could not read the iPhone's public key");
        return;
    };
    let dev_pkcs1 = unpem(&String::from_utf8_lossy(&unbase64(&dev_key)));
    let host = load_host();
    let root_pub = rsa_public_der(&host.root.n);
    let root_cert = pem("CERTIFICATE", &certificate(&root_pub, &host.root, true, 0));
    let host_cert = pem("CERTIFICATE", &certificate(&rsa_public_der(&host.host.n), &host.root, false, 1));
    let dev_cert = pem("CERTIFICATE", &certificate(&dev_pkcs1, &host.root, false, 2));
    set_status("unlock your iPhone and tap \"Trust\"");
    let start = uptime_ms();
    loop {
        if job.ch.gone.load(Ordering::Relaxed) {
            set_status("iPhone unplugged");
            return;
        }
        let record = Pv::Dict(alloc::vec![
            ("DeviceCertificate", Pv::D(dev_cert.as_bytes())),
            ("HostCertificate", Pv::D(host_cert.as_bytes())),
            ("HostID", Pv::S(&host.host_id)),
            ("RootCertificate", Pv::D(root_cert.as_bytes())),
            ("SystemBUID", Pv::S(&host.buid)),
        ]);
        let pair = plist(alloc::vec![
            ("Label", Pv::S("MayOS")),
            ("PairRecord", record),
            ("PairingOptions", Pv::Dict(alloc::vec![("ExtendedPairingErrors", Pv::B(true))])),
            ("ProtocolVersion", Pv::S("2")),
            ("Request", Pv::S("Pair")),
        ]);
        let Some(r) = mux.request(&pair) else {
            set_status("the iPhone stopped answering");
            return;
        };
        match plist_get(&r, "Error").as_deref() {
            None => {
                let _ = fs::write_file(&marker, host.host_id.as_bytes());
                set_status("paired: the iPhone trusts this computer (turn on Personal Hotspot)");
                return;
            }
            Some("PairingDialogResponsePending") | Some("PasswordProtected") => {
                if uptime_ms() - start > 120_000 {
                    set_status("no answer on the iPhone: unplug and plug it in again");
                    return;
                }
                crate::proc::sched::sleep_ms(1500);
            }
            Some("UserDeniedPairing") => {
                set_status("\"Don't Trust\" was tapped on the iPhone");
                return;
            }
            Some(e) => {
                set_status(&format!("pairing failed: {}", e));
                return;
            }
        }
    }
}

/// An iPhone's "Apple Mobile Device" interface is ready: pair with it.
pub fn start(ch: Arc<UsbNet>, udid: String) {
    let job = alloc::boxed::Box::new(Job { ch, udid });
    crate::proc::sched::spawn_kernel("iphone", pair_thread, alloc::boxed::Box::into_raw(job) as usize);
}
