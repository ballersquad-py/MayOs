//! The desktop: window manager, compositor and built-in applications.
//!
//! For now the desktop runs as a kernel thread. It moves to a user-space
//! compositor once IPC and shared memory exist (Phase 2).

pub mod about;
pub mod app;
pub mod cursor;
pub mod dialog;
pub mod display;
pub mod editor;
pub mod explorer;
pub mod imageview;
pub mod keep_resolution;
pub mod settings_app;
pub mod shell;
pub mod terminal;
pub mod theme;
pub mod thumbs;
pub mod widgets;
pub mod player;
pub mod wallpaper;
pub mod wm;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use app::{App, AppKind, Ctx};
use display::Display;

use crate::drivers::virtio_input::VirtioInput;
use crate::sync::Spin;

static DISPLAY: Spin<Option<Display>> = Spin::new(None);
static INPUTS: Spin<Vec<VirtioInput>> = Spin::new(Vec::new());
static DISPLAY_DESC: Spin<Option<String>> = Spin::new(None);
static DISPLAY_MODES: Spin<(Vec<(u32, u32)>, (u32, u32))> = Spin::new((Vec::new(), (0, 0)));
/// Frames composited so far (used by the self-test).
pub static FRAMES: AtomicU64 = AtomicU64::new(0);
/// Frames per second and average frame time (µs) over the last second.
pub static FPS: AtomicU64 = AtomicU64::new(0);
pub static FRAME_US: AtomicU64 = AtomicU64::new(0);

pub fn set_display(d: Display) {
    update_display_description(&d);
    *DISPLAY.lock() = Some(d);
}

/// Record the display's current mode and the modes it offers.
pub fn update_display_description(d: &Display) {
    let (w, h) = d.size();
    *DISPLAY_DESC.lock() = Some(format!("{}x{} ({})", w, h, d.name()));
    *DISPLAY_MODES.lock() = (d.modes(), (w as u32, h as u32));
}

pub fn display_modes() -> Vec<(u32, u32)> {
    DISPLAY_MODES.lock().0.clone()
}

pub fn display_mode() -> (u32, u32) {
    DISPLAY_MODES.lock().1
}

pub fn add_input(d: VirtioInput) {
    INPUTS.lock().push(d);
}

pub fn display_description() -> String {
    DISPLAY_DESC.lock().clone().unwrap_or_else(|| String::from("none"))
}

pub fn launch(kind: AppKind) -> Option<Box<dyn App>> {
    Some(match kind {
        AppKind::Explorer => Box::new(explorer::Explorer::new("/")),
        AppKind::Terminal => Box::new(terminal::Terminal::new()),
        AppKind::Editor => Box::new(editor::Editor::new_empty()),
        AppKind::Settings => settings_app::boxed(),
        AppKind::About => Box::new(about::About::new()),
        AppKind::Other => return None,
    })
}

/// Open a path with its default app: folders in the explorer, programs in
/// a terminal, everything else in the text editor.
pub fn open_path(path: &str, ctx: &mut Ctx) -> Result<(), crate::fs::FsError> {
    let e = crate::fs::stat(path)?;
    if e.is_dir {
        ctx.open(Box::new(explorer::Explorer::new(path)));
        return Ok(());
    }
    let mut head = [0u8; 4];
    if let Ok(data) = crate::fs::read_file(path) {
        let n = data.len().min(4);
        head[..n].copy_from_slice(&data[..n]);
    }
    let name = crate::fs::file_name(path);
    if imageview::is_image_name(name) {
        ctx.open(Box::new(imageview::ImageViewer::open(path)));
        return Ok(());
    }
    if player::is_media_name(name) {
        ctx.open(Box::new(player::Player::open(path)));
        return Ok(());
    }
    if &head == b"\x7fELF" {
        let dir = crate::fs::parent(path);
        let cmd = if path.contains(' ') { format!("\"{}\"", path) } else { String::from(path) };
        ctx.open(Box::new(terminal::Terminal::with_command(&dir, &cmd)));
    } else {
        ctx.open(Box::new(editor::Editor::open(path)));
    }
    Ok(())
}

/// Desktop thread entry point.
pub extern "C" fn desktop_main(_: usize) {
    let Some(display) = DISPLAY.lock().take() else {
        crate::kprintln!("gui: no display available, desktop not started");
        return;
    };
    let mut wm = wm::Wm::new(display, launch);
    if let Some((w, h)) = crate::settings::get().resolution {
        wm.set_resolution(w, h);
    }
    crate::kprintln!("gui: desktop running on {}", display_description());
    crate::audio::play_system(crate::audio::SystemSound::Startup);

    // First-run layout: a file explorer and a terminal.
    wm.open_kind(AppKind::Explorer);
    wm.open_kind(AppKind::Terminal);

    let (mut stat_frames, mut stat_us, mut stat_since) = (0u64, 0u64, 0u64);
    let mut vbox_checked = 0u64;
    loop {
        for dev in INPUTS.lock().iter_mut() {
            dev.poll();
        }
        crate::drivers::vmmdev::poll_mouse();
        let now = crate::time::uptime_ms();
        if now - vbox_checked >= 500 {
            vbox_checked = now;
            // Follow the VirtualBox window size.
            if let Some((w, h)) = crate::drivers::vmmdev::display_change() {
                let (w, h) = (w.max(640), h.max(480));
                if !wm.set_resolution(w, h) {
                    crate::kprintln!("vbox: cannot switch to {}x{}", w, h);
                }
            }
        }
        let mut handled = 0;
        while let Some(ev) = crate::input::pop() {
            wm.handle(ev);
            handled += 1;
            if handled > 256 {
                break;
            }
        }
        let start = crate::time::uptime_ms();
        let t0 = crate::time::uptime_us();
        wm.frame();
        FRAMES.fetch_add(1, Ordering::Relaxed);
        stat_frames += 1;
        stat_us += crate::time::uptime_us() - t0;
        if start - stat_since >= 1000 {
            FPS.store(stat_frames, Ordering::Relaxed);
            FRAME_US.store(stat_us / stat_frames.max(1), Ordering::Relaxed);
            stat_frames = 0;
            stat_us = 0;
            stat_since = start;
        }
        // Up to ~120 frames a second while anything moves (animations,
        // drags, pointer); a short idle nap otherwise keeps input snappy.
        let spent = crate::time::uptime_ms() - start;
        let budget: u64 = if wm.is_animating() || handled > 0 { 8 } else { 4 };
        crate::proc::sched::sleep_ms(budget.saturating_sub(spent).max(1));
    }
}
