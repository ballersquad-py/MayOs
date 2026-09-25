//! MP4 / MOV / M4A (ISO base media file format), including fragmented MP4.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::demux::*;

#[derive(Clone, Copy)]
struct Sample {
    offset: u64,
    size: u32,
    dts: i64,
    cts: i32,
    key: bool,
}

struct Track {
    info: TrackInfo,
    id: u32,
    timescale: u32,
    samples: Vec<Sample>,
    /// Media time where presentation starts (edit list), in timescale units.
    start: i64,
    next: usize,
    default_duration: u32,
    default_size: u32,
}

pub struct Mp4 {
    src: Box<dyn Source + Send>,
    info: MediaInfo,
    tracks: Vec<Track>,
}

struct BoxHdr {
    kind: [u8; 4],
    start: u64,
    body: u64,
    end: u64,
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}
fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}
fn be64(b: &[u8]) -> u64 {
    u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

fn read_box(src: &mut dyn Source, pos: u64, limit: u64) -> Result<Option<BoxHdr>> {
    if pos + 8 > limit {
        return Ok(None);
    }
    let h = read_vec(src, pos, 8)?;
    let mut size = be32(&h) as u64;
    let kind = [h[4], h[5], h[6], h[7]];
    let mut body = pos + 8;
    if size == 1 {
        let l = read_vec(src, pos + 8, 8)?;
        size = be64(&l);
        body += 8;
    } else if size == 0 {
        size = limit - pos;
    }
    if size < body - pos {
        return Err(Error::Invalid("mp4 box size"));
    }
    Ok(Some(BoxHdr { kind, start: pos, body, end: pos.saturating_add(size).min(limit) }))
}

/// Iterate over child boxes in [start, end).
fn children(src: &mut dyn Source, start: u64, end: u64) -> Result<Vec<BoxHdr>> {
    let mut v = Vec::new();
    let mut p = start;
    while let Some(b) = read_box(src, p, end)? {
        p = b.end;
        let done = b.end <= b.start;
        v.push(b);
        if done || v.len() > 100_000 {
            break;
        }
    }
    Ok(v)
}

fn find(boxes: &[BoxHdr], kind: &[u8; 4]) -> Option<usize> {
    boxes.iter().position(|b| &b.kind == kind)
}

/// Parse an MPEG-4 descriptor length.
fn desc_len(d: &[u8], p: &mut usize) -> usize {
    let mut len = 0;
    for _ in 0..4 {
        let Some(&b) = d.get(*p) else { break };
        *p += 1;
        len = (len << 7) | (b & 0x7f) as usize;
        if b & 0x80 == 0 {
            break;
        }
    }
    len
}

/// (objectTypeIndication, DecoderSpecificInfo) from an esds body.
fn parse_esds(d: &[u8]) -> Option<(u8, Vec<u8>)> {
    let mut p = 4; // version + flags
    if d.get(p)? != &0x03 {
        return None;
    }
    p += 1;
    desc_len(d, &mut p);
    p += 2;
    let flags = *d.get(p)?;
    p += 1;
    if flags & 0x80 != 0 {
        p += 2;
    }
    if flags & 0x40 != 0 {
        let n = *d.get(p)? as usize;
        p += 1 + n;
    }
    if flags & 0x20 != 0 {
        p += 2;
    }
    if d.get(p)? != &0x04 {
        return None;
    }
    p += 1;
    desc_len(d, &mut p);
    let oti = *d.get(p)?;
    p += 13;
    let mut dsi = Vec::new();
    if d.get(p) == Some(&0x05) {
        p += 1;
        let n = desc_len(d, &mut p);
        dsi = d.get(p..p + n)?.to_vec();
    }
    Some((oti, dsi))
}

impl Mp4 {
    pub fn open(mut src: Box<dyn Source + Send>) -> Result<Mp4> {
        let len = src.len();
        let top = children(&mut *src, 0, len)?;
        let moov = find(&top, b"moov").ok_or(Error::Invalid("no moov box (incomplete download?)"))?;
        let moov = &top[moov];
        let mut info = MediaInfo { format: "MP4", ..Default::default() };
        let mut tracks = Vec::new();
        let mut movie_timescale = 1000u32;
        let mut movie_duration = 0u64;
        for b in children(&mut *src, moov.body, moov.end)? {
            match &b.kind {
                b"mvhd" => {
                    let d = read_vec(&mut *src, b.body, (b.end - b.body).min(32) as usize)?;
                    if d[0] == 1 {
                        movie_timescale = be32(&d[20..]);
                        movie_duration = be64(&d[24..]);
                    } else {
                        movie_timescale = be32(&d[12..]);
                        movie_duration = be32(&d[16..]) as u64;
                    }
                }
                b"trak" => {
                    if let Some(t) = Self::parse_trak(&mut *src, &b, movie_timescale)? {
                        tracks.push(t);
                    }
                }
                _ => {}
            }
        }
        // Fragmented MP4: samples live in moof boxes.
        let mut mvex_defaults: Vec<(u32, u32, u32)> = Vec::new();
        if let Some(i) = children(&mut *src, moov.body, moov.end)?.iter().position(|b| &b.kind == b"mvex") {
            let mv = &children(&mut *src, moov.body, moov.end)?[i];
            for t in children(&mut *src, mv.body, mv.end)? {
                if &t.kind == b"trex" {
                    let d = read_vec(&mut *src, t.body, 24)?;
                    mvex_defaults.push((be32(&d[4..]), be32(&d[12..]), be32(&d[16..])));
                }
            }
        }
        for (id, dur, size) in mvex_defaults {
            if let Some(t) = tracks.iter_mut().find(|t| t.id == id) {
                t.default_duration = dur;
                t.default_size = size;
            }
        }
        let mut frag_dts: Vec<i64> = tracks.iter().map(|t| t.samples.last().map(|s| s.dts + t.default_duration as i64).unwrap_or(0)).collect();
        for b in top.iter().filter(|b| &b.kind == b"moof") {
            Self::parse_moof(&mut *src, b, &mut tracks, &mut frag_dts)?;
        }
        tracks.retain(|t| !t.samples.is_empty());
        if tracks.is_empty() {
            return Err(Error::Invalid("no playable tracks"));
        }
        info.duration_us = (movie_duration as i128 * 1_000_000 / movie_timescale.max(1) as i128) as i64;
        for t in tracks.iter() {
            let last = t.samples.iter().map(|s| s.dts + s.cts as i64).max().unwrap_or(0);
            let d = ((last - t.start) as i128 * 1_000_000 / t.timescale.max(1) as i128) as i64;
            info.duration_us = info.duration_us.max(d);
        }
        for t in tracks.iter_mut() {
            if t.info.kind == Kind::Video && t.samples.len() > 1 {
                let span = t.samples.last().unwrap().dts - t.samples[0].dts;
                t.info.frame_us = (span as i128 * 1_000_000 / t.timescale.max(1) as i128 / (t.samples.len() as i128 - 1)) as u64;
            }
            info.tracks.push(t.info.clone());
        }
        Ok(Mp4 { src, info, tracks })
    }

    fn parse_trak(src: &mut dyn Source, trak: &BoxHdr, movie_timescale: u32) -> Result<Option<Track>> {
        let kids = children(src, trak.body, trak.end)?;
        let mut id = 0;
        if let Some(i) = find(&kids, b"tkhd") {
            let d = read_vec(src, kids[i].body, 24)?;
            id = if d[0] == 1 { be32(&d[20..]) } else { be32(&d[12..]) };
        }
        let mut edit_start: i64 = 0;
        let mut edit_empty: i64 = 0;
        if let Some(i) = find(&kids, b"edts") {
            for e in children(src, kids[i].body, kids[i].end)? {
                if &e.kind == b"elst" {
                    let d = read_vec(src, e.body, (e.end - e.body).min(4096) as usize)?;
                    let count = be32(&d[4..]) as usize;
                    let v1 = d[0] == 1;
                    let entry = if v1 { 20 } else { 12 };
                    for k in 0..count.min((d.len() - 8) / entry) {
                        let p = 8 + k * entry;
                        let (dur, mt) = if v1 { (be64(&d[p..]) as i64, be64(&d[p + 8..]) as i64) } else { (be32(&d[p..]) as i64, be32(&d[p + 4..]) as i32 as i64) };
                        if mt == -1 {
                            edit_empty += dur;
                        } else {
                            edit_start = mt;
                            break;
                        }
                    }
                }
            }
        }
        let mdia = match find(&kids, b"mdia") {
            Some(i) => children(src, kids[i].body, kids[i].end)?,
            None => return Ok(None),
        };
        let mut timescale = 1000;
        if let Some(i) = find(&mdia, b"mdhd") {
            let d = read_vec(src, mdia[i].body, 32)?;
            timescale = if d[0] == 1 { be32(&d[20..]) } else { be32(&d[12..]) };
        }
        let mut handler = [0u8; 4];
        if let Some(i) = find(&mdia, b"hdlr") {
            let d = read_vec(src, mdia[i].body, 12)?;
            handler.copy_from_slice(&d[8..12]);
        }
        let kind = match &handler {
            b"vide" => Kind::Video,
            b"soun" => Kind::Audio,
            _ => return Ok(None),
        };
        let minf = match find(&mdia, b"minf") {
            Some(i) => children(src, mdia[i].body, mdia[i].end)?,
            None => return Ok(None),
        };
        let stbl = match find(&minf, b"stbl") {
            Some(i) => children(src, minf[i].body, minf[i].end)?,
            None => return Ok(None),
        };
        let get = |src: &mut dyn Source, k: &[u8; 4]| -> Result<Option<Vec<u8>>> {
            match find(&stbl, k) {
                Some(i) => {
                    let b = &stbl[i];
                    if b.end - b.body > 256 << 20 {
                        return Err(Error::Invalid("sample table too large"));
                    }
                    Ok(Some(read_vec(src, b.body, (b.end - b.body) as usize)?))
                }
                None => Ok(None),
            }
        };
        let stsd = get(src, b"stsd")?.ok_or(Error::Invalid("no stsd"))?;
        let mut info = TrackInfo { kind, codec: Codec::Other(String::from("unknown")), width: 0, height: 0, sample_rate: 0, channels: 0, frame_us: 0 };
        Self::parse_stsd(src, &stsd, stbl[find(&stbl, b"stsd").unwrap()].body, &mut info)?;
        let mut t = Track { info, id, timescale, samples: Vec::new(), start: edit_start - (edit_empty as i128 * timescale as i128 / movie_timescale.max(1) as i128) as i64, next: 0, default_duration: 0, default_size: 0 };
        // Sample tables.
        let stts = get(src, b"stts")?.unwrap_or_default();
        let ctts = get(src, b"ctts")?;
        let stss = get(src, b"stss")?;
        let stsc = get(src, b"stsc")?.unwrap_or_default();
        let (sizes, fixed) = if let Some(stsz) = get(src, b"stsz")? {
            let fixed = be32(&stsz[4..]);
            let n = be32(&stsz[8..]) as usize;
            let mut v = Vec::with_capacity(if fixed == 0 { n } else { 0 });
            if fixed == 0 {
                for k in 0..n.min((stsz.len() - 12) / 4) {
                    v.push(be32(&stsz[12 + k * 4..]));
                }
            }
            (v, (fixed, n))
        } else if let Some(stz2) = get(src, b"stz2")? {
            let field = stz2[7];
            let n = be32(&stz2[8..]) as usize;
            let mut v = Vec::with_capacity(n);
            for k in 0..n {
                v.push(match field {
                    4 => ((stz2[12 + k / 2] >> if k % 2 == 0 { 4 } else { 0 }) & 15) as u32,
                    8 => stz2[12 + k] as u32,
                    _ => be16(&stz2[12 + k * 2..]) as u32,
                });
            }
            (v, (0, n))
        } else {
            (Vec::new(), (0, 0))
        };
        let nsamples = if fixed.0 != 0 { fixed.1 } else { sizes.len() };
        let mut chunks: Vec<u64> = Vec::new();
        if let Some(stco) = get(src, b"stco")? {
            let n = be32(&stco[4..]) as usize;
            for k in 0..n.min((stco.len() - 8) / 4) {
                chunks.push(be32(&stco[8 + k * 4..]) as u64);
            }
        } else if let Some(co64) = get(src, b"co64")? {
            let n = be32(&co64[4..]) as usize;
            for k in 0..n.min((co64.len() - 8) / 8) {
                chunks.push(be64(&co64[8 + k * 8..]));
            }
        }
        let mut samples = Vec::with_capacity(nsamples);
        // stsc: (first_chunk, samples_per_chunk)
        let nstsc = if stsc.len() >= 8 { be32(&stsc[4..]) as usize } else { 0 };
        let entries: Vec<(usize, usize)> = (0..nstsc.min((stsc.len().saturating_sub(8)) / 12)).map(|k| (be32(&stsc[8 + k * 12..]) as usize, be32(&stsc[12 + k * 12..]) as usize)).collect();
        let mut si = 0;
        for (e, &(first, per)) in entries.iter().enumerate() {
            let last = entries.get(e + 1).map(|x| x.0).unwrap_or(chunks.len() + 1);
            for c in first..last {
                let Some(&base) = chunks.get(c.wrapping_sub(1)) else { break };
                let mut off = base;
                for _ in 0..per {
                    if si >= nsamples {
                        break;
                    }
                    let size = if fixed.0 != 0 { fixed.0 } else { sizes[si] };
                    samples.push(Sample { offset: off, size, dts: 0, cts: 0, key: stss.is_none() });
                    off += size as u64;
                    si += 1;
                }
            }
        }
        let mut dts = 0i64;
        let mut k = 0;
        if stts.len() >= 8 {
            let n = be32(&stts[4..]) as usize;
            for e in 0..n.min((stts.len() - 8) / 8) {
                let count = be32(&stts[8 + e * 8..]) as usize;
                let delta = be32(&stts[12 + e * 8..]) as i64;
                for _ in 0..count {
                    if k >= samples.len() {
                        break;
                    }
                    samples[k].dts = dts;
                    dts += delta;
                    k += 1;
                }
            }
        }
        if let Some(ctts) = ctts {
            let n = be32(&ctts[4..]) as usize;
            let mut k = 0;
            for e in 0..n.min((ctts.len() - 8) / 8) {
                let count = be32(&ctts[8 + e * 8..]) as usize;
                let off = be32(&ctts[12 + e * 8..]) as i32;
                for _ in 0..count {
                    if k >= samples.len() {
                        break;
                    }
                    samples[k].cts = off;
                    k += 1;
                }
            }
        }
        if let Some(stss) = stss {
            let n = be32(&stss[4..]) as usize;
            for e in 0..n.min((stss.len() - 8) / 4) {
                let idx = be32(&stss[8 + e * 4..]) as usize;
                if idx >= 1 && idx <= samples.len() {
                    samples[idx - 1].key = true;
                }
            }
        }
        t.samples = samples;
        Ok(Some(t))
    }

    fn parse_stsd(src: &mut dyn Source, d: &[u8], body: u64, info: &mut TrackInfo) -> Result<()> {
        if d.len() < 16 {
            return Err(Error::Invalid("stsd"));
        }
        let entry_size = be32(&d[8..]) as usize;
        let fourcc = [d[12], d[13], d[14], d[15]];
        let e = &d[8..(8 + entry_size).min(d.len())];
        let entry_body = body + 8 + 8;
        let entry_end = body + 8 + entry_size as u64;
        match info.kind {
            Kind::Video => {
                if e.len() >= 8 + 78 - 8 + 8 {
                    info.width = be16(&e[32..]) as u32;
                    info.height = be16(&e[34..]) as u32;
                }
                let kids = children(src, entry_body + 78, entry_end).unwrap_or_default();
                info.codec = match &fourcc {
                    b"avc1" | b"avc3" => {
                        let i = find(&kids, b"avcC").ok_or(Error::Invalid("avc1 without avcC"))?;
                        Codec::H264(read_vec(src, kids[i].body, (kids[i].end - kids[i].body) as usize)?)
                    }
                    b"jpeg" | b"mjpa" | b"mjpb" | b"MJPG" => Codec::Mjpeg,
                    b"hvc1" | b"hev1" => Codec::Other(String::from("HEVC (H.265)")),
                    b"av01" => Codec::Other(String::from("AV1")),
                    b"vp09" => Codec::Other(String::from("VP9")),
                    b"mp4v" => Codec::Other(String::from("MPEG-4 Part 2")),
                    other => Codec::Other(format!("video '{}'", String::from_utf8_lossy(other))),
                };
            }
            Kind::Audio => {
                if e.len() < 36 {
                    return Err(Error::Invalid("audio sample entry"));
                }
                let version = be16(&e[16..]);
                info.channels = be16(&e[24..]);
                let bits = be16(&e[26..]);
                info.sample_rate = be32(&e[32..]) >> 16;
                let extra = match version {
                    1 => 16,
                    2 => 36,
                    _ => 0,
                };
                if version == 2 && e.len() >= 36 + 36 {
                    // 64-bit float sample rate and 32-bit channel count.
                    info.sample_rate = float_to_int(&e[40..48]) as u32;
                    info.channels = be32(&e[48..]) as u16;
                }
                let mut kids = children(src, entry_body + 28 + extra, entry_end).unwrap_or_default();
                // QuickTime nests esds inside a 'wave' box.
                if let Some(w) = find(&kids, b"wave") {
                    let (a, b) = (kids[w].body, kids[w].end);
                    kids = children(src, a, b).unwrap_or_default();
                }
                info.codec = match &fourcc {
                    b"mp4a" => match find(&kids, b"esds") {
                        Some(i) => {
                            let esds = read_vec(src, kids[i].body, (kids[i].end - kids[i].body) as usize)?;
                            match parse_esds(&esds) {
                                Some((0x40 | 0x66 | 0x67 | 0x68, dsi)) => Codec::Aac(dsi),
                                Some((0x69 | 0x6b, _)) => Codec::Mp3,
                                Some((oti, _)) => Codec::Other(format!("audio type {:#x}", oti)),
                                None => Codec::Other(String::from("unknown mp4a")),
                            }
                        }
                        None => Codec::Other(String::from("mp4a without esds")),
                    },
                    b".mp3" | b"mp3 " => Codec::Mp3,
                    b"sowt" => Codec::Pcm { bits: 16, little_endian: true, signed: true },
                    b"twos" => Codec::Pcm { bits: bits.max(8), little_endian: false, signed: true },
                    b"raw " => Codec::Pcm { bits: 8, little_endian: true, signed: false },
                    b"in24" => Codec::Pcm { bits: 24, little_endian: false, signed: true },
                    b"lpcm" => Codec::Pcm { bits: 16, little_endian: true, signed: true },
                    b"fl32" => Codec::PcmFloat { little_endian: false },
                    b"ac-3" => Codec::Other(String::from("AC-3 (Dolby Digital)")),
                    b"ec-3" => Codec::Other(String::from("E-AC-3")),
                    b"Opus" => Codec::Other(String::from("Opus")),
                    b"alac" => Codec::Other(String::from("ALAC")),
                    other => Codec::Other(format!("audio '{}'", String::from_utf8_lossy(other))),
                };
            }
        }
        Ok(())
    }

    fn parse_moof(src: &mut dyn Source, moof: &BoxHdr, tracks: &mut [Track], frag_dts: &mut [i64]) -> Result<()> {
        for traf in children(src, moof.body, moof.end)?.into_iter().filter(|b| &b.kind == b"traf") {
            let kids = children(src, traf.body, traf.end)?;
            let Some(h) = find(&kids, b"tfhd") else { continue };
            let d = read_vec(src, kids[h].body, (kids[h].end - kids[h].body).min(64) as usize)?;
            let flags = be32(&d) & 0xff_ffff;
            let id = be32(&d[4..]);
            let Some(ti) = tracks.iter().position(|t| t.id == id) else { continue };
            let mut p = 8;
            let mut base = moof.start;
            if flags & 1 != 0 {
                base = be64(&d[p..]);
                p += 8;
            }
            if flags & 2 != 0 {
                p += 4;
            }
            let mut def_dur = tracks[ti].default_duration;
            let mut def_size = tracks[ti].default_size;
            if flags & 8 != 0 {
                def_dur = be32(&d[p..]);
                p += 4;
            }
            if flags & 0x10 != 0 {
                def_size = be32(&d[p..]);
                p += 4;
            }
            let def_flags = if flags & 0x20 != 0 { be32(&d[p..]) } else { 0 };
            if let Some(i) = find(&kids, b"tfdt") {
                let t = read_vec(src, kids[i].body, 12)?;
                frag_dts[ti] = if t[0] == 1 { be64(&t[4..]) as i64 } else { be32(&t[4..]) as i64 };
            }
            let mut data_off = base;
            for r in kids.iter().filter(|b| &b.kind == b"trun") {
                let d = read_vec(src, r.body, (r.end - r.body) as usize)?;
                let f = be32(&d) & 0xff_ffff;
                let n = be32(&d[4..]) as usize;
                let mut p = 8;
                if f & 1 != 0 {
                    data_off = (base as i64 + be32(&d[p..]) as i32 as i64) as u64;
                    p += 4;
                }
                let mut first_flags = None;
                if f & 4 != 0 {
                    first_flags = Some(be32(&d[p..]));
                    p += 4;
                }
                for k in 0..n {
                    let mut dur = def_dur;
                    let mut size = def_size;
                    let mut sflags = if k == 0 { first_flags.unwrap_or(def_flags) } else { def_flags };
                    let mut cts = 0i32;
                    if f & 0x100 != 0 {
                        dur = be32(d.get(p..p + 4).ok_or(Error::Invalid("trun"))?);
                        p += 4;
                    }
                    if f & 0x200 != 0 {
                        size = be32(d.get(p..p + 4).ok_or(Error::Invalid("trun"))?);
                        p += 4;
                    }
                    if f & 0x400 != 0 {
                        sflags = be32(d.get(p..p + 4).ok_or(Error::Invalid("trun"))?);
                        p += 4;
                    }
                    if f & 0x800 != 0 {
                        cts = be32(d.get(p..p + 4).ok_or(Error::Invalid("trun"))?) as i32;
                        p += 4;
                    }
                    let key = sflags & 0x0001_0000 == 0;
                    tracks[ti].samples.push(Sample { offset: data_off, size, dts: frag_dts[ti], cts, key });
                    data_off += size as u64;
                    frag_dts[ti] += dur as i64;
                }
            }
        }
        Ok(())
    }

    fn to_us(t: &Track, v: i64) -> i64 {
        ((v - t.start) as i128 * 1_000_000 / t.timescale.max(1) as i128) as i64
    }
}

impl Demuxer for Mp4 {
    fn info(&self) -> &MediaInfo {
        &self.info
    }

    fn next_packet(&mut self) -> Option<Result<Packet>> {
        // The track whose next sample has the earliest decode time.
        let mut best: Option<(usize, i64)> = None;
        for (i, t) in self.tracks.iter().enumerate() {
            if let Some(s) = t.samples.get(t.next) {
                let us = Self::to_us(t, s.dts);
                if best.map(|b| us < b.1).unwrap_or(true) {
                    best = Some((i, us));
                }
            }
        }
        let (ti, dts) = best?;
        let t = &mut self.tracks[ti];
        let s = t.samples[t.next];
        t.next += 1;
        let pts = Self::to_us(t, s.dts + s.cts as i64);
        if s.size > 64 << 20 {
            return Some(Err(Error::Invalid("sample too large")));
        }
        Some(read_vec(&mut *self.src, s.offset, s.size as usize).map(|data| Packet { track: ti, data, pts, dts, key: s.key }))
    }

    fn seek(&mut self, us: i64) -> Result<()> {
        // Video: last keyframe at or before the target; others follow it.
        let mut target = us;
        if let Some(vi) = self.tracks.iter().position(|t| t.info.kind == Kind::Video) {
            let t = &mut self.tracks[vi];
            let mut idx = 0;
            for (i, s) in t.samples.iter().enumerate() {
                if Self::to_us(t, s.dts + s.cts as i64) > us {
                    break;
                }
                if s.key {
                    idx = i;
                }
            }
            t.next = idx;
            target = t.samples.get(idx).map(|s| Self::to_us(t, s.dts)).unwrap_or(0);
        }
        for t in self.tracks.iter_mut() {
            if t.info.kind == Kind::Video {
                continue;
            }
            let i = t.samples.partition_point(|s| Self::to_us(t, s.dts) < target);
            t.next = i;
        }
        Ok(())
    }
}
