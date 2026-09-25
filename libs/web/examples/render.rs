//! Parse, style and lay out an HTML file with fake font metrics:
//! `render <file.html> [width]`; prints a summary and the first items.
use web::layout::{FontSpec, Fonts, Images};

struct F;
impl Fonts for F {
    fn width(&self, t: &str, f: FontSpec) -> i32 {
        (t.chars().count() as i32 * f.size as i32 * 55) / 100
    }
    fn line_height(&self, f: FontSpec) -> i32 {
        f.size as i32 + 4
    }
}
struct I;
impl Images for I {
    fn size(&self, _: &str) -> Option<(u32, u32)> {
        None
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let src = String::from_utf8_lossy(&std::fs::read(&a[1]).unwrap()).into_owned();
    let w: i32 = a.get(2).and_then(|w| w.parse().ok()).unwrap_or(1000);
    let t = std::time::Instant::now();
    let doc = web::html::parse(&src);
    let (mut sheets, links) = web::style::stylesheets(&doc);
    sheets.insert(0, web::css::parse(web::style::USER_AGENT_CSS));
    let styled = web::style::style_tree(&doc, &sheets);
    let page = web::layout::layout(&styled, w, &F, &I);
    eprintln!("{:?}: {} items, {} links, {} images, height {}, css links {:?}", t.elapsed(), page.items.len(), page.links.len(), page.images.len(), page.height, links.len());
    let title = doc.find("title").map(|t| t.text_content()).unwrap_or_default();
    eprintln!("title: {}", title.trim());
    for it in page.items.iter().filter(|i| matches!(i, web::layout::Item::Text { .. })).take(25) {
        if let web::layout::Item::Text { x, y, text, font, .. } = it {
            println!("{:4},{:5} {:2}{} {}", x, y, font.size, if font.bold { "b" } else { " " }, text);
        }
    }
}
