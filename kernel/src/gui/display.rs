//! Output devices for the compositor: the virtio GPU (preferred, with a
//! hardware cursor) or the boot framebuffer from Limine.

use alloc::vec;
use alloc::vec::Vec;

use gfx::Rect;

use crate::drivers::virtio_gpu::{VirtioGpu, CURSOR_SIZE};

pub enum Display {
    Virtio(VirtioGpu),
    Framebuffer { back: Vec<u32>, fb: *mut u8, pitch: usize, width: u32, height: u32 },
}

unsafe impl Send for Display {}

impl Display {
    pub fn from_boot_framebuffer() -> Option<Display> {
        let fb = crate::boot::FRAMEBUFFER.response()?.first()?;
        if fb.bpp != 32 {
            return None;
        }
        let (w, h) = (fb.width as u32, fb.height as u32);
        Some(Display::Framebuffer { back: vec![0; (w * h) as usize], fb: fb.address, pitch: fb.pitch as usize, width: w, height: h })
    }

    pub fn size(&self) -> (i32, i32) {
        match self {
            Display::Virtio(g) => (g.width as i32, g.height as i32),
            Display::Framebuffer { width, height, .. } => (*width as i32, *height as i32),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Display::Virtio(_) => "virtio-gpu",
            Display::Framebuffer { .. } => "boot framebuffer",
        }
    }

    /// The buffer the compositor draws into.
    pub fn buffer(&mut self) -> &mut [u32] {
        match self {
            Display::Virtio(g) => g.framebuffer(),
            Display::Framebuffer { back, .. } => back,
        }
    }

    /// Make a region of the buffer visible.
    pub fn present(&mut self, r: Rect) {
        let (w, h) = self.size();
        let r = r.intersect(&Rect::new(0, 0, w, h));
        if r.is_empty() {
            return;
        }
        match self {
            Display::Virtio(g) => g.flush(r.x as u32, r.y as u32, r.w as u32, r.h as u32),
            Display::Framebuffer { back, fb, pitch, width, .. } => {
                for y in r.y..r.bottom() {
                    let src = &back[(y as u32 * *width + r.x as u32) as usize..][..r.w as usize];
                    unsafe {
                        let dst = fb.add(y as usize * *pitch + r.x as usize * 4) as *mut u32;
                        core::ptr::copy_nonoverlapping(src.as_ptr(), dst, r.w as usize);
                    }
                }
            }
        }
    }

    /// Resolutions the user may pick. The boot framebuffer is fixed by the
    /// firmware, so it only offers its current mode.
    pub fn modes(&self) -> alloc::vec::Vec<(u32, u32)> {
        match self {
            Display::Virtio(_) => alloc::vec![
                (1024, 768),
                (1280, 720),
                (1280, 800),
                (1366, 768),
                (1440, 900),
                (1600, 900),
                (1680, 1050),
                (1920, 1080),
            ],
            Display::Framebuffer { width, height, .. } => alloc::vec![(*width, *height)],
        }
    }

    pub fn set_mode(&mut self, w: u32, h: u32) -> bool {
        match self {
            Display::Virtio(g) => g.set_mode(w, h),
            Display::Framebuffer { width, height, .. } => (*width, *height) == (w, h),
        }
    }

    pub fn has_hw_cursor(&self) -> bool {
        matches!(self, Display::Virtio(_))
    }

    pub fn set_cursor(&mut self, image: &[u32], hot: (i32, i32), pos: (i32, i32)) {
        if let Display::Virtio(g) = self {
            g.set_cursor(image, hot.0 as u32, hot.1 as u32, pos.0 as u32, pos.1 as u32);
        }
    }

    pub fn move_cursor(&mut self, x: i32, y: i32) {
        if let Display::Virtio(g) = self {
            g.move_cursor(x.max(0) as u32, y.max(0) as u32);
        }
    }
}

pub const CURSOR_DIM: i32 = CURSOR_SIZE as i32;
