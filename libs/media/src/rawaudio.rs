//! Plain audio files: MP3 and ADTS AAC frame streams, and WAV.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use crate::demux::*;

/// MP3 or ADTS file: a sequence of self-describing frames.
pub struct Frames {
    src: Box<dyn Source + Send>,
    info: MediaInfo,
    start: u64,
    pos: u64,
    time: i64,
    adts: bool,
    /// Bytes per second estimate for seeking.
    byte_rate: u64,
    rd: Reader,
}

fn id3_len(h: &[u8]) -> u64 {
    if h.len() >= 10 && &h[..3] == b"ID3" {
        10 + (((h[6] as u64 & 0x7f) << 21) | ((h[7] as u64 & 0x7f) << 14) | ((h[8] as u64 & 0x7f) << 7) | (h[9] as u64 & 0x7f))
    } else {
        0
    }
}

impl Frames {
    pub fn detect(head: &[u8]) -> bool {
        if head.len() >= 3 && &head[..3] == b"ID3" {
            return true;
        }
        head.len() >= 4 && (crate::mp3::Header::parse(head).is_some() || crate::aac::AacDecoder::parse_adts(head).is_some())
    }

    pub fn open(mut src: Box<dyn Source + Send>) -> Result<Frames> {
        let len = src.len();
        let h = read_vec(&mut *src, 0, (len as usize).min(10))?;
        let mut start = id3_len(&h);
        // Find the first frame.
        let probe = read_vec(&mut *src, start, (len - start.min(len)).min(64 * 1024) as usize)?;
        let mut found = None;
        for i in 0..probe.len().saturating_sub(4) {
            if let Some(hd) = crate::mp3::Header::parse(&probe[i..]) {
                // Require a second frame right after, to avoid false syncs.
                if probe.len() > i + hd.frame_len + 4 && crate::mp3::Header::parse(&probe[i + hd.frame_len..]).is_none() {
                    continue;
                }
                found = Some((i, false, hd.sample_rate, hd.channels as u16, hd.bitrate as u64 / 8));
                break;
            }
            if let Some((_, flen, sfi, ch)) = crate::aac::AacDecoder::parse_adts(&probe[i..]) {
                if sfi < 12 {
                    found = Some((i, true, crate::aac::RATES[sfi], ch.max(1) as u16, 0));
                    let _ = flen;
                    break;
                }
            }
        }
        let (off, adts, rate, ch, mut byte_rate) = found.ok_or(Error::Invalid("no audio frames found"))?;
        start += off as u64;
        let mut duration_us = 0;
        if !adts {
            // Xing/Info header gives the frame count for VBR files.
            let first = &probe[off..];
            let hd = crate::mp3::Header::parse(first).unwrap();
            let side = match (hd.lsf, hd.channels) {
                (false, 1) => 17,
                (false, _) => 32,
                (true, 1) => 9,
                (true, _) => 17,
            };
            let x = 4 + side;
            if first.len() > x + 12 && (&first[x..x + 4] == b"Xing" || &first[x..x + 4] == b"Info") {
                let flags = u32::from_be_bytes([first[x + 4], first[x + 5], first[x + 6], first[x + 7]]);
                if flags & 1 != 0 {
                    let frames = u32::from_be_bytes([first[x + 8], first[x + 9], first[x + 10], first[x + 11]]) as i64;
                    duration_us = frames * hd.samples() as i64 * 1_000_000 / hd.sample_rate as i64;
                }
                // Skip the (silent) info frame.
                start += hd.frame_len as u64;
            }
            if duration_us == 0 && byte_rate > 0 {
                duration_us = ((len - start) as i128 * 1_000_000 / byte_rate as i128) as i64;
            }
            if duration_us > 0 {
                byte_rate = ((len - start) as i128 * 1_000_000 / duration_us as i128) as u64;
            }
        } else {
            // Estimate from the first frames' sizes.
            let mut p = off;
            let mut frames = 0;
            let mut bytes = 0;
            while let Some((_, flen, _, _)) = crate::aac::AacDecoder::parse_adts(&probe[p.min(probe.len())..]) {
                bytes += flen;
                frames += 1;
                p += flen;
                if frames >= 50 || flen == 0 {
                    break;
                }
            }
            if frames > 0 {
                byte_rate = (bytes as u64 * rate as u64) / (frames as u64 * 1024);
                duration_us = ((len - start) as i128 * 1_000_000 / byte_rate.max(1) as i128) as i64;
            }
        }
        let codec = if adts { Codec::AacAdts } else { Codec::Mp3 };
        let info = MediaInfo {
            format: if adts { "AAC" } else { "MP3" },
            duration_us,
            tracks: alloc::vec![TrackInfo { kind: Kind::Audio, codec, width: 0, height: 0, sample_rate: rate, channels: ch, frame_us: 0 }],
        };
        Ok(Frames { src, info, start, pos: start, time: 0, adts, byte_rate: byte_rate.max(1), rd: Reader::new(start) })
    }
}

impl Demuxer for Frames {
    fn info(&self) -> &MediaInfo {
        &self.info
    }

    fn next_packet(&mut self) -> Option<Result<Packet>> {
        let len = self.src.len();
        let mut resync = 0;
        loop {
            if self.pos + 4 > len || resync > 64 * 1024 {
                return None;
            }
            self.rd.seek(self.pos);
            let h = match self.rd.bytes(&mut *self.src, 8.min((len - self.pos) as usize)) {
                Ok(h) => h.to_vec(),
                Err(e) => return Some(Err(e)),
            };
            let (flen, samples, rate) = if self.adts {
                match crate::aac::AacDecoder::parse_adts(&h) {
                    Some((_, flen, sfi, _)) if sfi < 12 => (flen, 1024, crate::aac::RATES[sfi]),
                    _ => (0, 0, 1),
                }
            } else {
                match crate::mp3::Header::parse(&h) {
                    Some(hd) => (hd.frame_len, hd.samples(), hd.sample_rate),
                    None => (0, 0, 1),
                }
            };
            if flen < 7 || self.pos + flen as u64 > len {
                if flen >= 7 {
                    return None;
                }
                self.pos += 1;
                resync += 1;
                continue;
            }
            self.rd.seek(self.pos);
            let data = match self.rd.bytes(&mut *self.src, flen) {
                Ok(d) => d.to_vec(),
                Err(e) => return Some(Err(e)),
            };
            self.pos += flen as u64;
            let t = self.time;
            self.time += samples as i64 * 1_000_000 / rate as i64;
            return Some(Ok(Packet { track: 0, data, pts: t, dts: t, key: true }));
        }
    }

    fn seek(&mut self, us: i64) -> Result<()> {
        let off = (us.max(0) as u128 * self.byte_rate as u128 / 1_000_000) as u64;
        self.pos = (self.start + off).min(self.src.len());
        self.time = us.max(0);
        Ok(())
    }
}

/// WAV file with PCM data.
pub struct Wav {
    src: Box<dyn Source + Send>,
    info: MediaInfo,
    data_start: u64,
    data_end: u64,
    pos: u64,
    block_align: u64,
}

impl Wav {
    pub fn open(mut src: Box<dyn Source + Send>) -> Result<Wav> {
        let len = src.len();
        let mut p = 12u64;
        let mut fmt: Option<Vec<u8>> = None;
        while p + 8 <= len {
            let h = read_vec(&mut *src, p, 8)?;
            let size = u32::from_le_bytes([h[4], h[5], h[6], h[7]]) as u64;
            if &h[..4] == b"fmt " {
                fmt = Some(read_vec(&mut *src, p + 8, size.min(64) as usize)?);
            } else if &h[..4] == b"data" {
                let f = fmt.ok_or(Error::Invalid("WAV without fmt"))?;
                let tag = u16::from_le_bytes([f[0], f[1]]);
                let ch = u16::from_le_bytes([f[2], f[3]]);
                let rate = u32::from_le_bytes([f[4], f[5], f[6], f[7]]);
                let align = u16::from_le_bytes([f[12], f[13]]).max(1) as u64;
                let bits = u16::from_le_bytes([f[14], f[15]]);
                let codec = match tag {
                    1 | 0xfffe => Codec::Pcm { bits, little_endian: true, signed: bits > 8 },
                    3 => Codec::PcmFloat { little_endian: true },
                    0x55 => Codec::Mp3,
                    t => Codec::Other(alloc::format!("WAV format {:#x}", t)),
                };
                let data_end = (p + 8 + size).min(len);
                let duration_us = ((data_end - p - 8) / align) as i64 * 1_000_000 / rate.max(1) as i64;
                let info = MediaInfo {
                    format: "WAV",
                    duration_us,
                    tracks: alloc::vec![TrackInfo { kind: Kind::Audio, codec, width: 0, height: 0, sample_rate: rate, channels: ch, frame_us: 0 }],
                };
                return Ok(Wav { src, info, data_start: p + 8, data_end, pos: p + 8, block_align: align });
            }
            p += 8 + size + (size & 1);
        }
        Err(Error::Unsupported(String::from("WAV without audio data")))
    }
}

impl Demuxer for Wav {
    fn info(&self) -> &MediaInfo {
        &self.info
    }

    fn next_packet(&mut self) -> Option<Result<Packet>> {
        if self.pos >= self.data_end {
            return None;
        }
        let chunk = (self.block_align * 4096).min(self.data_end - self.pos);
        let t = ((self.pos - self.data_start) / self.block_align) as i64 * 1_000_000 / self.info.tracks[0].sample_rate.max(1) as i64;
        let r = read_vec(&mut *self.src, self.pos, chunk as usize);
        self.pos += chunk;
        Some(r.map(|data| Packet { track: 0, data, pts: t, dts: t, key: true }))
    }

    fn seek(&mut self, us: i64) -> Result<()> {
        let frames = (us.max(0) as u128 * self.info.tracks[0].sample_rate as u128 / 1_000_000) as u64;
        self.pos = (self.data_start + frames * self.block_align).min(self.data_end);
        Ok(())
    }
}
