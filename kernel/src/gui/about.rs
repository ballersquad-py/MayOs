//! "About MayOS": system information.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use gfx::icons::Icon;
use gfx::{rgb, Canvas, Rect};

use super::app::{App, AppEvent, AppKind, Ctx};
use super::theme::{self, fonts};

pub struct About {
    last_update: u64,
}

impl About {
    pub fn new() -> About {
        About { last_update: 0 }
    }
}

fn cpu_brand() -> String {
    let mut out = Vec::new();
    for leaf in 0x8000_0002u32..=0x8000_0004 {
        let r = unsafe { core::arch::x86_64::__cpuid(leaf) };
        for v in [r.eax, r.ebx, r.ecx, r.edx] {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    let s = String::from_utf8_lossy(&out).into_owned();
    String::from(s.trim_matches(|c: char| c == '\0' || c == ' '))
}

pub fn system_info() -> Vec<(&'static str, String)> {
    let (free, total) = crate::mem::pmm::stats();
    let (heap_used, heap_total) = crate::mem::heap::stats();
    let up = crate::time::uptime_ms() / 1000;
    let mut rows = Vec::new();
    rows.push(("Version", String::from(crate::VERSION)));
    rows.push(("Processor", cpu_brand()));
    rows.push((
        "Memory",
        format!("{} MB used of {} MB", (total - free) * 4 / 1024, total * 4 / 1024),
    ));
    rows.push(("Kernel heap", format!("{} KB used of {} KB", heap_used / 1024, heap_total / 1024)));
    rows.push(("Display", crate::gui::display_description()));
    match crate::fs::stats() {
        Ok(s) => rows.push((
            "Disk",
            format!(
                "{} free of {} \u{00b7} {}",
                crate::fs::format_size(s.free_bytes()),
                crate::fs::format_size(s.total_bytes()),
                crate::fs::backend()
            ),
        )),
        Err(_) => rows.push(("Disk", String::from("no disk mounted"))),
    }
    rows.push(("Uptime", format!("{}h {:02}m {:02}s", up / 3600, (up / 60) % 60, up % 60)));
    rows.push(("Processes", format!("{}", crate::proc::process::list().len())));
    rows
}

impl App for About {
    fn title(&self) -> String {
        String::from("About MayOS")
    }
    fn icon(&self) -> Icon {
        Icon::Info
    }
    fn kind(&self) -> AppKind {
        AppKind::About
    }
    fn initial_size(&self) -> (i32, i32) {
        (460, 380)
    }
    fn resizable(&self) -> bool {
        false
    }

    fn render(&mut self, c: &mut Canvas, (w, h): (i32, i32), _focused: bool) {
        let f = fonts();
        c.fill_rect(Rect::new(0, 0, w, h), theme::WINDOW_BG);
        c.fill_gradient_v(Rect::new(0, 0, w, 110), rgb(0x2f, 0x7c, 0xf6), rgb(0x7a, 0x4d, 0xe8));
        gfx::icons::draw(c, Icon::Info, 24, 23, 64);
        c.draw_text(&f.large, 104, 58, "MayOS", rgb(255, 255, 255));
        c.draw_text(&f.ui, 106, 82, "A from-scratch operating system", rgb(235, 238, 255));
        let mut y = 140;
        for (k, v) in system_info() {
            c.draw_text(&f.bold, 24, y, k, theme::TEXT_DIM);
            c.draw_text_clipped(&f.ui, 130, y, &v, w - 150, theme::TEXT);
            y += 26;
        }
    }

    fn event(&mut self, _ev: &AppEvent, _ctx: &mut Ctx) {}

    fn tick(&mut self, ctx: &mut Ctx) {
        let now = crate::time::uptime_ms();
        if now - self.last_update >= 1000 {
            self.last_update = now;
            ctx.redraw();
        }
    }
}
