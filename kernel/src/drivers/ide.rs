//! Legacy IDE (ATA PIO) disk driver, for machines whose disks sit on an
//! IDE controller (VirtualBox's PIIX4 "IDE" controller, QEMU `-M pc`).
//! Slow but simple: 28/48-bit LBA, 16-bit programmed I/O.

use alloc::string::String;
use alloc::vec::Vec;

use crate::arch::cpu::{inb, inw, outb, outw};
use crate::time::uptime_ms;

const CHANNELS: [(u16, u16); 2] = [(0x1f0, 0x3f6), (0x170, 0x376)];

pub struct IdeDisk {
    io: u16,
    ctrl: u16,
    slave: bool,
    pub sectors: u64,
    pub model: String,
    pub name: String,
}

unsafe impl Send for IdeDisk {}

impl IdeDisk {
    fn status(&self) -> u8 {
        unsafe { inb(self.io + 7) }
    }

    fn wait_ready(&self, ms: u64) -> bool {
        let start = uptime_ms();
        loop {
            let s = self.status();
            if s & 0x80 == 0 {
                return s & 0x01 == 0;
            }
            if uptime_ms() - start > ms {
                return false;
            }
        }
    }

    fn wait_drq(&self) -> bool {
        let start = uptime_ms();
        loop {
            let s = self.status();
            if s & 0x01 != 0 {
                return false;
            }
            if s & 0x80 == 0 && s & 0x08 != 0 {
                return true;
            }
            if uptime_ms() - start > 2000 {
                return false;
            }
        }
    }

    fn delay(&self) {
        for _ in 0..4 {
            unsafe { inb(self.ctrl) };
        }
    }

    fn setup(&self, lba: u64, count: u16, cmd: u8) {
        unsafe {
            outb(self.io + 6, 0x40 | if self.slave { 0x10 } else { 0 });
            self.delay();
            // 48-bit LBA: high bytes first, then low bytes.
            outb(self.io + 2, (count >> 8) as u8);
            outb(self.io + 3, (lba >> 24) as u8);
            outb(self.io + 4, (lba >> 32) as u8);
            outb(self.io + 5, (lba >> 40) as u8);
            outb(self.io + 2, count as u8);
            outb(self.io + 3, lba as u8);
            outb(self.io + 4, (lba >> 8) as u8);
            outb(self.io + 5, (lba >> 16) as u8);
            outb(self.io + 7, cmd);
        }
    }
}

/// Probe the two legacy channels for ATA hard disks.
pub fn probe() -> Vec<IdeDisk> {
    let mut out = Vec::new();
    for (ch, &(io, ctrl)) in CHANNELS.iter().enumerate() {
        // A floating bus reads 0xff.
        if unsafe { inb(io + 7) } == 0xff {
            continue;
        }
        for slave in [false, true] {
            let mut d = IdeDisk { io, ctrl, slave, sectors: 0, model: String::new(), name: alloc::format!("ide{}", ch * 2 + slave as usize) };
            unsafe {
                outb(io + 6, if slave { 0xb0 } else { 0xa0 });
                d.delay();
                outb(io + 2, 0);
                outb(io + 3, 0);
                outb(io + 4, 0);
                outb(io + 5, 0);
                outb(io + 7, 0xec); // IDENTIFY
                if inb(io + 7) == 0 {
                    continue;
                }
            }
            if !d.wait_ready(500) {
                continue;
            }
            // ATAPI devices set a signature in the LBA mid/high registers.
            if unsafe { inb(io + 4) } != 0 || unsafe { inb(io + 5) } != 0 {
                continue;
            }
            if !d.wait_drq() {
                continue;
            }
            let mut id = [0u16; 256];
            for w in id.iter_mut() {
                *w = unsafe { inw(io) };
            }
            let lba48 = (0..4).map(|i| (id[100 + i] as u64) << (16 * i)).sum::<u64>();
            d.sectors = if lba48 != 0 { lba48 } else { id[60] as u64 | (id[61] as u64) << 16 };
            let mut model = Vec::new();
            for &w in &id[27..47] {
                model.push((w >> 8) as u8);
                model.push(w as u8);
            }
            d.model = String::from_utf8_lossy(&model).trim().into();
            if d.sectors > 0 {
                out.push(d);
            }
        }
    }
    out
}

impl fat32::BlockDevice for IdeDisk {
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), ()> {
        let total = buf.len() / 512;
        let mut done = 0;
        while done < total {
            let n = (total - done).min(256);
            self.setup(lba + done as u64, n as u16 & 0xff | if n == 256 { 0x100 } else { 0 }, 0x24);
            for s in 0..n {
                if !self.wait_drq() {
                    return Err(());
                }
                let off = (done + s) * 512;
                for i in 0..256 {
                    let v = unsafe { inw(self.io) };
                    buf[off + i * 2] = v as u8;
                    buf[off + i * 2 + 1] = (v >> 8) as u8;
                }
            }
            done += n;
        }
        Ok(())
    }

    fn write(&mut self, lba: u64, buf: &[u8]) -> Result<(), ()> {
        let total = buf.len() / 512;
        let mut done = 0;
        while done < total {
            let n = (total - done).min(256);
            self.setup(lba + done as u64, n as u16 & 0xff | if n == 256 { 0x100 } else { 0 }, 0x34);
            for s in 0..n {
                if !self.wait_drq() {
                    return Err(());
                }
                let off = (done + s) * 512;
                for i in 0..256 {
                    let v = buf[off + i * 2] as u16 | (buf[off + i * 2 + 1] as u16) << 8;
                    unsafe { outw(self.io, v) };
                }
            }
            done += n;
            if !self.wait_ready(2000) {
                return Err(());
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), ()> {
        unsafe { outb(self.io + 7, 0xea) }; // FLUSH CACHE EXT
        if self.wait_ready(5000) { Ok(()) } else { Err(()) }
    }
}
