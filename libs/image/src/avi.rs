//! AVI (RIFF) container parsing for Motion-JPEG video with PCM audio.

use alloc::vec::Vec;

use crate::ImageError;

pub struct Avi<'a> {
    pub width: u32,
    pub height: u32,
    /// Frame duration in microseconds.
    pub frame_us: u64,
    /// Byte ranges of the video frames (each a JPEG image).
    pub frames: Vec<&'a [u8]>,
    /// PCM audio, if any: (channels, sample rate, bits, interleaved data).
    pub audio: Option<(u16, u32, u16, Vec<u8>)>,
    pub codec: [u8; 4],
}

fn le16(b: &[u8], o: usize) -> u32 {
    u16::from_le_bytes([b[o], b[o + 1]]) as u32
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

pub fn is_avi(data: &[u8]) -> bool {
    data.len() > 12 && &data[..4] == b"RIFF" && &data[8..12] == b"AVI "
}

struct Stream {
    kind: [u8; 4],
    handler: [u8; 4],
    scale: u32,
    rate: u32,
    format: Vec<u8>,
}

pub fn parse(data: &[u8]) -> Result<Avi<'_>, ImageError> {
    if !is_avi(data) {
        return Err(ImageError::Unsupported);
    }
    let mut avi = Avi { width: 0, height: 0, frame_us: 33_333, frames: Vec::new(), audio: None, codec: *b"????" };
    let mut streams: Vec<Stream> = Vec::new();
    let mut audio_data = Vec::new();
    // Walk chunks, descending into LISTs.
    fn walk<'a>(data: &'a [u8], mut pos: usize, end: usize, f: &mut dyn FnMut(&'a [u8], &'a [u8])) {
        while pos + 8 <= end {
            let id = &data[pos..pos + 4];
            let len = le32(data, pos + 4) as usize;
            let body_start = pos + 8;
            let body_end = (body_start + len).min(end);
            if id == b"LIST" || id == b"RIFF" {
                if body_start + 4 <= body_end {
                    f(&data[body_start..body_start + 4], &[]);
                    walk(data, body_start + 4, body_end, f);
                }
            } else {
                f(id, &data[body_start..body_end]);
            }
            pos = body_start + len + (len & 1);
        }
    }
    walk(data, 12, data.len(), &mut |id, body| match id {
        b"avih" if body.len() >= 40 => {
            let us = le32(body, 0);
            if us > 0 {
                avi.frame_us = us as u64;
            }
            avi.width = le32(body, 32);
            avi.height = le32(body, 36);
        }
        b"strh" if body.len() >= 28 => {
            let mut kind = [0u8; 4];
            kind.copy_from_slice(&body[..4]);
            let mut handler = [0u8; 4];
            handler.copy_from_slice(&body[4..8]);
            streams.push(Stream { kind, handler, scale: le32(body, 20), rate: le32(body, 24), format: Vec::new() });
        }
        b"strf" => {
            if let Some(s) = streams.last_mut() {
                s.format = body.to_vec();
            }
        }
        _ if id.len() == 4 && id[0].is_ascii_digit() && id[1].is_ascii_digit() => {
            let n = ((id[0] - b'0') * 10 + (id[1] - b'0')) as usize;
            match (&id[2..4], streams.get(n).map(|s| &s.kind)) {
                (b"dc" | b"db", Some(b"vids")) => avi.frames.push(body),
                (b"wb", Some(b"auds")) => audio_data.extend_from_slice(body),
                _ => {}
            }
        }
        _ => {}
    });
    let video = streams.iter().find(|s| &s.kind == b"vids").ok_or(ImageError::Unsupported)?;
    avi.codec = video.handler;
    if video.format.len() >= 20 {
        let compression = &video.format[16..20];
        if avi.width == 0 {
            avi.width = le32(&video.format, 4);
            avi.height = le32(&video.format, 8);
        }
        if !compression.eq_ignore_ascii_case(b"MJPG") && !video.handler.eq_ignore_ascii_case(b"MJPG") {
            return Err(ImageError::Unsupported);
        }
        avi.codec.copy_from_slice(compression);
    }
    if video.scale > 0 && video.rate > 0 {
        avi.frame_us = video.scale as u64 * 1_000_000 / video.rate as u64;
    }
    if let Some(a) = streams.iter().find(|s| &s.kind == b"auds")
        && a.format.len() >= 16
        && le16(&a.format, 0) == 1
    {
        avi.audio = Some((le16(&a.format, 2) as u16, le32(&a.format, 4), le16(&a.format, 14) as u16, audio_data));
    }
    if avi.frames.is_empty() {
        return Err(ImageError::Corrupt);
    }
    Ok(avi)
}
