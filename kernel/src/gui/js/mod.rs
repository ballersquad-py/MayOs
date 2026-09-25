//! JavaScript for the browser: the QuickJS engine (C, vendored in
//! `vendor/quickjs`, MIT licence) plus the C library pieces it needs and
//! the host side of the DOM. The DOM API itself is `prelude.js`.

pub mod libc;

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use web::dom::{self, Node, NodeType};
use web::url::Url;

use crate::sync::Spin;

#[repr(C)]
struct Mjs {
    _p: [u8; 0],
}

unsafe extern "C" {
    fn mjs_new() -> *mut Mjs;
    fn mjs_free(m: *mut Mjs);
    fn mjs_eval(m: *mut Mjs, src: *const u8, len: usize, file: *const u8) -> i32;
    fn mjs_eval_string(m: *mut Mjs, src: *const u8, len: usize, out: *mut u8, cap: i64) -> i64;
    fn mjs_call(m: *mut Mjs, name: *const u8, a: i32, s: *const u8, b: i32, c: i32, s2: *const u8) -> i32;
}

const PRELUDE: &str = include_str!("prelude.js");
/// A script may run this long before it is stopped (endless loops).
const SLICE_MS: u64 = 3000;
/// The id the scripts use for `document` itself.
pub const DOCUMENT_ID: u32 = u32::MAX - 1;

/// Things scripts asked the browser to do.
pub enum Request {
    Navigate(String),
    Alert(String),
    Scroll(i32),
    Canvas(u32, String),
    Back,
}

/// What the scripts can see and change while they run.
pub struct Host {
    pub doc: Node,
    /// Nodes created by scripts or removed from the page (still usable).
    pub limbo: Vec<Node>,
    pub url: Url,
    pub title: String,
    pub dirty: bool,
    pub requests: Vec<Request>,
    pub console: Vec<(String, String)>,
    pub boxes: Vec<(u32, i32, i32, i32, i32)>,
    pub scroll_y: i32,
    pub viewport: (i32, i32),
}

/// The host of the script currently running (set around every call).
static HOST: Spin<Option<usize>> = Spin::new(None);
static DEADLINE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub struct Engine {
    m: *mut Mjs,
}

unsafe impl Send for Engine {}

impl Engine {
    /// A fresh JavaScript world with the DOM installed.
    pub fn new(host: &mut Host) -> Engine {
        let e = Engine { m: unsafe { mjs_new() } };
        e.with(host, |m| unsafe { mjs_eval(m, PRELUDE.as_ptr(), PRELUDE.len(), c"prelude.js".as_ptr() as *const u8) });
        e
    }

    fn with<R>(&self, host: &mut Host, f: impl FnOnce(*mut Mjs) -> R) -> R {
        *HOST.lock() = Some(host as *mut Host as usize);
        DEADLINE.store(crate::time::uptime_ms() + SLICE_MS, core::sync::atomic::Ordering::Relaxed);
        let r = f(self.m);
        *HOST.lock() = None;
        r
    }

    pub fn run(&self, host: &mut Host, src: &str, name: &str) {
        let mut file = String::from(name);
        file.push('\0');
        self.with(host, |m| unsafe { mjs_eval(m, src.as_ptr(), src.len(), file.as_ptr()) });
    }

    /// Evaluate for the console: the result as text.
    pub fn eval(&self, host: &mut Host, src: &str) -> String {
        let mut out = alloc::vec![0u8; 4096];
        let n = self.with(host, |m| unsafe { mjs_eval_string(m, src.as_ptr(), src.len(), out.as_mut_ptr(), out.len() as i64) });
        out.truncate(n.max(0) as usize);
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Call `__tick`; returns ms until timers want to run again (-1: none).
    pub fn tick(&self, host: &mut Host) -> i32 {
        self.call(host, "__tick\0", 0, "", 0, 0, "")
    }

    /// Deliver an input event; true if the page cancelled the default.
    pub fn dispatch(&self, host: &mut Host, node: i32, kind: &str, x: i32, y: i32, key: &str) -> bool {
        self.call(host, "__dispatch\0", node, kind, x, y, key) != 0
    }

    pub fn loaded(&self, host: &mut Host) {
        self.call(host, "__loaded\0", 0, "", 0, 0, "");
    }

    fn call(&self, host: &mut Host, name: &str, a: i32, s: &str, b: i32, c: i32, s2: &str) -> i32 {
        let (mut s, mut s2) = (String::from(s), String::from(s2));
        s.push('\0');
        s2.push('\0');
        self.with(host, |m| unsafe { mjs_call(m, name.as_ptr(), a, s.as_ptr(), b, c, s2.as_ptr()) })
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        unsafe { mjs_free(self.m) };
    }
}

/// Scripts of a document, in order: (external src, inline code).
pub fn scripts(doc: &Node) -> Vec<(Option<String>, String)> {
    let mut v = Vec::new();
    doc.for_each_element(&mut |e, n| {
        if e.tag_name != "script" {
            return;
        }
        let ty = e.attrs.get("type").map(|t| t.to_ascii_lowercase()).unwrap_or_default();
        if !(ty.is_empty() || ty.contains("javascript") || ty == "text/ecmascript") || e.attrs.contains_key("nomodule") {
            return;
        }
        match e.attrs.get("src") {
            Some(s) => v.push((Some(s.clone()), String::new())),
            None => v.push((None, n.text_content())),
        }
    });
    v
}

// -------------------------------------------------------------------------
// Callbacks from C
// -------------------------------------------------------------------------

fn host() -> Option<&'static mut Host> {
    let p = (*HOST.lock())?;
    Some(unsafe { &mut *(p as *mut Host) })
}

unsafe fn cstr<'a>(p: *const u8) -> &'a str {
    if p.is_null() {
        return "";
    }
    let mut n = 0;
    while unsafe { *p.add(n) } != 0 {
        n += 1;
    }
    core::str::from_utf8(unsafe { core::slice::from_raw_parts(p, n) }).unwrap_or("")
}

unsafe fn bytes<'a>(p: *const u8, len: i64) -> &'a str {
    if p.is_null() || len <= 0 {
        return "";
    }
    core::str::from_utf8(unsafe { core::slice::from_raw_parts(p, len as usize) }).unwrap_or("")
}

fn put(s: &str, out: *mut u8, cap: i64) -> i64 {
    let n = s.len().min(cap.max(0) as usize);
    unsafe { core::ptr::copy_nonoverlapping(s.as_ptr(), out, n) };
    s.len() as i64
}

impl Host {
    fn node(&self, id: u32) -> Option<&Node> {
        self.doc.find_id(id).or_else(|| self.limbo.iter().find_map(|n| n.find_id(id)))
    }

    fn node_mut(&mut self, id: u32) -> Option<&mut Node> {
        if self.doc.find_id(id).is_some() {
            return self.doc.find_id_mut(id);
        }
        self.limbo.iter_mut().find_map(|n| n.find_id_mut(id))
    }

    /// Take a node out of wherever it is.
    fn take(&mut self, id: u32) -> Option<Node> {
        if let Some(n) = self.doc.detach(id) {
            self.dirty = true;
            return Some(n);
        }
        if let Some(i) = self.limbo.iter().position(|n| n.id == id) {
            return Some(self.limbo.remove(i));
        }
        self.limbo.iter_mut().find_map(|n| n.detach(id))
    }

    fn in_doc(&self, id: u32) -> bool {
        self.doc.find_id(id).is_some()
    }

    fn resolve(&self, href: &str) -> String {
        self.url.join(href).map(|u| u.to_string()).unwrap_or_else(|| href.to_string())
    }
}

#[unsafe(no_mangle)]
extern "C" fn rs_query(root: i32, sel: *const u8, all: i32, out: *mut i32, max: i32) -> i32 {
    let Some(h) = host() else { return 0 };
    let sel = unsafe { cstr(sel) };
    let root = if root as u32 == DOCUMENT_ID { h.doc.id } else { root as u32 };
    let tree: &Node = if h.in_doc(root) { &h.doc } else { match h.limbo.iter().find(|n| n.find_id(root).is_some()) { Some(t) => t, None => return 0 } };
    let mut ids = web::style::query(tree, root, sel, all != 0);
    if root == tree.id {
        // The root element itself can match too (`querySelector("html")`).
        let mut own = web::style::query(&dom::elem("#doc".into(), Default::default(), alloc::vec![tree.clone()]), u32::MAX, sel, true);
        own.retain(|&i| i == root);
        ids.splice(0..0, own);
    }
    let n = ids.len().min(max as usize);
    for (i, id) in ids.iter().take(n).enumerate() {
        unsafe { *out.add(i) = *id as i32 };
    }
    n as i32
}

#[unsafe(no_mangle)]
extern "C" fn rs_create(tag: *const u8, text: i32) -> i32 {
    let Some(h) = host() else { return -1 };
    let n = if text != 0 { dom::text(String::new()) } else { dom::elem(unsafe { cstr(tag) }.to_ascii_lowercase(), Default::default(), Vec::new()) };
    let id = n.id;
    h.limbo.push(n);
    id as i32
}

const W_TEXT: i32 = 0;
const W_HTML: i32 = 1;
const W_ATTR: i32 = 2;
const W_TAG: i32 = 3;
const W_STYLE: i32 = 4;
const W_OUTER: i32 = 5;
const W_TYPE: i32 = 6;
const W_VALUE: i32 = 7;
const W_RMATTR: i32 = 8;

#[unsafe(no_mangle)]
extern "C" fn rs_get(id: i32, what: i32, name: *const u8, out: *mut u8, cap: i64) -> i64 {
    let Some(h) = host() else { return -1 };
    if id as u32 == DOCUMENT_ID {
        return if what == W_TYPE { put("9", out, cap) } else { -1 };
    }
    let Some(n) = h.node(id as u32) else { return -1 };
    let name = unsafe { cstr(name) };
    let s = match (what, &n.node_type) {
        (W_TYPE, NodeType::Element(_)) => "1".to_string(),
        (W_TYPE, NodeType::Text(_)) => "3".to_string(),
        (W_TEXT, _) => n.text_content(),
        (W_HTML, _) => n.inner_html(),
        (W_OUTER, _) => n.outer_html(),
        (W_TAG, NodeType::Element(e)) => e.tag_name.clone(),
        (W_ATTR, NodeType::Element(e)) if name == "\0names" || name.is_empty() => e.attrs.keys().cloned().collect::<Vec<_>>().join("\0"),
        (W_ATTR, NodeType::Element(e)) => match e.attrs.get(name) {
            Some(v) => v.clone(),
            None => return -1,
        },
        (W_VALUE, NodeType::Element(e)) => {
            if e.tag_name == "textarea" { n.text_content() } else { e.attrs.get("value").cloned().unwrap_or_default() }
        }
        (W_STYLE, NodeType::Element(e)) => {
            let decls = web::css::parse_declarations(e.attrs.get("style").map(|s| s.as_str()).unwrap_or(""));
            decls.iter().rev().find(|d| d.name == name).map(|d| d.value.clone()).unwrap_or_default()
        }
        _ => return -1,
    };
    put(&s, out, cap)
}

#[unsafe(no_mangle)]
extern "C" fn rs_set(id: i32, what: i32, name: *const u8, value: *const u8, vlen: i64) {
    let Some(h) = host() else { return };
    let name = unsafe { cstr(name) }.to_string();
    let value = if vlen < 0 { None } else { Some(unsafe { bytes(value, vlen) }.to_string()) };
    let in_doc = h.in_doc(id as u32);
    let Some(n) = h.node_mut(id as u32) else { return };
    match what {
        W_TEXT => match &mut n.node_type {
            NodeType::Text(t) => *t = value.unwrap_or_default(),
            NodeType::Element(_) => {
                n.children.clear();
                n.children.push(dom::text(value.unwrap_or_default()));
            }
        },
        W_HTML => n.children = web::html::parse_fragment(&value.unwrap_or_default()),
        W_ATTR | W_VALUE | W_RMATTR | W_STYLE => {
            let NodeType::Element(e) = &mut n.node_type else { return };
            match what {
                W_ATTR => {
                    e.attrs.insert(name, value.unwrap_or_default());
                }
                W_VALUE => {
                    e.attrs.insert(String::from("value"), value.unwrap_or_default());
                }
                W_RMATTR => {
                    e.attrs.remove(&name);
                }
                _ => {
                    let mut decls: Vec<(String, String)> = web::css::parse_declarations(e.attrs.get("style").map(|s| s.as_str()).unwrap_or(""))
                        .into_iter()
                        .filter(|d| d.name != name)
                        .map(|d| (d.name, d.value))
                        .collect();
                    if let Some(v) = value.filter(|v| !v.is_empty()) {
                        decls.push((name, v));
                    }
                    let s: Vec<String> = decls.iter().map(|(k, v)| alloc::format!("{}: {}", k, v)).collect();
                    e.attrs.insert(String::from("style"), s.join("; "));
                }
            }
        }
        _ => return,
    }
    if in_doc {
        h.dirty = true;
    }
}

#[unsafe(no_mangle)]
extern "C" fn rs_rel(id: i32, rel: i32) -> i32 {
    let Some(h) = host() else { return -1 };
    let id = id as u32;
    if id == DOCUMENT_ID {
        return if rel == 1 || rel == 4 { h.doc.id as i32 } else { -1 };
    }
    if id == h.doc.id && rel == 0 {
        return DOCUMENT_ID as i32;
    }
    let find_parent = |t: &Node| t.parent_of(id).map(|(p, i)| (p.id, i, p.children.iter().map(|c| c.id).collect::<Vec<_>>()));
    match rel {
        1 | 4 => {
            let Some(n) = h.node(id) else { return -1 };
            let c = if rel == 1 { n.children.first() } else { n.children.last() };
            c.map(|c| c.id as i32).unwrap_or(-1)
        }
        _ => {
            let found = find_parent(&h.doc).or_else(|| h.limbo.iter().find_map(|t| find_parent(t)));
            let Some((pid, i, sibs)) = found else { return -1 };
            match rel {
                0 => pid as i32,
                2 => sibs.get(i + 1).map(|&s| s as i32).unwrap_or(-1),
                _ => if i > 0 { sibs[i - 1] as i32 } else { -1 },
            }
        }
    }
}

#[unsafe(no_mangle)]
extern "C" fn rs_children(id: i32, out: *mut i32, max: i32) -> i32 {
    let Some(h) = host() else { return 0 };
    let id = if id as u32 == DOCUMENT_ID { h.doc.id } else { id as u32 };
    if id as i32 == -1 {
        return 0;
    }
    let Some(n) = h.node(id) else { return 0 };
    let k = n.children.len().min(max as usize);
    for (i, c) in n.children.iter().take(k).enumerate() {
        unsafe { *out.add(i) = c.id as i32 };
    }
    k as i32
}

#[unsafe(no_mangle)]
extern "C" fn rs_insert(parent: i32, child: i32, before: i32) {
    let Some(h) = host() else { return };
    if parent == child {
        return;
    }
    let parent = if parent as u32 == DOCUMENT_ID { h.doc.id } else { parent as u32 };
    let Some(node) = h.take(child as u32) else { return };
    let in_doc = h.in_doc(parent);
    match h.node_mut(parent) {
        Some(p) => {
            let at = if before < 0 { p.children.len() } else { p.children.iter().position(|c| c.id == before as u32).unwrap_or(p.children.len()) };
            p.children.insert(at, node);
        }
        None => h.limbo.push(node),
    }
    if in_doc {
        h.dirty = true;
    }
}

#[unsafe(no_mangle)]
extern "C" fn rs_remove(id: i32) {
    let Some(h) = host() else { return };
    if id as u32 == h.doc.id {
        return;
    }
    if let Some(n) = h.take(id as u32) {
        h.limbo.push(n);
    }
}

#[unsafe(no_mangle)]
extern "C" fn rs_special(which: i32) -> i32 {
    let Some(h) = host() else { return -1 };
    match which {
        0 => h.doc.id as i32,
        1 | 2 => {
            let tag = if which == 1 { "head" } else { "body" };
            if let Some(n) = h.doc.find(tag) {
                return n.id as i32;
            }
            // Pages without the tag still have one.
            let n = dom::elem(tag.to_string(), Default::default(), Vec::new());
            let id = n.id;
            if which == 2 {
                let kids = core::mem::take(&mut h.doc.children);
                let (head, rest): (Vec<Node>, Vec<Node>) = kids.into_iter().partition(|c| c.element().map(|e| e.tag_name == "head").unwrap_or(false));
                let mut body = n;
                body.children = rest;
                h.doc.children = head;
                h.doc.children.push(body);
            } else {
                h.doc.children.insert(0, n);
            }
            id as i32
        }
        _ => DOCUMENT_ID as i32,
    }
}

#[unsafe(no_mangle)]
extern "C" fn rs_call(op: i32, a: *const u8, alen: i64, b: *const u8, blen: i64) {
    let Some(h) = host() else { return };
    let (a, b) = unsafe { (bytes(a, alen).to_string(), bytes(b, blen).to_string()) };
    match op {
        0 => {
            if b == "error" {
                crate::kprintln!("browser: {}", a.lines().next().unwrap_or(""));
            }
            if h.console.len() > 500 {
                h.console.remove(0);
            }
            h.console.push((b, a));
        }
        1 => {
            let u = h.resolve(&a);
            h.requests.push(Request::Navigate(u));
        }
        2 => h.requests.push(Request::Alert(a)),
        3 => h.title = a,
        4 | 5 | 6 => storage_op(op, &h.url, &a, &b),
        7 => h.requests.push(Request::Scroll(a.parse::<f64>().unwrap_or(0.0) as i32)),
        8 => set_cookie(&h.url, &a),
        9 => h.requests.push(Request::Canvas(a.parse().unwrap_or(0), b)),
        10 => h.requests.push(Request::Back),
        _ => {}
    }
}

#[unsafe(no_mangle)]
extern "C" fn rs_ask(op: i32, a: *const u8, alen: i64, out: *mut u8, cap: i64) -> i64 {
    let Some(h) = host() else { return -1 };
    let a = unsafe { bytes(a, alen) };
    let s = match op {
        0 => h.url.to_string(),
        1 => match storage_get(&h.url, a) {
            Some(v) => v,
            None => return -1,
        },
        2 => storage_keys(&h.url, a),
        3 => cookie_header(&h.url),
        4 => crate::network::http::USER_AGENT.to_string(),
        5 => alloc::format!("{},{}", h.viewport.0, h.viewport.1),
        6 => h.scroll_y.to_string(),
        7 => h.resolve(a),
        8 => h.title.clone(),
        9 => {
            let (base, rel) = a.split_once('\0').unwrap_or((a, ""));
            Url::parse(base).and_then(|b| b.join(rel)).map(|u| u.to_string()).unwrap_or_default()
        }
        _ => return -1,
    };
    put(&s, out, cap)
}

#[unsafe(no_mangle)]
extern "C" fn rs_now() -> f64 {
    crate::time::uptime_us() as f64 / 1000.0
}

#[unsafe(no_mangle)]
extern "C" fn rs_box(id: i32, out: *mut i32) {
    let mut b = [0i32; 4];
    if let Some(h) = host() {
        // Union of the element's boxes.
        let mut first = true;
        for &(n, x, y, w, hh) in h.boxes.iter().filter(|bx| bx.0 == id as u32) {
            if first {
                b = [x, y, w, hh];
                first = false;
            } else {
                let (x0, y0) = (b[0].min(x), b[1].min(y));
                let (x1, y1) = ((b[0] + b[2]).max(x + w), (b[1] + b[3]).max(y + hh));
                b = [x0, y0, x1 - x0, y1 - y0];
            }
            let _ = n;
        }
    }
    unsafe { core::ptr::copy_nonoverlapping(b.as_ptr(), out, 4) };
}

#[unsafe(no_mangle)]
extern "C" fn rs_interrupt() -> i32 {
    (crate::time::uptime_ms() > DEADLINE.load(core::sync::atomic::Ordering::Relaxed)) as i32
}

// -------------------------------------------------------------------------
// Storage and cookies (per site)
// -------------------------------------------------------------------------

/// localStorage per site, persisted in /config/web/<host>.storage;
/// sessionStorage kept in memory.
static STORAGE: Spin<BTreeMap<String, BTreeMap<String, String>>> = Spin::new(BTreeMap::new());
static COOKIES: Spin<BTreeMap<String, BTreeMap<String, String>>> = Spin::new(BTreeMap::new());

fn storage_file(host: &str) -> String {
    alloc::format!("/config/web/{}.storage", host.replace(['/', '\\', ':'], "_"))
}

fn with_store<R>(url: &Url, kind: &str, f: impl FnOnce(&mut BTreeMap<String, String>) -> R) -> R {
    let key = alloc::format!("{}{}", kind, url.host);
    let mut all = STORAGE.lock();
    if !all.contains_key(&key) {
        let mut m = BTreeMap::new();
        if kind == "L"
            && let Ok(d) = crate::fs::read_file(&storage_file(&url.host))
        {
            for line in String::from_utf8_lossy(&d).lines() {
                if let Some((k, v)) = line.split_once('\t') {
                    m.insert(unescape(k), unescape(v));
                }
            }
        }
        all.insert(key.clone(), m);
    }
    f(all.get_mut(&key).unwrap())
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\n', "\\n").replace('\t', "\\t")
}

fn unescape(s: &str) -> String {
    let mut o = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('n') => o.push('\n'),
                Some('t') => o.push('\t'),
                Some(x) => o.push(x),
                None => {}
            }
        } else {
            o.push(c);
        }
    }
    o
}

fn storage_op(op: i32, url: &Url, a: &str, b: &str) {
    let (kind, key) = a.split_at(a.len().min(1));
    with_store(url, kind, |m| match op {
        4 => {
            m.insert(key.to_string(), b.to_string());
        }
        5 => {
            m.remove(key);
        }
        _ => m.clear(),
    });
    if kind == "L" {
        let text = with_store(url, "L", |m| m.iter().map(|(k, v)| alloc::format!("{}\t{}\n", escape(k), escape(v))).collect::<String>());
        let _ = crate::fs::create_dir("/config/web");
        let _ = crate::fs::write_file(&storage_file(&url.host), text.as_bytes());
    }
}

fn storage_get(url: &Url, a: &str) -> Option<String> {
    let (kind, key) = a.split_at(a.len().min(1));
    with_store(url, kind, |m| m.get(key).cloned())
}

fn storage_keys(url: &Url, kind: &str) -> String {
    with_store(url, kind, |m| m.keys().cloned().collect::<Vec<_>>().join("\n"))
}

/// Remember a cookie (`name=value; attributes`) for the page's site.
pub fn set_cookie(url: &Url, s: &str) {
    let first = s.split(';').next().unwrap_or("");
    let Some((k, v)) = first.split_once('=') else { return };
    let expired = s.to_ascii_lowercase().contains("max-age=0") || s.to_ascii_lowercase().contains("expires=thu, 01 jan 1970");
    let mut all = COOKIES.lock();
    let jar = all.entry(site(&url.host)).or_default();
    if expired {
        jar.remove(k.trim());
    } else {
        jar.insert(k.trim().to_string(), v.trim().to_string());
    }
}

/// The `Cookie:` header value for a request to this URL.
pub fn cookie_header(url: &Url) -> String {
    COOKIES.lock().get(&site(&url.host)).map(|j| j.iter().map(|(k, v)| alloc::format!("{}={}", k, v)).collect::<Vec<_>>().join("; ")).unwrap_or_default()
}

/// Cookies are shared by a site and its subdomains (www.x.com ~ x.com).
fn site(host: &str) -> String {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() > 2 { parts[parts.len() - 2..].join(".") } else { host.to_string() }
}
