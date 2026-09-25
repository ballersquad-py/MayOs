//! Song metadata: ID3v2/ID3v1 (MP3) and iTunes-style MP4 tags.

use alloc::string::String;
use alloc::vec::Vec;

use crate::demux::{read_vec, Source};

#[derive(Default, Clone, Debug)]
pub struct Tags {
    pub title: String,
    pub artist: String,
    pub album: String,
    /// Embedded cover picture (JPEG or PNG bytes).
    pub cover: Option<Vec<u8>>,
}

fn latin1(b: &[u8]) -> String {
    b.iter().take_while(|&&c| c != 0).map(|&c| c as char).collect()
}

fn utf16(b: &[u8], big_endian: bool) -> String {
    let units: Vec<u16> = b
        .chunks_exact(2)
        .map(|c| if big_endian { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) })
        .take_while(|&u| u != 0)
        .collect();
    char::decode_utf16(units.into_iter()).map(|r| r.unwrap_or('\u{fffd}')).collect()
}

/// Decode an ID3 text field with its encoding byte.
fn text(enc: u8, b: &[u8]) -> String {
    let s = match enc {
        0 => latin1(b),
        1 => {
            if b.len() >= 2 && b[0] == 0xfe && b[1] == 0xff {
                utf16(&b[2..], true)
            } else if b.len() >= 2 && b[0] == 0xff && b[1] == 0xfe {
                utf16(&b[2..], false)
            } else {
                utf16(b, false)
            }
        }
        2 => utf16(b, true),
        _ => String::from_utf8_lossy(b.split(|&c| c == 0).next().unwrap_or(&[])).into_owned(),
    };
    String::from(s.trim())
}

fn syncsafe(b: &[u8]) -> usize {
    ((b[0] as usize & 0x7f) << 21) | ((b[1] as usize & 0x7f) << 14) | ((b[2] as usize & 0x7f) << 7) | (b[3] as usize & 0x7f)
}

fn id3v2(data: &[u8]) -> Tags {
    let mut t = Tags::default();
    let ver = data[3];
    let flags = data[5];
    let size = syncsafe(&data[6..10]).min(data.len() - 10);
    let mut body: Vec<u8> = data[10..10 + size].to_vec();
    if flags & 0x80 != 0 && ver < 4 {
        // Undo unsynchronisation (FF 00 -> FF).
        let mut out = Vec::with_capacity(body.len());
        let mut i = 0;
        while i < body.len() {
            out.push(body[i]);
            if body[i] == 0xff && body.get(i + 1) == Some(&0) {
                i += 1;
            }
            i += 1;
        }
        body = out;
    }
    let mut p = 0;
    if flags & 0x40 != 0 && body.len() >= 4 {
        let ext = if ver >= 4 { syncsafe(&body[0..4]) } else { u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize + 4 };
        p = ext.min(body.len());
    }
    let (id_len, hdr_len) = if ver == 2 { (3, 6) } else { (4, 10) };
    while p + hdr_len <= body.len() {
        let id = &body[p..p + id_len];
        if id[0] == 0 {
            break;
        }
        let fsize = match ver {
            2 => ((body[p + 3] as usize) << 16) | ((body[p + 4] as usize) << 8) | body[p + 5] as usize,
            4 => syncsafe(&body[p + 4..p + 8]),
            _ => u32::from_be_bytes([body[p + 4], body[p + 5], body[p + 6], body[p + 7]]) as usize,
        };
        let start = p + hdr_len;
        let end = (start + fsize).min(body.len());
        let f = &body[start..end];
        p = end;
        if f.is_empty() {
            continue;
        }
        match id {
            b"TIT2" | b"TT2" => t.title = text(f[0], &f[1..]),
            b"TPE1" | b"TP1" => t.artist = text(f[0], &f[1..]),
            b"TALB" | b"TAL" => t.album = text(f[0], &f[1..]),
            b"APIC" | b"PIC" if t.cover.is_none() => {
                let enc = f[0];
                let mut q = 1;
                if id == b"PIC" {
                    q += 3; // image format
                } else {
                    while q < f.len() && f[q] != 0 {
                        q += 1;
                    }
                    q += 1;
                }
                q += 1; // picture type
                // Description, terminated by 1 or 2 zero bytes.
                if enc == 1 || enc == 2 {
                    while q + 1 < f.len() && !(f[q] == 0 && f[q + 1] == 0) {
                        q += 2;
                    }
                    q += 2;
                } else {
                    while q < f.len() && f[q] != 0 {
                        q += 1;
                    }
                    q += 1;
                }
                if q < f.len() {
                    t.cover = Some(f[q..].to_vec());
                }
            }
            _ => {}
        }
    }
    t
}

fn mp4_tags(src: &mut dyn Source) -> Option<Tags> {
    // moov/udta/meta/ilst
    let len = src.len();
    let find = |src: &mut dyn Source, start: u64, end: u64, kind: &[u8; 4]| -> Option<(u64, u64)> {
        let mut p = start;
        while p + 8 <= end {
            let h = read_vec(src, p, 8).ok()?;
            let mut size = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as u64;
            let mut hl = 8;
            if size == 1 {
                let l = read_vec(src, p + 8, 8).ok()?;
                size = u64::from_be_bytes([l[0], l[1], l[2], l[3], l[4], l[5], l[6], l[7]]);
                hl = 16;
            } else if size == 0 {
                size = end - p;
            }
            if size < hl {
                return None;
            }
            if &h[4..8] == kind {
                return Some((p + hl, (p + size).min(end)));
            }
            p += size;
        }
        None
    };
    let moov = find(src, 0, len, b"moov")?;
    let udta = find(src, moov.0, moov.1, b"udta")?;
    let meta = find(src, udta.0, udta.1, b"meta")?;
    let ilst = find(src, meta.0 + 4, meta.1, b"ilst")?;
    if ilst.1 - ilst.0 > 16 << 20 {
        return None;
    }
    let d = read_vec(src, ilst.0, (ilst.1 - ilst.0) as usize).ok()?;
    let mut t = Tags::default();
    let mut p = 0;
    while p + 8 <= d.len() {
        let size = u32::from_be_bytes([d[p], d[p + 1], d[p + 2], d[p + 3]]) as usize;
        if size < 8 || p + size > d.len() {
            break;
        }
        let kind = &d[p + 4..p + 8];
        let item = &d[p + 8..p + size];
        // First 'data' child: type(4) locale(4) payload.
        if item.len() >= 16 && &item[4..8] == b"data" {
            let dsize = (u32::from_be_bytes([item[0], item[1], item[2], item[3]]) as usize).min(item.len());
            let payload = &item[16..dsize.max(16)];
            match kind {
                b"\xa9nam" => t.title = String::from_utf8_lossy(payload).into_owned(),
                b"\xa9ART" | b"aART" if t.artist.is_empty() => t.artist = String::from_utf8_lossy(payload).into_owned(),
                b"\xa9alb" => t.album = String::from_utf8_lossy(payload).into_owned(),
                b"covr" => t.cover = Some(payload.to_vec()),
                _ => {}
            }
        }
        p += size;
    }
    Some(t)
}

/// Read whatever tags the file has.
pub fn read(src: &mut dyn Source) -> Tags {
    let len = src.len();
    let head = read_vec(src, 0, (len as usize).min(10)).unwrap_or_default();
    if head.len() == 10 && &head[..3] == b"ID3" {
        let size = syncsafe(&head[6..10]) + 10;
        if size <= 32 << 20 {
            if let Ok(d) = read_vec(src, 0, size.min(len as usize)) {
                return id3v2(&d);
            }
        }
    }
    if head.len() >= 8 && (&head[4..8] == b"ftyp" || &head[4..8] == b"moov") {
        if let Some(t) = mp4_tags(src) {
            return t;
        }
    }
    // ID3v1 at the end.
    if len >= 128 {
        if let Ok(d) = read_vec(src, len - 128, 128) {
            if &d[..3] == b"TAG" {
                return Tags { title: latin1(&d[3..33]).trim().into(), artist: latin1(&d[33..63]).trim().into(), album: latin1(&d[63..93]).trim().into(), cover: None };
            }
        }
    }
    Tags::default()
}
