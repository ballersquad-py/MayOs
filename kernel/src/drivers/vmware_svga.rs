//! VMware SVGA II display adapter: VirtualBox's default "VMSVGA"
//! controller (also "VBoxSVGA") and QEMU's `-vga vmware`.
//!
//! Registers are reached through an index/value I/O port pair at BAR 0;
//! the framebuffer is BAR 1 and the command FIFO BAR 2. We use it in 2D:
//! set a mode, draw into VRAM, and send UPDATE commands for changed areas.
//!
//! All of VRAM is mapped once at start-up, and every mode is checked to
//! fit inside it before it is used.

use crate::arch::cpu::{inl, outl};
use crate::drivers::pci::PciDevice;
use crate::mem::paging;
use crate::time::uptime_ms;

const REG_ID: u32 = 0;
const REG_ENABLE: u32 = 1;
const REG_WIDTH: u32 = 2;
const REG_HEIGHT: u32 = 3;
const REG_MAX_WIDTH: u32 = 4;
const REG_MAX_HEIGHT: u32 = 5;
const REG_BITS_PER_PIXEL: u32 = 7;
const REG_BYTES_PER_LINE: u32 = 12;
const REG_FB_START: u32 = 13;
const REG_FB_OFFSET: u32 = 14;
const REG_VRAM_SIZE: u32 = 15;
const REG_CAPABILITIES: u32 = 17;
const REG_MEM_START: u32 = 18;
const REG_MEM_SIZE: u32 = 19;
const REG_CONFIG_DONE: u32 = 20;
const REG_SYNC: u32 = 21;
const REG_BUSY: u32 = 22;
const REG_GUEST_ID: u32 = 23;

const SVGA_ID_2: u32 = 0x9000_0002;
const CAP_EXTENDED_FIFO: u32 = 0x8000;
const CMD_UPDATE: u32 = 1;

const FIFO_MIN: usize = 0;
const FIFO_MAX: usize = 1;
const FIFO_NEXT_CMD: usize = 2;
const FIFO_STOP: usize = 3;
/// Number of FIFO registers when the extended FIFO is in use: commands
/// must start after them, or the device's own register writes would
/// land in our command stream.
const FIFO_NUM_REGS: u32 = 293;

pub struct VmwareSvga {
    io: u16,
    fb_phys: u64,
    fb_virt: u64,
    fb_len: usize,
    fifo: *mut u32,
    pub vram: usize,
    pub max: (u32, u32),
    pub width: u32,
    pub height: u32,
    pitch: usize,
    offset: usize,
    stalled: bool,
    /// UPDATE commands queued since the last nudge.
    pending: bool,
}

unsafe impl Send for VmwareSvga {}

impl VmwareSvga {
    fn read(&self, reg: u32) -> u32 {
        unsafe {
            outl(self.io, reg);
            inl(self.io + 1)
        }
    }

    fn write(&self, reg: u32, v: u32) {
        unsafe {
            outl(self.io, reg);
            outl(self.io + 1, v);
        }
    }

    pub fn new(pci: &PciDevice) -> Option<VmwareSvga> {
        let bar0 = pci.read32(0x10);
        if bar0 & 1 == 0 {
            return None; // not the I/O-port register interface
        }
        let cmd = pci.read32(0x04);
        pci.write32(0x04, (cmd & 0xffff) | 0x7);
        let mut dev = VmwareSvga {
            io: (bar0 & !3) as u16,
            fb_phys: 0,
            fb_virt: 0,
            fb_len: 0,
            fifo: core::ptr::null_mut(),
            vram: 0,
            max: (0, 0),
            width: 0,
            height: 0,
            pitch: 0,
            offset: 0,
            stalled: false,
            pending: false,
        };
        dev.write(REG_ID, SVGA_ID_2);
        if dev.read(REG_ID) != SVGA_ID_2 {
            return None;
        }
        dev.fb_phys = dev.read(REG_FB_START) as u64;
        if dev.fb_phys == 0 {
            dev.fb_phys = pci.bar(1);
        }
        dev.vram = dev.read(REG_VRAM_SIZE) as usize;
        dev.max = (dev.read(REG_MAX_WIDTH), dev.read(REG_MAX_HEIGHT));
        if dev.fb_phys == 0 || dev.vram < 1024 * 1024 {
            return None;
        }
        // Map all of VRAM (up to 256 MiB) once.
        dev.fb_len = dev.vram.min(256 * 1024 * 1024);
        dev.fb_virt = paging::map_framebuffer(dev.fb_phys, dev.fb_len)?;

        let fifo_phys = match dev.read(REG_MEM_START) as u64 {
            0 => pci.bar(2),
            p => p,
        };
        let fifo_size = dev.read(REG_MEM_SIZE) as usize;
        if fifo_phys == 0 || fifo_size < 4096 {
            return None;
        }
        dev.fifo = paging::map_mmio(fifo_phys, fifo_size) as *mut u32;
        let caps = dev.read(REG_CAPABILITIES);
        let min = if caps & CAP_EXTENDED_FIFO != 0 { FIFO_NUM_REGS * 4 } else { 16 };
        unsafe {
            let f = dev.fifo;
            for i in 0..(min as usize / 4) {
                f.add(i).write_volatile(0);
            }
            f.add(FIFO_MIN).write_volatile(min);
            f.add(FIFO_MAX).write_volatile(fifo_size as u32);
            f.add(FIFO_NEXT_CMD).write_volatile(min);
            f.add(FIFO_STOP).write_volatile(min);
        }
        dev.write(REG_GUEST_ID, 0x500a); // "other 64-bit"
        dev.write(REG_CONFIG_DONE, 1);
        crate::kprintln!(
            "svga: vram {} MiB at {:#x}, max {}x{}, fifo {} KiB, caps {:#x}",
            dev.vram / (1024 * 1024),
            dev.fb_phys,
            dev.max.0,
            dev.max.1,
            fifo_size / 1024,
            caps
        );
        Some(dev)
    }

    pub fn supports(&self, w: u32, h: u32) -> bool {
        w >= 640
            && h >= 480
            && w <= self.max.0.max(640)
            && h <= self.max.1.max(480)
            && (w as usize * h as usize * 4) <= self.fb_len
    }

    fn apply_mode(&mut self, w: u32, h: u32) -> bool {
        self.write(REG_WIDTH, w);
        self.write(REG_HEIGHT, h);
        self.write(REG_BITS_PER_PIXEL, 32);
        self.write(REG_ENABLE, 1);
        if self.read(REG_WIDTH) != w || self.read(REG_HEIGHT) != h {
            return false;
        }
        let pitch = self.read(REG_BYTES_PER_LINE) as usize;
        let offset = self.read(REG_FB_OFFSET) as usize;
        if pitch < w as usize * 4 || offset + pitch * h as usize > self.fb_len {
            crate::kprintln!("svga: mode {}x{} has pitch {} offset {} beyond VRAM", w, h, pitch, offset);
            return false;
        }
        self.pitch = pitch;
        self.offset = offset;
        self.width = w;
        self.height = h;
        true
    }

    pub fn set_mode(&mut self, w: u32, h: u32) -> bool {
        if !self.supports(w, h) {
            return false;
        }
        let old = (self.width, self.height);
        if self.apply_mode(w, h) {
            crate::kprintln!("svga: mode {}x{} pitch {}", w, h, self.pitch);
            return true;
        }
        // Put the previous mode back.
        if old.0 != 0 {
            self.apply_mode(old.0, old.1);
        }
        false
    }

    /// Framebuffer pointer, pitch and the number of bytes that are safe to
    /// write from that pointer.
    pub fn framebuffer(&self) -> (*mut u8, usize, usize) {
        ((self.fb_virt as usize + self.offset) as *mut u8, self.pitch, self.fb_len - self.offset)
    }

    fn fifo_reg(&self, i: usize) -> u32 {
        unsafe { self.fifo.add(i).read_volatile() }
    }

    /// Ask the device to process the FIFO; wait at most `ms` for it.
    fn sync(&mut self, ms: u64) -> bool {
        self.write(REG_SYNC, 1);
        self.pending = false;
        let start = uptime_ms();
        let mut backoff = crate::time::Backoff::new();
        while self.read(REG_BUSY) != 0 {
            if uptime_ms() - start > ms {
                return false;
            }
            backoff.wait();
        }
        true
    }

    /// Queue an UPDATE command so the host redraws a rectangle.
    pub fn update(&mut self, x: u32, y: u32, w: u32, h: u32) {
        let words = [CMD_UPDATE, x, y, w, h];
        let (min, max) = (self.fifo_reg(FIFO_MIN), self.fifo_reg(FIFO_MAX));
        let free = |next: u32, stop: u32| if next >= stop { (max - next) + (stop - min) } else { stop - next };
        if free(self.fifo_reg(FIFO_NEXT_CMD), self.fifo_reg(FIFO_STOP)) <= (words.len() * 4) as u32 + 4 {
            if !self.sync(20) {
                if !self.stalled {
                    crate::kprintln!("svga: command FIFO is not draining; skipping screen updates");
                    self.stalled = true;
                }
                return;
            }
            if free(self.fifo_reg(FIFO_NEXT_CMD), self.fifo_reg(FIFO_STOP)) <= (words.len() * 4) as u32 + 4 {
                return;
            }
        }
        self.stalled = false;
        let mut next = self.fifo_reg(FIFO_NEXT_CMD);
        for word in words {
            unsafe { self.fifo.add(next as usize / 4).write_volatile(word) };
            next += 4;
            if next >= max {
                next = min;
            }
        }
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        unsafe { self.fifo.add(FIFO_NEXT_CMD).write_volatile(next) };
        self.pending = true;
    }

    /// Tell the device new commands are waiting (without blocking). Only
    /// when there are some: each nudge wakes the host's graphics thread.
    pub fn kick(&mut self) {
        if self.pending {
            self.pending = false;
            self.write(REG_SYNC, 1);
        }
    }
}
