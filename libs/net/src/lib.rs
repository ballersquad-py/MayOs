//! Network protocol encoding and decoding for MayOS: Ethernet, ARP, IPv4,
//! ICMP, UDP, DHCP and DNS.
//!
//! This crate only turns bytes into structures and back; the kernel owns
//! the devices, timers and state machines. It is `no_std` + `alloc` and is
//! unit-tested on the host.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod dhcp;
pub mod dns;

use alloc::vec::Vec;
use core::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Default, Hash, PartialOrd, Ord)]
pub struct Ipv4(pub [u8; 4]);

impl Ipv4 {
    pub const UNSPECIFIED: Ipv4 = Ipv4([0, 0, 0, 0]);
    pub const BROADCAST: Ipv4 = Ipv4([255, 255, 255, 255]);

    pub fn to_u32(self) -> u32 {
        u32::from_be_bytes(self.0)
    }
    pub fn from_u32(v: u32) -> Ipv4 {
        Ipv4(v.to_be_bytes())
    }
    pub fn is_unspecified(self) -> bool {
        self == Ipv4::UNSPECIFIED
    }
    /// Same subnet under `mask`.
    pub fn same_subnet(self, other: Ipv4, mask: Ipv4) -> bool {
        self.to_u32() & mask.to_u32() == other.to_u32() & mask.to_u32()
    }
    pub fn parse(s: &str) -> Option<Ipv4> {
        let mut out = [0u8; 4];
        let mut parts = s.trim().split('.');
        for b in out.iter_mut() {
            *b = parts.next()?.parse().ok()?;
        }
        if parts.next().is_some() {
            return None;
        }
        Some(Ipv4(out))
    }
    pub fn prefix_len(self) -> u32 {
        self.to_u32().leading_ones()
    }
}

impl fmt::Display for Ipv4 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}.{}", self.0[0], self.0[1], self.0[2], self.0[3])
    }
}

impl fmt::Debug for Ipv4 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Mac(pub [u8; 6]);

impl Mac {
    pub const BROADCAST: Mac = Mac([0xff; 6]);
    pub const ZERO: Mac = Mac([0; 6]);
}

impl fmt::Display for Mac {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let m = self.0;
        write!(f, "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
    }
}

impl fmt::Debug for Mac {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

fn be16(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([b[o], b[o + 1]])
}

/// The Internet checksum (RFC 1071) over `data`, starting from `initial`.
pub fn checksum(data: &[u8], initial: u32) -> u16 {
    let mut sum = initial;
    let mut chunks = data.chunks_exact(2);
    for c in &mut chunks {
        sum += u16::from_be_bytes([c[0], c[1]]) as u32;
    }
    if let [last] = chunks.remainder() {
        sum += (*last as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

// ---------------------------------------------------------------------------
// Ethernet
// ---------------------------------------------------------------------------

pub const ETH_IPV4: u16 = 0x0800;
pub const ETH_ARP: u16 = 0x0806;

pub struct EthFrame<'a> {
    pub dst: Mac,
    pub src: Mac,
    pub ethertype: u16,
    pub payload: &'a [u8],
}

impl<'a> EthFrame<'a> {
    pub fn parse(b: &'a [u8]) -> Option<EthFrame<'a>> {
        if b.len() < 14 {
            return None;
        }
        let mut dst = [0; 6];
        let mut src = [0; 6];
        dst.copy_from_slice(&b[0..6]);
        src.copy_from_slice(&b[6..12]);
        Some(EthFrame { dst: Mac(dst), src: Mac(src), ethertype: be16(b, 12), payload: &b[14..] })
    }
}

pub fn build_eth(dst: Mac, src: Mac, ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(14 + payload.len().max(46));
    f.extend_from_slice(&dst.0);
    f.extend_from_slice(&src.0);
    f.extend_from_slice(&ethertype.to_be_bytes());
    f.extend_from_slice(payload);
    // Pad to the 60-byte minimum (the NIC adds the CRC).
    while f.len() < 60 {
        f.push(0);
    }
    f
}

// ---------------------------------------------------------------------------
// ARP
// ---------------------------------------------------------------------------

pub const ARP_REQUEST: u16 = 1;
pub const ARP_REPLY: u16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arp {
    pub op: u16,
    pub sender_mac: Mac,
    pub sender_ip: Ipv4,
    pub target_mac: Mac,
    pub target_ip: Ipv4,
}

impl Arp {
    pub fn parse(b: &[u8]) -> Option<Arp> {
        if b.len() < 28 || be16(b, 0) != 1 || be16(b, 2) != ETH_IPV4 || b[4] != 6 || b[5] != 4 {
            return None;
        }
        let mac = |o: usize| {
            let mut m = [0; 6];
            m.copy_from_slice(&b[o..o + 6]);
            Mac(m)
        };
        let ip = |o: usize| Ipv4([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        Some(Arp { op: be16(b, 6), sender_mac: mac(8), sender_ip: ip(14), target_mac: mac(18), target_ip: ip(24) })
    }

    pub fn build(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(28);
        v.extend_from_slice(&1u16.to_be_bytes());
        v.extend_from_slice(&ETH_IPV4.to_be_bytes());
        v.push(6);
        v.push(4);
        v.extend_from_slice(&self.op.to_be_bytes());
        v.extend_from_slice(&self.sender_mac.0);
        v.extend_from_slice(&self.sender_ip.0);
        v.extend_from_slice(&self.target_mac.0);
        v.extend_from_slice(&self.target_ip.0);
        v
    }
}

// ---------------------------------------------------------------------------
// IPv4
// ---------------------------------------------------------------------------

pub const PROTO_ICMP: u8 = 1;
pub const PROTO_TCP: u8 = 6;
pub const PROTO_UDP: u8 = 17;

pub struct Ipv4Packet<'a> {
    pub src: Ipv4,
    pub dst: Ipv4,
    pub protocol: u8,
    pub ttl: u8,
    pub payload: &'a [u8],
}

impl<'a> Ipv4Packet<'a> {
    /// Parse and validate a packet. Fragments are rejected.
    pub fn parse(b: &'a [u8]) -> Option<Ipv4Packet<'a>> {
        if b.len() < 20 || b[0] >> 4 != 4 {
            return None;
        }
        let ihl = (b[0] & 0xf) as usize * 4;
        let total = be16(b, 2) as usize;
        if ihl < 20 || total < ihl || total > b.len() {
            return None;
        }
        if checksum(&b[..ihl], 0) != 0 {
            return None;
        }
        let flags_frag = be16(b, 6);
        if flags_frag & 0x3fff != 0 {
            return None; // more-fragments set or non-zero offset
        }
        Some(Ipv4Packet {
            ttl: b[8],
            protocol: b[9],
            src: Ipv4([b[12], b[13], b[14], b[15]]),
            dst: Ipv4([b[16], b[17], b[18], b[19]]),
            payload: &b[ihl..total],
        })
    }
}

pub fn build_ipv4(src: Ipv4, dst: Ipv4, protocol: u8, id: u16, payload: &[u8]) -> Vec<u8> {
    let total = 20 + payload.len();
    let mut h = [0u8; 20];
    h[0] = 0x45;
    h[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    h[4..6].copy_from_slice(&id.to_be_bytes());
    h[6] = 0x40; // don't fragment
    h[8] = 64;
    h[9] = protocol;
    h[12..16].copy_from_slice(&src.0);
    h[16..20].copy_from_slice(&dst.0);
    let c = checksum(&h, 0);
    h[10..12].copy_from_slice(&c.to_be_bytes());
    let mut v = Vec::with_capacity(total);
    v.extend_from_slice(&h);
    v.extend_from_slice(payload);
    v
}

// ---------------------------------------------------------------------------
// ICMP
// ---------------------------------------------------------------------------

pub const ICMP_ECHO_REPLY: u8 = 0;
pub const ICMP_UNREACHABLE: u8 = 3;
pub const ICMP_ECHO_REQUEST: u8 = 8;
pub const ICMP_TIME_EXCEEDED: u8 = 11;

pub struct Icmp<'a> {
    pub kind: u8,
    pub code: u8,
    /// Identifier and sequence for echo messages.
    pub id: u16,
    pub seq: u16,
    pub data: &'a [u8],
}

impl<'a> Icmp<'a> {
    pub fn parse(b: &'a [u8]) -> Option<Icmp<'a>> {
        if b.len() < 8 || checksum(b, 0) != 0 {
            return None;
        }
        Some(Icmp { kind: b[0], code: b[1], id: be16(b, 4), seq: be16(b, 6), data: &b[8..] })
    }
}

pub fn build_icmp_echo(kind: u8, id: u16, seq: u16, data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + data.len());
    v.push(kind);
    v.push(0);
    v.extend_from_slice(&[0, 0]);
    v.extend_from_slice(&id.to_be_bytes());
    v.extend_from_slice(&seq.to_be_bytes());
    v.extend_from_slice(data);
    let c = checksum(&v, 0);
    v[2..4].copy_from_slice(&c.to_be_bytes());
    v
}

// ---------------------------------------------------------------------------
// UDP
// ---------------------------------------------------------------------------

pub struct Udp<'a> {
    pub src_port: u16,
    pub dst_port: u16,
    pub payload: &'a [u8],
}

fn pseudo_header_sum(src: Ipv4, dst: Ipv4, proto: u8, len: usize) -> u32 {
    let mut sum = 0u32;
    for ip in [src, dst] {
        sum += be16(&ip.0, 0) as u32 + be16(&ip.0, 2) as u32;
    }
    sum + proto as u32 + len as u32
}

impl<'a> Udp<'a> {
    pub fn parse(b: &'a [u8], src: Ipv4, dst: Ipv4) -> Option<Udp<'a>> {
        if b.len() < 8 {
            return None;
        }
        let len = be16(b, 4) as usize;
        if len < 8 || len > b.len() {
            return None;
        }
        if be16(b, 6) != 0 && checksum(&b[..len], pseudo_header_sum(src, dst, PROTO_UDP, len)) != 0 {
            return None;
        }
        Some(Udp { src_port: be16(b, 0), dst_port: be16(b, 2), payload: &b[8..len] })
    }
}

pub fn build_udp(src: Ipv4, dst: Ipv4, src_port: u16, dst_port: u16, payload: &[u8]) -> Vec<u8> {
    let len = 8 + payload.len();
    let mut v = Vec::with_capacity(len);
    v.extend_from_slice(&src_port.to_be_bytes());
    v.extend_from_slice(&dst_port.to_be_bytes());
    v.extend_from_slice(&(len as u16).to_be_bytes());
    v.extend_from_slice(&[0, 0]);
    v.extend_from_slice(payload);
    let mut c = checksum(&v, pseudo_header_sum(src, dst, PROTO_UDP, len));
    if c == 0 {
        c = 0xffff;
    }
    v[6..8].copy_from_slice(&c.to_be_bytes());
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_rfc1071_example() {
        // Example from RFC 1071 section 3.
        let data = [0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7];
        assert_eq!(checksum(&data, 0), !0xddf2);
    }

    #[test]
    fn ipv4_roundtrip() {
        let p = build_ipv4(Ipv4([10, 0, 2, 15]), Ipv4([8, 8, 8, 8]), PROTO_UDP, 7, b"hello");
        let parsed = Ipv4Packet::parse(&p).unwrap();
        assert_eq!(parsed.src, Ipv4([10, 0, 2, 15]));
        assert_eq!(parsed.dst, Ipv4([8, 8, 8, 8]));
        assert_eq!(parsed.protocol, PROTO_UDP);
        assert_eq!(parsed.payload, b"hello");
        let mut bad = p.clone();
        bad[12] ^= 1;
        assert!(Ipv4Packet::parse(&bad).is_none(), "checksum must be verified");
    }

    #[test]
    fn known_ipv4_header_checksum() {
        // Wikipedia's IPv4 header checksum example.
        let h = [
            0x45, 0x00, 0x00, 0x73, 0x00, 0x00, 0x40, 0x00, 0x40, 0x11, 0x00, 0x00, 0xc0, 0xa8, 0x00, 0x01, 0xc0,
            0xa8, 0x00, 0xc7,
        ];
        assert_eq!(checksum(&h, 0), 0xb861);
    }

    #[test]
    fn udp_and_icmp_roundtrip() {
        let (a, b) = (Ipv4([192, 168, 1, 2]), Ipv4([192, 168, 1, 1]));
        let u = build_udp(a, b, 68, 67, b"payload!");
        let p = Udp::parse(&u, a, b).unwrap();
        assert_eq!((p.src_port, p.dst_port, p.payload), (68, 67, &b"payload!"[..]));
        assert!(Udp::parse(&u, a, Ipv4([1, 2, 3, 4])).is_none(), "pseudo header must count");
        let e = build_icmp_echo(ICMP_ECHO_REQUEST, 0x1234, 9, b"abcdefg");
        let i = Icmp::parse(&e).unwrap();
        assert_eq!((i.kind, i.id, i.seq, i.data), (ICMP_ECHO_REQUEST, 0x1234, 9, &b"abcdefg"[..]));
    }

    #[test]
    fn arp_and_ethernet() {
        let arp = Arp {
            op: ARP_REQUEST,
            sender_mac: Mac([1, 2, 3, 4, 5, 6]),
            sender_ip: Ipv4([10, 0, 2, 15]),
            target_mac: Mac::ZERO,
            target_ip: Ipv4([10, 0, 2, 2]),
        };
        let frame = build_eth(Mac::BROADCAST, arp.sender_mac, ETH_ARP, &arp.build());
        assert_eq!(frame.len(), 60);
        let eth = EthFrame::parse(&frame).unwrap();
        assert_eq!(eth.ethertype, ETH_ARP);
        assert_eq!(Arp::parse(eth.payload).unwrap(), arp);
    }

    #[test]
    fn address_helpers() {
        assert_eq!(Ipv4::parse("10.0.2.15"), Some(Ipv4([10, 0, 2, 15])));
        assert_eq!(Ipv4::parse("10.0.2"), None);
        assert_eq!(Ipv4::parse("1.2.3.256"), None);
        assert_eq!(Ipv4([255, 255, 255, 0]).prefix_len(), 24);
        assert!(Ipv4([10, 0, 2, 15]).same_subnet(Ipv4([10, 0, 2, 2]), Ipv4([255, 255, 255, 0])));
        assert_eq!(alloc::format!("{}", Mac([0x52, 0x54, 0, 0x12, 0x34, 0x56])), "52:54:00:12:34:56");
    }
}
