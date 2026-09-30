//! NVMe SSD driver (NVM Express 1.x): the disks in most PCs from the last
//! years. Polled, one admin queue and one I/O queue pair, one command at a
//! time through a contiguous bounce buffer (PRP list for the pages after
//! the first).

use alloc::string::String;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};

use super::pci::PciDevice;
use crate::mem::{paging, DmaBuf};
use crate::time::uptime_ms;

const CAP: usize = 0x00;
const CC: usize = 0x14;
const CSTS: usize = 0x1c;
const AQA: usize = 0x24;
const ASQ: usize = 0x28;
const ACQ: usize = 0x30;

const QUEUE_LEN: u16 = 16;
const BOUNCE: usize = 64 * 1024;

const ADMIN_CREATE_SQ: u8 = 0x01;
const ADMIN_CREATE_CQ: u8 = 0x05;
const ADMIN_IDENTIFY: u8 = 0x06;
const IO_FLUSH: u8 = 0x00;
const IO_WRITE: u8 = 0x01;
const IO_READ: u8 = 0x02;

struct Queue {
    sq: DmaBuf,
    cq: DmaBuf,
    id: u16,
    tail: u16,
    head: u16,
    phase: bool,
}

pub struct NvmeDisk {
    regs: usize,
    stride: usize,
    admin: Queue,
    io: Queue,
    bounce: DmaBuf,
    prp_list: DmaBuf,
    nsid: u32,
    /// Logical block size in bytes (512 or 4096).
    block: usize,
    /// Size in 512-byte sectors (what the file system uses).
    pub sectors: u64,
    pub model: String,
    pub name: String,
    cid: u16,
}

unsafe impl Send for NvmeDisk {}

fn r32(a: usize) -> u32 {
    unsafe { read_volatile(a as *const u32) }
}
fn w32(a: usize, v: u32) {
    unsafe { write_volatile(a as *mut u32, v) }
}
fn r64(a: usize) -> u64 {
    r32(a) as u64 | (r32(a + 4) as u64) << 32
}
fn w64(a: usize, v: u64) {
    w32(a, v as u32);
    w32(a + 4, (v >> 32) as u32);
}

impl Queue {
    fn new(id: u16) -> Option<Queue> {
        Some(Queue {
            sq: DmaBuf::try_new(QUEUE_LEN as usize * 64)?,
            cq: DmaBuf::try_new(QUEUE_LEN as usize * 16)?,
            id,
            tail: 0,
            head: 0,
            phase: true,
        })
    }
}

/// All NVMe namespaces (usually one per SSD) on a controller.
pub fn probe(pci: &PciDevice, index: usize) -> Vec<NvmeDisk> {
    let mut out = Vec::new();
    pci.enable();
    // Legacy INTx off: we poll.
    pci.write32(0x04, (pci.read32(0x04) & 0xffff) | 0x6 | (1 << 10));
    let bar = pci.bar(0);
    if bar == 0 {
        return out;
    }
    let regs = paging::map_mmio(bar, 0x4000) as usize;
    match NvmeDisk::new(regs, index) {
        Some(d) => out.push(d),
        None => crate::kprintln!("nvme{}: controller did not start", index),
    }
    out
}

impl NvmeDisk {
    fn new(regs: usize, index: usize) -> Option<NvmeDisk> {
        let cap = r64(regs + CAP);
        let stride = 4usize << ((cap >> 32) & 0xf);
        let timeout = ((cap >> 24) & 0xff).max(1) * 500 + 1000;
        // Reset: disable and wait for not ready.
        w32(regs + CC, r32(regs + CC) & !1);
        if !wait(|| r32(regs + CSTS) & 1 == 0, timeout) {
            return None;
        }
        let admin = Queue::new(0)?;
        w32(regs + AQA, (QUEUE_LEN as u32 - 1) << 16 | (QUEUE_LEN as u32 - 1));
        w64(regs + ASQ, admin.sq.phys);
        w64(regs + ACQ, admin.cq.phys);
        // NVM command set, 4 KiB pages, 64-byte SQ entries, 16-byte CQ entries.
        w32(regs + CC, (4 << 20) | (6 << 16) | 1);
        if !wait(|| r32(regs + CSTS) & 3 == 1, timeout) {
            return None;
        }
        let mut d = NvmeDisk {
            regs,
            stride,
            admin,
            io: Queue::new(1)?,
            bounce: DmaBuf::try_new(BOUNCE)?,
            prp_list: DmaBuf::try_new(4096)?,
            nsid: 1,
            block: 512,
            sectors: 0,
            model: String::new(),
            name: alloc::format!("nvme{}", index),
            cid: 0,
        };
        // Identify controller: model name.
        let buf = d.bounce.phys;
        if !d.admin_cmd(ADMIN_IDENTIFY, 0, buf, [1, 0, 0, 0, 0, 0]) {
            return None;
        }
        d.model = String::from_utf8_lossy(&d.bounce.as_slice()[24..64]).trim().into();
        // First active namespace.
        if !d.admin_cmd(ADMIN_IDENTIFY, 0, buf, [2, 0, 0, 0, 0, 0]) {
            return None;
        }
        let nsid = u32::from_le_bytes(d.bounce.as_slice()[0..4].try_into().unwrap());
        d.nsid = if nsid == 0 { 1 } else { nsid };
        if !d.admin_cmd(ADMIN_IDENTIFY, d.nsid, buf, [0, 0, 0, 0, 0, 0]) {
            return None;
        }
        let ns = d.bounce.as_slice();
        let nsze = u64::from_le_bytes(ns[0..8].try_into().unwrap());
        let flbas = (ns[26] & 0xf) as usize;
        let lbaf = u32::from_le_bytes(ns[128 + flbas * 4..132 + flbas * 4].try_into().unwrap());
        let lbads = (lbaf >> 16) & 0xff;
        if !(9..=12).contains(&lbads) {
            return None;
        }
        d.block = 1 << lbads;
        d.sectors = nsze * (d.block as u64 / 512);
        // I/O queues: completion first, then submission (physically contiguous).
        let (cq, sq) = (d.io.cq.phys, d.io.sq.phys);
        let q = (QUEUE_LEN as u32 - 1) << 16 | 1;
        if !d.admin_cmd(ADMIN_CREATE_CQ, 0, cq, [q, 1, 0, 0, 0, 0]) {
            return None;
        }
        if !d.admin_cmd(ADMIN_CREATE_SQ, 0, sq, [q, 1 << 16 | 1, 0, 0, 0, 0]) {
            return None;
        }
        Some(d)
    }

    fn admin_cmd(&mut self, op: u8, nsid: u32, prp1: u64, cdw: [u32; 6]) -> bool {
        let regs = self.regs;
        let stride = self.stride;
        self.cid = self.cid.wrapping_add(1);
        submit(regs, stride, &mut self.admin, op, self.cid, nsid, prp1, 0, cdw)
    }

    /// One read or write of `blocks` logical blocks through the bounce buffer.
    fn io_cmd(&mut self, op: u8, lba: u64, blocks: usize) -> bool {
        let bytes = blocks * self.block;
        let first = self.bounce.phys;
        let prp2 = if bytes <= 4096 {
            0
        } else if bytes <= 8192 {
            first + 4096
        } else {
            let list = self.prp_list.as_mut_slice();
            for i in 1..bytes.div_ceil(4096) {
                list[(i - 1) * 8..i * 8].copy_from_slice(&(first + i as u64 * 4096).to_le_bytes());
            }
            self.prp_list.phys
        };
        let cdw = if op == IO_FLUSH { [0; 6] } else { [lba as u32, (lba >> 32) as u32, (blocks as u32).saturating_sub(1), 0, 0, 0] };
        let (regs, stride, nsid) = (self.regs, self.stride, self.nsid);
        self.cid = self.cid.wrapping_add(1);
        submit(regs, stride, &mut self.io, op, self.cid, nsid, if op == IO_FLUSH { 0 } else { first }, prp2, cdw)
    }

    /// Transfer 512-byte sectors, splitting into bounce-sized pieces.
    fn transfer(&mut self, lba: u64, len: usize, mut each: impl FnMut(&mut Self, usize, usize) -> bool) -> Result<(), ()> {
        let per = self.block / 512;
        if lba as usize % per != 0 || len % self.block != 0 || lba + (len / 512) as u64 > self.sectors {
            return Err(());
        }
        let mut done = 0;
        while done < len {
            let n = (len - done).min(BOUNCE);
            if !each(self, done, n) {
                return Err(());
            }
            done += n;
        }
        Ok(())
    }
}

fn wait(mut f: impl FnMut() -> bool, ms: u64) -> bool {
    let start = uptime_ms();
    let mut b = crate::time::Backoff::new();
    while !f() {
        if uptime_ms() - start > ms {
            return false;
        }
        b.wait();
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn submit(regs: usize, stride: usize, q: &mut Queue, op: u8, cid: u16, nsid: u32, prp1: u64, prp2: u64, cdw: [u32; 6]) -> bool {
    let e = q.sq.virt() as usize + q.tail as usize * 64;
    unsafe {
        core::ptr::write_bytes(e as *mut u8, 0, 64);
        write_volatile(e as *mut u32, op as u32 | (cid as u32) << 16);
        write_volatile((e + 4) as *mut u32, nsid);
        write_volatile((e + 24) as *mut u64, prp1);
        write_volatile((e + 32) as *mut u64, prp2);
        for (i, v) in cdw.iter().enumerate() {
            write_volatile((e + 40 + i * 4) as *mut u32, *v);
        }
    }
    q.tail = (q.tail + 1) % QUEUE_LEN;
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    w32(regs + 0x1000 + (2 * q.id as usize) * stride, q.tail as u32);
    // Wait for the completion entry whose phase bit flips.
    let c = q.cq.virt() as usize + q.head as usize * 16;
    let phase = q.phase;
    if !wait(|| unsafe { read_volatile((c + 14) as *const u16) } & 1 == phase as u16, 10_000) {
        return false;
    }
    let status = unsafe { read_volatile((c + 14) as *const u16) } >> 1;
    q.head = (q.head + 1) % QUEUE_LEN;
    if q.head == 0 {
        q.phase = !q.phase;
    }
    w32(regs + 0x1000 + (2 * q.id as usize + 1) * stride, q.head as u32);
    status & 0x7ff == 0
}

impl fat32::BlockDevice for NvmeDisk {
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), ()> {
        let block = self.block;
        self.transfer(lba, buf.len(), |d, done, n| {
            let at = lba * 512 / block as u64 + (done / block) as u64;
            if !d.io_cmd(IO_READ, at, n / block) {
                return false;
            }
            buf[done..done + n].copy_from_slice(&d.bounce.as_slice()[..n]);
            true
        })
    }

    fn write(&mut self, lba: u64, buf: &[u8]) -> Result<(), ()> {
        let block = self.block;
        self.transfer(lba, buf.len(), |d, done, n| {
            d.bounce.as_mut_slice()[..n].copy_from_slice(&buf[done..done + n]);
            let at = lba * 512 / block as u64 + (done / block) as u64;
            d.io_cmd(IO_WRITE, at, n / block)
        })
    }

    fn flush(&mut self) -> Result<(), ()> {
        if self.io_cmd(IO_FLUSH, 0, 0) { Ok(()) } else { Err(()) }
    }
}
