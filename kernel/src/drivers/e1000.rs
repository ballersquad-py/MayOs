//! Intel 8254x / 82574 ("e1000") gigabit Ethernet driver.
//!
//! This is the default NIC in QEMU (e1000/e1000e) and one of VirtualBox's
//! choices ("Intel PRO/1000 MT Desktop"). Legacy descriptors, polled.

use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};

use super::pci::PciDevice;
use crate::mem::{paging, DmaBuf};
use net::Mac;

pub const DEVICE_IDS: &[u16] = &[0x100e, 0x100f, 0x1004, 0x100c, 0x10d3, 0x10ea, 0x1502, 0x153a];

const CTRL: usize = 0x0000;
const STATUS: usize = 0x0008;
const EERD: usize = 0x0014;
const IMC: usize = 0x00d8;
const RCTL: usize = 0x0100;
const TCTL: usize = 0x0400;
const TIPG: usize = 0x0410;
const RDBAL: usize = 0x2800;
const RDBAH: usize = 0x2804;
const RDLEN: usize = 0x2808;
const RDH: usize = 0x2810;
const RDT: usize = 0x2818;
const TDBAL: usize = 0x3800;
const TDBAH: usize = 0x3804;
const TDLEN: usize = 0x3808;
const TDH: usize = 0x3810;
const TDT: usize = 0x3818;
const MTA: usize = 0x5200;
const RAL: usize = 0x5400;
const RAH: usize = 0x5404;

const N_RX: usize = 64;
const N_TX: usize = 32;
const BUF: usize = 2048;

pub struct E1000 {
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

impl E1000 {
    fn r(&self, reg: usize) -> u32 {
        unsafe { read_volatile((self.mmio + reg) as *const u32) }
    }
    fn w(&self, reg: usize, v: u32) {
        unsafe { write_volatile((self.mmio + reg) as *mut u32, v) }
    }

    fn eeprom_read(&self, addr: u8) -> Option<u16> {
        self.w(EERD, 1 | (addr as u32) << 8);
        for _ in 0..100_000 {
            let v = self.r(EERD);
            if v & (1 << 4) != 0 {
                return Some((v >> 16) as u16);
            }
        }
        None
    }

    pub fn new(pci: &PciDevice) -> Option<E1000> {
        pci.enable();
        let bar = pci.bar(0);
        if bar == 0 {
            return None;
        }
        let mmio = paging::map_mmio(bar, 128 * 1024) as usize;
        let model = match pci.device {
            0x10d3 | 0x10ea | 0x1502 | 0x153a => "Intel 82574L-class Gigabit Ethernet",
            _ => "Intel PRO/1000 (8254x) Gigabit Ethernet",
        };
        let mut nic = E1000 {
            mmio,
            rx_ring: DmaBuf::new(N_RX * 16),
            tx_ring: DmaBuf::new(N_TX * 16),
            rx_bufs: DmaBuf::new(N_RX * BUF),
            tx_bufs: DmaBuf::new(N_TX * BUF),
            rx_cur: 0,
            tx_cur: 0,
            mac: Mac::ZERO,
            model,
        };
        // Reset and mask all interrupts (we poll).
        nic.w(IMC, 0xffff_ffff);
        nic.w(CTRL, nic.r(CTRL) | (1 << 26));
        for _ in 0..100_000 {
            if nic.r(CTRL) & (1 << 26) == 0 {
                break;
            }
        }
        nic.w(IMC, 0xffff_ffff);
        // Set link up, auto speed detection.
        nic.w(CTRL, (nic.r(CTRL) | (1 << 6) | (1 << 5)) & !(1 << 3) & !(1 << 31) & !(1 << 7));

        // MAC address: receive-address registers, else EEPROM.
        let lo = nic.r(RAL);
        let hi = nic.r(RAH);
        let mut mac = [lo as u8, (lo >> 8) as u8, (lo >> 16) as u8, (lo >> 24) as u8, hi as u8, (hi >> 8) as u8];
        if mac == [0; 6] {
            for i in 0..3 {
                let v = nic.eeprom_read(i)?;
                mac[i as usize * 2] = v as u8;
                mac[i as usize * 2 + 1] = (v >> 8) as u8;
            }
            nic.w(RAL, u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]));
            nic.w(RAH, u16::from_le_bytes([mac[4], mac[5]]) as u32 | (1 << 31));
        }
        nic.mac = Mac(mac);
        for i in 0..128 {
            nic.w(MTA + i * 4, 0);
        }

        // Receive ring.
        for i in 0..N_RX {
            let d = nic.rx_ring.virt() as usize + i * 16;
            unsafe {
                write_volatile(d as *mut u64, nic.rx_bufs.phys + (i * BUF) as u64);
                write_volatile((d + 8) as *mut u64, 0);
            }
        }
        nic.w(RDBAL, nic.rx_ring.phys as u32);
        nic.w(RDBAH, (nic.rx_ring.phys >> 32) as u32);
        nic.w(RDLEN, (N_RX * 16) as u32);
        nic.w(RDH, 0);
        nic.w(RDT, (N_RX - 1) as u32);
        // Enable, accept broadcast, strip CRC, 2 KiB buffers.
        nic.w(RCTL, (1 << 1) | (1 << 15) | (1 << 26));

        // Transmit ring.
        for i in 0..N_TX {
            let d = nic.tx_ring.virt() as usize + i * 16;
            unsafe {
                write_volatile(d as *mut u64, nic.tx_bufs.phys + (i * BUF) as u64);
                write_volatile((d + 8) as *mut u64, 0);
                // Mark as done so the slot counts as free.
                write_volatile((d + 12) as *mut u8, 1);
            }
        }
        nic.w(TDBAL, nic.tx_ring.phys as u32);
        nic.w(TDBAH, (nic.tx_ring.phys >> 32) as u32);
        nic.w(TDLEN, (N_TX * 16) as u32);
        nic.w(TDH, 0);
        nic.w(TDT, 0);
        nic.w(TCTL, (1 << 1) | (1 << 3) | (0x0f << 4) | (0x40 << 12));
        nic.w(TIPG, 0x0060_200a);
        Some(nic)
    }

    pub fn link_up(&self) -> bool {
        self.r(STATUS) & 2 != 0
    }

    pub fn speed_mbps(&self) -> u32 {
        match (self.r(STATUS) >> 6) & 3 {
            0 => 10,
            1 => 100,
            _ => 1000,
        }
    }

    pub fn send(&mut self, frame: &[u8]) -> bool {
        if frame.len() > BUF {
            return false;
        }
        let d = self.tx_ring.virt() as usize + self.tx_cur * 16;
        // Wait (briefly) for the slot to be free.
        let mut spins = 0;
        while unsafe { read_volatile((d + 12) as *const u8) } & 1 == 0 {
            spins += 1;
            if spins > 1_000_000 {
                return false;
            }
            core::hint::spin_loop();
        }
        let buf = self.tx_bufs.virt() as usize + self.tx_cur * BUF;
        unsafe {
            core::ptr::copy_nonoverlapping(frame.as_ptr(), buf as *mut u8, frame.len());
            write_volatile((d + 8) as *mut u16, frame.len() as u16);
            write_volatile((d + 10) as *mut u8, 0);
            // EOP | IFCS | RS
            write_volatile((d + 11) as *mut u8, 1 | 2 | 8);
            write_volatile((d + 12) as *mut u8, 0);
        }
        self.tx_cur = (self.tx_cur + 1) % N_TX;
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        self.w(TDT, self.tx_cur as u32);
        true
    }

    pub fn recv(&mut self) -> Option<Vec<u8>> {
        let d = self.rx_ring.virt() as usize + self.rx_cur * 16;
        let status = unsafe { read_volatile((d + 12) as *const u8) };
        if status & 1 == 0 {
            return None;
        }
        let len = unsafe { read_volatile((d + 8) as *const u16) } as usize;
        let errors = unsafe { read_volatile((d + 13) as *const u8) };
        let buf = self.rx_bufs.virt() as usize + self.rx_cur * BUF;
        let frame = if status & 2 != 0 && errors == 0 {
            let mut v = alloc::vec![0u8; len.min(BUF)];
            unsafe { core::ptr::copy_nonoverlapping(buf as *const u8, v.as_mut_ptr(), v.len()) };
            Some(v)
        } else {
            Some(Vec::new()) // errored or partial frame: drop it but keep going
        };
        unsafe { write_volatile((d + 12) as *mut u8, 0) };
        let old = self.rx_cur;
        self.rx_cur = (self.rx_cur + 1) % N_RX;
        self.w(RDT, old as u32);
        frame
    }
}
