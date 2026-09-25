//! CSS parser. Started from robinson's parser; rewritten to never fail on
//! real stylesheets: unknown syntax is skipped, `@media` blocks are
//! evaluated for a desktop-sized screen, and selectors support
//! descendant (` `) and child (`>`) combinators.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

#[derive(Debug, Clone, Default)]
pub struct Stylesheet {
    pub rules: Vec<Rule>,
    pub font_faces: Vec<FontFace>,
}

/// An `@font-face` rule.
#[derive(Debug, Clone)]
pub struct FontFace {
    pub family: String,
    pub bold: bool,
    pub italic: bool,
    /// (url, format) in order of preference.
    pub sources: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub selectors: Vec<Selector>,
    pub declarations: Vec<Declaration>,
}

/// A complex selector, rightmost compound last.
#[derive(Debug, Clone)]
pub struct Selector {
    pub parts: Vec<(Combinator, Compound)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Combinator {
    /// First compound (no combinator), or descendant.
    Descendant,
    Child,
}

#[derive(Debug, Clone, Default)]
pub struct Compound {
    pub tag: Option<String>,
    pub id: Option<String>,
    pub classes: Vec<String>,
    /// `[attr]` / `[attr=value]`
    pub attrs: Vec<(String, Option<String>)>,
    /// Contains something we can't evaluate (`:hover`, `::before`...).
    pub never: bool,
}

#[derive(Debug, Clone)]
pub struct Declaration {
    pub name: String,
    pub value: String,
    pub important: bool,
}

pub type Specificity = (usize, usize, usize);

impl Selector {
    pub fn specificity(&self) -> Specificity {
        let mut s = (0, 0, 0);
        for (_, c) in &self.parts {
            s.0 += c.id.is_some() as usize;
            s.1 += c.classes.len() + c.attrs.len();
            s.2 += c.tag.is_some() as usize;
        }
        s
    }
}

/// The width our `@media` queries are evaluated against.
pub const MEDIA_WIDTH: f32 = 1024.0;

pub fn parse(source: &str) -> Stylesheet {
    let src = strip_comments(source);
    let mut rules = Vec::new();
    let mut faces = Vec::new();
    parse_rules(&src, &mut rules, &mut faces);
    Stylesheet { rules, font_faces: faces }
}

/// Parse `name: value; ...` (a `style` attribute).
pub fn parse_declarations(s: &str) -> Vec<Declaration> {
    let mut out = Vec::new();
    for part in split_top(s, ';') {
        let Some((name, value)) = part.split_once(':') else { continue };
        let name = name.trim().to_ascii_lowercase();
        let mut value = value.trim().to_string();
        let mut important = false;
        if let Some(p) = value.to_ascii_lowercase().find("!important") {
            value.truncate(p);
            value = value.trim().to_string();
            important = true;
        }
        if !name.is_empty() && !value.is_empty() {
            out.push(Declaration { name, value, important });
        }
    }
    out
}

fn strip_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find("/*") {
        out.push_str(&rest[..p]);
        rest = match rest[p + 2..].find("*/") {
            Some(e) => &rest[p + 2 + e + 2..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

/// Split on `sep` outside parentheses and quotes.
fn split_top(s: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut quote, mut start) = (0i32, None::<char>, 0);
    for (i, c) in s.char_indices() {
        match (c, quote) {
            ('"' | '\'', None) => quote = Some(c),
            (q, Some(open)) if q == open => quote = None,
            (_, Some(_)) => {}
            ('(', None) => depth += 1,
            (')', None) => depth -= 1,
            (c, None) if c == sep && depth == 0 => {
                out.push(&s[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// Index of the `}` matching the `{` just before `from`.
fn block_end(s: &str, from: usize) -> usize {
    let mut depth = 1;
    let mut quote = None;
    for (i, c) in s[from..].char_indices() {
        match (c, quote) {
            ('"' | '\'', None) => quote = Some(c),
            (q, Some(open)) if q == open => quote = None,
            (_, Some(_)) => {}
            ('{', None) => depth += 1,
            ('}', None) => {
                depth -= 1;
                if depth == 0 {
                    return from + i;
                }
            }
            _ => {}
        }
    }
    s.len()
}

fn parse_rules(s: &str, out: &mut Vec<Rule>, faces: &mut Vec<FontFace>) {
    let mut i = 0;
    while i < s.len() {
        let rest = &s[i..];
        let Some(open) = rest.find(['{', ';']) else { break };
        let prelude = rest[..open].trim();
        if rest.as_bytes()[open] == b';' {
            // `@import ...;` or junk.
            i += open + 1;
            continue;
        }
        let body_start = i + open + 1;
        let end = block_end(s, body_start);
        let body = &s[body_start..end.min(s.len())];
        i = end + 1;
        if let Some(at) = prelude.strip_prefix('@') {
            let lower = at.to_ascii_lowercase();
            if lower.starts_with("media") {
                if media_matches(&lower[5..]) {
                    parse_rules(body, out, faces);
                }
            } else if lower.starts_with("supports") || lower.starts_with("layer") || lower.starts_with("document") {
                parse_rules(body, out, faces);
            } else if lower.starts_with("font-face") {
                let d = parse_declarations(body);
                let get = |n: &str| d.iter().rev().find(|x| x.name == n).map(|x| x.value.clone()).unwrap_or_default();
                let family = get("font-family").trim().trim_matches(['"', '\'']).to_ascii_lowercase();
                let weight = get("font-weight").to_ascii_lowercase();
                let bold = weight.contains("bold") || weight.split_whitespace().next().and_then(|w| w.parse::<u32>().ok()).map(|w| w >= 600).unwrap_or(false);
                let italic = get("font-style").to_ascii_lowercase().contains("italic");
                let mut sources = Vec::new();
                for part in split_top(&get("src"), ',') {
                    let part = part.trim();
                    let Some(start) = part.find("url(") else { continue };
                    let rest = &part[start + 4..];
                    let Some(end) = rest.find(')') else { continue };
                    let url = rest[..end].trim().trim_matches(['"', '\'']).to_string();
                    let format = part.find("format(").map(|f| part[f + 7..].split(')').next().unwrap_or("").trim_matches(['"', '\'', ' ']).to_ascii_lowercase()).unwrap_or_default();
                    sources.push((url, format));
                }
                if !family.is_empty() && !sources.is_empty() {
                    faces.push(FontFace { family, bold, italic, sources });
                }
            }
            // @font-face, @keyframes, @page ...: skipped.
            continue;
        }
        let selectors: Vec<Selector> = split_top(prelude, ',').iter().filter_map(|s| parse_selector(s)).collect();
        if selectors.is_empty() {
            continue;
        }
        out.push(Rule { selectors, declarations: parse_declarations(body) });
    }
}

/// Evaluate a media query list for a desktop screen `MEDIA_WIDTH` wide.
fn media_matches(q: &str) -> bool {
    split_top(q, ',').iter().any(|q| {
        let q = q.trim();
        if q.starts_with("print") || q.starts_with("speech") || q.starts_with("not screen") {
            return false;
        }
        let mut ok = true;
        for cond in q.split('(').skip(1) {
            let cond = cond.split(')').next().unwrap_or("");
            let Some((feat, val)) = cond.split_once(':') else { continue };
            let v = parse_px(val.trim(), 16.0, MEDIA_WIDTH).unwrap_or(0.0);
            match feat.trim() {
                "max-width" => ok &= MEDIA_WIDTH <= v,
                "min-width" => ok &= MEDIA_WIDTH >= v,
                "prefers-color-scheme" => ok &= val.trim() == "light",
                "orientation" => ok &= val.trim() == "landscape",
                _ => {}
            }
        }
        ok
    })
}

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_' || !c.is_ascii()
}

/// Parse a comma-separated selector list (for `querySelector`).
pub fn parse_selector_list(s: &str) -> Vec<Selector> {
    split_top(s, ',').iter().filter_map(|s| parse_selector(s)).map(|mut s| {
        // Pseudo-classes that only matter for styling don't stop a match.
        for (_, c) in s.parts.iter_mut() {
            c.never = false;
        }
        s
    }).collect()
}

fn parse_selector(s: &str) -> Option<Selector> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    let mut comb = Combinator::Descendant;
    let mut cur = Compound::default();
    let mut have = false;
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let ident = |i: &mut usize| -> String {
        let st = *i;
        while *i < chars.len() && (is_ident(chars[*i]) || chars[*i] == '\\') {
            *i += 1;
        }
        chars[st..*i].iter().collect::<String>()
    };
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\n' | '\r' | '>' | '+' | '~' => {
                let mut sep = Combinator::Descendant;
                let mut sibling = false;
                while i < chars.len() && matches!(chars[i], ' ' | '\t' | '\n' | '\r' | '>' | '+' | '~') {
                    match chars[i] {
                        '>' => sep = Combinator::Child,
                        '+' | '~' => sibling = true,
                        _ => {}
                    }
                    i += 1;
                }
                if have {
                    if sibling {
                        cur.never = true;
                    }
                    parts.push((comb, core::mem::take(&mut cur)));
                    have = false;
                }
                comb = sep;
            }
            '#' => {
                i += 1;
                cur.id = Some(ident(&mut i));
                have = true;
            }
            '.' => {
                i += 1;
                cur.classes.push(ident(&mut i));
                have = true;
            }
            '*' => {
                i += 1;
                have = true;
            }
            '[' => {
                let st = i + 1;
                while i < chars.len() && chars[i] != ']' {
                    i += 1;
                }
                let inner: String = chars[st..i.min(chars.len())].iter().collect();
                i += 1;
                match inner.split_once('=') {
                    Some((k, v)) if !k.ends_with(['~', '|', '^', '$', '*']) => {
                        cur.attrs.push((k.trim().to_ascii_lowercase(), Some(v.trim().trim_matches(['"', '\'']).to_string())))
                    }
                    Some(_) => cur.never = true,
                    None => cur.attrs.push((inner.trim().to_ascii_lowercase(), None)),
                }
                have = true;
            }
            ':' => {
                i += 1;
                let double = i < chars.len() && chars[i] == ':';
                if double {
                    i += 1;
                }
                let name = ident(&mut i).to_ascii_lowercase();
                // Skip an argument list.
                if i < chars.len() && chars[i] == '(' {
                    let mut depth = 0;
                    while i < chars.len() {
                        if chars[i] == '(' {
                            depth += 1;
                        } else if chars[i] == ')' {
                            depth -= 1;
                            if depth == 0 {
                                i += 1;
                                break;
                            }
                        }
                        i += 1;
                    }
                }
                if double || !matches!(name.as_str(), "link" | "visited" | "root" | "first-child" | "is" | "where") {
                    cur.never = true;
                }
                have = true;
            }
            c if is_ident(c) => {
                cur.tag = Some(ident(&mut i).to_ascii_lowercase());
                have = true;
            }
            _ => return None,
        }
    }
    if have {
        parts.push((comb, cur));
    }
    if parts.is_empty() { None } else { Some(Selector { parts }) }
}

// ---------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------

/// Resolve a length to pixels. `em` is the font size, `pct` what 100% is.
pub fn parse_px(v: &str, em: f32, pct: f32) -> Option<f32> {
    let v = v.trim();
    if v == "0" {
        return Some(0.0);
    }
    let num_end = v.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+')).unwrap_or(v.len());
    let n: f32 = v[..num_end].parse().ok()?;
    Some(match v[num_end..].trim().to_ascii_lowercase().as_str() {
        "px" | "" => n,
        "em" => n * em,
        "rem" => n * 16.0,
        "%" => n * pct / 100.0,
        "pt" => n * 4.0 / 3.0,
        "vw" => n * MEDIA_WIDTH / 100.0,
        "vh" => n * 7.68,
        "ch" | "ex" => n * em / 2.0,
        _ => return None,
    })
}

/// A colour as 0xAARRGGBB.
pub fn parse_color(v: &str) -> Option<u32> {
    let v = v.trim().to_ascii_lowercase();
    if let Some(h) = v.strip_prefix('#') {
        let d = |s: &str| u32::from_str_radix(s, 16).ok();
        return match h.len() {
            3 | 4 => {
                let c: Vec<u32> = h.chars().map(|c| c.to_digit(16).unwrap_or(0) * 17).collect();
                let a = if h.len() == 4 { c[3] } else { 255 };
                Some(a << 24 | c[0] << 16 | c[1] << 8 | c[2])
            }
            6 => Some(0xff00_0000 | d(h)?),
            8 => {
                let v = d(h)?;
                Some((v & 0xff) << 24 | v >> 8)
            }
            _ => None,
        };
    }
    if let Some(args) = v.strip_prefix("rgb(").or_else(|| v.strip_prefix("rgba(")) {
        let args = args.trim_end_matches(')');
        let parts: Vec<&str> = args.split([',', ' ', '/']).filter(|s| !s.is_empty()).collect();
        if parts.len() < 3 {
            return None;
        }
        let ch = |s: &str| -> u32 {
            if let Some(p) = s.strip_suffix('%') { (p.parse::<f32>().unwrap_or(0.0) * 2.55) as u32 } else { s.parse::<f32>().unwrap_or(0.0) as u32 }.min(255)
        };
        let a = parts.get(3).map(|s| {
            if let Some(p) = s.strip_suffix('%') { (p.parse::<f32>().unwrap_or(100.0) * 2.55) as u32 } else { (s.parse::<f32>().unwrap_or(1.0) * 255.0) as u32 }
        });
        return Some(a.unwrap_or(255).min(255) << 24 | ch(parts[0]) << 16 | ch(parts[1]) << 8 | ch(parts[2]));
    }
    let rgb = match v.as_str() {
        "black" => 0x000000,
        "white" => 0xffffff,
        "red" => 0xff0000,
        "green" => 0x008000,
        "blue" => 0x0000ff,
        "yellow" => 0xffff00,
        "orange" => 0xffa500,
        "purple" => 0x800080,
        "gray" | "grey" => 0x808080,
        "silver" => 0xc0c0c0,
        "maroon" => 0x800000,
        "navy" => 0x000080,
        "teal" => 0x008080,
        "olive" => 0x808000,
        "lime" => 0x00ff00,
        "aqua" | "cyan" => 0x00ffff,
        "fuchsia" | "magenta" => 0xff00ff,
        "darkgray" | "darkgrey" => 0xa9a9a9,
        "lightgray" | "lightgrey" => 0xd3d3d3,
        "whitesmoke" => 0xf5f5f5,
        "darkblue" => 0x00008b,
        "darkred" => 0x8b0000,
        "darkgreen" => 0x006400,
        "brown" => 0xa52a2a,
        "pink" => 0xffc0cb,
        "gold" => 0xffd700,
        "beige" => 0xf5f5dc,
        "ivory" => 0xfffff0,
        "linen" => 0xfaf0e6,
        "lightblue" => 0xadd8e6,
        "skyblue" => 0x87ceeb,
        "steelblue" => 0x4682b4,
        "royalblue" => 0x4169e1,
        "dodgerblue" => 0x1e90ff,
        "crimson" => 0xdc143c,
        "tomato" => 0xff6347,
        "coral" => 0xff7f50,
        "salmon" => 0xfa8072,
        "indigo" => 0x4b0082,
        "violet" => 0xee82ee,
        "tan" => 0xd2b48c,
        "khaki" => 0xf0e68c,
        "slategray" | "slategrey" => 0x708090,
        "dimgray" | "dimgrey" => 0x696969,
        "gainsboro" => 0xdcdcdc,
        "aliceblue" => 0xf0f8ff,
        "ghostwhite" => 0xf8f8ff,
        "honeydew" => 0xf0fff0,
        "mintcream" => 0xf5fffa,
        "lavender" => 0xe6e6fa,
        "transparent" => return Some(0),
        _ => return None,
    };
    Some(0xff00_0000 | rgb)
}

/// First colour found in a shorthand like `background: #fff url(..)` or
/// `border: 1px solid red`.
pub fn find_color(v: &str) -> Option<u32> {
    for tok in split_top(v, ' ') {
        if let Some(c) = parse_color(tok) {
            return Some(c);
        }
    }
    None
}

pub fn tokens(v: &str) -> Vec<&str> {
    split_top(v.trim(), ' ').into_iter().filter(|s| !s.is_empty()).collect()
}

impl core::fmt::Display for Declaration {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: {}", self.name, self.value)
    }
}
