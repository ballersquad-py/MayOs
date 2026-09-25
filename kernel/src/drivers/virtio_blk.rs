//! virtio block device driver.

use super::pci::PciDevice;
use super::virtio::{Buf, Transport, Virtqueue};
use crate::mem::DmaBuf;

const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const F_FLUSH: u64 = 1 << 9;
const MAX_SECTORS: usize = 128; // 64 KiB per request

pub struct VirtioBlk {
    _t: Transport,
    q: Virtqueue,
    hdr: DmaBuf,
    bounce: DmaBuf,
    pub capacity: u64,
    can_flush: bool,
}

impl VirtioBlk {
    pub fn new(pci: &PciDevice) -> Option<VirtioBlk> {
        let t = Transport::new(pci)?;
        if !t.negotiate(F_FLUSH) {
            return None;
        }
        let q = t.setup_queue(0, 64)?;
        t.driver_ok();
        let capacity = t.cfg_read32(0) as u64 | (t.cfg_read32(4) as u64) << 32;
        Some(VirtioBlk {
            _t: t,
            q,
            hdr: DmaBuf::new(4096),
            bounce: DmaBuf::new(MAX_SECTORS * 512),
            capacity,
            can_flush: true,
        })
    }

    fn request(&mut self, kind: u32, sector: u64, len: usize) -> bool {
        let h = self.hdr.virt() as *mut u8;
        unsafe {
            core::ptr::write_volatile(h as *mut u32, kind);
            core::ptr::write_volatile(h.add(4) as *mut u32, 0);
            core::ptr::write_volatile(h.add(8) as *mut u64, sector);
            core::ptr::write_volatile(h.add(16), 0xff);
        }
        let hdr = Buf { phys: self.hdr.phys, len: 16, device_writes: false };
        let status = Buf { phys: self.hdr.phys + 16, len: 1, device_writes: true };
        let ok = if len > 0 {
            let data = Buf { phys: self.bounce.phys, len: len as u32, device_writes: kind == T_IN };
            self.q.submit_and_wait(&[hdr, data, status]).is_some()
        } else {
            self.q.submit_and_wait(&[hdr, status]).is_some()
        };
        ok && unsafe { core::ptr::read_volatile(h.add(16)) } == 0
    }
}

impl fat32::BlockDevice for VirtioBlk {
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), ()> {
        let mut done = 0;
        while done < buf.len() {
            let n = (buf.len() - done).min(MAX_SECTORS * 512);
            let sector = lba + (done / 512) as u64;
            if sector + (n / 512) as u64 > self.capacity || !self.request(T_IN, sector, n) {
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
            let n = (buf.len() - done).min(MAX_SECTORS * 512);
            let sector = lba + (done / 512) as u64;
            if sector + (n / 512) as u64 > self.capacity {
                return Err(());
            }
            self.bounce.as_mut_slice()[..n].copy_from_slice(&buf[done..done + n]);
            if !self.request(T_OUT, sector, n) {
                return Err(());
            }
            done += n;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), ()> {
        if self.can_flush && !self.request(T_FLUSH, 0, 0) {
            // Device does not support flush; stop asking.
            self.can_flush = false;
        }
        Ok(())
    }
}
