//! Output devices for the compositor:
//! - virtio GPU (QEMU), with a hardware cursor
//! - VMware SVGA II (VirtualBox "VMSVGA"/"VBoxSVGA", QEMU `-vga vmware`)
//! - Bochs VBE (VirtualBox "VBoxVGA", QEMU `-vga std`)
//! - the boot framebuffer from the firmware (fixed resolution)
//!
//! Except for virtio-gpu (whose backing store is ordinary RAM), the
//! compositor draws into a back buffer in RAM and `present` copies changed
//! rectangles to video memory.

use alloc::vec;
use alloc::vec::Vec;

use gfx::Rect;

use crate::drivers::bochs_vga::BochsVga;
use crate::drivers::virtio_gpu::{VirtioGpu, CURSOR_SIZE};
use crate::drivers::vmware_svga::VmwareSvga;

pub enum Display {
    Virtio(VirtioGpu),
    Svga { dev: VmwareSvga, back: Vec<u32> },
    Bochs { dev: BochsVga, back: Vec<u32> },
    Framebuffer { back: Vec<u32>, fb: *mut u8, pitch: usize, width: u32, height: u32 },
}

unsafe impl Send for Display {}

/// Resolutions offered in Settings (filtered by what the adapter supports).
const COMMON_MODES: &[(u32, u32)] = &[
    (800, 600),
    (1024, 768),
    (1152, 864),
    (1280, 720),
    (1280, 800),
    (1280, 1024),
    (1366, 768),
    (1440, 900),
    (1600, 900),
    (1680, 1050),
    (1920, 1080),
    (1920, 1200),
    (2560, 1440),
];

/// Copy a rectangle of the back buffer to video memory, never writing past
/// `fb_len` bytes.
fn copy_rect(back: &[u32], width: u32, fb: *mut u8, pitch: usize, fb_len: usize, r: Rect) {
    if (r.bottom() as usize - 1) * pitch + r.right() as usize * 4 > fb_len {
        return;
    }
    for y in r.y..r.bottom() {
        let src = &back[(y as u32 * width + r.x as u32) as usize..][..r.w as usize];
        unsafe {
            let dst = fb.add(y as usize * pitch + r.x as usize * 4) as *mut u32;
            core::ptr::copy_nonoverlapping(src.as_ptr(), dst, r.w as usize);
        }
    }
}

impl Display {
    pub fn from_boot_framebuffer() -> Option<Display> {
        let fb = crate::boot::FRAMEBUFFER.response()?.first()?;
        if fb.bpp != 32 {
            return None;
        }
        let (w, h) = (fb.width as u32, fb.height as u32);
        Some(Display::Framebuffer { back: vec![0; (w * h) as usize], fb: fb.address, pitch: fb.pitch as usize, width: w, height: h })
    }

    /// Size of the firmware framebuffer, used as the starting mode for
    /// adapters we drive ourselves (so the screen doesn't jump at boot).
    pub fn boot_size() -> (u32, u32) {
        crate::boot::FRAMEBUFFER
            .response()
            .and_then(|r| r.first())
            .map(|f| (f.width as u32, f.height as u32))
            .unwrap_or((1280, 800))
    }

    pub fn from_svga(mut dev: VmwareSvga) -> Option<Display> {
        let (w, h) = Self::boot_size();
        if !dev.set_mode(w, h) && !dev.set_mode(1024, 768) {
            return None;
        }
        let back = vec![0; (dev.width * dev.height) as usize];
        Some(Display::Svga { dev, back })
    }

    pub fn from_bochs(mut dev: BochsVga) -> Option<Display> {
        let (w, h) = Self::boot_size();
        if !dev.set_mode(w, h) && !dev.set_mode(1024, 768) {
            return None;
        }
        let back = vec![0; (dev.width * dev.height) as usize];
        Some(Display::Bochs { dev, back })
    }

    pub fn size(&self) -> (i32, i32) {
        match self {
            Display::Virtio(g) => (g.width as i32, g.height as i32),
            Display::Svga { dev, .. } => (dev.width as i32, dev.height as i32),
            Display::Bochs { dev, .. } => (dev.width as i32, dev.height as i32),
            Display::Framebuffer { width, height, .. } => (*width as i32, *height as i32),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Display::Virtio(_) => "virtio-gpu",
            Display::Svga { .. } => "VMware SVGA II",
            Display::Bochs { .. } => "Bochs VBE",
            Display::Framebuffer { .. } => "boot framebuffer",
        }
    }

    /// The buffer the compositor draws into.
    pub fn buffer(&mut self) -> &mut [u32] {
        match self {
            Display::Virtio(g) => g.framebuffer(),
            Display::Svga { back, .. } | Display::Bochs { back, .. } | Display::Framebuffer { back, .. } => back,
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
            Display::Svga { dev, back } => {
                let (fb, pitch, len) = dev.framebuffer();
                copy_rect(back, dev.width, fb, pitch, len, r);
                dev.update(r.x as u32, r.y as u32, r.w as u32, r.h as u32);
            }
            Display::Bochs { dev, back } => {
                let (fb, pitch, len) = dev.framebuffer();
                copy_rect(back, dev.width, fb, pitch, len, r);
            }
            Display::Framebuffer { back, fb, pitch, width, height } => {
                let len = *pitch * *height as usize;
                copy_rect(back, *width, *fb, *pitch, len, r)
            }
        }
    }

    /// Resolutions the user may pick. The boot framebuffer is fixed by the
    /// firmware, so it only offers its current mode.
    pub fn modes(&self) -> Vec<(u32, u32)> {
        let (cw, ch) = self.size();
        let mut v: Vec<(u32, u32)> = match self {
            Display::Virtio(_) => COMMON_MODES.iter().copied().filter(|&(w, h)| w <= 1920 && h <= 1200).collect(),
            Display::Svga { dev, .. } => COMMON_MODES.iter().copied().filter(|&(w, h)| dev.supports(w, h)).collect(),
            Display::Bochs { dev, .. } => COMMON_MODES.iter().copied().filter(|&(w, h)| dev.supports(w, h)).collect(),
            Display::Framebuffer { .. } => Vec::new(),
        };
        if !v.contains(&(cw as u32, ch as u32)) {
            v.push((cw as u32, ch as u32));
            v.sort();
        }
        v
    }

    pub fn set_mode(&mut self, w: u32, h: u32) -> bool {
        match self {
            Display::Virtio(g) => g.set_mode(w, h),
            Display::Svga { dev, back } => {
                if !dev.set_mode(w, h) {
                    return false;
                }
                *back = vec![0; (w * h) as usize];
                true
            }
            Display::Bochs { dev, back } => {
                if !dev.set_mode(w, h) {
                    return false;
                }
                *back = vec![0; (w * h) as usize];
                true
            }
            Display::Framebuffer { width, height, .. } => (*width, *height) == (w, h),
        }
    }

    /// Called once per composited frame, after all `present`s.
    pub fn end_frame(&mut self) {
        if let Display::Svga { dev, .. } = self {
            dev.kick();
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
