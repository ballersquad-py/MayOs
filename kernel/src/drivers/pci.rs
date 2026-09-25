//! PCI configuration space access (legacy port I/O mechanism) and bus scan.

use alloc::vec::Vec;

use crate::arch::cpu::{inl, outl};
use crate::sync::Spin;

#[derive(Clone, Copy, Debug)]
pub struct PciDevice {
    pub bus: u8,
    pub slot: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
    pub prog_if: u8,
}

static LOCK: Spin<()> = Spin::new(());

fn addr(bus: u8, slot: u8, func: u8, off: u8) -> u32 {
    0x8000_0000 | (bus as u32) << 16 | (slot as u32) << 11 | (func as u32) << 8 | (off as u32 & 0xfc)
}

pub fn read32(bus: u8, slot: u8, func: u8, off: u8) -> u32 {
    let _g = LOCK.lock();
    unsafe {
        outl(0xcf8, addr(bus, slot, func, off));
        inl(0xcfc)
    }
}

pub fn write32(bus: u8, slot: u8, func: u8, off: u8, v: u32) {
    let _g = LOCK.lock();
    unsafe {
        outl(0xcf8, addr(bus, slot, func, off));
        outl(0xcfc, v);
    }
}

impl PciDevice {
    pub fn read32(&self, off: u8) -> u32 {
        read32(self.bus, self.slot, self.func, off)
    }

    pub fn read8(&self, off: u8) -> u8 {
        (self.read32(off & !3) >> ((off & 3) * 8)) as u8
    }

    pub fn write32(&self, off: u8, v: u32) {
        write32(self.bus, self.slot, self.func, off, v)
    }

    /// Enable memory decoding and bus mastering (needed for DMA).
    pub fn enable(&self) {
        let cmd = self.read32(0x04);
        self.write32(0x04, (cmd & 0xffff) | 0x6);
    }

    /// Physical address of a memory BAR (handles 64-bit BARs).
    pub fn bar(&self, i: u8) -> u64 {
        let off = 0x10 + i * 4;
        let lo = self.read32(off);
        if lo & 1 != 0 {
            return (lo & !3) as u64; // I/O BAR
        }
        let mut addr = (lo & !0xf) as u64;
        if (lo >> 1) & 3 == 2 {
            addr |= (self.read32(off + 4) as u64) << 32;
        }
        addr
    }

    /// Walk the capability list, returning offsets of capabilities with `id`.
    pub fn capabilities(&self, id: u8) -> Vec<u8> {
        let mut out = Vec::new();
        let status = self.read32(0x04) >> 16;
        if status & 0x10 == 0 {
            return out;
        }
        let mut p = self.read8(0x34) & !3;
        let mut guard = 0;
        while p != 0 && guard < 48 {
            if self.read8(p) == id {
                out.push(p);
            }
            p = self.read8(p + 1) & !3;
            guard += 1;
        }
        out
    }

    pub fn class_name(&self) -> &'static str {
        match (self.class, self.subclass) {
            (0x01, 0x00) => "SCSI storage",
            (0x01, 0x01) => "IDE controller",
            (0x01, 0x06) => "SATA controller",
            (0x01, 0x08) => "NVMe controller",
            (0x01, _) => "Storage controller",
            (0x02, _) => "Network controller",
            (0x03, _) => "Display controller",
            (0x04, _) => "Multimedia controller",
            (0x06, 0x00) => "Host bridge",
            (0x06, 0x01) => "ISA bridge",
            (0x06, 0x04) => "PCI bridge",
            (0x06, _) => "Bridge",
            (0x09, _) => "Input controller",
            (0x0c, 0x03) => "USB controller",
            (0x0c, 0x05) => "SMBus controller",
            (0x0c, _) => "Serial bus controller",
            _ => "Device",
        }
    }
}

static DEVICES: Spin<Vec<PciDevice>> = Spin::new(Vec::new());

pub fn scan() {
    let mut found = Vec::new();
    for bus in 0..=255u8 {
        for slot in 0..32u8 {
            let id = read32(bus, slot, 0, 0);
            if id & 0xffff == 0xffff {
                continue;
            }
            let header = (read32(bus, slot, 0, 0x0c) >> 16) as u8;
            let funcs = if header & 0x80 != 0 { 8 } else { 1 };
            for func in 0..funcs {
                let id = read32(bus, slot, func, 0);
                if id & 0xffff == 0xffff {
                    continue;
                }
                let class = read32(bus, slot, func, 0x08);
                found.push(PciDevice {
                    bus,
                    slot,
                    func,
                    vendor: id as u16,
                    device: (id >> 16) as u16,
                    class: (class >> 24) as u8,
                    subclass: (class >> 16) as u8,
                    prog_if: (class >> 8) as u8,
                });
            }
        }
    }
    *DEVICES.lock() = found;
}

pub fn devices() -> Vec<PciDevice> {
    DEVICES.lock().clone()
}
