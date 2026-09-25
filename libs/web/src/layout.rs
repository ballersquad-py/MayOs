//! Layout: turns a styled tree into a display list (rectangles, text runs,
//! images) plus clickable link areas. Block boxes stack vertically with
//! margins, padding and borders; inline content is broken into lines.
//! Tables and flex rows are laid out as rows of equal-width columns
//! unless widths are given. There is no float or absolute positioning.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::dom::NodeType;
use crate::style::{Align, Display, ListStyle, Style, StyledNode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FontSpec {
    pub size: u16,
    pub bold: bool,
    pub italic: bool,
    pub mono: bool,
}

/// Text measurement, provided by whoever paints.
pub trait Fonts {
    fn width(&self, text: &str, font: FontSpec) -> i32;
    fn line_height(&self, font: FontSpec) -> i32;
}

/// Sizes of images that have been loaded (by resolved `src`).
pub trait Images {
    fn size(&self, src: &str) -> Option<(u32, u32)>;
}

#[derive(Debug, Clone)]
pub enum Item {
    Rect { x: i32, y: i32, w: i32, h: i32, color: u32 },
    /// `y` is the top of the line box, `h` its height (text sits on the
    /// shared baseline `y + base`).
    Text { x: i32, y: i32, base: i32, text: String, font: FontSpec, color: u32, underline: bool, strike: bool },
    Image { x: i32, y: i32, w: i32, h: i32, src: String },
}

#[derive(Debug, Clone)]
pub struct Link {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub href: String,
}

#[derive(Debug, Default)]
pub struct Page {
    pub items: Vec<Item>,
    pub links: Vec<Link>,
    pub height: i32,
    pub background: Option<u32>,
    /// `src`s of every image on the page (to load).
    pub images: Vec<String>,
}

pub fn layout(root: &StyledNode, width: i32, fonts: &dyn Fonts, images: &dyn Images) -> Page {
    let mut l = Layout { page: Page::default(), fonts, images };
    // Page background: body's or html's.
    l.page.background = root.style.background.or_else(|| root.children.iter().find(|c| tag(c) == Some("body")).and_then(|b| b.style.background));
    let h = l.block_children(root, 0.0, width as f32, 0.0);
    l.page.height = h as i32 + 16;
    l.page
}

fn tag<'a>(n: &'a StyledNode) -> Option<&'a str> {
    n.node.element().map(|e| e.tag_name.as_str())
}

fn font_of(s: &Style) -> FontSpec {
    FontSpec { size: s.font_size.clamp(6.0, 96.0) as u16, bold: s.bold, italic: s.italic, mono: s.mono }
}

fn is_block_level(d: Display) -> bool {
    matches!(d, Display::Block | Display::ListItem | Display::Table | Display::TableRow | Display::TableCell | Display::Flex)
}

/// A piece of inline content.
enum Tok<'a> {
    Word { text: String, space_before: bool, style: &'a Style, link: Option<&'a str> },
    Image { w: f32, h: f32, src: String, link: Option<&'a str>, space_before: bool },
    Break,
}

struct Layout<'f> {
    page: Page,
    fonts: &'f dyn Fonts,
    images: &'f dyn Images,
}

impl Layout<'_> {
    /// Lay out the children of a block container inside `[x, x + w)`
    /// starting at `y`; returns the height used.
    fn block_children(&mut self, n: &StyledNode, x: f32, w: f32, y: f32) -> f32 {
        if n.style.display == Display::TableRow || (n.style.display == Display::Flex && n.style.flex_row && n.children.iter().filter(|c| c.node.element().is_some()).count() > 1) {
            return self.row(n, x, w, y);
        }
        let mut cursor = y;
        let mut pending_margin = 0.0f32;
        let mut run: Vec<&StyledNode> = Vec::new();
        let mut item_no = 0;
        for c in &n.children {
            if is_block_level(c.style.display) {
                if !run.is_empty() {
                    cursor += pending_margin;
                    pending_margin = 0.0;
                    cursor += self.inline_run(&run, &n.style, x, w, cursor);
                    run.clear();
                }
                let gap = pending_margin.max(c.style.margin.top);
                let marker = if c.style.display == Display::ListItem {
                    item_no += 1;
                    Some(item_no)
                } else {
                    None
                };
                let h = self.block(c, x, w, cursor + gap, marker);
                cursor += gap + h;
                pending_margin = c.style.margin.bottom;
            } else {
                run.push(c);
            }
        }
        if !run.is_empty() {
            cursor += pending_margin;
            pending_margin = 0.0;
            cursor += self.inline_run(&run, &n.style, x, w, cursor);
        }
        cursor + pending_margin - y
    }

    /// A block box whose top margin edge has been placed at `top` (margins
    /// excluded); returns its border-box height.
    fn block(&mut self, n: &StyledNode, cx: f32, cw: f32, top: f32, marker: Option<usize>) -> f32 {
        let s = &n.style;
        let (m, p, b) = (s.margin, s.padding, s.border);
        let chrome = p.left + p.right + b.left + b.right;
        let mut w = match s.width {
            Some(l) => l.resolve(cw),
            None => cw - m.left - m.right - chrome,
        };
        if let Some(mx) = s.max_width {
            w = w.min(mx.resolve(cw));
        }
        w = w.min(cw - chrome).max(0.0);
        let outer = w + chrome;
        let centre = s.margin_auto_x && outer < cw;
        let bx = if centre { cx + (cw - outer) / 2.0 } else { cx + m.left };
        let content_x = bx + b.left + p.left;
        let content_y = top + b.top + p.top;
        let bg_index = self.page.items.len();
        if s.background.is_some() {
            self.page.items.push(Item::Rect { x: 0, y: 0, w: 0, h: 0, color: 0 });
        }
        if let Some(i) = marker
            && s.list_style != ListStyle::None
        {
            let text = if s.list_style == ListStyle::Decimal { alloc::format!("{}.", i) } else { "\u{2022}".to_string() };
            let font = font_of(s);
            let tw = self.fonts.width(&text, font) as f32;
            let lh = self.fonts.line_height(font);
            self.page.items.push(Item::Text {
                x: (content_x - tw - 6.0) as i32,
                y: content_y as i32,
                base: lh,
                text,
                font,
                color: s.color,
                underline: false,
                strike: false,
            });
        }
        let mut h = self.block_children(n, content_x, w, content_y);
        if let Some(fixed) = s.height {
            h = h.max(fixed);
        }
        let box_h = h + p.top + p.bottom + b.top + b.bottom;
        if let Some(c) = s.background {
            self.page.items[bg_index] = Item::Rect { x: bx as i32, y: top as i32, w: outer as i32, h: box_h as i32, color: c };
        }
        let bc = s.border_color;
        let (xi, yi, wi, hi) = (bx as i32, top as i32, outer as i32, box_h as i32);
        for (on, r) in [
            (b.top, (xi, yi, wi, b.top as i32)),
            (b.bottom, (xi, yi + hi - b.bottom as i32, wi, b.bottom as i32)),
            (b.left, (xi, yi, b.left as i32, hi)),
            (b.right, (xi + wi - b.right as i32, yi, b.right as i32, hi)),
        ] {
            if on >= 0.5 {
                self.page.items.push(Item::Rect { x: r.0, y: r.1, w: r.2.max(1), h: r.3.max(1), color: bc });
            }
        }
        box_h
    }

    /// Children side by side (table rows, flex rows).
    fn row(&mut self, n: &StyledNode, x: f32, w: f32, y: f32) -> f32 {
        let cells: Vec<&StyledNode> = n.children.iter().filter(|c| c.node.element().is_some() || !is_blank(c)).collect();
        if cells.is_empty() {
            return 0.0;
        }
        let fixed: Vec<Option<f32>> = cells.iter().map(|c| c.style.width.map(|l| l.resolve(w) + c.style.padding.left + c.style.padding.right)).collect();
        let used: f32 = fixed.iter().flatten().sum();
        let free = fixed.iter().filter(|f| f.is_none()).count();
        let share = if free > 0 { ((w - used) / free as f32).max(20.0) } else { 0.0 };
        let mut cx = x;
        let mut height: f32 = 0.0;
        for (c, f) in cells.iter().zip(fixed) {
            let cw = f.unwrap_or(share).min(x + w - cx).max(0.0);
            let h = if is_block_level(c.style.display) || c.node.element().is_some() {
                self.block_ref(c, cx, cw, y)
            } else {
                self.inline_run(&[*c], &n.style, cx, cw, y)
            };
            height = height.max(h);
            cx += cw;
        }
        height
    }

    fn block_ref(&mut self, c: &StyledNode, x: f32, w: f32, y: f32) -> f32 {
        // Cells fill their column (their own width was used for it).
        let m = c.style.margin;
        let top = y + m.top;
        let inner = w - m.left - m.right;
        let h = self.block_sized(c, x + m.left, inner.max(0.0), top);
        h + m.top + m.bottom
    }

    /// Like `block`, but the border box is exactly `w` wide.
    fn block_sized(&mut self, n: &StyledNode, bx: f32, outer: f32, top: f32) -> f32 {
        let s = &n.style;
        let (p, b) = (s.padding, s.border);
        let w = (outer - p.left - p.right - b.left - b.right).max(0.0);
        let bg_index = self.page.items.len();
        if s.background.is_some() {
            self.page.items.push(Item::Rect { x: 0, y: 0, w: 0, h: 0, color: 0 });
        }
        let h = self.block_children(n, bx + b.left + p.left, w, top + b.top + p.top);
        let box_h = h.max(s.height.unwrap_or(0.0)) + p.top + p.bottom + b.top + b.bottom;
        if let Some(c) = s.background {
            self.page.items[bg_index] = Item::Rect { x: bx as i32, y: top as i32, w: outer as i32, h: box_h as i32, color: c };
        }
        if b.bottom >= 0.5 {
            self.page.items.push(Item::Rect { x: bx as i32, y: (top + box_h - b.bottom) as i32, w: outer as i32, h: b.bottom as i32, color: s.border_color });
        }
        if b.top >= 0.5 {
            self.page.items.push(Item::Rect { x: bx as i32, y: top as i32, w: outer as i32, h: b.top as i32, color: s.border_color });
        }
        box_h
    }

    // --- inline content ------------------------------------------------

    fn inline_run(&mut self, nodes: &[&StyledNode], block: &Style, x: f32, w: f32, y: f32) -> f32 {
        let mut toks = Vec::new();
        let mut space = false;
        for n in nodes {
            self.flatten(n, None, &mut toks, &mut space);
        }
        self.lines(&toks, block, x, w, y)
    }

    fn flatten<'a>(&mut self, n: &'a StyledNode<'a>, link: Option<&'a str>, out: &mut Vec<Tok<'a>>, space: &mut bool) {
        match &n.node.node_type {
            NodeType::Text(t) => {
                let s = &n.style;
                if s.pre {
                    for (i, line) in t.split('\n').enumerate() {
                        if i > 0 {
                            out.push(Tok::Break);
                        }
                        if !line.is_empty() {
                            let text = line.replace('\t', "    ");
                            out.push(Tok::Word { text, space_before: false, style: s, link });
                        }
                    }
                    *space = false;
                    return;
                }
                let text = if s.uppercase { t.to_uppercase() } else { t.clone() };
                if text.starts_with(char::is_whitespace) {
                    *space = true;
                }
                if s.nowrap {
                    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
                    if !joined.is_empty() {
                        out.push(Tok::Word { text: joined, space_before: *space, style: s, link });
                        *space = false;
                    }
                } else {
                    for word in text.split_whitespace() {
                        out.push(Tok::Word { text: word.to_string(), space_before: *space, style: s, link });
                        *space = true;
                    }
                    *space = text.ends_with(char::is_whitespace) || (*space && text.trim().is_empty());
                    if !text.split_whitespace().any(|_| true) {
                        *space = true;
                    }
                }
            }
            NodeType::Element(e) => {
                if n.style.hidden {
                    return;
                }
                let block = is_block_level(n.style.display);
                if block {
                    out.push(Tok::Break);
                    *space = false;
                }
                // Inline list items (menus, Wikipedia's lists) are usually
                // separated by generated content we don't support: keep a gap.
                if e.tag_name == "li" && !block {
                    *space = true;
                }
                let link = if e.tag_name == "a" { e.attrs.get("href").map(|h| h.as_str()).or(link) } else { link };
                match e.tag_name.as_str() {
                    "br" => {
                        out.push(Tok::Break);
                        *space = false;
                    }
                    "img" => {
                        let src = e.attrs.get("src").cloned().unwrap_or_default();
                        let aw = n.style.width.map(|l| l.resolve(800.0));
                        let ah = n.style.height;
                        let (w, h) = match (aw, ah, self.images.size(&src)) {
                            (Some(w), Some(h), _) => (w, h),
                            (Some(w), None, Some((iw, ih))) => (w, w * ih as f32 / iw.max(1) as f32),
                            (None, Some(h), Some((iw, ih))) => (h * iw as f32 / ih.max(1) as f32, h),
                            (None, None, Some((iw, ih))) => (iw as f32, ih as f32),
                            _ => (0.0, 0.0),
                        };
                        if !src.is_empty() && !self.page.images.contains(&src) {
                            self.page.images.push(src.clone());
                        }
                        if w >= 1.0 && h >= 1.0 {
                            out.push(Tok::Image { w, h, src, link, space_before: *space });
                        } else if let Some(alt) = e.attrs.get("alt").filter(|a| !a.trim().is_empty()) {
                            out.push(Tok::Word { text: alloc::format!("[{}]", alt.trim()), space_before: *space, style: &n.style, link });
                        }
                        *space = false;
                    }
                    "input" => {
                        let kind = e.attrs.get("type").map(|t| t.to_ascii_lowercase()).unwrap_or_default();
                        let text = e.attrs.get("value").or_else(|| e.attrs.get("placeholder")).cloned().unwrap_or_default();
                        let text = match kind.as_str() {
                            "checkbox" => "\u{2610}".to_string(),
                            "radio" => "\u{25cb}".to_string(),
                            _ => alloc::format!("[ {} ]", text),
                        };
                        out.push(Tok::Word { text, space_before: *space, style: &n.style, link });
                        *space = true;
                    }
                    _ => {
                        for c in &n.children {
                            self.flatten(c, link, out, space);
                        }
                    }
                }
                if block {
                    out.push(Tok::Break);
                    *space = false;
                }
            }
        }
    }

    /// Break tokens into lines; returns the height used.
    fn lines(&mut self, toks: &[Tok], block: &Style, x: f32, w: f32, y: f32) -> f32 {
        struct Placed<'a> {
            x: f32,
            w: f32,
            h: f32,
            tok: &'a Tok<'a>,
        }
        let mut cursor_y = y;
        let mut line: Vec<Placed> = Vec::new();
        let mut lx = 0.0f32;
        let mut any = false;
        let flush = |this: &mut Self, line: &mut Vec<Placed>, cursor_y: &mut f32, lx: f32| {
            if line.is_empty() {
                return;
            }
            // Line height and baseline.
            let mut lh = 0.0f32;
            for p in line.iter() {
                let h = match p.tok {
                    Tok::Word { style, .. } => this.fonts.line_height(font_of(style)) as f32 * 1.15,
                    Tok::Image { .. } => p.h,
                    Tok::Break => 0.0,
                };
                lh = lh.max(h);
            }
            let shift = match block.align {
                Align::Left => 0.0,
                Align::Center => ((w - lx) / 2.0).max(0.0),
                Align::Right => (w - lx).max(0.0),
            };
            let top = *cursor_y;
            // Merge neighbouring words with the same look into one run.
            let mut i = 0;
            while i < line.len() {
                match line[i].tok {
                    Tok::Word { style, link, .. } => {
                        let start_x = line[i].x;
                        let mut text = String::new();
                        let mut j = i;
                        let mut end_x = start_x;
                        while j < line.len() {
                            let Tok::Word { text: t, style: s2, link: l2, space_before } = line[j].tok else { break };
                            if !core::ptr::eq(*s2, *style) || *l2 != *link {
                                break;
                            }
                            if j > i && *space_before {
                                text.push(' ');
                            }
                            text.push_str(t);
                            end_x = line[j].x + line[j].w;
                            j += 1;
                        }
                        let font = font_of(style);
                        let fh = this.fonts.line_height(font) as f32;
                        if !style.hidden {
                            this.page.items.push(Item::Text {
                                x: (x + shift + start_x) as i32,
                                y: top as i32,
                                base: ((lh + fh) / 2.0) as i32,
                                text,
                                font,
                                color: style.color,
                                underline: style.underline,
                                strike: style.strike,
                            });
                        }
                        if let Some(href) = link {
                            this.page.links.push(Link { x: (x + shift + start_x) as i32, y: top as i32, w: (end_x - start_x) as i32, h: lh as i32, href: href.to_string() });
                        }
                        i = j;
                    }
                    Tok::Image { src, link, .. } => {
                        let (iw, ih) = (&line[i].w, &line[i].h);
                        let ix = (x + shift + line[i].x) as i32;
                        let iy = (top + lh - ih) as i32;
                        this.page.items.push(Item::Image { x: ix, y: iy, w: *iw as i32, h: *ih as i32, src: src.clone() });
                        if let Some(href) = link {
                            this.page.links.push(Link { x: ix, y: iy, w: *iw as i32, h: *ih as i32, href: href.to_string() });
                        }
                        i += 1;
                    }
                    Tok::Break => i += 1,
                }
            }
            *cursor_y += lh;
            line.clear();
        };
        for t in toks {
            match t {
                Tok::Break => {
                    if !line.is_empty() {
                        flush(self, &mut line, &mut cursor_y, lx);
                    }
                    lx = 0.0;
                }
                Tok::Word { text, space_before, style, .. } => {
                    let font = font_of(style);
                    let tw = self.fonts.width(text, font) as f32;
                    let sp = if *space_before && !line.is_empty() { self.fonts.width(" ", font) as f32 } else { 0.0 };
                    if !line.is_empty() && lx + sp + tw > w {
                        flush(self, &mut line, &mut cursor_y, lx);
                        line.push(Placed { x: 0.0, w: tw, h: 0.0, tok: t });
                        lx = tw;
                    } else {
                        line.push(Placed { x: lx + sp, w: tw, h: 0.0, tok: t });
                        lx += sp + tw;
                    }
                    any = true;
                }
                Tok::Image { w: iw, h: ih, space_before, .. } => {
                    let (mut iw, mut ih) = (*iw, *ih);
                    if iw > w && w > 0.0 {
                        ih = ih * w / iw;
                        iw = w;
                    }
                    let sp = if *space_before && !line.is_empty() { 4.0 } else { 0.0 };
                    if !line.is_empty() && lx + sp + iw > w {
                        flush(self, &mut line, &mut cursor_y, lx);
                        lx = 0.0;
                    }
                    let px = if line.is_empty() { 0.0 } else { lx + sp };
                    line.push(Placed { x: px, w: iw, h: ih, tok: t });
                    lx = px + iw;
                    any = true;
                }
            }
        }
        flush(self, &mut line, &mut cursor_y, lx);
        let _ = any;
        cursor_y - y
    }
}

fn is_blank(n: &StyledNode) -> bool {
    matches!(&n.node.node_type, NodeType::Text(t) if t.trim().is_empty())
}
