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
pub mod settings_app;
pub mod shell;
pub mod terminal;
pub mod theme;
pub mod widgets;
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
    if &head == b"RIFF" && crate::fs::extension(path).as_deref() == Some("wav") {
        let data = crate::fs::read_file(path)?;
        match crate::audio::wav::decode(&data) {
            Ok((_, samples)) if crate::audio::is_present() => {
                crate::audio::play(alloc::sync::Arc::new(samples));
            }
            _ => ctx.open(Box::new(editor::Editor::open(path))),
        }
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

    loop {
        for dev in INPUTS.lock().iter_mut() {
            dev.poll();
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
        wm.frame();
        FRAMES.fetch_add(1, Ordering::Relaxed);
        // About 60 frames a second while animating, otherwise idle politely.
        if wm.is_animating() {
            let spent = crate::time::uptime_ms() - start;
            crate::proc::sched::sleep_ms(16u64.saturating_sub(spent).max(1));
        } else {
            crate::proc::sched::sleep_ms(10);
        }
    }
}
