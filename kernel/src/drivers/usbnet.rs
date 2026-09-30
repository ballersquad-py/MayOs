//! USB network adapters: phone tethering (iPhone "Personal Hotspot" over
//! USB, Android "USB tethering") and USB Ethernet dongles.
//!
//! Protocols:
//! - Apple's iPhone Ethernet (as in Linux ipheth): raw frames, received
//!   with 2 bytes of padding in front, sent padded to a full frame.
//! - RNDIS (most Android phones): frames wrapped in RNDIS_PACKET_MSG.
//! - CDC ECM (dongles, some phones): raw frames.
//! - CDC NCM (newer Android phones): frames packed into NTB16 blocks.
//!
//! The xHCI driver moves the bytes; this file wraps and unwraps frames.
//! The network stack sees a `UsbNet`: queues of Ethernet frames.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::sync::Spin;
use net::Mac;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Proto {
    Ipheth,
    Rndis,
    Ecm,
    Ncm,
    /// A byte stream (the iPhone's usbmux interface), not a network.
    Raw,
}

pub const IPHETH_FRAME: usize = 1514;

/// The shared end between the USB thread and the network stack.
pub struct UsbNet {
    pub mac: Mac,
    pub model: &'static str,
    pub rx: Spin<VecDeque<Vec<u8>>>,
    pub tx: Spin<VecDeque<Vec<u8>>>,
    pub link: AtomicBool,
    /// Unplugged: the network stack drops it.
    pub gone: AtomicBool,
}

impl UsbNet {
    pub fn new(mac: Mac, model: &'static str) -> UsbNet {
        UsbNet {
            mac,
            model,
            rx: Spin::new(VecDeque::new()),
            tx: Spin::new(VecDeque::new()),
            link: AtomicBool::new(true),
            gone: AtomicBool::new(false),
        }
    }

    pub fn link_up(&self) -> bool {
        self.link.load(Ordering::Relaxed) && !self.gone.load(Ordering::Relaxed)
    }

    pub fn send(&self, frame: &[u8]) -> bool {
        let mut q = self.tx.lock();
        if q.len() >= 256 {
            return false;
        }
        q.push_back(frame.to_vec());
        true
    }

    pub fn recv(&self) -> Option<Vec<u8>> {
        self.rx.lock().pop_front()
    }

    pub fn deliver(&self, frame: Vec<u8>) {
        let mut q = self.rx.lock();
        if q.len() < 512 {
            q.push_back(frame);
        }
    }
}

fn u16_at(b: &[u8], o: usize) -> usize {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]) as usize).unwrap_or(0)
}

fn u32_at(b: &[u8], o: usize) -> usize {
    b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize).unwrap_or(0)
}

/// Ethernet frames in one received transfer.
pub fn unwrap(proto: Proto, data: &[u8], out: &mut Vec<Vec<u8>>) {
    match proto {
        Proto::Ipheth => {
            if data.len() > 2 + 14 {
                out.push(data[2..].to_vec());
            }
        }
        Proto::Raw => {
            if !data.is_empty() {
                out.push(data.to_vec());
            }
        }
        Proto::Ecm => {
            if data.len() >= 14 {
                out.push(data.to_vec());
            }
        }
        Proto::Rndis => {
            let mut o = 0;
            while o + 44 <= data.len() {
                let (kind, len) = (u32_at(data, o), u32_at(data, o + 4));
                if len < 44 || o + len > data.len() {
                    break;
                }
                if kind == 1 {
                    let (off, dlen) = (u32_at(data, o + 8), u32_at(data, o + 12));
                    let s = o + 8 + off;
                    if s + dlen <= o + len && dlen >= 14 {
                        out.push(data[s..s + dlen].to_vec());
                    }
                }
                o += len;
            }
        }
        Proto::Ncm => {
            if data.len() < 12 || &data[0..4] != b"NCMH" {
                return;
            }
            let mut ndp = u16_at(data, 10);
            let mut guard = 0;
            while ndp != 0 && ndp + 8 <= data.len() && guard < 16 {
                if &data[ndp..ndp + 3] != b"NCM" {
                    break;
                }
                let len = u16_at(data, ndp + 4);
                let mut p = ndp + 8;
                while p + 4 <= ndp + len && p + 4 <= data.len() {
                    let (idx, dlen) = (u16_at(data, p), u16_at(data, p + 2));
                    if idx == 0 || dlen == 0 {
                        break;
                    }
                    if idx + dlen <= data.len() && dlen >= 14 {
                        out.push(data[idx..idx + dlen].to_vec());
                    }
                    p += 4;
                }
                ndp = u16_at(data, ndp + 6);
                guard += 1;
            }
        }
    }
}

/// One frame ready to send.
pub fn wrap(proto: Proto, frame: &[u8], seq: &mut u16) -> Vec<u8> {
    match proto {
        Proto::Ipheth => {
            let mut v = frame.to_vec();
            v.resize(IPHETH_FRAME.max(frame.len()), 0);
            v
        }
        Proto::Ecm | Proto::Raw => frame.to_vec(),
        Proto::Rndis => {
            let mut v = alloc::vec![0u8; 44];
            let total = (44 + frame.len()) as u32;
            v[0..4].copy_from_slice(&1u32.to_le_bytes());
            v[4..8].copy_from_slice(&total.to_le_bytes());
            v[8..12].copy_from_slice(&36u32.to_le_bytes());
            v[12..16].copy_from_slice(&(frame.len() as u32).to_le_bytes());
            v.extend_from_slice(frame);
            v
        }
        Proto::Ncm => {
            // NTH16 (12) + NDP16 (16) + the datagram at offset 28.
            let mut v = alloc::vec![0u8; 28];
            let total = (28 + frame.len()) as u16;
            v[0..4].copy_from_slice(b"NCMH");
            v[4..6].copy_from_slice(&12u16.to_le_bytes());
            v[6..8].copy_from_slice(&seq.to_le_bytes());
            *seq = seq.wrapping_add(1);
            v[8..10].copy_from_slice(&total.to_le_bytes());
            v[10..12].copy_from_slice(&12u16.to_le_bytes());
            v[12..16].copy_from_slice(b"NCM0");
            v[16..18].copy_from_slice(&16u16.to_le_bytes());
            v[20..22].copy_from_slice(&28u16.to_le_bytes());
            v[22..24].copy_from_slice(&(frame.len() as u16).to_le_bytes());
            v.extend_from_slice(frame);
            v
        }
    }
}

// --- RNDIS control messages --------------------------------------------

pub const RNDIS_INIT: u32 = 2;
pub const RNDIS_QUERY: u32 = 4;
pub const RNDIS_SET: u32 = 5;
pub const OID_PERMANENT_ADDRESS: u32 = 0x0101_0101;
pub const OID_PACKET_FILTER: u32 = 0x0001_010e;

pub fn rndis_msg(kind: u32, id: u32, oid: u32, value: Option<u32>) -> Vec<u8> {
    let mut v = Vec::new();
    let put = |v: &mut Vec<u8>, x: u32| v.extend_from_slice(&x.to_le_bytes());
    match kind {
        RNDIS_INIT => {
            put(&mut v, kind);
            put(&mut v, 24);
            put(&mut v, id);
            put(&mut v, 1);
            put(&mut v, 0);
            put(&mut v, 16384);
        }
        _ => {
            let buflen = if value.is_some() { 4 } else { 0 };
            put(&mut v, kind);
            put(&mut v, 28 + buflen);
            put(&mut v, id);
            put(&mut v, oid);
            put(&mut v, buflen);
            put(&mut v, if value.is_some() { 20 } else { 0 });
            put(&mut v, 0);
            if let Some(x) = value {
                put(&mut v, x);
            }
        }
    }
    v
}

/// The information buffer of a QUERY completion.
pub fn rndis_query_result(r: &[u8]) -> Option<&[u8]> {
    if u32_at(r, 0) != 0x8000_0004 || u32_at(r, 12) != 0 {
        return None;
    }
    let (len, off) = (u32_at(r, 16), u32_at(r, 20));
    r.get(8 + off..8 + off + len)
}

/// MAC address from a USB string descriptor ("020000000001" in UTF-16).
pub fn mac_from_string(desc: &[u8]) -> Option<Mac> {
    let chars: Vec<u8> = desc.get(2..)?.chunks(2).map(|c| c[0]).collect();
    if chars.len() < 12 {
        return None;
    }
    let mut m = [0u8; 6];
    for (i, b) in m.iter_mut().enumerate() {
        let s = core::str::from_utf8(&chars[i * 2..i * 2 + 2]).ok()?;
        *b = u8::from_str_radix(s, 16).ok()?;
    }
    Some(Mac(m))
}

/// A made-up local MAC for devices that do not tell theirs.
pub fn local_mac(seed: u16) -> Mac {
    Mac([0x02, 0x4d, 0x61, 0x79, (seed >> 8) as u8, seed as u8])
}
