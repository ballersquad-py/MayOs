//! AVI demuxer (RIFF), with idx1 index or a linear scan of 'movi'.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::demux::*;

fn le16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}
fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

struct Stream {
    info: TrackInfo,
    scale: u32,
    rate: u32,
    sample_size: u32,
    block_align: u32,
    avg_bytes: u32,
    /// Chunks: (file offset of data, size, keyframe).
    chunks: Vec<(u64, u32, bool)>,
    /// Timestamp (us) of each chunk.
    times: Vec<i64>,
}

pub struct Avi {
    src: Box<dyn Source + Send>,
    info: MediaInfo,
    streams: Vec<Stream>,
    /// Interleaved order: (stream, chunk index).
    order: Vec<(u8, u32)>,
    next: usize,
}

impl Avi {
    pub fn open(mut src: Box<dyn Source + Send>) -> Result<Avi> {
        let len = src.len();
        let mut streams: Vec<Stream> = Vec::new();
        let mut movi: Option<(u64, u64)> = None;
        let mut idx1: Option<(u64, u64)> = None;
        // Walk the RIFF list.
        let mut stack = alloc::vec![(12u64, len.min(8 + le32(&read_vec(&mut *src, 4, 4)?) as u64))];
        while let Some((mut p, end)) = stack.pop() {
            while p + 8 <= end {
                let h = read_vec(&mut *src, p, 12.min((end - p) as usize))?;
                let id = [h[0], h[1], h[2], h[3]];
                let size = le32(&h[4..]) as u64;
                let body = p + 8;
                let next = body + size + (size & 1);
                match &id {
                    b"LIST" if h.len() >= 12 => {
                        let kind = [h[8], h[9], h[10], h[11]];
                        match &kind {
                            b"hdrl" => stack.push((body + 4, (body + size).min(end))),
                            b"strl" => {
                                let s = Self::parse_strl(&mut *src, body + 4, (body + size).min(end))?;
                                streams.push(s);
                            }
                            b"movi" => movi = Some((body + 4, (body + size).min(len))),
                            _ => {}
                        }
                    }
                    b"idx1" => idx1 = Some((body, size)),
                    _ => {}
                }
                if next <= p {
                    break;
                }
                p = next;
            }
        }
        let (movi_start, movi_end) = movi.ok_or(Error::Invalid("AVI without movi"))?;
        let mut order = Vec::new();
        let mut used_index = false;
        if let Some((ip, isz)) = idx1 {
            let d = read_vec(&mut *src, ip, isz.min(64 << 20) as usize)?;
            let n = d.len() / 16;
            // Offsets are relative to 'movi' (or absolute in some files).
            let first_off = if n > 0 { le32(&d[8..]) as u64 } else { 0 };
            let base = if first_off >= movi_start.saturating_sub(4) && first_off < len && n > 0 && {
                let probe = read_vec(&mut *src, first_off, 4).unwrap_or_default();
                probe[..] == d[..4]
            } {
                0
            } else {
                movi_start - 4
            };
            for k in 0..n {
                let e = &d[k * 16..k * 16 + 16];
                let Some(si) = stream_index(&e[..4]) else { continue };
                if si >= streams.len() {
                    continue;
                }
                let flags = le32(&e[4..]);
                let off = base + le32(&e[8..]) as u64 + 8;
                let size = le32(&e[12..]);
                if off + size as u64 > len {
                    continue;
                }
                let s = &mut streams[si];
                order.push((si as u8, s.chunks.len() as u32));
                s.chunks.push((off, size, flags & 0x10 != 0));
            }
            used_index = !order.is_empty();
        }
        if !used_index {
            let mut p = movi_start;
            while p + 8 <= movi_end {
                let h = read_vec(&mut *src, p, 8)?;
                let size = le32(&h[4..]) as u64;
                if &h[..4] == b"LIST" {
                    p += 12;
                    continue;
                }
                if let Some(si) = stream_index(&h[..4]) {
                    if si < streams.len() {
                        let s = &mut streams[si];
                        order.push((si as u8, s.chunks.len() as u32));
                        s.chunks.push((p + 8, size as u32, s.info.kind == Kind::Audio || s.chunks.is_empty()));
                    }
                }
                p += 8 + size + (size & 1);
            }
            // Without an index, mark H.264 IDR / all MJPEG frames as keys.
            for s in streams.iter_mut() {
                if s.info.codec == Codec::Mjpeg {
                    for c in s.chunks.iter_mut() {
                        c.2 = true;
                    }
                }
            }
        }
        // Timestamps.
        let mut duration_us = 0i64;
        for s in streams.iter_mut() {
            let mut bytes: u64 = 0;
            let rate = s.rate.max(1) as i128;
            let scale = s.scale.max(1) as i128;
            for (i, c) in s.chunks.iter().enumerate() {
                let t = if s.info.kind == Kind::Video || s.sample_size == 0 {
                    (i as i128 * scale * 1_000_000 / rate) as i64
                } else if s.block_align > 0 && s.info.sample_rate > 0 && matches!(s.info.codec, Codec::Pcm { .. }) {
                    (bytes as i128 / s.block_align as i128 * 1_000_000 / s.info.sample_rate as i128) as i64
                } else if s.avg_bytes > 0 {
                    (bytes as i128 * 1_000_000 / s.avg_bytes as i128) as i64
                } else {
                    (bytes as i128 * scale * 1_000_000 / (rate * s.sample_size.max(1) as i128)) as i64
                };
                s.times.push(t);
                bytes += c.1 as u64;
            }
            if s.info.kind == Kind::Video {
                s.info.frame_us = (scale * 1_000_000 / rate) as u64;
            }
            if let Some(&t) = s.times.last() {
                duration_us = duration_us.max(t);
            }
        }
        let info = MediaInfo { format: "AVI", duration_us, tracks: streams.iter().map(|s| s.info.clone()).collect() };
        Ok(Avi { src, info, streams, order, next: 0 })
    }

    fn parse_strl(src: &mut dyn Source, start: u64, end: u64) -> Result<Stream> {
        let mut p = start;
        let mut s = Stream {
            info: TrackInfo { kind: Kind::Video, codec: Codec::Other(String::from("unknown")), width: 0, height: 0, sample_rate: 0, channels: 0, frame_us: 0 },
            scale: 1,
            rate: 25,
            sample_size: 0,
            block_align: 0,
            avg_bytes: 0,
            chunks: Vec::new(),
            times: Vec::new(),
        };
        let mut handler = [0u8; 4];
        while p + 8 <= end {
            let h = read_vec(src, p, 8)?;
            let size = le32(&h[4..]) as u64;
            let d = read_vec(src, p + 8, size.min(4096) as usize)?;
            match &h[..4] {
                b"strh" if d.len() >= 48 => {
                    s.info.kind = if &d[..4] == b"auds" { Kind::Audio } else { Kind::Video };
                    handler.copy_from_slice(&d[4..8]);
                    s.scale = le32(&d[20..]);
                    s.rate = le32(&d[24..]);
                    s.sample_size = le32(&d[44..]);
                }
                b"strf" => {
                    if s.info.kind == Kind::Video && d.len() >= 20 {
                        s.info.width = le32(&d[4..]);
                        s.info.height = (le32(&d[8..]) as i32).unsigned_abs();
                        let c = &d[16..20];
                        s.info.codec = match c.to_ascii_uppercase().as_slice() {
                            b"MJPG" | b"AVRN" | b"LJPG" | b"JPGL" => Codec::Mjpeg,
                            b"H264" | b"X264" | b"AVC1" | b"DAVC" => Codec::H264AnnexB,
                            b"XVID" | b"DIVX" | b"DX50" | b"FMP4" | b"MP4V" => Codec::Other(String::from("MPEG-4 Part 2 (Xvid/DivX)")),
                            b"HEVC" | b"H265" => Codec::Other(String::from("HEVC (H.265)")),
                            _ => Codec::Other(format!("video '{}'", String::from_utf8_lossy(c))),
                        };
                    } else if d.len() >= 16 {
                        let tag = le16(&d[..]);
                        s.info.channels = le16(&d[2..]);
                        s.info.sample_rate = le32(&d[4..]);
                        s.avg_bytes = le32(&d[8..]);
                        s.block_align = le16(&d[12..]) as u32;
                        let bits = le16(&d[14..]);
                        s.info.codec = match tag {
                            1 => Codec::Pcm { bits, little_endian: true, signed: bits > 8 },
                            3 => Codec::PcmFloat { little_endian: true },
                            0x55 | 0x50 => Codec::Mp3,
                            0xff | 0x1610 | 0x706d => Codec::AacAdts,
                            0x2000 => Codec::Other(String::from("AC-3")),
                            t => Codec::Other(format!("audio format {:#x}", t)),
                        };
                        if tag == 0xff && d.len() >= 20 {
                            // Raw AAC with an AudioSpecificConfig after cbSize.
                            let extra = le16(&d[16..]) as usize;
                            if extra >= 2 && d.len() >= 18 + extra {
                                s.info.codec = Codec::Aac(d[18..18 + extra].to_vec());
                            }
                        }
                    }
                }
                _ => {}
            }
            p += 8 + size + (size & 1);
        }
        Ok(s)
    }
}

fn stream_index(id: &[u8]) -> Option<usize> {
    if id[0].is_ascii_digit() && id[1].is_ascii_digit() {
        Some(((id[0] - b'0') * 10 + (id[1] - b'0')) as usize)
    } else {
        None
    }
}

impl Demuxer for Avi {
    fn info(&self) -> &MediaInfo {
        &self.info
    }

    fn next_packet(&mut self) -> Option<Result<Packet>> {
        loop {
            let &(si, ci) = self.order.get(self.next)?;
            self.next += 1;
            let s = &self.streams[si as usize];
            let (off, size, key) = s.chunks[ci as usize];
            if size == 0 {
                continue;
            }
            let t = s.times[ci as usize];
            return Some(read_vec(&mut *self.src, off, size as usize).map(|data| Packet { track: si as usize, data, pts: t, dts: t, key }));
        }
    }

    fn seek(&mut self, us: i64) -> Result<()> {
        let vi = self.streams.iter().position(|s| s.info.kind == Kind::Video);
        let mut target = 0usize;
        match vi {
            Some(v) => {
                for (i, &(si, ci)) in self.order.iter().enumerate() {
                    if si as usize != v {
                        continue;
                    }
                    let s = &self.streams[v];
                    if s.times[ci as usize] > us {
                        break;
                    }
                    if s.chunks[ci as usize].2 {
                        target = i;
                    }
                }
            }
            None => {
                target = self.order.iter().position(|&(si, ci)| self.streams[si as usize].times[ci as usize] >= us).unwrap_or(0);
            }
        }
        // Start slightly earlier so interleaved audio before the keyframe is included.
        self.next = target;
        Ok(())
    }
}
