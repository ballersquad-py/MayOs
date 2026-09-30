//! Realtek RTL8111/8168/8169 (and 8101/8136 fast Ethernet) driver: the
//! wired network chip on most desktop motherboards. C+ descriptor rings,
//! polled. Register layout from Realtek's datasheets (as in Linux r8169
//! and the BSD re(4) drivers).

use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};

use super::pci::PciDevice;
use crate::mem::{paging, DmaBuf};
use net::Mac;

pub const VENDOR: u16 = 0x10ec;
pub const DEVICE_IDS: &[u16] = &[0x8168, 0x8161, 0x8169, 0x8136];

const MAR0: usize = 0x08;
const TNPDS: usize = 0x20;
const CR: usize = 0x37;
const TPPOLL: usize = 0x38;
const IMR: usize = 0x3c;
const ISR: usize = 0x3e;
const TCR: usize = 0x40;
const RCR: usize = 0x44;
const CR9346: usize = 0x50;
const PHYSTATUS: usize = 0x6c;
const RMS: usize = 0xda;
const RDSAR: usize = 0xe4;
const MTPS: usize = 0xec;

const OWN: u32 = 1 << 31;
const EOR: u32 = 1 << 30;
const FS: u32 = 1 << 29;
const LS: u32 = 1 << 28;

const N_RX: usize = 64;
const N_TX: usize = 32;
const BUF: usize = 2048;

pub struct Rtl8169 {
    mmio: usize,
    rx_ring: DmaBuf,
    tx_ring: DmaBuf,
    rx_bufs: DmaBuf,
    tx_bufs: DmaBuf,
    rx_cur: usize,
    tx_cur: usize,
    pub mac: Mac,
    pub model: &'static str,
}

impl Rtl8169 {
    fn r8(&self, reg: usize) -> u8 {
        unsafe { read_volatile((self.mmio + reg) as *const u8) }
    }
    fn w8(&self, reg: usize, v: u8) {
        unsafe { write_volatile((self.mmio + reg) as *mut u8, v) }
    }
    fn w16(&self, reg: usize, v: u16) {
        unsafe { write_volatile((self.mmio + reg) as *mut u16, v) }
    }
    fn r32(&self, reg: usize) -> u32 {
        unsafe { read_volatile((self.mmio + reg) as *const u32) }
    }
    fn w32(&self, reg: usize, v: u32) {
        unsafe { write_volatile((self.mmio + reg) as *mut u32, v) }
    }

    fn desc(ring: &DmaBuf, i: usize) -> usize {
        ring.virt() as usize + i * 16
    }

    pub fn new(pci: &PciDevice) -> Option<Rtl8169> {
        pci.enable();
        // The registers are in the first memory BAR (BAR 2 on 8168, BAR 1 on 8169).
        let bar = [2u8, 1, 4].iter().map(|&i| (i, pci.read32(0x10 + i * 4))).find(|&(_, v)| v != 0 && v & 1 == 0).map(|(i, _)| pci.bar(i))?;
        let mmio = paging::map_mmio(bar, 0x1000) as usize;
        let model = match pci.device {
            0x8136 => "Realtek RTL8101E/8102E Fast Ethernet",
            0x8169 => "Realtek RTL8169 Gigabit Ethernet",
            _ => "Realtek RTL8111/8168 Gigabit Ethernet",
        };
        let mut nic = Rtl8169 {
            mmio,
            rx_ring: DmaBuf::try_new(N_RX * 16)?,
            tx_ring: DmaBuf::try_new(N_TX * 16)?,
            rx_bufs: DmaBuf::try_new(N_RX * BUF)?,
            tx_bufs: DmaBuf::try_new(N_TX * BUF)?,
            rx_cur: 0,
            tx_cur: 0,
            mac: Mac::ZERO,
            model,
        };
        // Reset.
        nic.w8(CR, 0x10);
        let mut ok = false;
        for _ in 0..1_000_000 {
            if nic.r8(CR) & 0x10 == 0 {
                ok = true;
                break;
            }
            core::hint::spin_loop();
        }
        if !ok {
            return None;
        }
        let mut mac = [0u8; 6];
        for (i, b) in mac.iter_mut().enumerate() {
            *b = nic.r8(i);
        }
        nic.mac = Mac(mac);

        for i in 0..N_RX {
            let d = Self::desc(&nic.rx_ring, i);
            let eor = if i == N_RX - 1 { EOR } else { 0 };
            unsafe {
                write_volatile((d + 4) as *mut u32, 0);
                write_volatile((d + 8) as *mut u64, nic.rx_bufs.phys + (i * BUF) as u64);
                write_volatile(d as *mut u32, OWN | eor | BUF as u32);
            }
        }
        for i in 0..N_TX {
            let d = Self::desc(&nic.tx_ring, i);
            unsafe {
                write_volatile(d as *mut u32, if i == N_TX - 1 { EOR } else { 0 });
                write_volatile((d + 8) as *mut u64, nic.tx_bufs.phys + (i * BUF) as u64);
            }
        }

        nic.w8(CR9346, 0xc0); // unlock config registers
        nic.w16(IMR, 0); // polled
        nic.w16(ISR, 0xffff);
        nic.w16(RMS, BUF as u16);
        nic.w8(MTPS, 0x3b);
        nic.w32(TNPDS, nic.tx_ring.phys as u32);
        nic.w32(TNPDS + 4, (nic.tx_ring.phys >> 32) as u32);
        nic.w32(RDSAR, nic.rx_ring.phys as u32);
        nic.w32(RDSAR + 4, (nic.rx_ring.phys >> 32) as u32);
        nic.w8(CR, 0x0c); // RX and TX on
        // Unlimited DMA bursts, normal inter-frame gap.
        nic.w32(TCR, (nic.r32(TCR) & !0x0700) | 0x0300_0700);
        // Our MAC, broadcast and multicast; no RX threshold; unlimited DMA.
        nic.w32(RCR, (nic.r32(RCR) & !0xffff) | 0xe70e);
        nic.w32(MAR0, 0xffff_ffff);
        nic.w32(MAR0 + 4, 0xffff_ffff);
        nic.w8(CR9346, 0x00);
        Some(nic)
    }

    pub fn link_up(&self) -> bool {
        self.r8(PHYSTATUS) & 0x02 != 0
    }

    pub fn speed_mbps(&self) -> u32 {
        let s = self.r8(PHYSTATUS);
        if s & 0x10 != 0 {
            1000
        } else if s & 0x08 != 0 {
            100
        } else {
            10
        }
    }

    pub fn send(&mut self, frame: &[u8]) -> bool {
        if frame.len() > BUF {
            return false;
        }
        let d = Self::desc(&self.tx_ring, self.tx_cur);
        let mut spins = 0;
        while unsafe { read_volatile(d as *const u32) } & OWN != 0 {
            spins += 1;
            if spins > 1_000_000 {
                return false;
            }
            core::hint::spin_loop();
        }
        let buf = self.tx_bufs.virt() as usize + self.tx_cur * BUF;
        // Frames shorter than 60 bytes are padded by hand.
        let len = frame.len().max(60);
        unsafe {
            core::ptr::write_bytes(buf as *mut u8, 0, len);
            core::ptr::copy_nonoverlapping(frame.as_ptr(), buf as *mut u8, frame.len());
            write_volatile((d + 4) as *mut u32, 0);
            let eor = if self.tx_cur == N_TX - 1 { EOR } else { 0 };
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            write_volatile(d as *mut u32, OWN | eor | FS | LS | len as u32);
        }
        self.tx_cur = (self.tx_cur + 1) % N_TX;
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        self.w8(TPPOLL, 0x40);
        true
    }

    pub fn recv(&mut self) -> Option<Vec<u8>> {
        let d = Self::desc(&self.rx_ring, self.rx_cur);
        let opts = unsafe { read_volatile(d as *const u32) };
        if opts & OWN != 0 {
            return None;
        }
        let whole = opts & (FS | LS) == FS | LS;
        let error = opts & (1 << 21) != 0;
        let len = (opts & 0x3fff) as usize;
        let frame = if whole && !error && len > 4 {
            let n = (len - 4).min(BUF); // without the CRC
            let buf = self.rx_bufs.virt() as usize + self.rx_cur * BUF;
            let mut v = alloc::vec![0u8; n];
            unsafe { core::ptr::copy_nonoverlapping(buf as *const u8, v.as_mut_ptr(), n) };
            v
        } else {
            Vec::new()
        };
        let eor = if self.rx_cur == N_RX - 1 { EOR } else { 0 };
        unsafe {
            write_volatile((d + 4) as *mut u32, 0);
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            write_volatile(d as *mut u32, OWN | eor | BUF as u32);
        }
        self.rx_cur = (self.rx_cur + 1) % N_RX;
        Some(frame)
    }
}
