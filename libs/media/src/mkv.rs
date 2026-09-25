//! Matroska / WebM demuxer.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::demux::*;

const SEGMENT: u32 = 0x1853_8067;
const SEEKHEAD: u32 = 0x114D_9B74;
const INFO: u32 = 0x1549_A966;
const TRACKS: u32 = 0x1654_AE6B;
const CLUSTER: u32 = 0x1F43_B675;
const CUES: u32 = 0x1C53_BB6B;

struct MkvTrack {
    number: u64,
    info: TrackInfo,
    /// Header-stripping compression: bytes removed from every frame.
    strip: Vec<u8>,
}

pub struct Mkv {
    src: Box<dyn Source + Send>,
    info: MediaInfo,
    tracks: Vec<MkvTrack>,
    segment_start: u64,
    segment_end: u64,
    timescale_ns: u64,
    first_cluster: u64,
    cues: Vec<(i64, u64)>,
    rd: Reader,
    cluster_end: u64,
    cluster_time: i64,
    pending: VecDeque<Packet>,
}

/// Read an EBML variable-length integer; `strip` removes the length marker.
fn vint(rd: &mut Reader, src: &mut dyn Source, strip: bool) -> Result<(u64, usize)> {
    let first = rd.u8(src)?;
    if first == 0 {
        return Err(Error::Invalid("EBML vint"));
    }
    let len = first.leading_zeros() as usize + 1;
    let mut v = if strip { (first as u64) & (0xff >> len) } else { first as u64 };
    for _ in 1..len {
        v = (v << 8) | rd.u8(src)? as u64;
    }
    Ok((v, len))
}

/// Element header: (id, data size or None when unknown).
fn element(rd: &mut Reader, src: &mut dyn Source) -> Result<(u32, Option<u64>)> {
    let (id, _) = vint(rd, src, false)?;
    let (size, len) = vint(rd, src, true)?;
    let unknown = size == (1u64 << (7 * len)) - 1;
    Ok((id as u32, if unknown { None } else { Some(size) }))
}

fn uint(b: &[u8]) -> u64 {
    b.iter().fold(0u64, |a, &x| (a << 8) | x as u64)
}

impl Mkv {
    pub fn open(mut src: Box<dyn Source + Send>) -> Result<Mkv> {
        let len = src.len();
        let mut rd = Reader::new(0);
        // EBML header.
        let (id, size) = element(&mut rd, &mut *src)?;
        if id != 0x1A45_DFA3 {
            return Err(Error::Invalid("not EBML"));
        }
        let hdr = rd.bytes(&mut *src, size.unwrap_or(0) as usize)?.to_vec();
        let webm = hdr.windows(4).any(|w| w == b"webm");
        let (id, size) = element(&mut rd, &mut *src)?;
        if id != SEGMENT {
            return Err(Error::Invalid("no Matroska segment"));
        }
        let segment_start = rd.pos;
        let segment_end = size.map(|s| segment_start + s).unwrap_or(len).min(len);
        let mut m = Mkv {
            src,
            info: MediaInfo { format: if webm { "WebM" } else { "Matroska" }, ..Default::default() },
            tracks: Vec::new(),
            segment_start,
            segment_end,
            timescale_ns: 1_000_000,
            first_cluster: 0,
            cues: Vec::new(),
            rd: Reader::new(segment_start),
            cluster_end: 0,
            cluster_time: 0,
            pending: VecDeque::new(),
        };
        let mut cues_pos = None;
        let mut duration_raw: Option<Vec<u8>> = None;
        // Walk top-level elements until the first cluster.
        let mut pos = segment_start;
        while pos < segment_end {
            let mut r = Reader::new(pos);
            let (id, size) = element(&mut r, &mut *m.src)?;
            let body = r.pos;
            if id == CLUSTER {
                m.first_cluster = pos;
                break;
            }
            let Some(size) = size else { return Err(Error::Invalid("unknown-size element")) };
            match id {
                INFO => {
                    let d = read_vec(&mut *m.src, body, size as usize)?;
                    let mut p = 0;
                    while p < d.len() {
                        let (cid, csz, hl) = parse_child(&d[p..])?;
                        let v = &d[p + hl..p + hl + csz];
                        match cid {
                            0x2AD7B1 => m.timescale_ns = uint(v).max(1),
                            0x4489 => duration_raw = Some(v.to_vec()),
                            _ => {}
                        }
                        p += hl + csz;
                    }
                }
                TRACKS => {
                    let d = read_vec(&mut *m.src, body, size as usize)?;
                    m.parse_tracks(&d)?;
                }
                SEEKHEAD => {
                    let d = read_vec(&mut *m.src, body, size as usize)?;
                    let mut p = 0;
                    while p < d.len() {
                        let (sid_el, csz, hl) = parse_child(&d[p..])?;
                        if sid_el != 0x4DBB {
                            p += hl + csz;
                            continue;
                        }
                        let seek = &d[p + hl..p + hl + csz];
                        let (mut sid, mut spos) = (0u64, 0u64);
                        let mut q = 0;
                        while q < seek.len() {
                            let (cid, csz, hl) = parse_child(&seek[q..])?;
                            let v = &seek[q + hl..q + hl + csz];
                            match cid {
                                0x53AB => sid = uint(v),
                                0x53AC => spos = uint(v),
                                _ => {}
                            }
                            q += hl + csz;
                        }
                        if sid as u32 == CUES {
                            cues_pos = Some(segment_start + spos);
                        }
                        p += hl + csz;
                    }
                }
                CUES => cues_pos = Some(pos),
                _ => {}
            }
            pos = body + size;
        }
        if m.first_cluster == 0 {
            return Err(Error::Invalid("no clusters"));
        }
        if let Some(raw) = duration_raw {
            // Duration is a float in timescale units.
            let ticks = float_to_int(&raw);
            m.info.duration_us = (ticks as i128 * m.timescale_ns as i128 / 1000) as i64;
        }
        if let Some(cp) = cues_pos {
            let _ = m.parse_cues(cp);
        }
        if m.tracks.is_empty() {
            return Err(Error::Invalid("no tracks"));
        }
        m.info.tracks = m.tracks.iter().map(|t| t.info.clone()).collect();
        m.rd = Reader::new(m.first_cluster);
        Ok(m)
    }

    fn parse_tracks(&mut self, d: &[u8]) -> Result<()> {
        let mut p = 0;
        while p < d.len() {
            let (id, sz, hl) = parse_child(&d[p..])?;
            if id == 0xAE {
                if let Some(t) = parse_track(&d[p + hl..p + hl + sz])? {
                    self.tracks.push(t);
                }
            }
            p += hl + sz;
        }
        Ok(())
    }

    fn parse_cues(&mut self, pos: u64) -> Result<()> {
        let mut r = Reader::new(pos);
        let (id, size) = element(&mut r, &mut *self.src)?;
        if id != CUES {
            return Ok(());
        }
        let d = read_vec(&mut *self.src, r.pos, size.unwrap_or(0).min(64 << 20) as usize)?;
        let mut p = 0;
        while p < d.len() {
            let (cid, csz, hl) = parse_child(&d[p..])?;
            if cid == 0xBB {
                let cp = &d[p + hl..p + hl + csz];
                let (mut time, mut cpos) = (0u64, None);
                let mut q = 0;
                while q < cp.len() {
                    let (id2, sz2, hl2) = parse_child(&cp[q..])?;
                    let v = &cp[q + hl2..q + hl2 + sz2];
                    if id2 == 0xB3 {
                        time = uint(v);
                    } else if id2 == 0xB7 {
                        let mut k = 0;
                        while k < v.len() {
                            let (id3, sz3, hl3) = parse_child(&v[k..])?;
                            if id3 == 0xF1 {
                                cpos = Some(uint(&v[k + hl3..k + hl3 + sz3]));
                            }
                            k += hl3 + sz3;
                        }
                    }
                    q += hl2 + sz2;
                }
                if let Some(cpos) = cpos {
                    let us = (time as i128 * self.timescale_ns as i128 / 1000) as i64;
                    self.cues.push((us, self.segment_start + cpos));
                }
            }
            p += hl + csz;
        }
        self.cues.sort();
        Ok(())
    }

    /// Parse blocks until at least one packet is queued.
    fn fill(&mut self) -> Result<bool> {
        loop {
            if self.rd.pos >= self.segment_end {
                return Ok(false);
            }
            if self.cluster_end != 0 && self.rd.pos >= self.cluster_end {
                self.cluster_end = 0;
            }
            let start = self.rd.pos;
            let (id, size) = match element(&mut self.rd, &mut *self.src) {
                Ok(v) => v,
                Err(_) => return Ok(false),
            };
            match id {
                CLUSTER => {
                    self.cluster_end = size.map(|s| self.rd.pos + s).unwrap_or(u64::MAX);
                    continue;
                }
                0xE7 => {
                    let v = self.rd.bytes(&mut *self.src, size.unwrap_or(0) as usize)?.to_vec();
                    self.cluster_time = uint(&v) as i64;
                }
                0xA3 => {
                    let size = size.ok_or(Error::Invalid("block size"))? as usize;
                    let data = self.rd.bytes(&mut *self.src, size)?.to_vec();
                    self.block(&data, true, true)?;
                    return Ok(true);
                }
                0xA0 => {
                    let size = size.ok_or(Error::Invalid("block group size"))? as usize;
                    let g = self.rd.bytes(&mut *self.src, size)?.to_vec();
                    let mut p = 0;
                    let mut block = None;
                    let mut refd = false;
                    while p < g.len() {
                        let (cid, csz, hl) = parse_child(&g[p..])?;
                        match cid {
                            0xA1 => block = Some(g[p + hl..p + hl + csz].to_vec()),
                            0xFB => refd = true,
                            _ => {}
                        }
                        p += hl + csz;
                    }
                    if let Some(b) = block {
                        self.block(&b, false, !refd)?;
                        return Ok(true);
                    }
                }
                _ => {
                    // Skip anything else (including top-level elements
                    // between clusters, like Cues or Tags).
                    match size {
                        Some(s) => self.rd.skip(s),
                        None => {
                            if start == self.rd.pos {
                                return Ok(false);
                            }
                        }
                    }
                }
            }
        }
    }

    fn block(&mut self, b: &[u8], simple: bool, group_key: bool) -> Result<()> {
        let mut p = 0;
        let first = *b.first().ok_or(Error::Invalid("empty block"))?;
        let len = first.leading_zeros() as usize + 1;
        if len > 8 || b.len() < len + 3 {
            return Err(Error::Invalid("block header"));
        }
        let mut num = (first as u64) & (0xff >> len);
        for i in 1..len {
            num = (num << 8) | b[i] as u64;
        }
        p += len;
        let rel = i16::from_be_bytes([b[p], b[p + 1]]) as i64;
        let flags = b[p + 2];
        p += 3;
        let Some(ti) = self.tracks.iter().position(|t| t.number == num) else { return Ok(()) };
        let key = if simple { flags & 0x80 != 0 } else { group_key };
        let t_us = ((self.cluster_time + rel) as i128 * self.timescale_ns as i128 / 1000) as i64;
        let lacing = (flags >> 1) & 3;
        let mut frames: Vec<&[u8]> = Vec::new();
        if lacing == 0 {
            frames.push(&b[p..]);
        } else {
            let n = *b.get(p).ok_or(Error::Invalid("lacing"))? as usize + 1;
            p += 1;
            let mut sizes = vec![0usize; n];
            match lacing {
                1 => {
                    for s in sizes.iter_mut().take(n - 1) {
                        loop {
                            let v = *b.get(p).ok_or(Error::Invalid("xiph lacing"))?;
                            p += 1;
                            *s += v as usize;
                            if v != 255 {
                                break;
                            }
                        }
                    }
                }
                3 => {
                    let mut rd = &b[p..];
                    let (first, l) = parse_vint_slice(rd, true)?;
                    rd = &rd[l..];
                    p += l;
                    sizes[0] = first as usize;
                    let mut prev = first as i64;
                    for s in sizes.iter_mut().take(n - 1).skip(1) {
                        let (raw, l) = parse_vint_slice(rd, true)?;
                        rd = &rd[l..];
                        p += l;
                        let bias = (1i64 << (7 * l - 1)) - 1;
                        prev += raw as i64 - bias;
                        *s = prev.max(0) as usize;
                    }
                }
                _ => {
                    let each = (b.len() - p) / n;
                    for s in sizes.iter_mut().take(n - 1) {
                        *s = each;
                    }
                }
            }
            let used: usize = sizes[..n - 1].iter().sum();
            sizes[n - 1] = (b.len() - p).checked_sub(used).ok_or(Error::Invalid("lacing sizes"))?;
            for s in sizes {
                frames.push(b.get(p..p + s).ok_or(Error::Invalid("lace"))?);
                p += s;
            }
        }
        let t = &self.tracks[ti];
        let step = if frames.len() > 1 { t.info.frame_us as i64 } else { 0 };
        for (i, f) in frames.into_iter().enumerate() {
            let mut data = Vec::with_capacity(t.strip.len() + f.len());
            data.extend_from_slice(&t.strip);
            data.extend_from_slice(f);
            let pts = t_us + step * i as i64;
            self.pending.push_back(Packet { track: ti, data, pts, dts: pts, key });
        }
        Ok(())
    }
}

fn parse_vint_slice(b: &[u8], strip: bool) -> Result<(u64, usize)> {
    let first = *b.first().ok_or(Error::Invalid("vint"))?;
    if first == 0 {
        return Err(Error::Invalid("vint"));
    }
    let len = first.leading_zeros() as usize + 1;
    if b.len() < len {
        return Err(Error::Invalid("vint"));
    }
    let mut v = if strip { (first as u64) & (0xff >> len) } else { first as u64 };
    for i in 1..len {
        v = (v << 8) | b[i] as u64;
    }
    Ok((v, len))
}

/// (id, size, header length) of a child element in a buffer.
fn parse_child(b: &[u8]) -> Result<(u32, usize, usize)> {
    let (id, l1) = parse_vint_slice(b, false)?;
    let (size, l2) = parse_vint_slice(&b[l1..], true)?;
    let hl = l1 + l2;
    let size = size as usize;
    if hl + size > b.len() {
        return Err(Error::Invalid("element size"));
    }
    Ok((id as u32, size, hl))
}

fn parse_track(d: &[u8]) -> Result<Option<MkvTrack>> {
    let mut number = 0;
    let mut ttype = 0;
    let mut codec_id = String::new();
    let mut private = Vec::new();
    let mut default_dur = 0u64;
    let (mut w, mut h, mut rate, mut ch, mut bits) = (0u32, 0u32, 8000u32, 1u16, 16u16);
    let mut strip = Vec::new();
    let mut p = 0;
    while p < d.len() {
        let (id, sz, hl) = parse_child(&d[p..])?;
        let v = &d[p + hl..p + hl + sz];
        match id {
            0xD7 => number = uint(v),
            0x83 => ttype = uint(v),
            0x86 => codec_id = String::from_utf8_lossy(v).into_owned(),
            0x63A2 => private = v.to_vec(),
            0x23E383 => default_dur = uint(v),
            0xE0 => {
                let mut q = 0;
                while q < v.len() {
                    let (cid, csz, chl) = parse_child(&v[q..])?;
                    let cv = &v[q + chl..q + chl + csz];
                    match cid {
                        0xB0 => w = uint(cv) as u32,
                        0xBA => h = uint(cv) as u32,
                        _ => {}
                    }
                    q += chl + csz;
                }
            }
            0xE1 => {
                let mut q = 0;
                while q < v.len() {
                    let (cid, csz, chl) = parse_child(&v[q..])?;
                    let cv = &v[q + chl..q + chl + csz];
                    match cid {
                        0xB5 => rate = float_to_int(cv) as u32,
                        0x9F => ch = uint(cv) as u16,
                        0x6264 => bits = uint(cv) as u16,
                        _ => {}
                    }
                    q += chl + csz;
                }
            }
            0x6D80 => {
                // ContentEncodings: only header stripping is supported.
                let mut stack = vec![v];
                while let Some(buf) = stack.pop() {
                    let mut q = 0;
                    while q < buf.len() {
                        let Ok((cid, csz, chl)) = parse_child(&buf[q..]) else { break };
                        let cv = &buf[q + chl..q + chl + csz];
                        match cid {
                            0x6240 | 0x5034 => stack.push(cv),
                            0x4255 => strip = cv.to_vec(),
                            0x4254 if uint(cv) != 3 => return Ok(None),
                            _ => {}
                        }
                        q += chl + csz;
                    }
                }
            }
            _ => {}
        }
        p += hl + sz;
    }
    let kind = match ttype {
        1 => Kind::Video,
        2 => Kind::Audio,
        _ => return Ok(None),
    };
    let codec = match codec_id.as_str() {
        "V_MPEG4/ISO/AVC" => Codec::H264(private),
        "V_MJPEG" => Codec::Mjpeg,
        "A_AAC" => Codec::Aac(private),
        c if c.starts_with("A_AAC/") => {
            // Old-style ID without CodecPrivate: build a config.
            let idx = crate::aac::RATES.iter().position(|&r| r == rate).unwrap_or(4) as u8;
            let aot = if c.contains("LC") { 2u8 } else { 2 };
            Codec::Aac(vec![(aot << 3) | (idx >> 1), ((idx & 1) << 7) | ((ch as u8) << 3)])
        }
        "A_MPEG/L3" => Codec::Mp3,
        "A_PCM/INT/LIT" => Codec::Pcm { bits, little_endian: true, signed: bits > 8 },
        "A_PCM/INT/BIG" => Codec::Pcm { bits, little_endian: false, signed: bits > 8 },
        "A_PCM/FLOAT/IEEE" => Codec::PcmFloat { little_endian: true },
        "V_MPEGH/ISO/HEVC" => Codec::Other(String::from("HEVC (H.265)")),
        "V_VP9" => Codec::Other(String::from("VP9")),
        "V_VP8" => Codec::Other(String::from("VP8")),
        "V_AV1" => Codec::Other(String::from("AV1")),
        "A_OPUS" => Codec::Other(String::from("Opus")),
        "A_VORBIS" => Codec::Other(String::from("Vorbis")),
        "A_AC3" | "A_EAC3" => Codec::Other(String::from("Dolby Digital (AC-3)")),
        "A_DTS" => Codec::Other(String::from("DTS")),
        "A_FLAC" => Codec::Other(String::from("FLAC")),
        other => Codec::Other(format!("'{}'", other)),
    };
    Ok(Some(MkvTrack {
        number,
        info: TrackInfo { kind, codec, width: w, height: h, sample_rate: rate, channels: ch, frame_us: default_dur / 1000 },
        strip,
    }))
}

impl Demuxer for Mkv {
    fn info(&self) -> &MediaInfo {
        &self.info
    }

    fn next_packet(&mut self) -> Option<Result<Packet>> {
        loop {
            if let Some(p) = self.pending.pop_front() {
                return Some(Ok(p));
            }
            match self.fill() {
                Ok(true) => continue,
                Ok(false) => return None,
                Err(e) => return Some(Err(e)),
            }
        }
    }

    fn seek(&mut self, us: i64) -> Result<()> {
        self.pending.clear();
        self.cluster_end = 0;
        let pos = self.cues.iter().rev().find(|c| c.0 <= us).map(|c| c.1).unwrap_or(self.first_cluster);
        self.rd = Reader::new(pos);
        // Skip forward to the keyframe at or before the target.
        let vi = self.tracks.iter().position(|t| t.info.kind == Kind::Video);
        if self.cues.is_empty() || vi.is_none() {
            // Without an index, read from the start and drop packets before
            // the target (the caller decodes from the first keyframe).
            return Ok(());
        }
        Ok(())
    }
}
