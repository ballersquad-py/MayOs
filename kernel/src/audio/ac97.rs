//! Intel ICH AC'97 audio controller (QEMU `-device AC97`, VirtualBox
//! "ICH AC97"). 48 kHz, 16-bit stereo output through the PCM-out DMA
//! engine and a ring of 32 buffer descriptors.

use crate::arch::cpu::{inb, inl, inw, outb, outl, outw};
use crate::drivers::pci::PciDevice;
use crate::mem::DmaBuf;

pub const VENDOR_INTEL: u16 = 0x8086;
pub const DEVICE_IDS: &[u16] = &[0x2415, 0x2425, 0x2445, 0x2485, 0x24c5, 0x24d5, 0x266e, 0x27de];

pub const BUFFERS: usize = 32;
/// Stereo frames per buffer (about 21 ms at 48 kHz).
pub const FRAMES: usize = 1024;

// Mixer (NAM) registers.
const NAM_RESET: u16 = 0x00;
const NAM_MASTER: u16 = 0x02;
const NAM_PCM_OUT: u16 = 0x18;
const NAM_EXT_ID: u16 = 0x28;
const NAM_EXT_CTRL: u16 = 0x2a;
const NAM_FRONT_RATE: u16 = 0x2c;

// Bus master (NABM) registers, PCM-out box at 0x10.
const PO_BDBAR: u16 = 0x10;
const PO_CIV: u16 = 0x14;
const PO_LVI: u16 = 0x15;
const PO_SR: u16 = 0x16;
const PO_CR: u16 = 0x1b;
const GLOB_CNT: u16 = 0x2c;

pub struct Ac97 {
    nam: u16,
    nabm: u16,
    bdl: DmaBuf,
    pub buffers: DmaBuf,
    lvi: u8,
    running: bool,
}

impl Ac97 {
    pub fn new(pci: &PciDevice) -> Option<Ac97> {
        pci.enable_io();
        let nam = (pci.bar(0) & !3) as u16;
        let nabm = (pci.bar(1) & !3) as u16;
        if nam == 0 || nabm == 0 {
            return None;
        }
        let dev = Ac97 {
            nam,
            nabm,
            bdl: DmaBuf::new(BUFFERS * 8),
            buffers: DmaBuf::new(BUFFERS * FRAMES * 4),
            lvi: 0,
            running: false,
        };
        unsafe {
            // Leave cold reset, then reset the mixer.
            outl(nabm + GLOB_CNT, 0x2);
            crate::proc::sched::sleep_ms(20);
            outw(nam + NAM_RESET, 1);
            crate::proc::sched::sleep_ms(20);
            outw(nam + NAM_MASTER, 0x0000);
            outw(nam + NAM_PCM_OUT, 0x0808);
            // Variable rate audio: pin the DAC to 48 kHz.
            if inw(nam + NAM_EXT_ID) & 1 != 0 {
                outw(nam + NAM_EXT_CTRL, inw(nam + NAM_EXT_CTRL) | 1);
                outw(nam + NAM_FRONT_RATE, 48000);
            }
            // Reset the PCM-out engine.
            outb(nabm + PO_CR, 0x2);
            for _ in 0..1000 {
                if inb(nabm + PO_CR) & 0x2 == 0 {
                    break;
                }
            }
        }
        for i in 0..BUFFERS {
            let d = dev.bdl.virt() as usize + i * 8;
            unsafe {
                core::ptr::write_volatile(d as *mut u32, (dev.buffers.phys + (i * FRAMES * 4) as u64) as u32);
                core::ptr::write_volatile((d + 4) as *mut u16, (FRAMES * 2) as u16);
                core::ptr::write_volatile((d + 6) as *mut u16, 0);
            }
        }
        unsafe { outl(nabm + PO_BDBAR, dev.bdl.phys as u32) };
        let _ = unsafe { inl(nabm + PO_BDBAR) };
        Some(dev)
    }

    pub fn buffer(&mut self, i: usize) -> &mut [i16] {
        unsafe {
            core::slice::from_raw_parts_mut((self.buffers.virt() as usize + i * FRAMES * 4) as *mut i16, FRAMES * 2)
        }
    }

    pub fn current(&self) -> u8 {
        unsafe { inb(self.nabm + PO_CIV) & 31 }
    }

    pub fn last_valid(&self) -> u8 {
        self.lvi
    }

    pub fn set_last_valid(&mut self, i: u8) {
        self.lvi = i & 31;
        unsafe { outb(self.nabm + PO_LVI, self.lvi) };
    }

    /// Start or resume DMA if the engine halted (e.g. it caught up with LVI).
    pub fn kick(&mut self) {
        unsafe {
            let sr = inw(self.nabm + PO_SR);
            if sr & 0x1c != 0 {
                outw(self.nabm + PO_SR, sr & 0x1c); // clear LVBCI, BCIS, FIFOE
            }
            if !self.running || sr & 1 != 0 {
                outb(self.nabm + PO_CR, 0x1);
                self.running = true;
            }
        }
    }

    pub fn stop(&mut self) {
        unsafe { outb(self.nabm + PO_CR, 0) };
        self.running = false;
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Hardware master volume: 0..=100.
    pub fn set_volume(&mut self, percent: u8, mute: bool) {
        let att = ((100 - percent.min(100) as u16) * 31 / 100) & 0x1f;
        let v = if mute || percent == 0 { 0x8000 } else { (att << 8) | att };
        unsafe { outw(self.nam + NAM_MASTER, v) };
    }
}
