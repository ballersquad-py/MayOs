//! Intel I225/I226 2.5G Ethernet ("igc"), the Intel network chip on most
//! boards since 2020, and the I210/I211 gigabit chips ("igb") of many
//! gaming boards. Advanced descriptors, one queue pair, polled.
//! Register layout from Intel's datasheet (as in Linux igc).

use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};

use super::pci::PciDevice;
use crate::mem::{paging, DmaBuf};
use net::Mac;

pub const VENDOR: u16 = 0x8086;
/// I225/I226 ("igc") and the older I210/I211/I350/82576 ("igb"), which
/// share the queue registers and descriptors.
pub const DEVICE_IDS: &[u16] = &[0x10c9, 0x10e6, 0x10e7, 0x1521, 0x1522, 0x1533, 0x1536, 0x1537, 0x1538, 0x1539, 0x157b, 0x157c, 0x15f2, 0x15f3, 0x15f7, 0x15f8, 0x15fd, 0x0d9f, 0x125b, 0x125c, 0x125d, 0x125f, 0x3100, 0x3101, 0x5502, 0x5503];

const CTRL: usize = 0x0000;
const STATUS: usize = 0x0008;
const IMC: usize = 0x150c;
const EIMC: usize = 0x1528;
const RCTL: usize = 0x0100;
const TCTL: usize = 0x0400;
const RDBAL: usize = 0xc000;
const RDBAH: usize = 0xc004;
const RDLEN: usize = 0xc008;
const SRRCTL: usize = 0xc00c;
const RDH: usize = 0xc010;
const RDT: usize = 0xc018;
const RXDCTL: usize = 0xc028;
const TDBAL: usize = 0xe000;
const TDBAH: usize = 0xe004;
const TDLEN: usize = 0xe008;
const TDH: usize = 0xe010;
const TDT: usize = 0xe018;
const TXDCTL: usize = 0xe028;
const MTA: usize = 0x5200;
const RAL: usize = 0x5400;
const RAH: usize = 0x5404;

const N_RX: usize = 64;
const N_TX: usize = 32;
const BUF: usize = 2048;

pub struct Igc {
    mmio: usize,
    rx_ring: DmaBuf,
    tx_ring: DmaBuf,
    rx_bufs: DmaBuf,
    tx_bufs: DmaBuf,
    rx_cur: usize,
    tx_cur: usize,
    tx_free: [bool; N_TX],
    pub mac: Mac,
    pub model: &'static str,
}

fn wait(mut f: impl FnMut() -> bool, ms: u64) -> bool {
    let start = crate::time::uptime_ms();
    while !f() {
        if crate::time::uptime_ms() - start > ms {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

impl Igc {
    fn r(&self, reg: usize) -> u32 {
        unsafe { read_volatile((self.mmio + reg) as *const u32) }
    }
    fn w(&self, reg: usize, v: u32) {
        unsafe { write_volatile((self.mmio + reg) as *mut u32, v) }
    }

    /// PHY register access through MDIC (PHY address 1).
    fn mdic(&self, v: u32) -> Option<u32> {
        self.w(0x20, v);
        for _ in 0..2000 {
            let r = self.r(0x20);
            if r & (1 << 28) != 0 {
                return if r & (1 << 30) != 0 { None } else { Some(r) };
            }
            for _ in 0..100 {
                core::hint::spin_loop();
            }
        }
        None
    }

    fn phy_read(&self, reg: u32) -> Option<u16> {
        self.mdic(reg << 16 | 1 << 21 | 2 << 26).map(|r| r as u16)
    }

    fn phy_write(&self, reg: u32, v: u16) {
        let _ = self.mdic(v as u32 | reg << 16 | 1 << 21 | 1 << 26);
    }

    fn rx_desc(&self, i: usize) -> usize {
        self.rx_ring.virt() as usize + i * 16
    }

    fn arm_rx(&self, i: usize) {
        let d = self.rx_desc(i);
        unsafe {
            write_volatile(d as *mut u64, self.rx_bufs.phys + (i * BUF) as u64);
            write_volatile((d + 8) as *mut u64, 0);
        }
    }

    pub fn new(pci: &PciDevice) -> Option<Igc> {
        pci.enable();
        let bar = pci.bar(0);
        if bar == 0 {
            return None;
        }
        let mmio = paging::map_mmio(bar, 128 * 1024) as usize;
        let igb = matches!(pci.device, 0x10c9 | 0x10e6 | 0x10e7 | 0x1521 | 0x1522 | 0x1533 | 0x1536..=0x1539 | 0x157b | 0x157c);
        let model = match pci.device {
            0x125b..=0x125f | 0x3100 | 0x3101 => "Intel I226 2.5G Ethernet",
            0x1539 => "Intel I211 Gigabit Ethernet",
            0x1533 | 0x1536..=0x1538 | 0x157b | 0x157c => "Intel I210 Gigabit Ethernet",
            0x1521 | 0x1522 => "Intel I350 Gigabit Ethernet",
            _ if igb => "Intel 82576 Gigabit Ethernet",
            _ => "Intel I225 2.5G Ethernet",
        };
        // Reset bit: DEV_RST on igc, RST on igb.
        let rst = if igb { 1u32 << 26 } else { 1 << 29 };
        let mut nic = Igc {
            mmio,
            rx_ring: DmaBuf::try_new(N_RX * 16)?,
            tx_ring: DmaBuf::try_new(N_TX * 16)?,
            rx_bufs: DmaBuf::try_new(N_RX * BUF)?,
            tx_bufs: DmaBuf::try_new(N_TX * BUF)?,
            rx_cur: 0,
            tx_cur: 0,
            tx_free: [true; N_TX],
            mac: Mac::ZERO,
            model,
        };
        // Device reset (the MAC address and PHY setup reload from flash).
        nic.w(IMC, 0xffff_ffff);
        nic.w(EIMC, 0xffff_ffff);
        nic.w(RCTL, 0);
        nic.w(TCTL, 0);
        nic.w(CTRL, nic.r(CTRL) | rst);
        crate::proc::sched::sleep_ms(20);
        if !wait(|| nic.r(CTRL) & rst == 0, 500) {
            return None;
        }
        crate::proc::sched::sleep_ms(10);
        nic.w(IMC, 0xffff_ffff);
        nic.w(EIMC, 0xffff_ffff);
        if igb {
            // "PF reset done": the chip starts moving packets.
            nic.w(0x18, nic.r(0x18) | (1 << 14));
        }
        // Link up, and (re)start the PHY's autonegotiation.
        nic.w(CTRL, (nic.r(CTRL) | (1 << 6)) & !(1 << 3));
        if let Some(bmcr) = nic.phy_read(0) {
            nic.phy_write(0, (bmcr | (1 << 12) | (1 << 9)) & !(1 << 11));
        }

        let (lo, hi) = (nic.r(RAL), nic.r(RAH));
        let mut mac = [lo as u8, (lo >> 8) as u8, (lo >> 16) as u8, (lo >> 24) as u8, hi as u8, (hi >> 8) as u8];
        if mac == [0; 6] {
            mac = [0x02, 0x4d, 0x61, 0x79, (pci.device >> 8) as u8, pci.device as u8];
        }
        nic.w(RAL, u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]));
        nic.w(RAH, u16::from_le_bytes([mac[4], mac[5]]) as u32 | (1 << 31));
        nic.mac = Mac(mac);
        for i in 0..128 {
            nic.w(MTA + i * 4, 0);
        }

        // Receive: one 2 KiB buffer per advanced descriptor.
        for i in 0..N_RX {
            nic.arm_rx(i);
        }
        nic.w(RDBAL, nic.rx_ring.phys as u32);
        nic.w(RDBAH, (nic.rx_ring.phys >> 32) as u32);
        nic.w(RDLEN, (N_RX * 16) as u32);
        nic.w(SRRCTL, 2 | (1 << 25));
        nic.w(RDH, 0);
        nic.w(RDT, 0);
        nic.w(RXDCTL, nic.r(RXDCTL) | (1 << 25));
        wait(|| nic.r(RXDCTL) & (1 << 25) != 0, 100);
        nic.w(RDT, (N_RX - 1) as u32);
        // Enable, broadcast, strip CRC.
        nic.w(RCTL, (1 << 1) | (1 << 15) | (1 << 26));

        nic.w(TDBAL, nic.tx_ring.phys as u32);
        nic.w(TDBAH, (nic.tx_ring.phys >> 32) as u32);
        nic.w(TDLEN, (N_TX * 16) as u32);
        nic.w(TDH, 0);
        nic.w(TDT, 0);
        nic.w(TXDCTL, nic.r(TXDCTL) | (1 << 25));
        wait(|| nic.r(TXDCTL) & (1 << 25) != 0, 100);
        // Enable, pad short packets, retransmit on late collision.
        nic.w(TCTL, (1 << 1) | (1 << 3) | (0x0f << 4) | (1 << 24));
        Some(nic)
    }

    pub fn link_up(&self) -> bool {
        self.r(STATUS) & 2 != 0
    }

    pub fn speed_mbps(&self) -> u32 {
        let s = self.r(STATUS);
        if s & (1 << 22) != 0 {
            2500
        } else {
            match (s >> 6) & 3 {
                0 => 10,
                1 => 100,
                _ => 1000,
            }
        }
    }

    pub fn send(&mut self, frame: &[u8]) -> bool {
        if frame.len() > BUF {
            return false;
        }
        let i = self.tx_cur;
        let d = self.tx_ring.virt() as usize + i * 16;
        if !self.tx_free[i] {
            // Wait (briefly) for the earlier frame in this slot to go out.
            let mut spins = 0;
            while unsafe { read_volatile((d + 12) as *const u32) } & 1 == 0 {
                spins += 1;
                if spins > 1_000_000 {
                    return false;
                }
                core::hint::spin_loop();
            }
        }
        let buf = self.tx_bufs.virt() as usize + i * BUF;
        unsafe {
            core::ptr::copy_nonoverlapping(frame.as_ptr(), buf as *mut u8, frame.len());
            write_volatile(d as *mut u64, self.tx_bufs.phys + (i * BUF) as u64);
            // Data descriptor: EOP, insert CRC, report status, extended.
            let cmd = frame.len() as u32 | (3 << 20) | (1 << 24) | (1 << 25) | (1 << 27) | (1 << 29);
            write_volatile((d + 8) as *mut u32, cmd);
            write_volatile((d + 12) as *mut u32, (frame.len() as u32) << 14);
        }
        self.tx_free[i] = false;
        self.tx_cur = (i + 1) % N_TX;
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        self.w(TDT, self.tx_cur as u32);
        true
    }

    pub fn recv(&mut self) -> Option<Vec<u8>> {
        let d = self.rx_desc(self.rx_cur);
        let status = unsafe { read_volatile((d + 8) as *const u32) };
        if status & 1 == 0 {
            return None;
        }
        let len = unsafe { read_volatile((d + 12) as *const u16) } as usize;
        let frame = if status & 2 != 0 && len >= 14 {
            let mut v = alloc::vec![0u8; len.min(BUF)];
            let buf = self.rx_bufs.virt() as usize + self.rx_cur * BUF;
            unsafe { core::ptr::copy_nonoverlapping(buf as *const u8, v.as_mut_ptr(), v.len()) };
            v
        } else {
            Vec::new()
        };
        self.arm_rx(self.rx_cur);
        let old = self.rx_cur;
        self.rx_cur = (self.rx_cur + 1) % N_RX;
        self.w(RDT, old as u32);
        Some(frame)
    }
}
