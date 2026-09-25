//! AHCI (SATA) disk driver: VirtualBox's "SATA" controller, QEMU q35's
//! built-in ICH9 AHCI and most real PCs. Polled DMA, one command at a time.

use alloc::string::String;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};

use super::pci::PciDevice;
use crate::mem::{paging, DmaBuf};
use crate::time::uptime_ms;

const GHC: usize = 0x04;
const PI: usize = 0x0c;
const PORT_BASE: usize = 0x100;

const P_CLB: usize = 0x00;
const P_CLBU: usize = 0x04;
const P_FB: usize = 0x08;
const P_FBU: usize = 0x0c;
const P_IS: usize = 0x10;
const P_IE: usize = 0x14;
const P_CMD: usize = 0x18;
const P_TFD: usize = 0x20;
const P_SIG: usize = 0x24;
const P_SSTS: usize = 0x28;
const P_SERR: usize = 0x30;
const P_CI: usize = 0x38;

const CMD_ST: u32 = 1 << 0;
const CMD_FRE: u32 = 1 << 4;
const CMD_FR: u32 = 1 << 14;
const CMD_CR: u32 = 1 << 15;

const SIG_ATA: u32 = 0x0000_0101;
const ATA_READ_DMA_EXT: u8 = 0x25;
const ATA_WRITE_DMA_EXT: u8 = 0x35;
const ATA_FLUSH_EXT: u8 = 0xea;
const ATA_IDENTIFY: u8 = 0xec;

const BOUNCE_SECTORS: usize = 128;

pub struct AhciDisk {
    port: usize,
    /// Command list (1 KiB), received FIS (256 B) and one command table.
    mem: DmaBuf,
    bounce: DmaBuf,
    pub sectors: u64,
    pub model: String,
    pub name: String,
}

unsafe impl Send for AhciDisk {}

fn r(addr: usize) -> u32 {
    unsafe { read_volatile(addr as *const u32) }
}
fn w(addr: usize, v: u32) {
    unsafe { write_volatile(addr as *mut u32, v) }
}

fn wait_clear(addr: usize, mask: u32, ms: u64) -> bool {
    let start = uptime_ms();
    let mut backoff = crate::time::Backoff::new();
    while r(addr) & mask != 0 {
        if uptime_ms() - start > ms {
            return false;
        }
        backoff.wait();
    }
    true
}

/// Find all SATA disks on an AHCI controller.
pub fn probe(pci: &PciDevice) -> Vec<AhciDisk> {
    let mut out = Vec::new();
    pci.enable();
    let abar = pci.bar(5);
    if abar == 0 {
        return out;
    }
    let hba = paging::map_mmio(abar, 0x1100) as usize;
    w(hba + GHC, r(hba + GHC) | (1 << 31)); // AHCI mode
    let implemented = r(hba + PI);
    for p in 0..32 {
        if implemented & (1 << p) == 0 {
            continue;
        }
        let port = hba + PORT_BASE + p * 0x80;
        let ssts = r(port + P_SSTS);
        if ssts & 0xf != 3 || (ssts >> 8) & 0xf != 1 {
            continue; // no device, or not active
        }
        if r(port + P_SIG) != SIG_ATA {
            continue; // ATAPI (CD/DVD) or port multiplier
        }
        if let Some(d) = AhciDisk::new(port, p) {
            out.push(d);
        }
    }
    out
}

impl AhciDisk {
    fn new(port: usize, index: usize) -> Option<AhciDisk> {
        // Stop the port while we install our command list.
        w(port + P_CMD, r(port + P_CMD) & !CMD_ST);
        if !wait_clear(port + P_CMD, CMD_CR, 500) {
            return None;
        }
        w(port + P_CMD, r(port + P_CMD) & !CMD_FRE);
        if !wait_clear(port + P_CMD, CMD_FR, 500) {
            return None;
        }
        let mem = DmaBuf::try_new(4096)?;
        let clb = mem.phys;
        let fb = mem.phys + 1024;
        w(port + P_CLB, clb as u32);
        w(port + P_CLBU, (clb >> 32) as u32);
        w(port + P_FB, fb as u32);
        w(port + P_FBU, (fb >> 32) as u32);
        w(port + P_SERR, 0xffff_ffff);
        w(port + P_IS, 0xffff_ffff);
        w(port + P_IE, 0);
        w(port + P_CMD, r(port + P_CMD) | CMD_FRE);
        w(port + P_CMD, r(port + P_CMD) | CMD_ST);

        let mut disk = AhciDisk {
            port,
            mem,
            bounce: DmaBuf::try_new(BOUNCE_SECTORS * 512)?,
            sectors: 0,
            model: String::new(),
            name: alloc::format!("sata{}", index),
        };
        if !disk.command(ATA_IDENTIFY, 0, 1, false) {
            return None;
        }
        let id = disk.bounce.as_slice();
        let word = |i: usize| u16::from_le_bytes([id[i * 2], id[i * 2 + 1]]);
        disk.sectors = (0..4).map(|i| (word(100 + i) as u64) << (16 * i)).sum();
        if disk.sectors == 0 {
            disk.sectors = word(60) as u64 | (word(61) as u64) << 16;
        }
        let mut model = Vec::new();
        for i in 27..47 {
            let v = word(i);
            model.push((v >> 8) as u8);
            model.push(v as u8);
        }
        disk.model = String::from_utf8_lossy(&model).trim().into();
        Some(disk)
    }

    /// Run one ATA command through command slot 0, transferring
    /// `count` sectors via the bounce buffer.
    fn command(&mut self, cmd: u8, lba: u64, count: usize, write: bool) -> bool {
        let port = self.port;
        if !wait_clear(port + P_TFD, 0x88, 1000) {
            return false; // BSY or DRQ stuck
        }
        let base = self.mem.virt() as usize;
        let table_phys = self.mem.phys + 2048;
        let table = base + 2048;
        unsafe {
            core::ptr::write_bytes(table as *mut u8, 0, 256);
            // Command header 0.
            let flags: u32 = 5 | if write { 1 << 6 } else { 0 } | (1 << 16);
            write_volatile(base as *mut u32, flags);
            write_volatile((base + 4) as *mut u32, 0);
            write_volatile((base + 8) as *mut u32, table_phys as u32);
            write_volatile((base + 12) as *mut u32, (table_phys >> 32) as u32);
            // Register H2D FIS.
            let f = table as *mut u8;
            let fis: [u8; 16] = [
                0x27,
                0x80,
                cmd,
                0,
                lba as u8,
                (lba >> 8) as u8,
                (lba >> 16) as u8,
                0x40,
                (lba >> 24) as u8,
                (lba >> 32) as u8,
                (lba >> 40) as u8,
                0,
                count as u8,
                (count >> 8) as u8,
                0,
                0,
            ];
            for (i, b) in fis.iter().enumerate() {
                write_volatile(f.add(i), *b);
            }
            // One PRD entry covering the bounce buffer.
            if count > 0 {
                let prd = table + 0x80;
                write_volatile(prd as *mut u64, self.bounce.phys);
                write_volatile((prd + 12) as *mut u32, (count * 512 - 1) as u32);
            }
        }
        w(port + P_IS, 0xffff_ffff);
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        w(port + P_CI, 1);
        let start = uptime_ms();
        let mut backoff = crate::time::Backoff::new();
        loop {
            if r(port + P_CI) & 1 == 0 {
                break;
            }
            if r(port + P_IS) & (1 << 30) != 0 || uptime_ms() - start > 5000 {
                return false;
            }
            backoff.wait();
        }
        r(port + P_TFD) & 1 == 0
    }
}

impl fat32::BlockDevice for AhciDisk {
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), ()> {
        let mut done = 0;
        while done < buf.len() {
            let n = (buf.len() - done).min(BOUNCE_SECTORS * 512);
            let sector = lba + (done / 512) as u64;
            if sector + (n / 512) as u64 > self.sectors || !self.command(ATA_READ_DMA_EXT, sector, n / 512, false) {
                return Err(());
            }
            buf[done..done + n].copy_from_slice(&self.bounce.as_slice()[..n]);
            done += n;
        }
        Ok(())
    }

    fn write(&mut self, lba: u64, buf: &[u8]) -> Result<(), ()> {
        let mut done = 0;
        while done < buf.len() {
            let n = (buf.len() - done).min(BOUNCE_SECTORS * 512);
            let sector = lba + (done / 512) as u64;
            if sector + (n / 512) as u64 > self.sectors {
                return Err(());
            }
            self.bounce.as_mut_slice()[..n].copy_from_slice(&buf[done..done + n]);
            if !self.command(ATA_WRITE_DMA_EXT, sector, n / 512, true) {
                return Err(());
            }
            done += n;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), ()> {
        if self.command(ATA_FLUSH_EXT, 0, 0, false) { Ok(()) } else { Err(()) }
    }
}
