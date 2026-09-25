//! Web browser: loads pages over HTTP/HTTPS (or `file://`), styles and
//! lays them out with the `web` engine and paints the result. Pages,
//! stylesheets and images load on a background thread. No JavaScript yet.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use gfx::icons::Icon;
use gfx::{rgb, with_alpha, Canvas, Rect, Surface};
use web::css::Stylesheet;
use web::dom::Node;
use web::layout::{FontSpec, Item, Page};
use web::url::Url;

use super::app::{App, AppEvent, AppKind, Ctx};
use super::theme::{self, fonts};
use super::widgets::{self, button, ButtonStyle, TextInput};
use crate::input::Key;
use crate::network::http;
use crate::sync::Spin;

const BAR_H: i32 = 46;
const STATUS_H: i32 = 22;
const MAX_IMAGES: usize = 60;
const HOME: &str = "about:home";

const HOME_PAGE: &str = r#"<html><head><title>MayOS Browser</title></head>
<body style="background:#f4f6fb; margin:0">
<div style="max-width:640px; margin:40px auto; padding:24px 32px; background:#ffffff; border:1px solid #dde2ec">
<h1 style="color:#2f55c8">MayOS Browser</h1>
<p>Type an address or something to search for in the bar above and press Enter.</p>
<h3>Try these</h3>
<ul>
<li><a href="https://en.wikipedia.org/wiki/Special:Random">A random Wikipedia article</a></li>
<li><a href="https://news.ycombinator.com/">Hacker News</a></li>
<li><a href="https://text.npr.org/">NPR (text edition)</a></li>
<li><a href="https://lite.cnn.com/">CNN Lite</a></li>
<li><a href="https://html.duckduckgo.com/html/">DuckDuckGo search</a></li>
<li><a href="https://example.com/">example.com</a></li>
<li><a href="file:///docs/">Your documents folder</a></li>
</ul>
<p style="color:#6b7385">This browser shows HTML and CSS pages with pictures and links. It does not run JavaScript yet, so some sites will look plain or ask you to enable it.</p>
</div></body></html>"#;

/// What the loader thread hands back.
struct Loaded {
    url: Url,
    doc: Node,
    sheets: Vec<Stylesheet>,
}

struct Shared {
    cancel: bool,
    status: String,
    result: Option<Result<Loaded, String>>,
    /// Decoded images not yet collected by the window.
    images: Vec<(String, Surface)>,
}

type SharedRef = Arc<Spin<Shared>>;

struct Job {
    url: Url,
    shared: SharedRef,
}

pub struct Browser {
    address: TextInput,
    editing: bool,
    back: Vec<Url>,
    forward: Vec<Url>,
    url: Option<Url>,
    title: String,
    doc: Option<Node>,
    sheets: Vec<Stylesheet>,
    page: Option<Page>,
    laid_out_width: i32,
    images: BTreeMap<String, Surface>,
    shared: Option<SharedRef>,
    loading: bool,
    status: String,
    error: Option<String>,
    scroll: i32,
    size: (i32, i32),
    hover_link: Option<String>,
    hover_btn: Option<u8>,
    dragging_scrollbar: bool,
}

impl Browser {
    pub fn new(url: Option<&str>) -> Browser {
        let mut b = Browser {
            address: TextInput::new(""),
            editing: false,
            back: Vec::new(),
            forward: Vec::new(),
            url: None,
            title: String::from("Browser"),
            doc: None,
            sheets: Vec::new(),
            page: None,
            laid_out_width: 0,
            images: BTreeMap::new(),
            shared: None,
            loading: false,
            status: String::new(),
            error: None,
            scroll: 0,
            size: (1000, 700),
            hover_link: None,
            hover_btn: None,
            dragging_scrollbar: false,
        };
        match url {
            Some(u) => b.go(u),
            None => b.show_home(),
        }
        b
    }

    fn show_home(&mut self) {
        self.cancel();
        self.set_document(Url { scheme: "about".into(), host: String::new(), port: 0, path: "home".into() }, web::html::parse(HOME_PAGE), Vec::new());
        self.address = TextInput::new("");
        self.editing = true;
    }

    /// Navigate to what the user typed: a URL, a path or a search.
    fn go(&mut self, input: &str) {
        let t = input.trim();
        if t.is_empty() || t == HOME {
            self.show_home();
            return;
        }
        let url = if t.starts_with('/') {
            Url::parse(&format!("file://{}", t))
        } else if t.contains("://") || (t.contains('.') && !t.contains(' ')) || t.starts_with("localhost") {
            Url::parse(t)
        } else {
            None
        };
        let url = url.unwrap_or_else(|| Url::parse(&format!("https://html.duckduckgo.com/html/?q={}", url_encode(t))).unwrap());
        self.navigate(url, true);
    }

    fn navigate(&mut self, url: Url, record: bool) {
        if record && let Some(cur) = self.url.take().filter(|u| u.scheme != "about") {
            self.back.push(cur);
            self.forward.clear();
        }
        self.cancel();
        self.address = TextInput::new(&url.to_string());
        self.editing = false;
        self.url = Some(url.clone());
        self.loading = true;
        self.error = None;
        self.status = format!("Connecting to {}\u{2026}", if url.host.is_empty() { "disk" } else { &url.host });
        let shared = Arc::new(Spin::new(Shared { cancel: false, status: String::new(), result: None, images: Vec::new() }));
        self.shared = Some(shared.clone());
        let job = Box::new(Job { url, shared });
        crate::proc::sched::spawn_kernel("browser-load", load_thread, Box::into_raw(job) as usize);
    }

    fn cancel(&mut self) {
        if let Some(s) = self.shared.take() {
            s.lock().cancel = true;
        }
        self.loading = false;
    }

    fn set_document(&mut self, url: Url, doc: Node, sheets: Vec<Stylesheet>) {
        self.title = doc.find("title").map(|t| t.text_content().split_whitespace().collect::<Vec<_>>().join(" ")).filter(|t| !t.is_empty()).unwrap_or_else(|| url.to_string());
        let mut all = alloc::vec![web::css::parse(web::style::USER_AGENT_CSS)];
        all.extend(sheets);
        self.sheets = all;
        self.doc = Some(doc);
        self.url = Some(url);
        self.images.clear();
        self.page = None;
        self.scroll = 0;
    }

    fn content_rect(&self) -> Rect {
        Rect::new(0, BAR_H, self.size.0 - 12, self.size.1 - BAR_H - STATUS_H)
    }

    fn relayout(&mut self) {
        let Some(doc) = &self.doc else { return };
        let width = self.content_rect().w;
        let styled = web::style::style_tree(doc, &self.sheets);
        let base = self.url.clone().unwrap_or_else(|| Url::parse("about:blank").unwrap_or(Url { scheme: "about".into(), host: String::new(), port: 0, path: String::new() }));
        let imgs = ImageSizes { base: &base, map: &self.images };
        self.page = Some(web::layout::layout(&styled, width, &MayFonts, &imgs));
        self.laid_out_width = width;
        self.clamp_scroll();
    }

    fn max_scroll(&self) -> i32 {
        let h = self.page.as_ref().map(|p| p.height).unwrap_or(0);
        (h - self.content_rect().h).max(0)
    }

    fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.clamp(0, self.max_scroll());
    }

    fn link_at(&self, x: i32, y: i32) -> Option<String> {
        let page = self.page.as_ref()?;
        let c = self.content_rect();
        if !c.contains(x, y) {
            return None;
        }
        let (px, py) = (x, y - c.y + self.scroll);
        page.links.iter().rev().find(|l| px >= l.x && px < l.x + l.w && py >= l.y && py < l.y + l.h.max(1)).map(|l| l.href.clone())
    }

    fn resolve(&self, href: &str) -> Option<Url> {
        match &self.url {
            Some(u) if u.scheme != "about" => u.join(href),
            _ => Url::parse(href),
        }
    }

    fn btn_rect(&self, i: u8) -> Rect {
        match i {
            0 => Rect::new(8, 8, 30, 30),
            1 => Rect::new(40, 8, 30, 30),
            2 => Rect::new(72, 8, 30, 30),
            _ => Rect::new(104, 8, 30, 30),
        }
    }

    fn address_rect(&self) -> Rect {
        Rect::new(142, 8, self.size.0 - 150, 30)
    }

    fn go_back(&mut self) {
        if let Some(u) = self.back.pop() {
            if let Some(cur) = self.url.take().filter(|u| u.scheme != "about") {
                self.forward.push(cur);
            }
            self.navigate(u, false);
        }
    }

    fn go_forward(&mut self) {
        if let Some(u) = self.forward.pop() {
            if let Some(cur) = self.url.take().filter(|u| u.scheme != "about") {
                self.back.push(cur);
            }
            self.navigate(u, false);
        }
    }

    fn reload(&mut self) {
        if let Some(u) = self.url.clone().filter(|u| u.scheme != "about") {
            self.navigate(u, false);
        }
    }

    fn paint_page(&self, c: &mut Canvas) {
        let r = self.content_rect();
        let Some(page) = &self.page else { return };
        c.fill_rect(Rect::new(r.x, r.y, r.w + 12, r.h), page.background.map(|b| b | 0xff00_0000).unwrap_or(rgb(255, 255, 255)));
        let old = c.push_clip(r);
        let (top, bottom) = (self.scroll - 64, self.scroll + r.h + 64);
        let f = fonts();
        let base = self.url.clone();
        for it in &page.items {
            match it {
                Item::Rect { x, y, w, h, color } => {
                    if y + h < top || *y > bottom || color >> 24 == 0 {
                        continue;
                    }
                    c.fill_rect(Rect::new(r.x + x, r.y + y - self.scroll, *w, *h), *color);
                }
                Item::Text { x, y, base: b, text, font, color, underline, strike } => {
                    if *y + 60 < top || *y > bottom {
                        continue;
                    }
                    let fnt = pick_font(*font);
                    let fh = fnt.line_height;
                    let baseline = r.y + y - self.scroll + b - fh + fnt.ascent;
                    let col = if color >> 24 == 0 { rgb(0, 0, 0) } else { *color };
                    let end = c.draw_text(fnt, r.x + x, baseline, text, col);
                    if *underline {
                        c.fill_rect(Rect::new(r.x + x, baseline + 2, (end - (r.x + x)).max(1), 1), col);
                    }
                    if *strike {
                        c.fill_rect(Rect::new(r.x + x, baseline - fnt.ascent / 3, (end - (r.x + x)).max(1), 1), col);
                    }
                    let _ = f;
                }
                Item::Image { x, y, w, h, src } => {
                    if y + h < top || *y > bottom {
                        continue;
                    }
                    let dst = Rect::new(r.x + x, r.y + y - self.scroll, *w, *h);
                    let key = base.as_ref().and_then(|b| if b.scheme == "about" { Url::parse(src) } else { b.join(src) }).map(|u| u.to_string());
                    match key.and_then(|k| self.images.get(&k)) {
                        Some(s) => c.blit_scaled(s, dst, 255, 0),
                        None => c.fill_rect(dst, rgb(0xe8, 0xeb, 0xf0)),
                    }
                }
            }
        }
        c.restore_clip(old);
        let track = Rect::new(r.right(), r.y, 12, r.h);
        widgets::draw_scrollbar(c, track, page.height, r.h, self.scroll);
    }
}

fn url_encode(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            o.push(b as char);
        } else if b == b' ' {
            o.push('+');
        } else {
            o.push_str(&format!("%{:02X}", b));
        }
    }
    o
}

fn pick_font(f: FontSpec) -> &'static gfx::font::Font {
    let fs = fonts();
    if f.mono {
        &fs.mono
    } else if f.size >= 21 {
        &fs.large
    } else if f.bold || f.size >= 18 {
        &fs.bold
    } else if f.size <= 11 {
        &fs.small_bold
    } else {
        &fs.ui
    }
}

struct MayFonts;

impl web::layout::Fonts for MayFonts {
    fn width(&self, text: &str, font: FontSpec) -> i32 {
        pick_font(font).measure(text)
    }
    fn line_height(&self, font: FontSpec) -> i32 {
        pick_font(font).line_height
    }
}

struct ImageSizes<'a> {
    base: &'a Url,
    map: &'a BTreeMap<String, Surface>,
}

impl web::layout::Images for ImageSizes<'_> {
    fn size(&self, src: &str) -> Option<(u32, u32)> {
        let u = if self.base.scheme == "about" { Url::parse(src)? } else { self.base.join(src)? };
        self.map.get(&u.to_string()).map(|s| (s.w as u32, s.h as u32))
    }
}

fn set_status(shared: &SharedRef, s: String) {
    shared.lock().status = s;
}

fn is_html(r: &http::Response) -> bool {
    let ct = r.header("content-type").unwrap_or("").to_ascii_lowercase();
    if ct.contains("html") {
        return true;
    }
    if ct.is_empty() {
        let p = r.url.path.to_ascii_lowercase();
        return p.ends_with(".html") || p.ends_with(".htm") || p.ends_with('/') || r.body.trim_ascii_start().starts_with(b"<");
    }
    false
}

extern "C" fn load_thread(arg: usize) {
    let job = unsafe { Box::from_raw(arg as *mut Job) };
    let shared = job.shared.clone();
    let cancelled = || shared.lock().cancel;
    let result = load_page(&job.url, &shared);
    let page_ok = result.as_ref().ok().map(|l| (l.url.clone(), image_list(&l.doc)));
    if cancelled() {
        return;
    }
    shared.lock().result = Some(result);
    // Then the pictures, one by one.
    let Some((base, srcs)) = page_ok else { return };
    for (i, src) in srcs.iter().take(MAX_IMAGES).enumerate() {
        if cancelled() {
            return;
        }
        let Some(u) = base.join(src) else { continue };
        set_status(&shared, format!("Loading pictures ({}/{})\u{2026}", i + 1, srcs.len().min(MAX_IMAGES)));
        let Ok(r) = http::get(&u) else { continue };
        if r.status != 200 || r.body.len() > 8 * 1024 * 1024 {
            continue;
        }
        let Ok(mut img) = image::decode(&r.body) else { continue };
        if img.width > 1600 {
            let h = (img.height as u64 * 1600 / img.width as u64).max(1) as u32;
            img = img.resized(1600, h);
        }
        let mut s = Surface::new(img.width as i32, img.height as i32, 0);
        for (d, p) in s.data.iter_mut().zip(img.pixels.iter()) {
            *d = image::over(*p, 0xffff_ffff);
        }
        shared.lock().images.push((u.to_string(), s));
    }
    set_status(&shared, String::new());
}

fn image_list(doc: &Node) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    doc.for_each_element(&mut |e, _| {
        if e.tag_name == "img"
            && let Some(s) = e.attrs.get("src")
            && !s.starts_with("data:")
            && !v.contains(s)
        {
            v.push(s.clone());
        }
    });
    v
}

fn load_page(url: &Url, shared: &SharedRef) -> Result<Loaded, String> {
    // A folder on disk: show a listing.
    if url.scheme == "file" {
        let path = http::percent_decode(url.path.split('?').next().unwrap_or("/"));
        if crate::fs::is_dir(&path) {
            let mut html = format!("<html><head><title>{0}</title></head><body><h1>{0}</h1><ul>", escape(&path));
            if path != "/" {
                html.push_str("<li><a href=\"../\">..</a></li>");
            }
            for e in crate::fs::read_dir(&path).map_err(|e| e.to_string())? {
                let slash = if e.is_dir { "/" } else { "" };
                html.push_str(&format!("<li><a href=\"{0}{1}\">{0}{1}</a></li>", escape(&e.name), slash));
            }
            html.push_str("</ul></body></html>");
            return Ok(Loaded { url: url.clone(), doc: web::html::parse(&html), sheets: Vec::new() });
        }
    }
    let r = http::get(url)?;
    let url = r.url.clone();
    set_status(shared, format!("Reading {}\u{2026}", url.host));
    if r.status >= 400 && r.body.is_empty() {
        return Err(format!("The server answered {}.", r.status));
    }
    let ct = r.header("content-type").unwrap_or("").to_ascii_lowercase();
    if ct.starts_with("image/") || image::is_image(&r.body) {
        let html = format!("<html><head><title>{0}</title></head><body style=\"background:#222222; text-align:center\"><img src=\"{1}\"></body></html>", escape(&url.to_string()), escape(&url.path));
        return Ok(Loaded { url, doc: web::html::parse(&html), sheets: Vec::new() });
    }
    let text = String::from_utf8_lossy(&r.body).into_owned();
    if !is_html(&r) {
        let html = format!("<html><body><pre>{}</pre></body></html>", escape(&text));
        return Ok(Loaded { url, doc: web::html::parse(&html), sheets: Vec::new() });
    }
    let doc = web::html::parse(&text);
    let (mut sheets, links) = web::style::stylesheets(&doc);
    // External stylesheets go first (in document order, roughly).
    let mut external = Vec::new();
    for (i, href) in links.iter().take(10).enumerate() {
        if shared.lock().cancel {
            break;
        }
        set_status(shared, format!("Loading styles ({}/{})\u{2026}", i + 1, links.len().min(10)));
        if let Some(u) = url.join(href)
            && let Ok(cr) = http::get(&u)
            && cr.status == 200
            && cr.body.len() < 2 * 1024 * 1024
        {
            external.push(web::css::parse(&String::from_utf8_lossy(&cr.body)));
        }
    }
    external.append(&mut sheets);
    Ok(Loaded { url, doc, sheets: external })
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

impl App for Browser {
    fn title(&self) -> String {
        format!("{} \u{2014} Browser", self.title)
    }

    fn icon(&self) -> Icon {
        Icon::Browser
    }

    fn kind(&self) -> AppKind {
        AppKind::Browser
    }

    fn initial_size(&self) -> (i32, i32) {
        (1000, 680)
    }

    fn min_size(&self) -> (i32, i32) {
        (420, 300)
    }

    fn render(&mut self, c: &mut Canvas, (w, h): (i32, i32), focused: bool) {
        self.size = (w, h);
        if self.page.is_none() || self.laid_out_width != self.content_rect().w {
            self.relayout();
        }
        let f = fonts();
        c.fill_rect(Rect::new(0, 0, w, h), rgb(255, 255, 255));
        self.paint_page(c);
        if let Some(e) = &self.error {
            let r = self.content_rect();
            c.fill_rect(r, rgb(0xf6, 0xf7, 0xfa));
            c.draw_text(&f.large, r.x + 40, r.y + 80, "Can't open this page", theme::TEXT);
            c.draw_text_clipped(&f.ui, r.x + 40, r.y + 116, e, r.w - 80, theme::TEXT_DIM);
        }
        // Toolbar.
        c.fill_rect(Rect::new(0, 0, w, BAR_H), theme::PANEL_BG);
        c.hline(0, BAR_H - 1, w, theme::SEPARATOR);
        let labels = ["\u{25c0}", "\u{25b6}", "", ""];
        let enabled = [!self.back.is_empty(), !self.forward.is_empty(), true, true];
        for i in 0..4u8 {
            button(c, self.btn_rect(i), labels[i as usize], ButtonStyle::Flat, self.hover_btn == Some(i), enabled[i as usize]);
        }
        // Reload (or stop) and home, drawn as shapes.
        let ink = theme::TEXT;
        let r = self.btn_rect(2);
        let (cx, cy) = (r.x + r.w / 2, r.y + r.h / 2);
        if self.loading {
            for d in -4..=4 {
                c.fill_rect(Rect::new(cx + d, cy + d, 2, 2), ink);
                c.fill_rect(Rect::new(cx + d, cy - d, 2, 2), ink);
            }
        } else {
            c.stroke_rounded_rect(Rect::new(cx - 6, cy - 6, 13, 13), 6, 2, ink);
            c.fill_rect(Rect::new(cx + 1, cy - 8, 7, 7), theme::PANEL_BG);
            for k in 0..4 {
                c.fill_rect(Rect::new(cx + 1 + k, cy - 7 + k, 4 - k, 1), ink);
                c.fill_rect(Rect::new(cx + 4, cy - 7, 1, 4), ink);
            }
        }
        let r = self.btn_rect(3);
        let (cx, cy) = (r.x + r.w / 2, r.y + r.h / 2);
        for k in 0..6 {
            c.fill_rect(Rect::new(cx - k, cy - 6 + k, 2 * k + 1, 1), ink);
        }
        c.fill_rect(Rect::new(cx - 5, cy, 11, 7), ink);
        c.fill_rect(Rect::new(cx - 1, cy + 3, 3, 4), theme::PANEL_BG);
        let ar = self.address_rect();
        self.address.render(c, ar, focused && self.editing);
        if self.loading {
            let t = (crate::time::uptime_ms() / 8 % (ar.w as u64)) as i32;
            c.fill_rect(Rect::new(ar.x + t.min(ar.w - 60), ar.bottom() - 2, 60, 2), theme::accent());
        }
        // Status bar.
        let sr = Rect::new(0, h - STATUS_H, w, STATUS_H);
        c.fill_rect(sr, theme::PANEL_BG);
        c.hline(0, sr.y, w, theme::SEPARATOR);
        let msg = self.hover_link.clone().or_else(|| if self.status.is_empty() { None } else { Some(self.status.clone()) }).unwrap_or_default();
        c.draw_text_clipped(&f.ui, 10, sr.y + 16, &msg, w - 20, theme::TEXT_DIM);
        let _ = with_alpha;
    }

    fn event(&mut self, ev: &AppEvent, ctx: &mut Ctx) {
        match ev {
            AppEvent::MouseMove { x, y, buttons } => {
                if self.dragging_scrollbar && buttons & 1 != 0 {
                    let r = self.content_rect();
                    let total = self.page.as_ref().map(|p| p.height).unwrap_or(1).max(1);
                    self.scroll = ((y - r.y) as i64 * total as i64 / r.h.max(1) as i64) as i32 - r.h / 2;
                    self.clamp_scroll();
                    ctx.redraw();
                    return;
                }
                let link = self.link_at(*x, *y).and_then(|h| self.resolve(&h)).map(|u| u.to_string());
                let btn = (0..4u8).find(|&i| self.btn_rect(i).contains(*x, *y));
                if link != self.hover_link || btn != self.hover_btn {
                    self.hover_link = link;
                    self.hover_btn = btn;
                    ctx.redraw();
                }
            }
            AppEvent::MouseUp { .. } => self.dragging_scrollbar = false,
            AppEvent::MouseDown { x, y, button: 0, .. } => {
                let (x, y) = (*x, *y);
                ctx.redraw();
                if self.address_rect().contains(x, y) {
                    if !self.editing {
                        self.editing = true;
                        self.address.all_selected = true;
                    }
                    return;
                }
                self.editing = false;
                match (0..4u8).find(|&i| self.btn_rect(i).contains(x, y)) {
                    Some(0) => return self.go_back(),
                    Some(1) => return self.go_forward(),
                    Some(2) => {
                        if self.loading {
                            self.cancel();
                            self.status.clear();
                        } else {
                            self.reload();
                        }
                        return;
                    }
                    Some(_) => return self.show_home(),
                    None => {}
                }
                let r = self.content_rect();
                if x >= r.right() && y >= r.y && y < r.bottom() {
                    self.dragging_scrollbar = true;
                    return;
                }
                if let Some(href) = self.link_at(x, y)
                    && let Some(u) = self.resolve(&href)
                {
                    self.navigate(u, true);
                }
            }
            AppEvent::Wheel { delta, .. } => {
                self.scroll += delta * 60;
                self.clamp_scroll();
                ctx.redraw();
            }
            AppEvent::Key(k) if k.pressed => {
                if self.editing {
                    match k.key {
                        Key::Enter => {
                            let t = self.address.text.clone();
                            self.go(&t);
                        }
                        Key::Escape => {
                            self.editing = false;
                            if let Some(u) = &self.url {
                                self.address = TextInput::new(&u.to_string());
                            }
                        }
                        _ => {
                            self.address.key(k);
                        }
                    }
                    ctx.redraw();
                    return;
                }
                let view = self.content_rect().h;
                match k.key {
                    Key::Down => self.scroll += 40,
                    Key::Up => self.scroll -= 40,
                    Key::PageDown | Key::Char(' ') => self.scroll += view - 40,
                    Key::PageUp => self.scroll -= view - 40,
                    Key::Home => self.scroll = 0,
                    Key::End => self.scroll = self.max_scroll(),
                    Key::Left if k.alt => self.go_back(),
                    Key::Right if k.alt => self.go_forward(),
                    Key::Backspace => self.go_back(),
                    Key::F(5) => self.reload(),
                    Key::Char('l') if k.ctrl => {
                        self.editing = true;
                        self.address.all_selected = true;
                    }
                    _ => return,
                }
                self.clamp_scroll();
                ctx.redraw();
            }
            AppEvent::Focus(_) | AppEvent::Resized { .. } => ctx.redraw(),
            _ => {}
        }
    }

    fn tick(&mut self, ctx: &mut Ctx) {
        let Some(shared) = self.shared.clone() else { return };
        let (result, images, status) = {
            let mut s = shared.lock();
            (s.result.take(), core::mem::take(&mut s.images), s.status.clone())
        };
        if let Some(r) = result {
            self.loading = false;
            match r {
                Ok(l) => {
                    self.address = TextInput::new(&l.url.to_string());
                    self.set_document(l.url, l.doc, l.sheets);
                }
                Err(e) => self.error = Some(e),
            }
            ctx.redraw();
        }
        if !images.is_empty() {
            for (k, s) in images {
                self.images.insert(k, s);
            }
            self.relayout();
            ctx.redraw();
        }
        if status != self.status {
            self.status = status;
            ctx.redraw();
        }
        if self.loading {
            ctx.redraw_rect(self.address_rect());
        }
    }
}
