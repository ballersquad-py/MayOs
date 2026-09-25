//! WAV (RIFF PCM) decoding into 48 kHz stereo 16-bit samples.

use alloc::vec::Vec;

pub struct WavInfo {
    pub channels: u16,
    pub rate: u32,
    pub bits: u16,
}

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Decode a PCM WAV file (8/16-bit, mono/stereo, any rate).
pub fn decode(b: &[u8]) -> Result<(WavInfo, Vec<i16>), &'static str> {
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Err("not a WAV file");
    }
    let mut info = None;
    let mut data: Option<&[u8]> = None;
    let mut i = 12;
    while i + 8 <= b.len() {
        let id = &b[i..i + 4];
        let len = u32le(b, i + 4) as usize;
        let body = &b[i + 8..(i + 8 + len).min(b.len())];
        match id {
            b"fmt " if body.len() >= 16 => {
                if u16le(body, 0) != 1 {
                    return Err("only uncompressed PCM WAV files are supported");
                }
                info = Some(WavInfo { channels: u16le(body, 2), rate: u32le(body, 4), bits: u16le(body, 14) });
            }
            b"data" => data = Some(body),
            _ => {}
        }
        i += 8 + len + (len & 1);
    }
    let info = info.ok_or("missing fmt chunk")?;
    let data = data.ok_or("missing data chunk")?;
    if !(info.channels == 1 || info.channels == 2) || !(info.bits == 8 || info.bits == 16) || info.rate == 0 {
        return Err("unsupported WAV format (need 8/16-bit mono or stereo)");
    }
    let bytes = (info.bits / 8) as usize;
    let frame = bytes * info.channels as usize;
    let n = data.len() / frame;
    let sample = |f: usize, ch: usize| -> i32 {
        let o = f * frame + ch.min(info.channels as usize - 1) * bytes;
        if bytes == 1 { (data[o] as i32 - 128) << 8 } else { i16::from_le_bytes([data[o], data[o + 1]]) as i32 }
    };
    // Resample to 48 kHz with linear interpolation (16.16 fixed point).
    let step = ((info.rate as u64) << 16) / 48000;
    let out_frames = (n as u64 * 48000 / info.rate as u64) as usize;
    let mut out = Vec::with_capacity(out_frames * 2);
    let mut pos: u64 = 0;
    for _ in 0..out_frames {
        let f = (pos >> 16) as usize;
        let frac = (pos & 0xffff) as i32;
        for ch in 0..2 {
            let a = sample(f.min(n - 1), ch);
            let b2 = sample((f + 1).min(n - 1), ch);
            out.push((a + (((b2 - a) * frac) >> 16)) as i16);
        }
        pos += step;
    }
    Ok((info, out))
}

/// Encode 48 kHz stereo samples as a 16-bit WAV file.
pub fn encode(samples: &[i16]) -> Vec<u8> {
    let data_len = samples.len() * 2;
    let mut v = Vec::with_capacity(44 + data_len);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&48000u32.to_le_bytes());
    v.extend_from_slice(&(48000u32 * 4).to_le_bytes());
    v.extend_from_slice(&4u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(data_len as u32).to_le_bytes());
    for s in samples {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}
