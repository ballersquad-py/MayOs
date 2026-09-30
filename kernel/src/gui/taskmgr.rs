//! Task Manager: a Processes page (programs with their CPU and memory use,
//! sortable, "End task") and a Performance page (CPU per core and memory
//! over the last minute). Ctrl+Shift+Esc opens it.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use gfx::icons::Icon;
use gfx::{rgb, with_alpha, Canvas, Color, Rect};

use super::app::{App, AppEvent, AppKind, Ctx};
use super::theme::{self, fonts};
use super::widgets::{self, ButtonStyle};
use crate::input::Key;

const HISTORY: usize = 60;
const ROW_H: i32 = 38;
const SIDEBAR_W: i32 = 200;
const CPU_COLOR: Color = rgb(0x2f, 0x7c, 0xf6);
const MEM_COLOR: Color = rgb(0x8b, 0x4d, 0xe8);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Processes,
    Performance,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sort {
    Name,
    Cpu,
    Memory,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    Page(Page),
    Sort(Sort),
    Row(u64),
    End,
}

struct Row {
    pid: u64,
    name: String,
    threads: usize,
    cpu: f32,
    mem_kb: u64,
    kernel: bool,
}

pub struct TaskManager {
    page: Page,
    last_update: u64,
    last_cpu: BTreeMap<u64, u64>,
    last_idle: Vec<u64>,
    rows: Vec<Row>,
    cpu_hist: Vec<f32>,
    core_hist: Vec<Vec<f32>>,
    mem_hist: Vec<f32>,
    threads_total: usize,
    selected: Option<u64>,
    scroll: i32,
    hover: Option<Hit>,
    hits: Vec<(Rect, Hit)>,
    size: (i32, i32),
    sort: Sort,
}

fn icon_for(name: &str, kernel: bool) -> Icon {
    if kernel {
        return Icon::MayOS;
    }
    let n = name.to_ascii_lowercase();
    if n.contains("firefox") {
        Icon::Firefox
    } else if n.contains("java") || n.contains("minecraft") {
        Icon::Minecraft
    } else if matches!(n.as_str(), "sh" | "bash" | "busybox" | "ash") {
        Icon::Terminal
    } else {
        Icon::Program
    }
}

impl TaskManager {
    pub fn new() -> TaskManager {
        let mut t = TaskManager {
            page: Page::Processes,
            last_update: 0,
            last_cpu: BTreeMap::new(),
            last_idle: Vec::new(),
            rows: Vec::new(),
            cpu_hist: Vec::new(),
            core_hist: Vec::new(),
            mem_hist: Vec::new(),
            threads_total: 0,
            selected: None,
            scroll: 0,
            hover: None,
            hits: Vec::new(),
            size: (860, 560),
            sort: Sort::Cpu,
        };
        t.refresh(1000);
        t
    }

    fn refresh(&mut self, elapsed_ms: u64) {
        let cpus = crate::smp::online().max(1);
        let threads = crate::proc::sched::list();
        self.threads_total = threads.len();
        let mut per: BTreeMap<u64, (u64, usize)> = BTreeMap::new();
        let mut idle = alloc::vec![0u64; cpus];
        let mut kernel = (0u64, 0usize);
        for t in &threads {
            match t.pid {
                Some(pid) => {
                    let e = per.entry(pid).or_default();
                    e.0 += t.cpu_ms;
                    e.1 += 1;
                }
                None if t.name.starts_with("idle") => {
                    let cpu: usize = t.name[4..].parse().unwrap_or(0);
                    if let Some(v) = idle.get_mut(cpu) {
                        *v += t.cpu_ms;
                    }
                }
                None => {
                    kernel.0 += t.cpu_ms;
                    kernel.1 += 1;
                }
            }
        }
        per.insert(0, kernel);
        let first = self.last_idle.len() != cpus;
        if first {
            self.last_idle = idle.clone();
        }
        // Busy share of each core since the last look.
        let span = elapsed_ms.max(1) as f32;
        self.core_hist.resize(cpus, Vec::new());
        let mut total = 0.0;
        for (i, &v) in idle.iter().enumerate() {
            let busy = (1.0 - v.saturating_sub(self.last_idle[i]) as f32 / span).clamp(0.0, 1.0) * 100.0;
            total += busy;
            push(&mut self.core_hist[i], busy);
        }
        self.last_idle = idle;
        let all_span = span * cpus as f32;
        let procs = crate::proc::process::list();
        let mut rows = Vec::new();
        for (&pid, &(ms, n)) in per.iter() {
            let prev = self.last_cpu.get(&pid).copied().unwrap_or(ms);
            let cpu = (ms.saturating_sub(prev)) as f32 / all_span * 100.0;
            let (name, mem_kb) = if pid == 0 {
                (String::from("MayOS kernel"), 0)
            } else {
                match procs.iter().find(|p| p.pid == pid) {
                    Some(p) if p.has_exited().is_none() => {
                        let name = p.name.rsplit('/').next().unwrap_or(&p.name);
                        (String::from(name), crate::mem::paging::count_user_pages(p.pml4()) * 4)
                    }
                    _ => continue,
                }
            };
            rows.push(Row { pid, name, threads: n, cpu, mem_kb, kernel: pid == 0 });
        }
        self.last_cpu = per.iter().map(|(&p, &(ms, _))| (p, ms)).collect();
        match self.sort {
            Sort::Memory => rows.sort_by(|a, b| b.mem_kb.cmp(&a.mem_kb)),
            Sort::Name => rows.sort_by_key(|r| r.name.to_ascii_lowercase()),
            Sort::Cpu => rows.sort_by(|a, b| b.cpu.partial_cmp(&a.cpu).unwrap_or(core::cmp::Ordering::Equal).then(a.pid.cmp(&b.pid))),
        }
        self.rows = rows;
        let (free, all) = crate::mem::pmm::stats();
        push(&mut self.cpu_hist, if first { 0.0 } else { total / cpus as f32 });
        push(&mut self.mem_hist, (all - free) as f32 / all.max(1) as f32 * 100.0);
    }

    fn end_selected(&mut self) {
        let Some(pid) = self.selected else { return };
        if pid == 0 {
            return;
        }
        if let Some(p) = crate::proc::process::list().into_iter().find(|p| p.pid == pid) {
            crate::proc::process::kill(&p);
        }
        self.selected = None;
        self.last_update = 0;
    }

    fn push_hit(&mut self, r: Rect, h: Hit) -> bool {
        self.hits.push((r, h));
        self.hover == Some(h)
    }

    fn card(c: &mut Canvas, r: Rect) {
        c.fill_rounded_rect(r, 12, theme::card_bg());
        c.stroke_rounded_rect(r, 12, 1, theme::border());
    }

    fn render_sidebar(&mut self, c: &mut Canvas, h: i32) {
        let f = fonts();
        c.fill_rect(Rect::new(0, 0, SIDEBAR_W, h), theme::sidebar_bg());
        c.vline(SIDEBAR_W - 1, 0, h, theme::separator());
        c.draw_text(&f.heavy, 20, 36, "Task Manager", theme::text());
        let mut y = 58;
        for (p, name, icon, color) in [
            (Page::Processes, "Processes", Icon::SymList, rgb(0x2f, 0x7c, 0xf6)),
            (Page::Performance, "Performance", Icon::SymPerf, rgb(0x2f, 0xa8, 0x5a)),
        ] {
            let r = Rect::new(10, y, SIDEBAR_W - 20, 38);
            let hov = self.push_hit(r, Hit::Page(p));
            if self.page == p {
                c.fill_rounded_rect(r, 9, with_alpha(theme::accent(), 40));
            } else if hov {
                c.fill_rounded_rect(r, 9, theme::shade(12));
            }
            c.fill_rounded_rect(Rect::new(r.x + 8, r.y + 6, 26, 26), 7, color);
            super::icons::draw(c, icon, r.x + 13, r.y + 11, 16);
            c.draw_text(if self.page == p { &f.bold } else { &f.ui }, r.x + 44, r.y + 24, name, theme::text());
            y += 42;
        }
        // Summary at the bottom.
        let cpu = self.cpu_hist.last().copied().unwrap_or(0.0);
        let mem = self.mem_hist.last().copied().unwrap_or(0.0);
        let mut by = h - 110;
        for (label, v, col) in [("CPU", cpu, CPU_COLOR), ("Memory", mem, MEM_COLOR)] {
            c.draw_text(&f.small_bold, 20, by, &label.to_ascii_uppercase(), theme::text_dim());
            let pct = format!("{:.0}%", v);
            let pw = f.small_bold.measure(&pct);
            c.draw_text(&f.small_bold, SIDEBAR_W - 20 - pw, by, &pct, theme::text());
            let bar = Rect::new(20, by + 8, SIDEBAR_W - 40, 6);
            c.fill_rounded_rect(bar, 3, theme::control_off());
            c.fill_rounded_rect(Rect::new(bar.x, bar.y, ((bar.w as f32 * v / 100.0) as i32).max(6), bar.h), 3, col);
            by += 44;
        }
    }

    fn render_processes(&mut self, c: &mut Canvas, x: i32, w: i32, h: i32) {
        let f = fonts();
        let pad = 28;
        let (x0, cw) = (x + pad, w - x - pad * 2);
        c.draw_text(&f.large, x0, 52, "Processes", theme::text());
        let can_end = self.selected.is_some_and(|p| p != 0);
        let eb = Rect::new(x0 + cw - 110, 28, 110, 34);
        let hov = self.push_hit(eb, Hit::End);
        widgets::button(c, eb, "End task", ButtonStyle::Danger, hov && can_end, can_end);
        let procs = self.rows.iter().filter(|r| !r.kernel).count();
        c.draw_text(&f.ui, x0, 76, &format!("{} programs \u{00b7} {} threads", procs, self.threads_total), theme::text_dim());

        let card = Rect::new(x0, 94, cw, h - 94 - pad);
        Self::card(c, card);
        // Columns: name | CPU | memory | PID (numbers right-aligned).
        let col_pid = card.right() - 20;
        let col_mem = col_pid - 80;
        let col_cpu = col_mem - 110;
        let head = Rect::new(card.x, card.y, card.w, 40);
        let hy = head.y + 25;
        for (label, sort, right) in [("Name", Some(Sort::Name), None), ("CPU", Some(Sort::Cpu), Some(col_cpu)), ("Memory", Some(Sort::Memory), Some(col_mem)), ("PID", None, Some(col_pid))] {
            let lw = f.small_bold.measure(&label.to_ascii_uppercase());
            let lx = right.map(|r| r - lw).unwrap_or(card.x + 20);
            let active = sort == Some(self.sort);
            if let Some(s) = sort {
                let hr = Rect::new(lx - 8, head.y + 6, lw + 16, 28);
                if self.push_hit(hr, Hit::Sort(s)) {
                    c.fill_rounded_rect(hr, 6, theme::shade(12));
                }
            }
            c.draw_text(&f.small_bold, lx, hy, &label.to_ascii_uppercase(), if active { theme::accent() } else { theme::text_dim() });
        }
        c.hline(card.x, head.bottom(), card.w, theme::separator());
        let list = Rect::new(card.x + 1, head.bottom() + 1, card.w - 2, card.bottom() - head.bottom() - 8);
        let visible = (list.h / ROW_H).max(1);
        self.scroll = self.scroll.clamp(0, (self.rows.len() as i32 - visible).max(0));
        let old = c.push_clip(list);
        let rows: Vec<(u64, String, bool, f32, u64, usize)> =
            self.rows.iter().skip(self.scroll as usize).take(visible as usize + 1).map(|r| (r.pid, r.name.clone(), r.kernel, r.cpu, r.mem_kb, r.threads)).collect();
        for (k, (pid, name, kernel, cpu, mem, threads)) in rows.into_iter().enumerate() {
            let r = Rect::new(list.x + 6, list.y + 4 + k as i32 * ROW_H, list.w - 12, ROW_H - 2);
            let hov = self.push_hit(r, Hit::Row(pid));
            if Some(pid) == self.selected {
                c.fill_rounded_rect(r, 8, with_alpha(theme::accent(), 45));
            } else if hov {
                c.fill_rounded_rect(r, 8, theme::shade(10));
            }
            // Busy processes get a warm tint behind their CPU value.
            if cpu >= 1.0 {
                let a = (30.0 + cpu.min(100.0) * 1.6) as u8;
                c.fill_rounded_rect(Rect::new(col_cpu - 70, r.y + 5, 76, r.h - 10), 6, with_alpha(0xf5a623, a));
            }
            super::icons::draw(c, icon_for(&name, kernel), r.x + 12, r.y + 7, 22);
            let base = r.y + (r.h + f.ui.ascent - f.ui.descent) / 2;
            let label = if threads > 1 { format!("{}  ({} threads)", name, threads) } else { name };
            c.draw_text_clipped(&f.ui, r.x + 44, base, &label, col_cpu - 90 - r.x - 44, if kernel { theme::text_dim() } else { theme::text() });
            let right = |c: &mut Canvas, xr: i32, t: &str, col: Color| {
                let tw = f.ui.measure(t);
                c.draw_text(&f.ui, xr - tw, base, t, col);
            };
            right(c, col_cpu, &format!("{:.1}%", cpu), theme::text());
            right(c, col_mem, &fmt_mem(mem), theme::text());
            if !kernel {
                right(c, col_pid, &format!("{}", pid), theme::text_dim());
            }
        }
        c.restore_clip(old);
        widgets::draw_scrollbar(c, Rect::new(card.right() - 10, list.y, 8, list.h), self.rows.len() as i32 * ROW_H, list.h, self.scroll * ROW_H);
    }

    fn render_performance(&mut self, c: &mut Canvas, x: i32, w: i32, h: i32) {
        let f = fonts();
        let pad = 28;
        let (x0, cw) = (x + pad, w - x - pad * 2);
        c.draw_text(&f.large, x0, 52, "Performance", theme::text());
        let cores = self.core_hist.len();
        c.draw_text(&f.ui, x0, 76, &format!("{} CPU cores \u{00b7} up {}", cores, uptime()), theme::text_dim());
        let avail = h - 94 - pad;
        let top_h = (avail * 55 / 100).max(160);
        // CPU: total graph.
        let cpu = self.cpu_hist.last().copied().unwrap_or(0.0);
        let gr = Rect::new(x0, 94, cw * 2 / 3 - 8, top_h);
        Self::card(c, gr);
        c.draw_text(&f.bold, gr.x + 18, gr.y + 28, "CPU", theme::text());
        let v = format!("{:.0}%", cpu);
        c.draw_text(&f.large, gr.right() - 18 - f.large.measure(&v), gr.y + 36, &v, CPU_COLOR);
        line_graph(c, Rect::new(gr.x + 18, gr.y + 50, gr.w - 36, gr.h - 68), &self.cpu_hist, CPU_COLOR);
        // Cores: small bars.
        let cr = Rect::new(gr.right() + 16, 94, x0 + cw - gr.right() - 16, top_h);
        Self::card(c, cr);
        c.draw_text(&f.bold, cr.x + 18, cr.y + 28, "Cores", theme::text());
        let rows = cores.max(1) as i32;
        let rh = ((cr.h - 50) / rows).clamp(14, 30);
        for (i, hist) in self.core_hist.iter().enumerate() {
            let v = hist.last().copied().unwrap_or(0.0);
            let y = cr.y + 46 + i as i32 * rh;
            if y + rh > cr.bottom() - 4 {
                break;
            }
            c.draw_text(&f.small_bold, cr.x + 18, y + rh / 2 + 4, &format!("{}", i), theme::text_dim());
            let bar = Rect::new(cr.x + 40, y + rh / 2 - 4, cr.w - 100, 8);
            c.fill_rounded_rect(bar, 4, theme::control_off());
            c.fill_rounded_rect(Rect::new(bar.x, bar.y, ((bar.w as f32 * v / 100.0) as i32).max(8), bar.h), 4, CPU_COLOR);
            let t = format!("{:.0}%", v);
            c.draw_text(&f.small_bold, cr.right() - 18 - f.small_bold.measure(&t), y + rh / 2 + 4, &t, theme::text());
        }
        // Memory.
        let (free, total) = crate::mem::pmm::stats();
        let used_gb = (total - free) as f32 * 4.0 / 1048576.0;
        let total_gb = total as f32 * 4.0 / 1048576.0;
        let mr = Rect::new(x0, gr.bottom() + 16, cw, h - pad - gr.bottom() - 16);
        Self::card(c, mr);
        c.draw_text(&f.bold, mr.x + 18, mr.y + 28, "Memory", theme::text());
        let v = format!("{:.1} of {:.1} GB", used_gb, total_gb);
        c.draw_text(&f.bold, mr.right() - 18 - f.bold.measure(&v), mr.y + 28, &v, MEM_COLOR);
        line_graph(c, Rect::new(mr.x + 18, mr.y + 42, mr.w - 36, mr.h - 58), &self.mem_hist, MEM_COLOR);
    }
}

fn uptime() -> String {
    let s = crate::time::uptime_ms() / 1000;
    if s >= 3600 {
        format!("{}h {:02}m", s / 3600, s / 60 % 60)
    } else {
        format!("{}m {:02}s", s / 60, s % 60)
    }
}

fn push(h: &mut Vec<f32>, v: f32) {
    h.push(v);
    if h.len() > HISTORY {
        h.remove(0);
    }
}

/// The last minute as a filled line, newest on the right.
fn line_graph(c: &mut Canvas, g: Rect, hist: &[f32], color: Color) {
    if g.w <= 4 || g.h <= 4 {
        return;
    }
    for i in 1..4 {
        c.hline(g.x, g.y + g.h * i / 4, g.w, theme::separator());
    }
    let n = hist.len();
    if n < 2 {
        return;
    }
    let step = g.w as f32 / (HISTORY - 1) as f32;
    let x_of = |i: usize| g.right() as f32 - (n - 1 - i) as f32 * step;
    let y_of = |v: f32| g.bottom() as f32 - v.clamp(0.0, 100.0) / 100.0 * (g.h - 2) as f32;
    let old = c.push_clip(g);
    let x_start = x_of(0) as i32;
    let mut prev: Option<i32> = None;
    for px in x_start.max(g.x)..g.right() {
        let t = (px as f32 - x_of(0)) / step;
        let i = (t as usize).min(n - 2);
        let fr = (t - i as f32).clamp(0.0, 1.0);
        let y = y_of(hist[i] * (1.0 - fr) + hist[i + 1] * fr);
        let yi = y as i32;
        c.fill_rect(Rect::new(px, yi, 1, g.bottom() - yi), with_alpha(color & 0xff_ffff, 50));
        // The line, joined to the previous column (no gaps on steep parts).
        let (lo, hi) = match prev {
            Some(p) => (p.min(yi), p.max(yi)),
            None => (yi, yi),
        };
        c.fill_rect(Rect::new(px, lo - 1, 2, hi - lo + 2), color);
        prev = Some(yi);
    }
    c.restore_clip(old);
}

fn fmt_mem(kb: u64) -> String {
    if kb == 0 {
        String::from("\u{2013}")
    } else if kb >= 1024 * 1024 {
        format!("{:.1} GB", kb as f32 / 1024.0 / 1024.0)
    } else if kb >= 1024 {
        format!("{:.1} MB", kb as f32 / 1024.0)
    } else {
        format!("{} KB", kb)
    }
}

impl App for TaskManager {
    fn title(&self) -> String {
        String::from("Task Manager")
    }
    fn icon(&self) -> Icon {
        Icon::Tasks
    }
    fn kind(&self) -> AppKind {
        AppKind::TaskManager
    }
    fn initial_size(&self) -> (i32, i32) {
        (860, 560)
    }
    fn min_size(&self) -> (i32, i32) {
        (640, 420)
    }

    fn render(&mut self, c: &mut Canvas, (w, h): (i32, i32), _focused: bool) {
        self.size = (w, h);
        self.hits.clear();
        c.fill_rect(Rect::new(SIDEBAR_W, 0, w - SIDEBAR_W, h), theme::panel_bg());
        self.render_sidebar(c, h);
        match self.page {
            Page::Processes => self.render_processes(c, SIDEBAR_W, w, h),
            Page::Performance => self.render_performance(c, SIDEBAR_W, w, h),
        }
    }

    fn event(&mut self, ev: &AppEvent, ctx: &mut Ctx) {
        let find = |s: &Self, x: i32, y: i32| s.hits.iter().rev().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h);
        match *ev {
            AppEvent::MouseDown { x, y, clicks, .. } => {
                match find(self, x, y) {
                    Some(Hit::Page(p)) => self.page = p,
                    Some(Hit::Sort(s)) => {
                        self.sort = s;
                        self.last_update = 0;
                    }
                    Some(Hit::Row(pid)) => {
                        self.selected = Some(pid);
                        let _ = clicks;
                    }
                    Some(Hit::End) => self.end_selected(),
                    None => {}
                }
                ctx.redraw();
            }
            AppEvent::MouseMove { x, y, .. } => {
                let h = find(self, x, y);
                if h != self.hover {
                    self.hover = h;
                    ctx.redraw();
                }
            }
            AppEvent::MouseLeave => {
                if self.hover.take().is_some() {
                    ctx.redraw();
                }
            }
            AppEvent::Wheel { delta, .. } => {
                self.scroll += delta;
                ctx.redraw();
            }
            AppEvent::Key(k) if k.pressed => match k.key {
                Key::Delete => {
                    self.end_selected();
                    ctx.redraw();
                }
                Key::Up | Key::Down => {
                    let i = self.rows.iter().position(|r| Some(r.pid) == self.selected);
                    let n = match (i, k.key) {
                        (None, _) => 0,
                        (Some(i), Key::Up) => i.saturating_sub(1),
                        (Some(i), _) => (i + 1).min(self.rows.len().saturating_sub(1)),
                    };
                    self.selected = self.rows.get(n).map(|r| r.pid);
                    ctx.redraw();
                }
                Key::Tab => {
                    self.page = if self.page == Page::Processes { Page::Performance } else { Page::Processes };
                    ctx.redraw();
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn tick(&mut self, ctx: &mut Ctx) {
        let now = crate::time::uptime_ms();
        let dt = now - self.last_update;
        if dt >= 1000 {
            self.refresh(if self.last_update == 0 { 1000 } else { dt });
            self.last_update = now;
            ctx.redraw();
        }
    }
}
