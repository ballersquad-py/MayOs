//! A forgiving HTML parser: real pages are rarely well-formed, so this
//! never fails. It handles void elements, implied end tags (`<p>`, `<li>`,
//! table cells...), raw-text elements (`<script>`, `<style>`), comments,
//! doctype and character references, and always returns an `<html>` root.

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::dom::{self, AttrMap, Node};

const VOID: &[&str] = &["area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"];
const RAW: &[&str] = &["script", "style", "textarea", "title", "noscript"];
/// Opening one of these closes an open `<p>`.
const CLOSES_P: &[&str] = &[
    "address", "article", "aside", "blockquote", "div", "dl", "fieldset", "footer", "form", "h1", "h2", "h3", "h4", "h5", "h6",
    "header", "hr", "main", "nav", "ol", "p", "pre", "section", "table", "ul", "figure",
];

pub fn is_void(tag: &str) -> bool {
    VOID.contains(&tag)
}

/// Parse an HTML fragment (for `innerHTML`): returns the top-level nodes.
pub fn parse_fragment(source: &str) -> Vec<Node> {
    parse(source).children
}

pub fn parse(source: &str) -> Node {
    let mut b = Builder { stack: vec![dom::elem("html".into(), AttrMap::new(), Vec::new())] };
    let s = source.as_bytes();
    let mut i = 0;
    let mut text_start = 0;
    while i < s.len() {
        if s[i] != b'<' {
            i += 1;
            continue;
        }
        // Flush text before the tag.
        if text_start < i {
            b.text(&decode_entities(&source[text_start..i]));
        }
        if source[i..].starts_with("<!--") {
            i = source[i + 4..].find("-->").map(|p| i + 4 + p + 3).unwrap_or(s.len());
            text_start = i;
            continue;
        }
        if s.get(i + 1) == Some(&b'!') || s.get(i + 1) == Some(&b'?') {
            i = source[i..].find('>').map(|p| i + p + 1).unwrap_or(s.len());
            text_start = i;
            continue;
        }
        let closing = s.get(i + 1) == Some(&b'/');
        let name_start = i + if closing { 2 } else { 1 };
        let mut j = name_start;
        while j < s.len() && (s[j].is_ascii_alphanumeric() || s[j] == b'-' || s[j] == b':') {
            j += 1;
        }
        if j == name_start {
            // Not a tag ("a < b"): keep as text.
            i += 1;
            continue;
        }
        let name = source[name_start..j].to_ascii_lowercase();
        let (attrs, end, self_closing) = parse_attrs(source, j);
        i = end;
        text_start = i;
        if closing {
            b.close(&name);
            continue;
        }
        b.open(name.clone(), attrs);
        if VOID.contains(&name.as_str()) || self_closing {
            b.close(&name);
        } else if RAW.contains(&name.as_str()) {
            // Everything up to the matching end tag is text.
            let lower = source[i..].to_ascii_lowercase();
            let close = alloc::format!("</{}", name);
            let stop = lower.find(&close).map(|p| i + p).unwrap_or(s.len());
            let raw = &source[i..stop];
            let t = if name == "title" || name == "textarea" { decode_entities(raw) } else { raw.to_string() };
            b.text(&t);
            b.close(&name);
            i = source[stop..].find('>').map(|p| stop + p + 1).unwrap_or(s.len());
            text_start = i;
        }
    }
    if text_start < s.len() {
        b.text(&decode_entities(&source[text_start..]));
    }
    b.finish()
}

/// Parse attributes starting at `i`; returns (attrs, index after '>', self-closing).
fn parse_attrs(src: &str, mut i: usize) -> (AttrMap, usize, bool) {
    let s = src.as_bytes();
    let mut attrs = AttrMap::new();
    let mut self_closing = false;
    loop {
        while i < s.len() && s[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= s.len() {
            return (attrs, i, self_closing);
        }
        match s[i] {
            b'>' => return (attrs, i + 1, self_closing),
            b'/' => {
                self_closing = true;
                i += 1;
                continue;
            }
            _ => {}
        }
        let start = i;
        while i < s.len() && !s[i].is_ascii_whitespace() && !b"=>/".contains(&s[i]) {
            i += 1;
        }
        if start == i {
            i += 1;
            continue;
        }
        let name = src[start..i].to_ascii_lowercase();
        self_closing = false;
        while i < s.len() && s[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < s.len() && s[i] == b'=' {
            i += 1;
            while i < s.len() && s[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < s.len() && (s[i] == b'"' || s[i] == b'\'') {
                let q = s[i];
                let vs = i + 1;
                i = vs;
                while i < s.len() && s[i] != q {
                    i += 1;
                }
                value = decode_entities(&src[vs..i]);
                i += 1;
            } else {
                let vs = i;
                while i < s.len() && !s[i].is_ascii_whitespace() && s[i] != b'>' {
                    i += 1;
                }
                value = decode_entities(&src[vs..i]);
            }
        }
        attrs.entry(name).or_insert(value);
    }
}

struct Builder {
    stack: Vec<Node>,
}

impl Builder {
    fn top_tag(&self) -> &str {
        self.stack.last().and_then(|n| n.element()).map(|e| e.tag_name.as_str()).unwrap_or("")
    }

    fn open(&mut self, name: String, attrs: AttrMap) {
        match name.as_str() {
            "html" => {
                // Merge attributes into the root instead of nesting.
                if let Some(crate::dom::NodeType::Element(e)) = self.stack.first_mut().map(|n| &mut n.node_type) {
                    for (k, v) in attrs {
                        e.attrs.entry(k).or_insert(v);
                    }
                }
                return;
            }
            "li" => self.close_implied(&["li"], &["ul", "ol"]),
            "dt" | "dd" => self.close_implied(&["dt", "dd"], &["dl"]),
            "tr" => self.close_implied(&["tr", "td", "th"], &["table", "tbody", "thead", "tfoot"]),
            "td" | "th" => self.close_implied(&["td", "th"], &["tr", "table"]),
            "option" => self.close_implied(&["option"], &["select"]),
            _ => {}
        }
        if CLOSES_P.contains(&name.as_str()) && self.stack.iter().any(|n| n.element().map(|e| e.tag_name == "p").unwrap_or(false)) {
            self.close("p");
        }
        self.stack.push(dom::elem(name, attrs, Vec::new()));
    }

    /// Close an open element named in `names`, without crossing `scope`.
    fn close_implied(&mut self, names: &[&str], scope: &[&str]) {
        for k in (1..self.stack.len()).rev() {
            let tag = self.stack[k].element().map(|e| e.tag_name.clone()).unwrap_or_default();
            if scope.contains(&tag.as_str()) {
                return;
            }
            if names.contains(&tag.as_str()) {
                while self.stack.len() > k {
                    self.pop();
                }
                return;
            }
        }
    }

    fn close(&mut self, name: &str) {
        if name == "html" || name == "body" {
            return;
        }
        // Only close if it is open; otherwise ignore the stray end tag.
        if let Some(k) = (1..self.stack.len()).rev().find(|&k| self.stack[k].element().map(|e| e.tag_name == name).unwrap_or(false)) {
            while self.stack.len() > k {
                self.pop();
            }
        }
    }

    fn pop(&mut self) {
        let n = self.stack.pop().unwrap();
        self.stack.last_mut().unwrap().children.push(n);
    }

    fn text(&mut self, t: &str) {
        if t.is_empty() {
            return;
        }
        // Whitespace between table parts means nothing.
        if t.trim().is_empty() && matches!(self.top_tag(), "table" | "tbody" | "thead" | "tr" | "ul" | "ol" | "html" | "head") {
            return;
        }
        let top = self.stack.last_mut().unwrap();
        if let Some(Node { node_type: crate::dom::NodeType::Text(prev), .. }) = top.children.last_mut() {
            prev.push_str(t);
        } else {
            top.children.push(dom::text(t.to_string()));
        }
    }

    fn finish(mut self) -> Node {
        while self.stack.len() > 1 {
            self.pop();
        }
        self.stack.pop().unwrap()
    }
}

/// Replace `&amp;`, `&#39;`, `&#x27;` and the common named references.
pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        rest = &rest[p..];
        let end = rest[1..].find(|c: char| c == ';' || c.is_whitespace() || c == '&' || c == '<').map(|e| e + 1);
        let Some(end) = end.filter(|&e| e <= 32 && rest.as_bytes().get(e) == Some(&b';')) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let name = &rest[1..end];
        let ch = if let Some(num) = name.strip_prefix('#') {
            let v = if let Some(h) = num.strip_prefix('x').or_else(|| num.strip_prefix('X')) { u32::from_str_radix(h, 16).ok() } else { num.parse().ok() };
            v.and_then(char::from_u32)
        } else {
            named(name)
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn named(n: &str) -> Option<char> {
    Some(match n {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "mdash" => '—',
        "ndash" => '–',
        "hellip" => '…',
        "laquo" => '«',
        "raquo" => '»',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "bull" => '•',
        "middot" => '·',
        "times" => '×',
        "euro" => '€',
        "pound" => '£',
        "deg" => '°',
        "rarr" => '→',
        "larr" => '←',
        "uarr" => '↑',
        "darr" => '↓',
        "auml" => 'ä',
        "ouml" => 'ö',
        "uuml" => 'ü',
        "Auml" => 'Ä',
        "Ouml" => 'Ö',
        "Uuml" => 'Ü',
        "szlig" => 'ß',
        "eacute" => 'é',
        "egrave" => 'è',
        "aacute" => 'á',
        "aring" => 'å',
        "Aring" => 'Å',
        "oslash" => 'ø',
        "aelig" => 'æ',
        "shy" => '\u{ad}',
        "zwj" | "zwnj" => '\u{200b}',
        _ => return None,
    })
}
