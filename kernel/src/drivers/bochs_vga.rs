//! Bochs/QEMU "DISPI" VBE display interface: QEMU's standard VGA
//! (`-vga std`) and VirtualBox's "VBoxVGA" controller. Mode setting is a
//! handful of index/data port writes; the framebuffer is PCI BAR 0.

use crate::arch::cpu::{inw, outw};
use crate::drivers::pci::PciDevice;
use crate::mem::paging;

const INDEX: u16 = 0x01ce;
const DATA: u16 = 0x01cf;

const REG_ID: u16 = 0;
const REG_XRES: u16 = 1;
const REG_YRES: u16 = 2;
const REG_BPP: u16 = 3;
const REG_ENABLE: u16 = 4;
const REG_VIRT_WIDTH: u16 = 6;
const REG_VIRT_HEIGHT: u16 = 7;
const REG_X_OFFSET: u16 = 8;
const REG_Y_OFFSET: u16 = 9;
const REG_VIDEO_MEMORY_64K: u16 = 0x0a;

const ENABLED: u16 = 0x01;
const LFB_ENABLED: u16 = 0x40;

pub struct BochsVga {
    fb_phys: u64,
    fb_virt: u64,
    pub vram: usize,
    pub width: u32,
    pub height: u32,
}

fn read(reg: u16) -> u16 {
    unsafe {
        outw(INDEX, reg);
        inw(DATA)
    }
}

fn write(reg: u16, v: u16) {
    unsafe {
        outw(INDEX, reg);
        outw(DATA, v);
    }
}

impl BochsVga {
    /// Accepts QEMU std VGA (1234:1111) and VBoxVGA (80ee:beef with a
    /// memory BAR 0).
    pub fn new(pci: &PciDevice) -> Option<BochsVga> {
        let id = read(REG_ID);
        if !(0xb0c0..=0xb0cf).contains(&id) {
            return None;
        }
        pci.enable();
        let fb_phys = pci.bar(0);
        if fb_phys == 0 || pci.read32(0x10) & 1 != 0 {
            return None;
        }
        let vram_64k = read(REG_VIDEO_MEMORY_64K) as usize;
        let vram = if vram_64k != 0 { vram_64k * 64 * 1024 } else { 16 * 1024 * 1024 };
        Some(BochsVga { fb_phys, fb_virt: 0, vram, width: 0, height: 0 })
    }

    pub fn supports(&self, w: u32, h: u32) -> bool {
        w >= 640 && h >= 480 && w <= 4096 && h <= 4096 && (w * h * 4) as usize <= self.vram
    }

    pub fn set_mode(&mut self, w: u32, h: u32) -> bool {
        if !self.supports(w, h) {
            return false;
        }
        write(REG_ENABLE, 0);
        write(REG_XRES, w as u16);
        write(REG_YRES, h as u16);
        write(REG_BPP, 32);
        write(REG_VIRT_WIDTH, w as u16);
        write(REG_VIRT_HEIGHT, h as u16);
        write(REG_X_OFFSET, 0);
        write(REG_Y_OFFSET, 0);
        write(REG_ENABLE, ENABLED | LFB_ENABLED);
        if read(REG_XRES) != w as u16 || read(REG_YRES) != h as u16 {
            return false;
        }
        self.fb_virt = paging::map_framebuffer(self.fb_phys, (w * h * 4) as usize);
        self.width = w;
        self.height = h;
        true
    }

    /// Framebuffer pointer and pitch in bytes.
    pub fn framebuffer(&self) -> (*mut u8, usize) {
        (self.fb_virt as *mut u8, self.width as usize * 4)
    }
}
