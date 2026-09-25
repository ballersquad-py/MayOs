//! Applying CSS to the DOM. Selector matching follows robinson, extended
//! with combinators, the cascade (`!important`, inline `style`), a
//! built-in default stylesheet, and computed values with inheritance.

use alloc::string::String;
use alloc::vec::Vec;

use crate::css::{self, Combinator, Compound, Declaration, Selector, Specificity, Stylesheet};
use crate::dom::{ElementData, Node, NodeType};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Display {
    Block,
    Inline,
    InlineBlock,
    ListItem,
    Table,
    TableRow,
    TableCell,
    Flex,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// Lengths may be "auto" (None).
#[derive(Debug, Clone, Copy, Default)]
pub struct Edges {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

#[derive(Debug, Clone)]
pub struct Style {
    pub display: Display,
    pub color: u32,
    pub background: Option<u32>,
    pub font_size: f32,
    pub bold: bool,
    pub italic: bool,
    pub mono: bool,
    pub underline: bool,
    pub strike: bool,
    pub align: Align,
    pub pre: bool,
    pub nowrap: bool,
    pub uppercase: bool,
    pub margin: Edges,
    pub margin_auto_x: bool,
    pub padding: Edges,
    pub border: Edges,
    pub border_color: u32,
    /// Explicit width: (pixels, or percent of the container).
    pub width: Option<Length>,
    pub max_width: Option<Length>,
    pub height: Option<f32>,
    pub list_style: ListStyle,
    pub hidden: bool,
    pub flex_row: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum Length {
    Px(f32),
    Percent(f32),
}

impl Length {
    pub fn resolve(self, container: f32) -> f32 {
        match self {
            Length::Px(p) => p,
            Length::Percent(p) => container * p / 100.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListStyle {
    Disc,
    Decimal,
    None,
}

impl Style {
    pub fn root() -> Style {
        Style {
            display: Display::Block,
            color: 0xff00_0000,
            background: None,
            font_size: 16.0,
            bold: false,
            italic: false,
            mono: false,
            underline: false,
            strike: false,
            align: Align::Left,
            pre: false,
            nowrap: false,
            uppercase: false,
            margin: Edges::default(),
            margin_auto_x: false,
            padding: Edges::default(),
            border: Edges::default(),
            border_color: 0xff00_0000,
            width: None,
            max_width: None,
            height: None,
            list_style: ListStyle::Disc,
            hidden: false,
            flex_row: false,
        }
    }

    /// Start from the parent's inherited properties.
    fn inherit(parent: &Style) -> Style {
        Style {
            display: Display::Inline,
            background: None,
            margin: Edges::default(),
            margin_auto_x: false,
            padding: Edges::default(),
            border: Edges::default(),
            width: None,
            max_width: None,
            height: None,
            flex_row: false,
            border_color: parent.color,
            ..parent.clone()
        }
    }
}

/// The browser's default look (a small part of the HTML standard's).
pub const USER_AGENT_CSS: &str = r#"
html, body, div, p, h1, h2, h3, h4, h5, h6, ul, ol, dl, dt, dd, pre, blockquote, form, fieldset,
header, footer, nav, main, section, article, aside, figure, figcaption, address, hr, center, details, summary, legend, menu
  { display: block; }
head, script, style, link, meta, title, noscript, template, base, iframe, svg, canvas, video, audio, object, embed,
input[type=hidden], datalist, dialog { display: none; }
li { display: list-item; }
table { display: table; }
tr { display: table-row; }
td, th { display: table-cell; padding: 2px 4px; }
thead, tbody, tfoot, caption { display: block; }
th { font-weight: bold; }
body { margin: 8px; }
p, dl, blockquote, figure { margin: 1em 0; }
blockquote, figure { margin-left: 40px; margin-right: 40px; }
ul, ol, menu { margin: 1em 0; padding-left: 40px; }
li ul, li ol { margin: 0; }
ol { list-style-type: decimal; }
dd { margin-left: 40px; }
h1 { font-size: 2em; margin: 0.67em 0; font-weight: bold; }
h2 { font-size: 1.5em; margin: 0.83em 0; font-weight: bold; }
h3 { font-size: 1.17em; margin: 1em 0; font-weight: bold; }
h4 { margin: 1.33em 0; font-weight: bold; }
h5 { font-size: 0.83em; margin: 1.67em 0; font-weight: bold; }
h6 { font-size: 0.67em; margin: 2.33em 0; font-weight: bold; }
b, strong, dt { font-weight: bold; }
i, em, cite, var, dfn, address { font-style: italic; }
u, ins { text-decoration: underline; }
s, strike, del { text-decoration: line-through; }
small, sub, sup { font-size: 0.83em; }
big { font-size: 1.17em; }
pre, code, kbd, samp, tt { font-family: monospace; }
pre { white-space: pre; margin: 1em 0; }
a { color: #0645ad; text-decoration: underline; }
hr { border-top: 1px solid #aaaaaa; margin: 0.5em 0; }
center { text-align: center; }
button, input, select, textarea { display: inline-block; border: 1px solid #999999; padding: 1px 4px; background: #efefef; }
img { display: inline; }
mark { background: #ffff00; }
"#;

pub struct StyledNode<'a> {
    pub node: &'a Node,
    pub style: Style,
    pub children: Vec<StyledNode<'a>>,
}

/// Style a whole document with the default and author stylesheets.
pub fn style_tree<'a>(root: &'a Node, sheets: &[Stylesheet]) -> StyledNode<'a> {
    let mut ancestors: Vec<&ElementData> = Vec::new();
    styled(root, &Style::root(), sheets, &mut ancestors)
}

fn styled<'a>(node: &'a Node, parent: &Style, sheets: &[Stylesheet], ancestors: &mut Vec<&'a ElementData>) -> StyledNode<'a> {
    let style = match &node.node_type {
        NodeType::Element(e) => compute(e, parent, sheets, ancestors),
        NodeType::Text(_) => {
            let mut s = Style::inherit(parent);
            s.display = Display::Inline;
            s
        }
    };
    let mut children = Vec::new();
    if style.display != Display::None {
        if let NodeType::Element(e) = &node.node_type {
            ancestors.push(e);
        }
        for c in &node.children {
            let sc = styled(c, &style, sheets, ancestors);
            if sc.style.display != Display::None {
                children.push(sc);
            }
        }
        if node.element().is_some() {
            ancestors.pop();
        }
    }
    StyledNode { node, style, children }
}

fn compute(e: &ElementData, parent: &Style, sheets: &[Stylesheet], ancestors: &[&ElementData]) -> Style {
    let mut matched: Vec<(bool, u8, Specificity, usize, &Declaration)> = Vec::new();
    let mut order = 0;
    for (origin, sheet) in sheets.iter().enumerate() {
        for rule in &sheet.rules {
            let best = rule.selectors.iter().filter(|s| matches(e, ancestors, s)).map(|s| s.specificity()).max();
            if let Some(spec) = best {
                for d in &rule.declarations {
                    matched.push((d.important, origin as u8, spec, order, d));
                    order += 1;
                }
            }
        }
    }
    let inline: Vec<Declaration> = e.attrs.get("style").map(|s| css::parse_declarations(s)).unwrap_or_default();
    for d in &inline {
        matched.push((d.important, 255, (1000, 0, 0), order, d));
        order += 1;
    }
    matched.sort_by(|a, b| (a.0, a.1, a.2, a.3).cmp(&(b.0, b.1, b.2, b.3)));

    let mut s = Style::inherit(parent);
    // Presentational attributes.
    if let Some(c) = e.attrs.get("bgcolor").and_then(|c| css::parse_color(c)) {
        s.background = Some(c);
    }
    if let Some(c) = e.attrs.get("color").and_then(|c| css::parse_color(c)) {
        s.color = c;
    }
    if let Some(a) = e.attrs.get("align") {
        match a.to_ascii_lowercase().as_str() {
            "center" => s.align = Align::Center,
            "right" => s.align = Align::Right,
            _ => {}
        }
    }
    for attr in ["width", "height"] {
        if let Some(v) = e.attrs.get(attr) {
            let len = if let Some(p) = v.strip_suffix('%') { p.trim().parse().ok().map(Length::Percent) } else { v.trim().trim_end_matches("px").parse().ok().map(Length::Px) };
            match (attr, len) {
                ("width", Some(l)) => s.width = Some(l),
                ("height", Some(Length::Px(p))) => s.height = Some(p),
                _ => {}
            }
        }
    }
    if e.attrs.contains_key("hidden") {
        s.display = Display::None;
    }
    // Font size first: `em` lengths depend on it.
    for (.., d) in &matched {
        if d.name == "font-size" {
            s.font_size = font_size(&d.value, parent.font_size).unwrap_or(s.font_size);
        } else if d.name == "font" {
            for t in css::tokens(&d.value) {
                if let Some(px) = font_size(t.split('/').next().unwrap_or(t), parent.font_size) {
                    s.font_size = px;
                }
            }
        }
    }
    for (.., d) in &matched {
        apply(&mut s, d, parent);
    }
    s
}

fn font_size(v: &str, parent: f32) -> Option<f32> {
    Some(match v.trim() {
        "xx-small" => 9.0,
        "x-small" => 10.0,
        "small" => 13.0,
        "medium" => 16.0,
        "large" => 18.0,
        "x-large" => 24.0,
        "xx-large" => 32.0,
        "smaller" => parent * 0.83,
        "larger" => parent * 1.2,
        v => css::parse_px(v, parent, parent)?,
    })
}

fn edges(v: &str, em: f32, pct: f32) -> (Edges, bool) {
    let t = css::tokens(v);
    let px = |s: &str| css::parse_px(s, em, pct).unwrap_or(0.0);
    let auto = |s: &str| s == "auto";
    let (top, right, bottom, left) = match t.len() {
        1 => (t[0], t[0], t[0], t[0]),
        2 => (t[0], t[1], t[0], t[1]),
        3 => (t[0], t[1], t[2], t[1]),
        4 => (t[0], t[1], t[2], t[3]),
        _ => return (Edges::default(), false),
    };
    (Edges { top: px(top), right: px(right), bottom: px(bottom), left: px(left) }, auto(left) && auto(right))
}

fn border_width(v: &str, em: f32) -> Option<f32> {
    for t in css::tokens(v) {
        match t {
            "none" | "hidden" => return Some(0.0),
            "thin" => return Some(1.0),
            "medium" => return Some(3.0),
            "thick" => return Some(5.0),
            t => {
                if let Some(p) = css::parse_px(t, em, 0.0) {
                    return Some(p);
                }
            }
        }
    }
    let lower = v.to_ascii_lowercase();
    if lower.contains("solid") || lower.contains("dashed") || lower.contains("dotted") || lower.contains("double") {
        return Some(3.0);
    }
    None
}

fn length(v: &str, em: f32) -> Option<Length> {
    let v = v.trim();
    if let Some(p) = v.strip_suffix('%') {
        return p.trim().parse().ok().map(Length::Percent);
    }
    css::parse_px(v, em, 0.0).map(Length::Px)
}

fn apply(s: &mut Style, d: &Declaration, parent: &Style) {
    let v = d.value.trim();
    let lower = v.to_ascii_lowercase();
    let em = s.font_size;
    match d.name.as_str() {
        "display" => {
            s.display = match lower.as_str() {
                "none" => Display::None,
                "inline" | "contents" => Display::Inline,
                "inline-block" | "inline-flex" | "inline-grid" | "inline-table" => Display::InlineBlock,
                "list-item" => Display::ListItem,
                "table" => Display::Table,
                "table-row" => Display::TableRow,
                "table-cell" => Display::TableCell,
                "flex" | "grid" => Display::Flex,
                _ => Display::Block,
            }
        }
        "visibility" => s.hidden = lower == "hidden" || lower == "collapse",
        "color" => {
            if let Some(c) = css::parse_color(v) {
                s.color = c;
            } else if lower == "inherit" || lower == "currentcolor" {
                s.color = parent.color;
            }
        }
        "background" | "background-color" => {
            if lower == "none" || lower == "transparent" {
                s.background = None;
            } else if let Some(c) = css::find_color(v).filter(|c| c >> 24 >= 0x40) {
                s.background = Some(c);
            }
        }
        "font-weight" => s.bold = lower == "bold" || lower == "bolder" || lower.parse::<u32>().map(|w| w >= 600).unwrap_or(false),
        "font-style" => s.italic = lower == "italic" || lower == "oblique",
        "font-family" => s.mono = lower.contains("mono") || lower.contains("courier") || lower.contains("consolas"),
        "font" => {
            s.bold = lower.contains("bold");
            s.italic = lower.contains("italic");
            s.mono = lower.contains("mono") || lower.contains("courier");
        }
        "text-decoration" | "text-decoration-line" => {
            s.underline = lower.contains("underline");
            s.strike = lower.contains("line-through");
        }
        "text-align" => {
            s.align = match lower.as_str() {
                "center" | "-webkit-center" => Align::Center,
                "right" | "end" => Align::Right,
                _ => Align::Left,
            }
        }
        "text-transform" => s.uppercase = lower == "uppercase",
        "white-space" => {
            s.pre = lower.starts_with("pre") && lower != "pre-line";
            s.nowrap = lower == "nowrap" || lower == "pre";
        }
        "margin" => {
            let (e, auto) = edges(&lower, em, 0.0);
            s.margin = e;
            s.margin_auto_x = auto;
        }
        "margin-top" => s.margin.top = css::parse_px(v, em, 0.0).unwrap_or(0.0),
        "margin-bottom" => s.margin.bottom = css::parse_px(v, em, 0.0).unwrap_or(0.0),
        "margin-left" => {
            s.margin.left = css::parse_px(v, em, 0.0).unwrap_or(0.0);
            if lower == "auto" {
                s.margin_auto_x = true;
            }
        }
        "margin-right" => s.margin.right = css::parse_px(v, em, 0.0).unwrap_or(0.0),
        "padding" => s.padding = edges(&lower, em, 0.0).0,
        "padding-top" => s.padding.top = css::parse_px(v, em, 0.0).unwrap_or(0.0),
        "padding-bottom" => s.padding.bottom = css::parse_px(v, em, 0.0).unwrap_or(0.0),
        "padding-left" => s.padding.left = css::parse_px(v, em, 0.0).unwrap_or(0.0),
        "padding-right" => s.padding.right = css::parse_px(v, em, 0.0).unwrap_or(0.0),
        "border" | "border-top" | "border-bottom" | "border-left" | "border-right" => {
            let w = border_width(&lower, em).unwrap_or(0.0);
            if let Some(c) = css::find_color(v) {
                s.border_color = c;
            }
            match d.name.as_str() {
                "border" => s.border = Edges { top: w, right: w, bottom: w, left: w },
                "border-top" => s.border.top = w,
                "border-bottom" => s.border.bottom = w,
                "border-left" => s.border.left = w,
                _ => s.border.right = w,
            }
        }
        "border-width" => {
            let (e, _) = edges(&lower, em, 0.0);
            s.border = e;
        }
        "border-color" => {
            if let Some(c) = css::find_color(v) {
                s.border_color = c;
            }
        }
        "width" => s.width = length(v, em),
        "max-width" => s.max_width = length(v, em),
        "height" => s.height = css::parse_px(v, em, 0.0),
        "list-style" | "list-style-type" => {
            s.list_style = if lower.contains("none") {
                ListStyle::None
            } else if lower.contains("decimal") || lower.contains("roman") || lower.contains("alpha") {
                ListStyle::Decimal
            } else {
                ListStyle::Disc
            }
        }
        "flex-direction" => s.flex_row = !lower.starts_with("column"),
        "float" if lower == "left" || lower == "right" => {
            if s.display == Display::Inline {
                s.display = Display::InlineBlock;
            }
        }
        "position" if lower == "fixed" || lower == "absolute" => {
            // Out-of-flow overlays (cookie banners, menus) would cover
            // the page in a flow-only layout: leave them out.
            if lower == "fixed" {
                s.display = Display::None;
            }
        }
        _ => {}
    }
    if s.display == Display::Flex && !d.name.starts_with("flex") && d.name == "display" {
        s.flex_row = true;
    }
}

fn matches(e: &ElementData, ancestors: &[&ElementData], sel: &Selector) -> bool {
    let n = sel.parts.len();
    let (_, last) = &sel.parts[n - 1];
    if !matches_compound(e, last) {
        return false;
    }
    // Walk left through the combinators.
    let mut anc = ancestors.len();
    let mut k = n - 1;
    while k > 0 {
        let comb = sel.parts[k].0;
        let want = &sel.parts[k - 1].1;
        match comb {
            Combinator::Child => {
                if anc == 0 || !matches_compound(ancestors[anc - 1], want) {
                    return false;
                }
                anc -= 1;
            }
            Combinator::Descendant => {
                loop {
                    if anc == 0 {
                        return false;
                    }
                    anc -= 1;
                    if matches_compound(ancestors[anc], want) {
                        break;
                    }
                }
            }
        }
        k -= 1;
    }
    true
}

fn matches_compound(e: &ElementData, c: &Compound) -> bool {
    if c.never {
        return false;
    }
    if let Some(t) = &c.tag
        && *t != e.tag_name
    {
        return false;
    }
    if let Some(id) = &c.id
        && e.id() != Some(id)
    {
        return false;
    }
    if !c.classes.iter().all(|cl| e.has_class(cl)) {
        return false;
    }
    c.attrs.iter().all(|(k, v)| match (e.attrs.get(k), v) {
        (Some(_), None) => true,
        (Some(have), Some(want)) => have.eq_ignore_ascii_case(want),
        _ => false,
    })
}

/// Collect the author stylesheets of a document: `<style>` blocks, and
/// the `href`s of `<link rel=stylesheet>` for the caller to fetch.
pub fn stylesheets(doc: &Node) -> (Vec<Stylesheet>, Vec<String>) {
    let mut sheets = Vec::new();
    let mut links = Vec::new();
    doc.for_each_element(&mut |e, n| {
        if e.tag_name == "style" {
            sheets.push(css::parse(&n.text_content()));
        } else if e.tag_name == "link"
            && e.attrs.get("rel").map(|r| r.to_ascii_lowercase().split_whitespace().any(|r| r == "stylesheet")).unwrap_or(false)
            && !e.attrs.get("media").map(|m| m.contains("print")).unwrap_or(false)
            && let Some(h) = e.attrs.get("href")
        {
            links.push(h.clone());
        }
    });
    (sheets, links)
}
