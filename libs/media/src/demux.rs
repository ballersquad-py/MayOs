//! Common demuxer interface and container detection.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Io,
    Invalid(&'static str),
    Unsupported(String),
}

pub type Result<T> = core::result::Result<T, Error>;

/// Random-access byte source (a file).
pub trait Source {
    fn len(&self) -> u64;
    /// Read up to `buf.len()` bytes at `off`; returns the count read.
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> core::result::Result<usize, ()>;
}

impl Source for Vec<u8> {
    fn len(&self) -> u64 {
        self.len() as u64
    }
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> core::result::Result<usize, ()> {
        let off = off as usize;
        if off >= Vec::len(self) {
            return Ok(0);
        }
        let n = buf.len().min(Vec::len(self) - off);
        buf[..n].copy_from_slice(&self[off..off + n]);
        Ok(n)
    }
}

/// Read exactly `n` bytes at `off`.
pub fn read_vec(src: &mut dyn Source, off: u64, n: usize) -> Result<Vec<u8>> {
    let mut v = vec![0u8; n];
    let mut done = 0;
    while done < n {
        let r = src.read_at(off + done as u64, &mut v[done..]).map_err(|_| Error::Io)?;
        if r == 0 {
            return Err(Error::Invalid("unexpected end of file"));
        }
        done += r;
    }
    Ok(v)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Codec {
    /// H.264 with an avcC record (length-prefixed NAL units).
    H264(Vec<u8>),
    /// H.264 Annex B byte stream.
    H264AnnexB,
    Mjpeg,
    /// AAC with its AudioSpecificConfig.
    Aac(Vec<u8>),
    /// AAC in ADTS frames.
    AacAdts,
    Mp3,
    /// Integer PCM: bits per sample, little endian, signed.
    Pcm { bits: u16, little_endian: bool, signed: bool },
    /// PCM 32-bit float.
    PcmFloat { little_endian: bool },
    Other(String),
}

impl Codec {
    pub fn name(&self) -> String {
        String::from(match self {
            Codec::H264(_) | Codec::H264AnnexB => "H.264",
            Codec::Mjpeg => "Motion JPEG",
            Codec::Aac(_) | Codec::AacAdts => "AAC",
            Codec::Mp3 => "MP3",
            Codec::Pcm { .. } | Codec::PcmFloat { .. } => "PCM",
            Codec::Other(s) => return s.clone(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Video,
    Audio,
}

#[derive(Debug, Clone)]
pub struct TrackInfo {
    pub kind: Kind,
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub sample_rate: u32,
    pub channels: u16,
    /// Average frame duration in microseconds (video), if known.
    pub frame_us: u64,
}

#[derive(Debug, Clone, Default)]
pub struct MediaInfo {
    pub format: &'static str,
    pub duration_us: i64,
    pub tracks: Vec<TrackInfo>,
}

#[derive(Debug, Clone)]
pub struct Packet {
    pub track: usize,
    pub data: Vec<u8>,
    /// Presentation / decoding time in microseconds.
    pub pts: i64,
    pub dts: i64,
    pub key: bool,
}

pub trait Demuxer {
    fn info(&self) -> &MediaInfo;
    /// Next packet in (roughly) decoding order; None at end of file.
    fn next_packet(&mut self) -> Option<Result<Packet>>;
    /// Seek so the next video packet is the keyframe at or before `us`.
    fn seek(&mut self, us: i64) -> Result<()>;
}

/// Identify the container from the first bytes of the file.
pub fn open(mut src: Box<dyn Source + Send>) -> Result<Box<dyn Demuxer + Send>> {
    let n = (src.len() as usize).min(64);
    let head = read_vec(&mut *src, 0, n)?;
    if head.len() >= 12 && (&head[4..8] == b"ftyp" || &head[4..8] == b"moov" || &head[4..8] == b"mdat" || &head[4..8] == b"free" || &head[4..8] == b"wide" || &head[4..8] == b"skip") {
        return Ok(Box::new(crate::mp4::Mp4::open(src)?));
    }
    if head.len() >= 4 && head[..4] == [0x1a, 0x45, 0xdf, 0xa3] {
        return Ok(Box::new(crate::mkv::Mkv::open(src)?));
    }
    if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"AVI " {
        return Ok(Box::new(crate::avi::Avi::open(src)?));
    }
    if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WAVE" {
        return Ok(Box::new(crate::rawaudio::Wav::open(src)?));
    }
    if crate::rawaudio::Frames::detect(&head) {
        return Ok(Box::new(crate::rawaudio::Frames::open(src)?));
    }
    Err(Error::Unsupported(String::from("unknown file format")))
}

/// Decode an IEEE 754 float (32 or 64 bit, big endian bytes) to an integer
/// (truncated), without using the FPU.
pub fn float_to_int(b: &[u8]) -> i64 {
    let (sign, exp, mant, bias, mbits) = match b.len() {
        4 => {
            let v = u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64;
            (v >> 31, ((v >> 23) & 0xff) as i64, v & 0x7f_ffff, 127i64, 23i64)
        }
        8 => {
            let v = u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
            (v >> 63, ((v >> 52) & 0x7ff) as i64, v & ((1u64 << 52) - 1), 1023i64, 52i64)
        }
        _ => return 0,
    };
    if exp == 0 {
        return 0;
    }
    let m = mant | (1u64 << mbits);
    let e = exp - bias - mbits;
    let v = if e >= 0 { if e > 62 - mbits { i64::MAX as u64 } else { m << e } } else if -e >= 64 { 0 } else { m >> (-e) };
    let v = v.min(i64::MAX as u64) as i64;
    if sign == 1 { -v } else { v }
}

/// Buffered sequential reader over a source.
pub struct Reader {
    pub pos: u64,
    buf: Vec<u8>,
    buf_start: u64,
}

impl Default for Reader {
    fn default() -> Self {
        Self::new(0)
    }
}

impl Reader {
    pub fn new(pos: u64) -> Reader {
        Reader { pos, buf: Vec::new(), buf_start: 0 }
    }

    pub fn seek(&mut self, pos: u64) {
        self.pos = pos;
    }

    fn fill(&mut self, src: &mut dyn Source, need: usize) -> Result<()> {
        let have_end = self.buf_start + self.buf.len() as u64;
        if self.pos >= self.buf_start && self.pos + need as u64 <= have_end {
            return Ok(());
        }
        let n = need.max(32 * 1024);
        let avail = src.len().saturating_sub(self.pos) as usize;
        let n = n.min(avail);
        if n < need {
            return Err(Error::Invalid("unexpected end of file"));
        }
        self.buf = read_vec(src, self.pos, n)?;
        self.buf_start = self.pos;
        Ok(())
    }

    pub fn bytes(&mut self, src: &mut dyn Source, n: usize) -> Result<&[u8]> {
        self.fill(src, n)?;
        let s = (self.pos - self.buf_start) as usize;
        self.pos += n as u64;
        Ok(&self.buf[s..s + n])
    }

    pub fn u8(&mut self, src: &mut dyn Source) -> Result<u8> {
        Ok(self.bytes(src, 1)?[0])
    }

    pub fn skip(&mut self, n: u64) {
        self.pos += n;
    }
}
