//! Decoder wrappers used by players: pick a decoder for a track, feed it
//! packets, get timestamped frames and PCM back.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::demux::{Codec, Kind, Packet, TrackInfo};
use crate::h264;

pub enum FrameData {
    Yuv(h264::Frame),
    Argb(Arc<image::Image>),
}

pub struct VideoFrame {
    pub pts: i64,
    pub width: usize,
    pub height: usize,
    pub data: FrameData,
}

enum VInner {
    H264(Box<h264::Decoder>),
    Mjpeg,
}

pub struct VideoDecoder {
    inner: VInner,
    /// Presentation times of packets fed but not yet output (sorted).
    pts: Vec<i64>,
    ready: VecDeque<VideoFrame>,
    annexb: bool,
    /// After a reset (seek, skip), pictures before the first keyframe's
    /// time may reference pictures we never decoded ("open GOP"): drop them.
    min_pts: Option<i64>,
    resync: bool,
}

pub fn unsupported_message(codec: &Codec) -> String {
    format!(
        "This file uses {}, which MayOS can't play yet. Convert it on your PC with:\nffmpeg -i input -c:v libx264 -c:a aac output.mp4",
        codec.name()
    )
}

impl VideoDecoder {
    pub fn new(t: &TrackInfo) -> Result<VideoDecoder, String> {
        let (inner, annexb) = match &t.codec {
            Codec::H264(avcc) => {
                let mut d = h264::Decoder::new();
                d.configure_avcc(avcc).map_err(|e| format!("H.264 setup: {}", e.as_str()))?;
                (VInner::H264(Box::new(d)), false)
            }
            Codec::H264AnnexB => (VInner::H264(Box::new(h264::Decoder::new())), true),
            Codec::Mjpeg => (VInner::Mjpeg, false),
            other => return Err(unsupported_message(other)),
        };
        Ok(VideoDecoder { inner, pts: Vec::new(), ready: VecDeque::new(), annexb, min_pts: None, resync: true })
    }

    pub fn decode(&mut self, p: &Packet) {
        if self.resync && p.key {
            self.resync = false;
            self.min_pts = Some(p.pts);
        }
        match &mut self.inner {
            VInner::H264(d) => {
                let i = self.pts.partition_point(|&x| x < p.pts);
                self.pts.insert(i, p.pts);
                let skipped = d.skipped_pictures;
                let _ = d.decode(&p.data);
                if d.skipped_pictures != skipped {
                    // No picture will come out for this packet: forget its
                    // time, or every later frame would be shown too early.
                    if let Some(k) = self.pts.iter().position(|&x| x == p.pts) {
                        self.pts.remove(k);
                    }
                }
                if self.annexb {
                    // AVI chunks hold whole access units.
                    d.flush_picture();
                }
                self.collect();
            }
            VInner::Mjpeg => {
                if let Ok(img) = image::decode(&p.data) {
                    let (w, h) = (img.width as usize, img.height as usize);
                    self.ready.push_back(VideoFrame { pts: p.pts, width: w, height: h, data: FrameData::Argb(Arc::new(img)) });
                }
            }
        }
    }

    fn collect(&mut self) {
        if let VInner::H264(d) = &mut self.inner {
            while let Some(f) = d.next_frame() {
                let pts = if self.pts.is_empty() { 0 } else { self.pts.remove(0) };
                if self.min_pts.map(|m| pts < m).unwrap_or(false) {
                    continue;
                }
                let (w, h) = (f.width, f.height);
                self.ready.push_back(VideoFrame { pts, width: w, height: h, data: FrameData::Yuv(f) });
            }
        }
    }

    /// Output the frames still held back for reordering (end of stream).
    pub fn flush(&mut self) {
        if let VInner::H264(d) = &mut self.inner {
            d.flush();
        }
        self.collect();
    }

    pub fn reset(&mut self) {
        if let VInner::H264(d) = &mut self.inner {
            d.reset();
        }
        self.pts.clear();
        self.ready.clear();
        self.resync = true;
        self.min_pts = None;
    }

    /// Skip decoding pictures that nothing references (when running late).
    pub fn set_skip_nonref(&mut self, on: bool) {
        if let VInner::H264(d) = &mut self.inner {
            d.skip_nonref = on;
        }
    }

    pub fn next_frame(&mut self) -> Option<VideoFrame> {
        self.ready.pop_front()
    }

    pub fn pending(&self) -> usize {
        self.ready.len()
    }
}

impl VideoFrame {
    /// Scale (bilinear) and convert to 0xAARRGGBB into `dst` (dw x dh,
    /// row stride `ds`).
    pub fn render(&self, dst: &mut [u32], dw: usize, dh: usize, ds: usize) {
        match &self.data {
            FrameData::Yuv(f) => {
                let b = &f.buf;
                if dw == f.width && dh == f.height {
                    let mut tmp = vec![0u32; dw * dh];
                    f.to_argb(&mut tmp);
                    for y in 0..dh {
                        dst[y * ds..y * ds + dw].copy_from_slice(&tmp[y * dw..y * dw + dw]);
                    }
                    return;
                }
                crate::yuv::scale_to_argb(&b.y, &b.cb, &b.cr, b.width, b.width / 2, f.crop_x, f.crop_y, f.width, f.height, f.matrix == 1, f.full_range, dst, dw, dh, ds);
            }
            FrameData::Argb(img) => {
                crate::yuv::scale_argb(&img.pixels, img.width as usize, img.height as usize, dst, dw, dh, ds);
            }
        }
    }
}

enum AInner {
    Aac(Box<crate::aac::AacDecoder>),
    AacAdts(Option<Box<crate::aac::AacDecoder>>),
    Mp3(Box<crate::mp3::Mp3Decoder>),
    Pcm { bits: u16, le: bool, signed: bool, channels: usize },
    Float { le: bool, channels: usize },
}

pub struct AudioDecoder {
    inner: AInner,
    pub sample_rate: u32,
    /// Output channels (1 or 2).
    pub channels: usize,
}

fn float_to_i16(bits: u32) -> i16 {
    let sign = bits >> 31;
    let exp = ((bits >> 23) & 0xff) as i32;
    let mant = (bits & 0x7f_ffff) | 0x80_0000;
    if exp == 0 {
        return 0;
    }
    // value = mant * 2^(exp - 150); we want value * 32768.
    let sh = exp - 150 + 15;
    let v: i64 = if sh >= 0 { if sh > 20 { i64::MAX } else { (mant as i64) << sh } } else if sh < -40 { 0 } else { (mant as i64) >> (-sh) };
    let v = v.clamp(0, 32767);
    if sign == 1 { -(v as i16) } else { v as i16 }
}

impl AudioDecoder {
    pub fn new(t: &TrackInfo) -> Result<AudioDecoder, String> {
        let ch_in = t.channels.max(1) as usize;
        let (inner, rate, ch) = match &t.codec {
            Codec::Aac(asc) => {
                let d = crate::aac::AacDecoder::from_config(asc).map_err(|e| format!("AAC: {:?}", e))?;
                let (r, c) = (d.sample_rate, d.channels);
                (AInner::Aac(Box::new(d)), r, c)
            }
            Codec::AacAdts => (AInner::AacAdts(None), t.sample_rate, ch_in.min(2)),
            Codec::Mp3 => (AInner::Mp3(Box::new(crate::mp3::Mp3Decoder::new())), t.sample_rate, ch_in.min(2)),
            Codec::Pcm { bits, little_endian, signed } => (AInner::Pcm { bits: *bits, le: *little_endian, signed: *signed, channels: ch_in }, t.sample_rate, ch_in.min(2)),
            Codec::PcmFloat { little_endian } => (AInner::Float { le: *little_endian, channels: ch_in }, t.sample_rate, ch_in.min(2)),
            other => return Err(unsupported_message(other)),
        };
        Ok(AudioDecoder { inner, sample_rate: rate, channels: ch })
    }

    /// Decode a packet into interleaved samples (`channels` per frame).
    pub fn decode(&mut self, p: &Packet) -> Vec<i16> {
        let mut out = Vec::new();
        match &mut self.inner {
            AInner::Aac(d) => {
                let _ = d.decode_frame(&p.data, &mut out);
                self.sample_rate = d.sample_rate;
                self.channels = d.channels;
            }
            AInner::AacAdts(dec) => {
                let mut pos = 0;
                while let Some((hdr, len, sfi, ch)) = crate::aac::AacDecoder::parse_adts(&p.data[pos..]) {
                    if pos + len > p.data.len() {
                        break;
                    }
                    if dec.is_none() {
                        *dec = crate::aac::AacDecoder::new(sfi, ch).ok().map(Box::new);
                    }
                    if let Some(d) = dec {
                        let _ = d.decode_frame(&p.data[pos + hdr..pos + len], &mut out);
                        self.sample_rate = d.sample_rate;
                        self.channels = d.channels;
                    }
                    pos += len;
                }
            }
            AInner::Mp3(d) => {
                let mut pos = 0;
                while let Some(h) = crate::mp3::Header::parse(&p.data[pos..]) {
                    if pos + h.frame_len > p.data.len() {
                        break;
                    }
                    let _ = d.decode_frame(&p.data[pos..pos + h.frame_len], &mut out);
                    self.sample_rate = h.sample_rate;
                    self.channels = h.channels;
                    pos += h.frame_len;
                }
            }
            AInner::Pcm { bits, le, signed, channels } => {
                let bps = (*bits as usize).div_ceil(8).max(1);
                let frame = bps * *channels;
                let n = p.data.len() / frame.max(1);
                let oc = (*channels).min(2);
                out.reserve(n * oc);
                for i in 0..n {
                    let mut acc = [0i32; 2];
                    for c in 0..*channels {
                        let s = &p.data[i * frame + c * bps..i * frame + c * bps + bps];
                        let v: i32 = match bps {
                            1 => (s[0] as i32 - if *signed { 0 } else { 128 }) << 8,
                            2 => (if *le { i16::from_le_bytes([s[0], s[1]]) } else { i16::from_be_bytes([s[0], s[1]]) }) as i32,
                            3 => (if *le { i32::from_le_bytes([0, s[0], s[1], s[2]]) } else { i32::from_be_bytes([s[0], s[1], s[2], 0]) }) >> 16,
                            _ => (if *le { i32::from_le_bytes([s[0], s[1], s[2], s[3]]) } else { i32::from_be_bytes([s[0], s[1], s[2], s[3]]) }) >> 16,
                        };
                        acc[c % 2] += v;
                    }
                    let div = if *channels > 2 { (*channels as i32 + 1) / 2 } else { 1 };
                    for a in acc.iter().take(oc) {
                        out.push((*a / div).clamp(-32768, 32767) as i16);
                    }
                }
            }
            AInner::Float { le, channels } => {
                let frame = 4 * *channels;
                let n = p.data.len() / frame.max(1);
                let oc = (*channels).min(2);
                for i in 0..n {
                    for c in 0..oc {
                        let s = &p.data[i * frame + c * 4..i * frame + c * 4 + 4];
                        let bits = if *le { u32::from_le_bytes([s[0], s[1], s[2], s[3]]) } else { u32::from_be_bytes([s[0], s[1], s[2], s[3]]) };
                        out.push(float_to_i16(bits));
                    }
                }
            }
        }
        out
    }
}

/// Pick the first video and audio tracks.
pub fn choose_tracks(tracks: &[TrackInfo]) -> (Option<usize>, Option<usize>) {
    (tracks.iter().position(|t| t.kind == Kind::Video), tracks.iter().position(|t| t.kind == Kind::Audio))
}
