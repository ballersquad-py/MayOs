//! DHCP client messages (RFC 2131 / 2132).

use alloc::vec::Vec;

use crate::{Ipv4, Mac};

pub const SERVER_PORT: u16 = 67;
pub const CLIENT_PORT: u16 = 68;

pub const DISCOVER: u8 = 1;
pub const OFFER: u8 = 2;
pub const REQUEST: u8 = 3;
pub const ACK: u8 = 5;
pub const NAK: u8 = 6;

const MAGIC: [u8; 4] = [99, 130, 83, 99];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lease {
    pub message: u8,
    pub xid: u32,
    pub your_ip: Ipv4,
    pub server: Ipv4,
    pub mask: Option<Ipv4>,
    pub router: Option<Ipv4>,
    pub dns: Vec<Ipv4>,
    pub lease_secs: Option<u32>,
}

/// Build a DISCOVER, or a REQUEST for `requested` from `server`.
pub fn build(kind: u8, xid: u32, mac: Mac, requested: Option<(Ipv4, Ipv4)>, hostname: &str) -> Vec<u8> {
    let mut m = alloc::vec![0u8; 236];
    m[0] = 1; // BOOTREQUEST
    m[1] = 1; // Ethernet
    m[2] = 6;
    m[4..8].copy_from_slice(&xid.to_be_bytes());
    m[10] = 0x80; // ask for broadcast replies (we have no IP yet)
    m[28..34].copy_from_slice(&mac.0);
    m.extend_from_slice(&MAGIC);
    m.extend_from_slice(&[53, 1, kind]);
    m.extend_from_slice(&[61, 7, 1]);
    m.extend_from_slice(&mac.0);
    if let Some((ip, server)) = requested {
        m.extend_from_slice(&[50, 4]);
        m.extend_from_slice(&ip.0);
        m.extend_from_slice(&[54, 4]);
        m.extend_from_slice(&server.0);
    }
    if !hostname.is_empty() && hostname.len() < 64 {
        m.push(12);
        m.push(hostname.len() as u8);
        m.extend_from_slice(hostname.as_bytes());
    }
    // Parameter request list: subnet mask, router, DNS, lease time.
    m.extend_from_slice(&[55, 4, 1, 3, 6, 51]);
    m.push(255);
    m
}

pub fn parse(b: &[u8]) -> Option<Lease> {
    if b.len() < 240 || b[0] != 2 || b[236..240] != MAGIC {
        return None;
    }
    let ip = |o: usize| Ipv4([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let mut lease = Lease {
        xid: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
        your_ip: ip(16),
        server: ip(20),
        ..Default::default()
    };
    let mut i = 240;
    while i < b.len() {
        let code = b[i];
        if code == 0 {
            i += 1;
            continue;
        }
        if code == 255 || i + 1 >= b.len() {
            break;
        }
        let len = b[i + 1] as usize;
        let d = i + 2;
        if d + len > b.len() {
            break;
        }
        match code {
            53 if len >= 1 => lease.message = b[d],
            1 if len >= 4 => lease.mask = Some(ip(d)),
            3 if len >= 4 => lease.router = Some(ip(d)),
            6 => lease.dns = (0..len / 4).map(|k| ip(d + k * 4)).collect(),
            51 if len >= 4 => lease.lease_secs = Some(u32::from_be_bytes([b[d], b[d + 1], b[d + 2], b[d + 3]])),
            54 if len >= 4 => lease.server = ip(d),
            _ => {}
        }
        i = d + len;
    }
    if lease.message == 0 { None } else { Some(lease) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_has_required_fields() {
        let mac = Mac([0x52, 0x54, 0, 0x12, 0x34, 0x56]);
        let m = build(DISCOVER, 0xdeadbeef, mac, None, "mayos");
        assert_eq!(&m[4..8], &0xdeadbeefu32.to_be_bytes());
        assert_eq!(&m[28..34], &mac.0);
        assert_eq!(&m[236..240], &MAGIC);
        assert_eq!(&m[240..243], &[53, 1, DISCOVER]);
        assert_eq!(*m.last().unwrap(), 255);
    }

    #[test]
    fn parses_an_offer() {
        // A server reply: reuse the request layout and patch it into a reply.
        let mut m = build(DISCOVER, 42, Mac::ZERO, None, "");
        m[0] = 2;
        m[16..20].copy_from_slice(&[10, 0, 2, 15]);
        m.truncate(240);
        m.extend_from_slice(&[53, 1, OFFER, 1, 4, 255, 255, 255, 0, 3, 4, 10, 0, 2, 2, 6, 8, 10, 0, 2, 3, 1, 1, 1, 1]);
        m.extend_from_slice(&[51, 4, 0, 1, 81, 128, 54, 4, 10, 0, 2, 2, 255]);
        let l = parse(&m).unwrap();
        assert_eq!(l.message, OFFER);
        assert_eq!(l.xid, 42);
        assert_eq!(l.your_ip, Ipv4([10, 0, 2, 15]));
        assert_eq!(l.mask, Some(Ipv4([255, 255, 255, 0])));
        assert_eq!(l.router, Some(Ipv4([10, 0, 2, 2])));
        assert_eq!(l.dns, alloc::vec![Ipv4([10, 0, 2, 3]), Ipv4([1, 1, 1, 1])]);
        assert_eq!(l.lease_secs, Some(86400));
        assert_eq!(l.server, Ipv4([10, 0, 2, 2]));
    }
}
