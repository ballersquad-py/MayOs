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
pub mod shell;
pub mod terminal;
pub mod theme;
pub mod widgets;
pub mod wm;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use app::{App, AppKind, Ctx};
use display::Display;

use crate::drivers::virtio_input::VirtioInput;
use crate::sync::{Once, Spin};

static DISPLAY: Spin<Option<Display>> = Spin::new(None);
static INPUTS: Spin<Vec<VirtioInput>> = Spin::new(Vec::new());
static DISPLAY_DESC: Once<String> = Once::new();
/// Frames composited so far (used by the self-test).
pub static FRAMES: AtomicU64 = AtomicU64::new(0);

pub fn set_display(d: Display) {
    let (w, h) = d.size();
    DISPLAY_DESC.set(format!("{}x{} ({})", w, h, d.name()));
    *DISPLAY.lock() = Some(d);
}

pub fn add_input(d: VirtioInput) {
    INPUTS.lock().push(d);
}

pub fn display_description() -> String {
    DISPLAY_DESC.get().cloned().unwrap_or_else(|| String::from("none"))
}

pub fn launch(kind: AppKind) -> Option<Box<dyn App>> {
    Some(match kind {
        AppKind::Explorer => Box::new(explorer::Explorer::new("/")),
        AppKind::Terminal => Box::new(terminal::Terminal::new()),
        AppKind::Editor => Box::new(editor::Editor::new_empty()),
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
    crate::kprintln!("gui: desktop running on {}", display_description());

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
        wm.frame();
        FRAMES.fetch_add(1, Ordering::Relaxed);
        crate::proc::sched::sleep_ms(10);
    }
}
