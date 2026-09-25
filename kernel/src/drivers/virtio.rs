//! virtio 1.x over PCI ("modern" transport) with split virtqueues.
//!
//! Drivers use polling: requests are submitted and the used ring is spun on
//! until the device answers. This keeps the drivers simple and is fast under
//! QEMU. Interrupt-driven completion can come later with MSI-X.

use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

use super::pci::PciDevice;
use crate::mem::{paging, DmaBuf};

pub const VENDOR: u16 = 0x1af4;
pub const DEVICE_BLOCK: u16 = 0x1042;
/// Transitional (legacy + modern) block device ID, QEMU's default.
pub const DEVICE_BLOCK_TRANSITIONAL: u16 = 0x1001;
pub const DEVICE_GPU: u16 = 0x1050;
pub const DEVICE_INPUT: u16 = 0x1052;

const STATUS_ACK: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FEATURES_OK: u8 = 8;
const STATUS_FAILED: u8 = 128;

const F_VERSION_1: u64 = 1 << 32;

pub const DESC_F_NEXT: u16 = 1;
pub const DESC_F_WRITE: u16 = 2;

// Common configuration structure offsets.
const C_DFSELECT: usize = 0x00;
const C_DF: usize = 0x04;
const C_GFSELECT: usize = 0x08;
const C_GF: usize = 0x0c;
const C_MSIX: usize = 0x10;
const C_STATUS: usize = 0x14;
const C_Q_SELECT: usize = 0x16;
const C_Q_SIZE: usize = 0x18;
const C_Q_MSIX: usize = 0x1a;
const C_Q_ENABLE: usize = 0x1c;
const C_Q_NOTIFY_OFF: usize = 0x1e;
const C_Q_DESC: usize = 0x20;
const C_Q_DRIVER: usize = 0x28;
const C_Q_DEVICE: usize = 0x30;

pub struct Transport {
    common: usize,
    notify: usize,
    notify_mult: u32,
    pub device_cfg: usize,
}

unsafe fn r8(a: usize) -> u8 {
    unsafe { read_volatile(a as *const u8) }
}
unsafe fn r16(a: usize) -> u16 {
    unsafe { read_volatile(a as *const u16) }
}
unsafe fn r32(a: usize) -> u32 {
    unsafe { read_volatile(a as *const u32) }
}
unsafe fn w8(a: usize, v: u8) {
    unsafe { write_volatile(a as *mut u8, v) }
}
unsafe fn w16(a: usize, v: u16) {
    unsafe { write_volatile(a as *mut u16, v) }
}
unsafe fn w32(a: usize, v: u32) {
    unsafe { write_volatile(a as *mut u32, v) }
}
unsafe fn w64(a: usize, v: u64) {
    unsafe {
        w32(a, v as u32);
        w32(a + 4, (v >> 32) as u32);
    }
}

impl Transport {
    /// Locate the virtio capabilities and reset the device. Returns `None`
    /// for legacy-only devices.
    pub fn new(pci: &PciDevice) -> Option<Transport> {
        pci.enable();
        let mut common = None;
        let mut notify = None;
        let mut device_cfg = None;
        let mut notify_mult = 0;
        for cap in pci.capabilities(0x09) {
            let cfg_type = pci.read8(cap + 3);
            let bar = pci.read8(cap + 4);
            let offset = pci.read32(cap + 8) as u64;
            let length = pci.read32(cap + 12) as usize;
            let base = pci.bar(bar);
            if base == 0 {
                continue;
            }
            let virt = paging::map_mmio(base + offset, length.max(0x1000)) as usize;
            match cfg_type {
                1 => common = Some(virt),
                2 => {
                    notify = Some(virt);
                    notify_mult = pci.read32(cap + 16);
                }
                4 => device_cfg = Some(virt),
                _ => {}
            }
        }
        let t = Transport { common: common?, notify: notify?, notify_mult, device_cfg: device_cfg.unwrap_or(0) };
        unsafe {
            w8(t.common + C_STATUS, 0);
            while r8(t.common + C_STATUS) != 0 {
                core::hint::spin_loop();
            }
            w8(t.common + C_STATUS, STATUS_ACK);
            w8(t.common + C_STATUS, STATUS_ACK | STATUS_DRIVER);
        }
        Some(t)
    }

    /// Accept `wanted` features (VERSION_1 is always requested).
    pub fn negotiate(&self, wanted: u64) -> bool {
        unsafe {
            w32(self.common + C_DFSELECT, 0);
            let lo = r32(self.common + C_DF) as u64;
            w32(self.common + C_DFSELECT, 1);
            let hi = r32(self.common + C_DF) as u64;
            let offered = lo | hi << 32;
            let accept = offered & (wanted | F_VERSION_1);
            if accept & F_VERSION_1 == 0 {
                w8(self.common + C_STATUS, STATUS_FAILED);
                return false;
            }
            w32(self.common + C_GFSELECT, 0);
            w32(self.common + C_GF, accept as u32);
            w32(self.common + C_GFSELECT, 1);
            w32(self.common + C_GF, (accept >> 32) as u32);
            let s = r8(self.common + C_STATUS);
            w8(self.common + C_STATUS, s | STATUS_FEATURES_OK);
            if r8(self.common + C_STATUS) & STATUS_FEATURES_OK == 0 {
                w8(self.common + C_STATUS, STATUS_FAILED);
                return false;
            }
            w16(self.common + C_MSIX, 0xffff);
        }
        true
    }

    pub fn setup_queue(&self, index: u16, max_size: u16) -> Option<Virtqueue> {
        unsafe {
            w16(self.common + C_Q_SELECT, index);
            let dev_size = r16(self.common + C_Q_SIZE);
            if dev_size == 0 {
                return None;
            }
            let size = dev_size.min(max_size);
            w16(self.common + C_Q_SIZE, size);
            w16(self.common + C_Q_MSIX, 0xffff);
            let q = Virtqueue::new(size);
            w64(self.common + C_Q_DESC, q.mem.phys);
            w64(self.common + C_Q_DRIVER, q.mem.phys + q.avail_off as u64);
            w64(self.common + C_Q_DEVICE, q.mem.phys + q.used_off as u64);
            let notify_off = r16(self.common + C_Q_NOTIFY_OFF) as usize;
            w16(self.common + C_Q_ENABLE, 1);
            let mut q = q;
            q.notify_addr = self.notify + notify_off * self.notify_mult as usize;
            q.index = index;
            Some(q)
        }
    }

    pub fn driver_ok(&self) {
        unsafe {
            let s = r8(self.common + C_STATUS);
            w8(self.common + C_STATUS, s | STATUS_DRIVER_OK);
        }
    }

    pub fn cfg_read32(&self, off: usize) -> u32 {
        unsafe { r32(self.device_cfg + off) }
    }

    pub fn cfg_read8(&self, off: usize) -> u8 {
        unsafe { r8(self.device_cfg + off) }
    }

    pub fn cfg_write8(&self, off: usize, v: u8) {
        unsafe { w8(self.device_cfg + off, v) }
    }
}

/// One buffer in a descriptor chain.
pub struct Buf {
    pub phys: u64,
    pub len: u32,
    pub device_writes: bool,
}

pub struct Virtqueue {
    mem: DmaBuf,
    size: u16,
    avail_off: usize,
    used_off: usize,
    free: Vec<u16>,
    last_used: u16,
    notify_addr: usize,
    index: u16,
}

unsafe impl Send for Virtqueue {}

impl Virtqueue {
    fn new(size: u16) -> Virtqueue {
        let n = size as usize;
        let desc = 16 * n;
        let avail = 6 + 2 * n;
        let used_off = (desc + avail).div_ceil(4096) * 4096;
        let used = 6 + 8 * n;
        let mem = DmaBuf::new(used_off + used);
        Virtqueue {
            mem,
            size,
            avail_off: desc,
            used_off,
            free: (0..size).rev().collect(),
            last_used: 0,
            notify_addr: 0,
            index: 0,
        }
    }

    fn base(&self) -> usize {
        self.mem.virt() as usize
    }

    pub fn size(&self) -> u16 {
        self.size
    }

    pub fn free_descriptors(&self) -> usize {
        self.free.len()
    }

    /// Publish a descriptor chain. Returns the head index.
    pub fn submit(&mut self, bufs: &[Buf]) -> Option<u16> {
        if bufs.is_empty() || self.free.len() < bufs.len() {
            return None;
        }
        let ids: Vec<u16> = (0..bufs.len()).map(|_| self.free.pop().unwrap()).collect();
        let base = self.base();
        for (i, b) in bufs.iter().enumerate() {
            let d = base + 16 * ids[i] as usize;
            let mut flags = if b.device_writes { DESC_F_WRITE } else { 0 };
            let next = if i + 1 < bufs.len() {
                flags |= DESC_F_NEXT;
                ids[i + 1]
            } else {
                0
            };
            unsafe {
                w64(d, b.phys);
                w32(d + 8, b.len);
                w16(d + 12, flags);
                w16(d + 14, next);
            }
        }
        let head = ids[0];
        unsafe {
            let avail = base + self.avail_off;
            let idx = r16(avail + 2);
            w16(avail + 4 + 2 * (idx % self.size) as usize, head);
            fence(Ordering::SeqCst);
            w16(avail + 2, idx.wrapping_add(1));
            fence(Ordering::SeqCst);
        }
        Some(head)
    }

    pub fn notify(&self) {
        unsafe { w16(self.notify_addr, self.index) };
    }

    /// Pop one completed chain: (head, bytes written by device).
    pub fn pop_used(&mut self) -> Option<(u16, u32)> {
        let base = self.base();
        let used = base + self.used_off;
        fence(Ordering::SeqCst);
        let idx = unsafe { r16(used + 2) };
        if idx == self.last_used {
            return None;
        }
        let slot = used + 4 + 8 * (self.last_used % self.size) as usize;
        let (id, len) = unsafe { (r32(slot) as u16, r32(slot + 4)) };
        self.last_used = self.last_used.wrapping_add(1);
        // Return the chain's descriptors to the free list.
        let mut d = id;
        loop {
            self.free.push(d);
            let flags = unsafe { r16(base + 16 * d as usize + 12) };
            if flags & DESC_F_NEXT == 0 {
                break;
            }
            d = unsafe { r16(base + 16 * d as usize + 14) };
        }
        Some((id, len))
    }

    /// Submit a chain, notify, and spin until it completes.
    pub fn submit_and_wait(&mut self, bufs: &[Buf]) -> Option<u32> {
        let head = self.submit(bufs)?;
        self.notify();
        loop {
            if let Some((id, len)) = self.pop_used() {
                if id == head {
                    return Some(len);
                }
                continue;
            }
            core::hint::spin_loop();
        }
    }
}
