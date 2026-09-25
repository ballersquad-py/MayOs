//! VirtualBox guest device (VMMDev, PCI 80ee:cafe).
//!
//! Gives MayOS the two "guest additions" features people notice most:
//! the desktop follows the size of the VirtualBox window, and the mouse
//! moves seamlessly in and out of the VM without being captured.
//!
//! Requests are small structures in physical memory; writing their physical
//! address to the device's I/O port makes the host process them
//! synchronously and fill in the result.

use crate::arch::cpu::outl;
use crate::drivers::pci::PciDevice;
use crate::mem::DmaBuf;
use crate::sync::Spin;

const VMMDEV_REQUEST_HEADER_VERSION: u32 = 0x10001;
const VMMDEV_VERSION: u32 = 0x0001_0004;

const REQ_GET_MOUSE_STATUS: u32 = 1;
const REQ_SET_MOUSE_STATUS: u32 = 2;
const REQ_REPORT_GUEST_INFO: u32 = 50;
const REQ_GET_DISPLAY_CHANGE_REQUEST2: u32 = 54;
const REQ_SET_GUEST_CAPABILITIES: u32 = 56;

const MOUSE_GUEST_CAN_ABSOLUTE: u32 = 1 << 0;
const MOUSE_HOST_WANTS_ABSOLUTE: u32 = 1 << 1;
const MOUSE_NEW_PROTOCOL: u32 = 1 << 4;
const GUEST_SUPPORTS_GRAPHICS: u32 = 1 << 2;
const EVENT_DISPLAY_CHANGE_REQUEST: u32 = 1 << 2;

struct VmmDev {
    port: u16,
    req: DmaBuf,
    last_mouse: (i32, i32),
    last_display: Option<(u32, u32)>,
}

static DEV: Spin<Option<VmmDev>> = Spin::new(None);

impl VmmDev {
    /// Send a request whose body is `body` (little-endian u32 words) and
    /// return the body the host wrote back, or `None` if the host refused.
    fn request(&mut self, kind: u32, body: &[u32]) -> Option<[u32; 8]> {
        let size = 24 + body.len() as u32 * 4;
        let words = self.req.virt() as *mut u32;
        let header = [size, VMMDEV_REQUEST_HEADER_VERSION, kind, (-1i32) as u32, 0, 0];
        unsafe {
            for (i, w) in header.iter().chain(body.iter()).enumerate() {
                core::ptr::write_volatile(words.add(i), *w);
            }
            outl(self.port, self.req.phys as u32);
            if core::ptr::read_volatile(words.add(3)) != 0 {
                return None;
            }
            let mut out = [0u32; 8];
            for (i, o) in out.iter_mut().enumerate().take(body.len()) {
                *o = core::ptr::read_volatile(words.add(6 + i));
            }
            Some(out)
        }
    }
}

/// Bring up the device if present. Returns true when the host accepted us.
pub fn init(d: &PciDevice) -> bool {
    let bar = d.read32(0x10);
    if bar & 1 == 0 {
        return false;
    }
    d.enable_io();
    // The request block must sit below 4 GiB: the port takes 32 bits.
    let Some(req) = DmaBuf::try_new(4096) else { return false };
    if req.phys >> 32 != 0 {
        return false;
    }
    let mut dev = VmmDev { port: (bar & !3) as u16, req, last_mouse: (-1, -1), last_display: None };
    // Unknown OS type (0); the host only needs the interface version.
    if dev.request(REQ_REPORT_GUEST_INFO, &[VMMDEV_VERSION, 0]).is_none() {
        crate::kprintln!("vbox: host rejected guest info");
        return false;
    }
    let graphics = dev.request(REQ_SET_GUEST_CAPABILITIES, &[GUEST_SUPPORTS_GRAPHICS, 0]).is_some();
    let mouse = dev.request(REQ_SET_MOUSE_STATUS, &[MOUSE_GUEST_CAN_ABSOLUTE | MOUSE_NEW_PROTOCOL, 0, 0]).is_some();
    crate::kprintln!(
        "vbox: guest integration active (auto-resize {}, mouse integration {})",
        if graphics { "on" } else { "off" },
        if mouse { "on" } else { "off" }
    );
    *DEV.lock() = Some(dev);
    true
}

/// Push an absolute pointer event if the host moved the mouse.
pub fn poll_mouse() {
    let mut g = DEV.lock();
    let Some(dev) = g.as_mut() else { return };
    let Some(r) = dev.request(REQ_GET_MOUSE_STATUS, &[0, 0, 0]) else { return };
    if r[0] & MOUSE_HOST_WANTS_ABSOLUTE == 0 {
        return;
    }
    let pos = (r[1] as i32, r[2] as i32);
    if pos != dev.last_mouse {
        dev.last_mouse = pos;
        crate::input::push(crate::input::InputEvent::MouseAbsolute {
            x: Some(pos.0.clamp(0, 0xffff) as u32),
            y: Some(pos.1.clamp(0, 0xffff) as u32),
        });
    }
}

/// A new screen size the host asked for (the VirtualBox window changed),
/// reported once per change.
pub fn display_change() -> Option<(u32, u32)> {
    let mut g = DEV.lock();
    let dev = g.as_mut()?;
    let r = dev.request(REQ_GET_DISPLAY_CHANGE_REQUEST2, &[0, 0, 0, EVENT_DISPLAY_CHANGE_REQUEST, 0])?;
    let (w, h) = (r[0], r[1]);
    if w == 0 || h == 0 || dev.last_display == Some((w, h)) {
        return None;
    }
    dev.last_display = Some((w, h));
    Some((w, h))
}
