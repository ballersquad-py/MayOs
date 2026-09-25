//! virtio GPU driver (2D): a host-backed scanout plus a hardware cursor.

use super::pci::PciDevice;
use super::virtio::{Buf, Transport, Virtqueue};
use crate::mem::DmaBuf;

const CMD_GET_DISPLAY_INFO: u32 = 0x0100;
const CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const CMD_RESOURCE_UNREF: u32 = 0x0102;
const CMD_RESOURCE_DETACH_BACKING: u32 = 0x0107;
const CMD_SET_SCANOUT: u32 = 0x0103;
const CMD_RESOURCE_FLUSH: u32 = 0x0104;
const CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const CMD_UPDATE_CURSOR: u32 = 0x0300;
const CMD_MOVE_CURSOR: u32 = 0x0301;
const RESP_OK_NODATA: u32 = 0x1100;
const RESP_OK_DISPLAY_INFO: u32 = 0x1101;

const FORMAT_B8G8R8A8: u32 = 1;
const FORMAT_B8G8R8X8: u32 = 2;

const FB_RESOURCE: u32 = 1;
const FB_RESOURCE_ALT: u32 = 3;
const CURSOR_RESOURCE: u32 = 2;
pub const CURSOR_SIZE: u32 = 64;

pub struct VirtioGpu {
    _t: Transport,
    ctrl: Virtqueue,
    cursorq: Virtqueue,
    cmd: DmaBuf,
    fb: DmaBuf,
    cursor: DmaBuf,
    pub width: u32,
    pub height: u32,
    fb_resource: u32,
}

/// Little helper to build command structures in the DMA page.
struct Writer<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl Writer<'_> {
    fn u32(&mut self, v: u32) -> &mut Self {
        self.buf[self.pos..self.pos + 4].copy_from_slice(&v.to_le_bytes());
        self.pos += 4;
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.buf[self.pos..self.pos + 8].copy_from_slice(&v.to_le_bytes());
        self.pos += 8;
        self
    }
    fn hdr(&mut self, kind: u32) -> &mut Self {
        self.u32(kind).u32(0).u64(0).u32(0).u32(0)
    }
}

impl VirtioGpu {
    pub fn new(pci: &PciDevice) -> Option<VirtioGpu> {
        let t = Transport::new(pci)?;
        if !t.negotiate(0) {
            return None;
        }
        let ctrl = t.setup_queue(0, 64)?;
        let cursorq = t.setup_queue(1, 16)?;
        t.driver_ok();
        let mut gpu = VirtioGpu {
            _t: t,
            ctrl,
            cursorq,
            cmd: DmaBuf::new(8192),
            fb: DmaBuf::new(4096),
            cursor: DmaBuf::new((CURSOR_SIZE * CURSOR_SIZE * 4) as usize),
            width: 0,
            height: 0,
            fb_resource: FB_RESOURCE,
        };
        let (w, h) = gpu.display_info().unwrap_or((1280, 800));
        gpu.width = w;
        gpu.height = h;
        gpu.fb = DmaBuf::new((w * h * 4) as usize);

        gpu.command(|c| {
            c.hdr(CMD_RESOURCE_CREATE_2D).u32(FB_RESOURCE).u32(FORMAT_B8G8R8X8).u32(w).u32(h);
        })?;
        let (phys, len) = (gpu.fb.phys, (w * h * 4) as u32);
        gpu.command(|c| {
            c.hdr(CMD_RESOURCE_ATTACH_BACKING).u32(FB_RESOURCE).u32(1).u64(phys).u32(len).u32(0);
        })?;
        gpu.command(|c| {
            c.hdr(CMD_SET_SCANOUT).u32(0).u32(0).u32(w).u32(h).u32(0).u32(FB_RESOURCE);
        })?;

        gpu.command(|c| {
            c.hdr(CMD_RESOURCE_CREATE_2D).u32(CURSOR_RESOURCE).u32(FORMAT_B8G8R8A8).u32(CURSOR_SIZE).u32(CURSOR_SIZE);
        })?;
        let (cphys, clen) = (gpu.cursor.phys, CURSOR_SIZE * CURSOR_SIZE * 4);
        gpu.command(|c| {
            c.hdr(CMD_RESOURCE_ATTACH_BACKING).u32(CURSOR_RESOURCE).u32(1).u64(cphys).u32(clen).u32(0);
        })?;
        Some(gpu)
    }

    /// Run one control command and check for an OK response.
    fn command(&mut self, build: impl FnOnce(&mut Writer)) -> Option<()> {
        let len = {
            let buf = &mut self.cmd.as_mut_slice()[..4096];
            let mut w = Writer { buf, pos: 0 };
            build(&mut w);
            w.pos
        };
        let req = Buf { phys: self.cmd.phys, len: len as u32, device_writes: false };
        let resp = Buf { phys: self.cmd.phys + 4096, len: 24, device_writes: true };
        self.ctrl.submit_and_wait(&[req, resp])?;
        let t = u32::from_le_bytes(self.cmd.as_slice()[4096..4100].try_into().unwrap());
        if t == RESP_OK_NODATA { Some(()) } else { None }
    }

    fn display_info(&mut self) -> Option<(u32, u32)> {
        {
            let buf = &mut self.cmd.as_mut_slice()[..4096];
            Writer { buf, pos: 0 }.hdr(CMD_GET_DISPLAY_INFO);
        }
        let req = Buf { phys: self.cmd.phys, len: 24, device_writes: false };
        let resp = Buf { phys: self.cmd.phys + 4096, len: 24 + 16 * 24, device_writes: true };
        self.ctrl.submit_and_wait(&[req, resp])?;
        let r = &self.cmd.as_slice()[4096..];
        let rd = |o: usize| u32::from_le_bytes(r[o..o + 4].try_into().unwrap());
        if rd(0) != RESP_OK_DISPLAY_INFO {
            return None;
        }
        // pmodes[0]: rect {x, y, w, h}, enabled, flags
        let (w, h, enabled) = (rd(24 + 8), rd(24 + 12), rd(24 + 16));
        if enabled == 0 || w == 0 || h == 0 { None } else { Some((w, h)) }
    }

    /// Switch to a new resolution: create a new scanout resource, show it,
    /// then release the old one.
    pub fn set_mode(&mut self, w: u32, h: u32) -> bool {
        if w < 640 || h < 480 || w > 3840 || h > 2160 {
            return false;
        }
        let new_res = if self.fb_resource == FB_RESOURCE { FB_RESOURCE_ALT } else { FB_RESOURCE };
        let Some(fb) = DmaBuf::try_new((w * h * 4) as usize) else { return false };
        let (phys, len) = (fb.phys, w * h * 4);
        let ok = self
            .command(|c| {
                c.hdr(CMD_RESOURCE_CREATE_2D).u32(new_res).u32(FORMAT_B8G8R8X8).u32(w).u32(h);
            })
            .and_then(|_| {
                self.command(|c| {
                    c.hdr(CMD_RESOURCE_ATTACH_BACKING).u32(new_res).u32(1).u64(phys).u32(len).u32(0);
                })
            })
            .and_then(|_| {
                self.command(|c| {
                    c.hdr(CMD_SET_SCANOUT).u32(0).u32(0).u32(w).u32(h).u32(0).u32(new_res);
                })
            });
        if ok.is_none() {
            let _ = self.command(|c| {
                c.hdr(CMD_RESOURCE_UNREF).u32(new_res).u32(0);
            });
            return false;
        }
        let old = self.fb_resource;
        let _ = self.command(|c| {
            c.hdr(CMD_RESOURCE_DETACH_BACKING).u32(old).u32(0);
        });
        let _ = self.command(|c| {
            c.hdr(CMD_RESOURCE_UNREF).u32(old).u32(0);
        });
        self.fb = fb;
        self.fb_resource = new_res;
        self.width = w;
        self.height = h;
        true
    }

    pub fn framebuffer(&mut self) -> &mut [u32] {
        let n = (self.width * self.height) as usize;
        unsafe { core::slice::from_raw_parts_mut(self.fb.virt() as *mut u32, n) }
    }

    /// Copy a rectangle of the guest framebuffer to the host and show it.
    pub fn flush(&mut self, x: u32, y: u32, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        let offset = (y as u64 * self.width as u64 + x as u64) * 4;
        let res = self.fb_resource;
        let _ = self.command(|c| {
            c.hdr(CMD_TRANSFER_TO_HOST_2D).u32(x).u32(y).u32(w).u32(h).u64(offset).u32(res).u32(0);
        });
        let _ = self.command(|c| {
            c.hdr(CMD_RESOURCE_FLUSH).u32(x).u32(y).u32(w).u32(h).u32(res).u32(0);
        });
    }

    /// Upload a 64x64 ARGB cursor image with its hotspot.
    pub fn set_cursor(&mut self, pixels: &[u32], hot_x: u32, hot_y: u32, x: u32, y: u32) {
        let n = (CURSOR_SIZE * CURSOR_SIZE) as usize;
        let dst = unsafe { core::slice::from_raw_parts_mut(self.cursor.virt() as *mut u32, n) };
        dst.copy_from_slice(&pixels[..n]);
        let _ = self.command(|c| {
            c.hdr(CMD_TRANSFER_TO_HOST_2D)
                .u32(0)
                .u32(0)
                .u32(CURSOR_SIZE)
                .u32(CURSOR_SIZE)
                .u64(0)
                .u32(CURSOR_RESOURCE)
                .u32(0);
        });
        self.cursor_command(CMD_UPDATE_CURSOR, x, y, CURSOR_RESOURCE, hot_x, hot_y);
    }

    pub fn move_cursor(&mut self, x: u32, y: u32) {
        self.cursor_command(CMD_MOVE_CURSOR, x, y, CURSOR_RESOURCE, 0, 0);
    }

    fn cursor_command(&mut self, kind: u32, x: u32, y: u32, res: u32, hx: u32, hy: u32) {
        let off = 6144;
        {
            let buf = &mut self.cmd.as_mut_slice()[off..off + 64];
            let mut w = Writer { buf, pos: 0 };
            w.hdr(kind).u32(0).u32(x).u32(y).u32(0).u32(res).u32(hx).u32(hy).u32(0);
        }
        let req = Buf { phys: self.cmd.phys + off as u64, len: 56, device_writes: false };
        let _ = self.cursorq.submit_and_wait(&[req]);
    }
}
