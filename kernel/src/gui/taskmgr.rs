//! Task Manager: running programs with their CPU and memory use, CPU and
//! memory graphs, and "End task". Ctrl+Shift+Esc opens it.

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
const ROW_H: i32 = 26;
const HEAD_H: i32 = 150;
const LIST_TOP: i32 = HEAD_H + 30;

struct Row {
    pid: u64,
    name: String,
    threads: usize,
    cpu: f32,
    mem_kb: u64,
    kernel: bool,
}

pub struct TaskManager {
    last_update: u64,
    last_cpu: BTreeMap<u64, u64>,
    last_idle: u64,
    rows: Vec<Row>,
    cpu_hist: Vec<f32>,
    mem_hist: Vec<f32>,
    selected: Option<u64>,
    scroll: i32,
    hover_end: bool,
    size: (i32, i32),
    sort_mem: bool,
}

impl TaskManager {
    pub fn new() -> TaskManager {
        let mut t = TaskManager {
            last_update: 0,
            last_cpu: BTreeMap::new(),
            last_idle: 0,
            rows: Vec::new(),
            cpu_hist: Vec::new(),
            mem_hist: Vec::new(),
            selected: None,
            scroll: 0,
            hover_end: false,
            size: (720, 520),
            sort_mem: false,
        };
        t.refresh(1);
        t
    }

    fn refresh(&mut self, elapsed_ms: u64) {
        let cpus = crate::smp::online().max(1) as u64;
        let threads = crate::proc::sched::list();
        let mut per: BTreeMap<u64, (u64, usize)> = BTreeMap::new();
        let mut idle = 0;
        let mut kernel = (0u64, 0usize);
        for t in &threads {
            match t.pid {
                Some(pid) => {
                    let e = per.entry(pid).or_default();
                    e.0 += t.cpu_ms;
                    e.1 += 1;
                }
                None if t.name.starts_with("idle") => idle += t.cpu_ms,
                None => {
                    kernel.0 += t.cpu_ms;
                    kernel.1 += 1;
                }
            }
        }
        per.insert(0, kernel);
        let span = (elapsed_ms * cpus).max(1) as f32;
        let busy = 1.0 - (idle.saturating_sub(self.last_idle)) as f32 / span;
        self.last_idle = idle;
        let procs = crate::proc::process::list();
        let mut rows = Vec::new();
        for (&pid, &(ms, n)) in per.iter() {
            let prev = self.last_cpu.get(&pid).copied().unwrap_or(ms);
            let cpu = (ms.saturating_sub(prev)) as f32 / span * 100.0;
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
        if self.sort_mem {
            rows.sort_by(|a, b| b.mem_kb.cmp(&a.mem_kb));
        } else {
            rows.sort_by(|a, b| b.cpu.partial_cmp(&a.cpu).unwrap_or(core::cmp::Ordering::Equal).then(a.pid.cmp(&b.pid)));
        }
        self.rows = rows;
        let (free, total) = crate::mem::pmm::stats();
        push(&mut self.cpu_hist, (busy * 100.0).clamp(0.0, 100.0));
        push(&mut self.mem_hist, (total - free) as f32 / total.max(1) as f32 * 100.0);
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

    fn end_button(&self) -> Rect {
        Rect::new(self.size.0 - 120, self.size.1 - 44, 104, 32)
    }
}

fn push(h: &mut Vec<f32>, v: f32) {
    h.push(v);
    if h.len() > HISTORY {
        h.remove(0);
    }
}

fn graph(c: &mut Canvas, r: Rect, hist: &[f32], color: Color, title: &str, value: &str) {
    let f = fonts();
    c.fill_rounded_rect(r, 10, theme::PANEL_BG);
    c.draw_text(&f.bold, r.x + 14, r.y + 22, title, theme::TEXT);
    c.draw_text(&f.large, r.x + 14, r.y + 56, value, color);
    let g = Rect::new(r.x + 14, r.y + 68, r.w - 28, r.h - 80);
    c.fill_rect(g, rgb(0xff, 0xff, 0xff));
    for i in 1..4 {
        c.fill_rect(Rect::new(g.x, g.y + g.h * i / 4, g.w, 1), theme::SEPARATOR);
    }
    let n = hist.len();
    if n >= 1 {
        let step = g.w as f32 / (HISTORY - 1) as f32;
        let x0 = g.x + g.w - ((n - 1) as f32 * step) as i32;
        for (i, &v) in hist.iter().enumerate() {
            let x = x0 + (i as f32 * step) as i32;
            let hgt = ((v / 100.0) * g.h as f32) as i32;
            let w = (step as i32).max(2);
            c.fill_rect(Rect::new(x - w / 2, g.y + g.h - hgt, w, hgt), with_alpha(color & 0xff_ffff, 90));
            c.fill_rect(Rect::new(x - w / 2, g.y + g.h - hgt, w, 2.min(hgt.max(1))), color);
        }
    }
}

fn fmt_mem(kb: u64) -> String {
    if kb == 0 {
        String::from("-")
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
        (720, 520)
    }
    fn min_size(&self) -> (i32, i32) {
        (520, 360)
    }

    fn render(&mut self, c: &mut Canvas, (w, h): (i32, i32), _focused: bool) {
        self.size = (w, h);
        let f = fonts();
        c.fill_rect(Rect::new(0, 0, w, h), theme::WINDOW_BG);
        let gw = (w - 48) / 2;
        let cpu = self.cpu_hist.last().copied().unwrap_or(0.0);
        let mem = self.mem_hist.last().copied().unwrap_or(0.0);
        let (free, total) = crate::mem::pmm::stats();
        graph(c, Rect::new(16, 12, gw, HEAD_H - 20), &self.cpu_hist, rgb(0x2f, 0x7c, 0xf6), &format!("CPU ({} cores)", crate::smp::online()), &format!("{:.0}%", cpu));
        graph(
            c,
            Rect::new(32 + gw, 12, gw, HEAD_H - 20),
            &self.mem_hist,
            rgb(0x8b, 0x4d, 0xe8),
            "Memory",
            &format!("{:.1} / {:.1} GB  ({:.0}%)", (total - free) as f32 * 4.0 / 1048576.0, total as f32 * 4.0 / 1048576.0, mem),
        );
        // Column headers.
        let cols = [(16, "Name"), (w - 360, "PID"), (w - 290, "Threads"), (w - 210, "CPU"), (w - 120, "Memory")];
        c.fill_rect(Rect::new(0, HEAD_H, w, 30), theme::PANEL_BG);
        for (x, t) in cols {
            let active = (t == "CPU" && !self.sort_mem) || (t == "Memory" && self.sort_mem);
            c.draw_text(&f.bold, x, HEAD_H + 20, t, if active { theme::accent() } else { theme::TEXT_DIM });
        }
        let list_h = h - LIST_TOP - 56;
        let visible = (list_h / ROW_H).max(1);
        self.scroll = self.scroll.clamp(0, (self.rows.len() as i32 - visible).max(0));
        let old_clip = c.push_clip(Rect::new(0, LIST_TOP, w, list_h));
        for (i, r) in self.rows.iter().enumerate().skip(self.scroll as usize).take(visible as usize + 1) {
            let y = LIST_TOP + (i as i32 - self.scroll) * ROW_H;
            if Some(r.pid) == self.selected {
                c.fill_rect(Rect::new(0, y, w, ROW_H), theme::HOVER);
            }
            // CPU heat: stronger yellow for busier processes.
            let heat = (r.cpu.min(100.0) * 2.2) as u32;
            if heat > 8 {
                c.fill_rect(Rect::new(w - 220, y + 1, 90, ROW_H - 2), with_alpha(0xf5b942, heat.min(200) as u8));
            }
            let ty = y + 18;
            c.draw_text_clipped(&f.ui, 16, ty, &r.name, w - 390, if r.kernel { theme::TEXT_DIM } else { theme::TEXT });
            if !r.kernel {
                c.draw_text(&f.ui, w - 360, ty, &format!("{}", r.pid), theme::TEXT_DIM);
            }
            c.draw_text(&f.ui, w - 290, ty, &format!("{}", r.threads), theme::TEXT_DIM);
            c.draw_text(&f.ui, w - 210, ty, &format!("{:.1}%", r.cpu), theme::TEXT);
            c.draw_text(&f.ui, w - 120, ty, &fmt_mem(r.mem_kb), theme::TEXT);
            c.fill_rect(Rect::new(0, y + ROW_H - 1, w, 1), theme::SEPARATOR);
        }
        c.restore_clip(old_clip);
        widgets::draw_scrollbar(c, Rect::new(w - 8, LIST_TOP, 6, list_h), self.rows.len() as i32 * ROW_H, list_h, self.scroll * ROW_H);
        c.fill_rect(Rect::new(0, h - 56, w, 1), theme::SEPARATOR);
        let procs = self.rows.iter().filter(|r| !r.kernel).count();
        c.draw_text(&f.ui, 16, h - 22, &format!("{} programs running", procs), theme::TEXT_DIM);
        let can_end = self.selected.is_some_and(|p| p != 0);
        widgets::button(c, self.end_button(), "End task", ButtonStyle::Danger, self.hover_end && can_end, can_end);
    }

    fn event(&mut self, ev: &AppEvent, ctx: &mut Ctx) {
        match *ev {
            AppEvent::MouseDown { x, y, .. } => {
                if self.end_button().contains(x, y) {
                    self.end_selected();
                } else if (HEAD_H..HEAD_H + 30).contains(&y) {
                    if x >= self.size.0 - 220 {
                        self.sort_mem = x >= self.size.0 - 130;
                        self.last_update = 0;
                    }
                } else if y >= LIST_TOP && y < self.size.1 - 56 {
                    let i = ((y - LIST_TOP) / ROW_H + self.scroll) as usize;
                    self.selected = self.rows.get(i).map(|r| r.pid);
                }
                ctx.redraw();
            }
            AppEvent::MouseMove { x, y, .. } => {
                let h = self.end_button().contains(x, y);
                if h != self.hover_end {
                    self.hover_end = h;
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
