//! DNS queries for A records (RFC 1035).

use alloc::string::String;
use alloc::vec::Vec;

use crate::Ipv4;

pub const PORT: u16 = 53;

pub fn build_query(id: u16, name: &str) -> Option<Vec<u8>> {
    let mut m = Vec::with_capacity(32 + name.len());
    m.extend_from_slice(&id.to_be_bytes());
    m.extend_from_slice(&[0x01, 0x00]); // recursion desired
    m.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]); // 1 question
    for label in name.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            return None;
        }
        m.push(label.len() as u8);
        m.extend_from_slice(label.as_bytes());
    }
    m.push(0);
    m.extend_from_slice(&[0, 1, 0, 1]); // type A, class IN
    Some(m)
}

/// Skip a (possibly compressed) name, returning the offset after it.
fn skip_name(b: &[u8], mut i: usize) -> Option<usize> {
    loop {
        let len = *b.get(i)? as usize;
        if len == 0 {
            return Some(i + 1);
        }
        if len & 0xc0 == 0xc0 {
            return Some(i + 2);
        }
        i += 1 + len;
    }
}

/// Read a name (following compression pointers) starting at `i`.
pub fn read_name(b: &[u8], mut i: usize) -> Option<String> {
    let mut out = String::new();
    let mut jumps = 0;
    loop {
        let len = *b.get(i)? as usize;
        if len == 0 {
            return Some(out);
        }
        if len & 0xc0 == 0xc0 {
            i = ((len & 0x3f) << 8) | *b.get(i + 1)? as usize;
            jumps += 1;
            if jumps > 16 {
                return None;
            }
            continue;
        }
        if !out.is_empty() {
            out.push('.');
        }
        out.push_str(&String::from_utf8_lossy(b.get(i + 1..i + 1 + len)?));
        i += 1 + len;
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    Addresses(Vec<Ipv4>),
    /// RCODE from the server (3 = no such name).
    Error(u8),
}

pub fn parse_response(b: &[u8], id: u16) -> Option<Answer> {
    if b.len() < 12 || u16::from_be_bytes([b[0], b[1]]) != id || b[2] & 0x80 == 0 {
        return None;
    }
    let rcode = b[3] & 0x0f;
    if rcode != 0 {
        return Some(Answer::Error(rcode));
    }
    let qd = u16::from_be_bytes([b[4], b[5]]) as usize;
    let an = u16::from_be_bytes([b[6], b[7]]) as usize;
    let mut i = 12;
    for _ in 0..qd {
        i = skip_name(b, i)? + 4;
    }
    let mut addrs = Vec::new();
    for _ in 0..an {
        i = skip_name(b, i)?;
        let h = b.get(i..i + 10)?;
        let kind = u16::from_be_bytes([h[0], h[1]]);
        let rdlen = u16::from_be_bytes([h[8], h[9]]) as usize;
        let data = b.get(i + 10..i + 10 + rdlen)?;
        if kind == 1 && rdlen == 4 {
            addrs.push(Ipv4([data[0], data[1], data[2], data[3]]));
        }
        i += 10 + rdlen;
    }
    Some(Answer::Addresses(addrs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_encoding() {
        let q = build_query(0x1234, "example.com").unwrap();
        assert_eq!(&q[12..], b"\x07example\x03com\x00\x00\x01\x00\x01");
        assert!(build_query(1, "bad..name").is_none());
    }

    #[test]
    fn parses_compressed_answer() {
        let mut r = build_query(7, "example.com").unwrap();
        r[2] = 0x81;
        r[3] = 0x80;
        r[7] = 2; // two answers: a CNAME and an A record
        // CNAME pointing at the question name.
        r.extend_from_slice(&[0xc0, 12, 0, 5, 0, 1, 0, 0, 0, 60, 0, 2, 0xc0, 12]);
        r.extend_from_slice(&[0xc0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 93, 184, 215, 14]);
        assert_eq!(parse_response(&r, 7), Some(Answer::Addresses(alloc::vec![Ipv4([93, 184, 215, 14])])));
        assert_eq!(read_name(&r, r.len() - 16).as_deref(), Some("example.com"));
        assert_eq!(parse_response(&r, 8), None, "wrong id");
        r[3] = 0x83;
        assert_eq!(parse_response(&r, 7), Some(Answer::Error(3)));
    }
}
