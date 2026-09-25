//! MP3 (MPEG-1/2/2.5 Audio Layer III) decoder, fixed point.

use alloc::vec;
use alloc::vec::Vec;

use crate::bits::BitReader;
use crate::dsp::pow43_table;
use crate::tables::*;
use crate::vlc::Vlc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Invalid(&'static str),
    Unsupported(&'static str),
}

pub type Result<T> = core::result::Result<T, Error>;

const BITRATE_V1: [u32; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
const BITRATE_V2: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
const RATES: [u32; 3] = [44100, 48000, 32000];

/// Fractional bits of spectral and subband samples.
const FRAC: u32 = 24;

#[derive(Clone, Copy, Debug)]
pub struct Header {
    pub lsf: bool,
    pub mpeg25: bool,
    pub crc: bool,
    pub bitrate: u32,
    pub sample_rate: u32,
    /// 0..9 (MPEG-1 44.1/48/32, MPEG-2 22.05/24/16, MPEG-2.5 11.025/12/8).
    pub sr_index: usize,
    pub padding: bool,
    pub mode: u8,
    pub mode_ext: u8,
    pub channels: usize,
    pub frame_len: usize,
}

impl Header {
    pub fn parse(b: &[u8]) -> Option<Header> {
        if b.len() < 4 {
            return None;
        }
        let h = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        if h >> 21 != 0x7ff {
            return None;
        }
        let ver = (h >> 19) & 3;
        let layer = (h >> 17) & 3;
        if ver == 1 || layer != 1 {
            return None;
        }
        let bri = ((h >> 12) & 15) as usize;
        let sri = ((h >> 10) & 3) as usize;
        if bri == 0 || bri == 15 || sri == 3 {
            return None;
        }
        let lsf = ver != 3;
        let mpeg25 = ver == 0;
        let sample_rate = RATES[sri] >> (lsf as u32 + mpeg25 as u32);
        let bitrate = if lsf { BITRATE_V2[bri] } else { BITRATE_V1[bri] } * 1000;
        let padding = (h >> 9) & 1 == 1;
        let mode = ((h >> 6) & 3) as u8;
        let frame_len = (if lsf { 72 } else { 144 }) * bitrate as usize / sample_rate as usize + padding as usize;
        Some(Header {
            lsf,
            mpeg25,
            crc: (h >> 16) & 1 == 0,
            bitrate,
            sample_rate,
            sr_index: sri + 3 * (lsf as usize + mpeg25 as usize),
            padding,
            mode,
            mode_ext: ((h >> 4) & 3) as u8,
            channels: if mode == 3 { 1 } else { 2 },
            frame_len,
        })
    }

    pub fn samples(&self) -> usize {
        if self.lsf { 576 } else { 1152 }
    }
}

#[derive(Clone, Copy)]
struct Granule {
    part2_3: usize,
    big_values: usize,
    global_gain: i32,
    sf_compress: u32,
    block_type: u8,
    switch_point: bool,
    table_select: [u8; 3],
    subblock_gain: [i32; 3],
    region_size: [usize; 3],
    preflag: bool,
    scalefac_scale: bool,
    count1_table: u8,
    scfsi: u8,
    scale_factors: [u8; 40],
    long_end: usize,
    short_start: usize,
}

impl Default for Granule {
    fn default() -> Granule {
        Granule {
            part2_3: 0,
            big_values: 0,
            global_gain: 0,
            sf_compress: 0,
            block_type: 0,
            switch_point: false,
            table_select: [0; 3],
            subblock_gain: [0; 3],
            region_size: [0; 3],
            preflag: false,
            scalefac_scale: false,
            count1_table: 0,
            scfsi: 0,
            scale_factors: [0; 40],
            long_end: 22,
            short_start: 13,
        }
    }
}

pub struct Mp3Decoder {
    huff: Vec<Option<Vlc>>,
    quad: [Vlc; 2],
    pow43: Vec<u32>,
    band_index: [[usize; 23]; 9],
    window: Vec<i32>,
    reservoir: Vec<u8>,
    overlap: [[[i32; 18]; 32]; 2],
    v: [[i32; 1024]; 2],
    voff: [usize; 2],
    granules: [[Granule; 2]; 2],
    pub sample_rate: u32,
    pub channels: usize,
}

impl Default for Mp3Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Mp3Decoder {
    pub fn new() -> Mp3Decoder {
        let mut huff: Vec<Option<Vlc>> = vec![None];
        let mut off = 0;
        for &m in MP3_HUFF_SIZES_M1.iter() {
            let n = m as usize + 1;
            let syms: Vec<i32> = MP3_HUFF_SYMS[off..off + n].iter().map(|&s| s as i32).collect();
            huff.push(Some(Vlc::from_lengths(&MP3_HUFF_LENS[off..off + n], &syms)));
            off += n;
        }
        let q = |t: usize| -> Vlc {
            let codes: Vec<(u32, u8, i32)> = (0..16).map(|i| (MP3_QUAD_CODES[t * 16 + i] as u32, MP3_QUAD_BITS[t * 16 + i], i as i32)).collect();
            Vlc::new(&codes)
        };
        let mut band_index = [[0usize; 23]; 9];
        for (i, bi) in band_index.iter_mut().enumerate() {
            let mut k = 0;
            for j in 0..22 {
                bi[j] = k;
                k += MP3_BAND_SIZE_LONG[i * 22 + j] as usize / 2;
            }
            bi[22] = k;
        }
        let mut window = vec![0i32; 512];
        for i in 0..257 {
            let mut v = MP3_ENWINDOW[i];
            window[i.min(511)] = v;
            if i & 63 != 0 {
                v = -v;
            }
            if i != 0 && i < 512 {
                window[512 - i] = v;
            }
        }
        let mut pow43 = pow43_table();
        // Values up to 8206 occur with linbits; extend the table.
        for q in 8192..8208u64 {
            let x = f64_free_cbrt::Q::from(q).pow43();
            pow43.push(x);
        }
        Mp3Decoder {
            huff,
            quad: [q(0), q(1)],
            pow43,
            band_index,
            window,
            reservoir: Vec::new(),
            overlap: [[[0; 18]; 32]; 2],
            v: [[0; 1024]; 2],
            voff: [0; 2],
            granules: [[Granule::default(); 2]; 2],
            sample_rate: 0,
            channels: 0,
        }
    }

    /// Decode one complete frame (header included). Appends interleaved
    /// samples (`channels` per sample frame).
    pub fn decode_frame(&mut self, frame: &[u8], out: &mut Vec<i16>) -> Result<()> {
        let h = Header::parse(frame).ok_or(Error::Invalid("mp3 header"))?;
        if frame.len() < h.frame_len {
            return Err(Error::Invalid("mp3 frame truncated"));
        }
        self.sample_rate = h.sample_rate;
        self.channels = h.channels;
        let side_start = 4 + if h.crc { 2 } else { 0 };
        let side_len = match (h.lsf, h.channels) {
            (false, 1) => 17,
            (false, _) => 32,
            (true, 1) => 9,
            (true, _) => 17,
        };
        if side_start + side_len > h.frame_len {
            return Err(Error::Invalid("mp3 side info"));
        }
        let mut br = BitReader::new(&frame[side_start..side_start + side_len]);
        let nch = h.channels;
        let ngr = if h.lsf { 1 } else { 2 };
        let main_data_begin;
        if h.lsf {
            main_data_begin = br.bits(8) as usize;
            br.skip(nch as u32);
        } else {
            main_data_begin = br.bits(9) as usize;
            br.skip(if nch == 2 { 3 } else { 5 });
            for ch in 0..nch {
                self.granules[ch][0].scfsi = 0;
                self.granules[ch][1].scfsi = br.bits(4) as u8;
            }
        }
        for gr in 0..ngr {
            for ch in 0..nch {
                let g = &mut self.granules[ch][gr];
                g.part2_3 = br.bits(12) as usize;
                g.big_values = br.bits(9) as usize;
                if g.big_values > 288 {
                    return Err(Error::Invalid("big_values"));
                }
                g.global_gain = br.bits(8) as i32;
                if h.mode == 1 && h.mode_ext & 3 == 2 {
                    g.global_gain -= 2; // M/S only: include 1/sqrt(2)
                }
                g.sf_compress = br.bits(if h.lsf { 9 } else { 4 });
                if br.flag() {
                    g.block_type = br.bits(2) as u8;
                    if g.block_type == 0 {
                        return Err(Error::Invalid("block type"));
                    }
                    g.switch_point = br.flag();
                    g.table_select = [br.bits(5) as u8, br.bits(5) as u8, 0];
                    for i in 0..3 {
                        g.subblock_gain[i] = br.bits(3) as i32;
                    }
                    let r0 = if g.block_type == 2 {
                        if h.sr_index != 8 { 18 } else { 36 }
                    } else if h.sr_index <= 2 {
                        18
                    } else if h.sr_index != 8 {
                        27
                    } else {
                        54
                    };
                    g.region_size = [r0, 288, 288];
                } else {
                    g.block_type = 0;
                    g.switch_point = false;
                    g.table_select = [br.bits(5) as u8, br.bits(5) as u8, br.bits(5) as u8];
                    g.subblock_gain = [0; 3];
                    let ra1 = br.bits(4) as usize;
                    let ra2 = br.bits(3) as usize;
                    let bi = &self.band_index[h.sr_index];
                    g.region_size = [bi[ra1 + 1], bi[(ra1 + ra2 + 2).min(22)], 288];
                }
                // Region boundaries -> sizes (in pairs).
                let mut j = 0;
                for i in 0..3 {
                    let k = g.region_size[i].min(g.big_values).max(j);
                    g.region_size[i] = k - j;
                    j = k;
                }
                if g.block_type == 2 {
                    if g.switch_point {
                        g.long_end = if h.sr_index <= 2 { 8 } else { 6 };
                        g.short_start = 3;
                    } else {
                        g.long_end = 0;
                        g.short_start = 0;
                    }
                } else {
                    g.short_start = 13;
                    g.long_end = 22;
                }
                g.preflag = if h.lsf { false } else { br.flag() };
                g.scalefac_scale = br.flag();
                g.count1_table = br.bits(1) as u8;
            }
        }
        // Bit reservoir.
        let main = &frame[side_start + side_len..h.frame_len];
        let have = self.reservoir.len();
        let ok = main_data_begin <= have;
        let mut data = Vec::with_capacity(main_data_begin + main.len());
        if ok {
            data.extend_from_slice(&self.reservoir[have - main_data_begin..]);
        }
        data.extend_from_slice(main);
        self.reservoir.extend_from_slice(main);
        if self.reservoir.len() > 4096 {
            let cut = self.reservoir.len() - 4096;
            self.reservoir.drain(..cut);
        }
        let mut sb = vec![[[0i32; 32]; 18]; nch * ngr];
        let mut br = BitReader::new(&data);
        for gr in 0..ngr {
            let mut xr = [[0i32; 576]; 2];
            for ch in 0..nch {
                if !ok {
                    continue;
                }
                let pos = br.position();
                self.scale_factors(&mut br, &h, ch, gr);
                let exps = self.exponents(&h, &self.granules[ch][gr]);
                let end = pos + self.granules[ch][gr].part2_3;
                self.huffman(&mut br, &self.granules[ch][gr].clone(), &exps, end, &mut xr[ch]);
                br.set_position(end);
            }
            if h.mode == 1 && nch == 2 {
                self.stereo(&h, gr, &mut xr);
            }
            for ch in 0..nch {
                let g = self.granules[ch][gr];
                reorder(&h, &g, &mut xr[ch]);
                antialias(&g, &mut xr[ch]);
                self.hybrid(&g, ch, &xr[ch], &mut sb[ch * ngr + gr]);
            }
        }
        // Polyphase synthesis.
        let n = ngr * 18 * 32;
        let base = out.len();
        out.resize(base + n * nch, 0);
        for ch in 0..nch {
            for gr in 0..ngr {
                for t in 0..18 {
                    let mut pcm = [0i32; 32];
                    let s = sb[ch * ngr + gr][t];
                    self.synth(ch, &s, &mut pcm);
                    for (j, &v) in pcm.iter().enumerate() {
                        let idx = (gr * 18 + t) * 32 + j;
                        out[base + idx * nch + ch] = ((v + (1 << (FRAC - 16))) >> (FRAC - 15)).clamp(-32768, 32767) as i16;
                    }
                }
            }
        }
        Ok(())
    }

    fn scale_factors(&mut self, br: &mut BitReader, h: &Header, ch: usize, gr: usize) {
        let mut g = self.granules[ch][gr];
        let mut sf = [0u8; 40];
        if !h.lsf {
            let slen1 = MP3_SLEN[g.sf_compress as usize] as u32;
            let slen2 = MP3_SLEN[16 + g.sf_compress as usize] as u32;
            let mut j = 0;
            if g.block_type == 2 {
                let n = if g.switch_point { 17 } else { 18 };
                for _ in 0..n {
                    sf[j] = br.bits(slen1) as u8;
                    j += 1;
                }
                for _ in 0..18 {
                    sf[j] = br.bits(slen2) as u8;
                    j += 1;
                }
            } else {
                let prev = self.granules[ch][0].scale_factors;
                for k in 0..4 {
                    let n = if k == 0 { 6 } else { 5 };
                    if g.scfsi & (8 >> k) == 0 || gr == 0 {
                        let slen = if k < 2 { slen1 } else { slen2 };
                        for _ in 0..n {
                            sf[j] = br.bits(slen) as u8;
                            j += 1;
                        }
                    } else {
                        for _ in 0..n {
                            sf[j] = prev[j];
                            j += 1;
                        }
                    }
                }
            }
        } else {
            let tindex = if g.block_type == 2 { if g.switch_point { 2 } else { 1 } } else { 0 };
            let mut s = g.sf_compress as usize;
            let mut slen = [0usize; 4];
            let split = |s: &mut usize, n: usize| -> usize {
                if n == 0 {
                    return 0;
                }
                let d = *s % n;
                *s /= n;
                d
            };
            let tindex2;
            let (n1, n2, n3);
            if h.mode_ext & 1 != 0 && ch == 1 {
                s >>= 1;
                if s < 180 {
                    (n1, n2, n3) = (6, 6, 0);
                    tindex2 = 3;
                } else if s < 244 {
                    s -= 180;
                    (n1, n2, n3) = (4, 4, 0);
                    tindex2 = 4;
                } else {
                    s -= 244;
                    (n1, n2, n3) = (3, 0, 0);
                    tindex2 = 5;
                }
            } else if s < 400 {
                (n1, n2, n3) = (5, 4, 4);
                tindex2 = 0;
            } else if s < 500 {
                s -= 400;
                (n1, n2, n3) = (5, 4, 0);
                tindex2 = 1;
            } else {
                s -= 500;
                (n1, n2, n3) = (3, 0, 0);
                tindex2 = 2;
                g.preflag = true;
            }
            slen[3] = split(&mut s, n3);
            slen[2] = split(&mut s, n2);
            slen[1] = split(&mut s, n1);
            slen[0] = s;
            let mut j = 0;
            for k in 0..4 {
                let n = MP3_LSF_NSF[tindex2 * 12 + tindex * 4 + k] as usize;
                for _ in 0..n {
                    if j < 40 {
                        sf[j] = br.bits(slen[k] as u32) as u8;
                    }
                    j += 1;
                }
            }
        }
        g.scale_factors = sf;
        self.granules[ch][gr] = g;
    }

    /// Per-coefficient exponent in quarter steps (bitstream order).
    fn exponents(&self, h: &Header, g: &Granule) -> [i16; 576] {
        let mut e = [0i16; 576];
        let gain = g.global_gain - 210;
        let shift = g.scalefac_scale as i32 + 1;
        let mut p = 0;
        for i in 0..g.long_end {
            let pre = if g.preflag { MP3_PRETAB[i] as i32 } else { 0 };
            let v = gain - ((g.scale_factors[i] as i32 + pre) << shift);
            for _ in 0..MP3_BAND_SIZE_LONG[h.sr_index * 22 + i] {
                if p < 576 {
                    e[p] = v as i16;
                    p += 1;
                }
            }
        }
        if g.short_start < 13 {
            let gains = [gain - (g.subblock_gain[0] << 3), gain - (g.subblock_gain[1] << 3), gain - (g.subblock_gain[2] << 3)];
            let mut k = g.long_end;
            for i in g.short_start..13 {
                let len = MP3_BAND_SIZE_SHORT[h.sr_index * 13 + i];
                for l in 0..3 {
                    let v = gains[l] - ((g.scale_factors[k.min(39)] as i32) << shift);
                    k += 1;
                    for _ in 0..len {
                        if p < 576 {
                            e[p] = v as i16;
                            p += 1;
                        }
                    }
                }
            }
        }
        e
    }

    #[inline]
    fn requant(&self, v: i32, exp: i16) -> i32 {
        if v == 0 {
            return 0;
        }
        let a = (v.unsigned_abs() as usize).min(self.pow43.len() - 1);
        let e = exp as i32;
        let m = (self.pow43[a] as i64 * POW2_QUARTER[(e & 3) as usize] as i64) >> 30; // Q13
        let shift = (e >> 2) + FRAC as i32 - 13;
        let r = if shift >= 0 { (m << shift.min(40)).min(1 << 30) } else if shift > -63 { (m + (1i64 << (-shift - 1))) >> -shift } else { 0 };
        let r = r as i32;
        if v < 0 { -r } else { r }
    }

    fn huffman(&self, br: &mut BitReader, g: &Granule, exps: &[i16; 576], end: usize, xr: &mut [i32; 576]) {
        let mut s = 0usize;
        for i in 0..3 {
            let pairs = g.region_size[i];
            if pairs == 0 {
                continue;
            }
            let t = g.table_select[i] as usize;
            let (idx, linbits) = (MP3_HUFF_DATA[t * 2] as usize, MP3_HUFF_DATA[t * 2 + 1] as u32);
            let Some(vlc) = self.huff.get(idx).and_then(|v| v.as_ref()) else {
                s += 2 * pairs;
                continue;
            };
            for _ in 0..pairs {
                if br.position() >= end || s >= 576 {
                    break;
                }
                let sym = vlc.read(br).unwrap_or(0);
                let mut vals = [(sym >> 4) & 15, sym & 15];
                for v in vals.iter_mut() {
                    if linbits > 0 && *v == 15 {
                        *v += br.bits(linbits) as i32;
                    }
                    if *v != 0 && br.bit() == 1 {
                        *v = -*v;
                    }
                }
                xr[s] = self.requant(vals[0], exps[s]);
                xr[s + 1] = self.requant(vals[1], exps[s + 1]);
                s += 2;
            }
        }
        let quad = &self.quad[g.count1_table as usize];
        while s + 4 <= 576 {
            let pos = br.position();
            if pos >= end {
                break;
            }
            let code = quad.read(br).unwrap_or(0);
            let mut vals = [(code >> 3) & 1, (code >> 2) & 1, (code >> 1) & 1, code & 1];
            for v in vals.iter_mut() {
                if *v != 0 && br.bit() == 1 {
                    *v = -*v;
                }
            }
            if br.position() > end {
                // Overread: the last quadruple is not part of this granule.
                br.set_position(pos);
                break;
            }
            for k in 0..4 {
                xr[s + k] = self.requant(vals[k], exps[s + k]);
            }
            s += 4;
        }
    }

    fn stereo(&self, h: &Header, gr: usize, xr: &mut [[i32; 576]; 2]) {
        let ms = h.mode_ext & 2 != 0;
        let isr = h.mode_ext & 1 != 0;
        const ISQRT2: i64 = 759250125; // 1/sqrt(2) in Q30
        if !isr {
            if ms {
                for i in 0..576 {
                    let (a, b) = (xr[0][i], xr[1][i]);
                    xr[0][i] = a.saturating_add(b);
                    xr[1][i] = a.saturating_sub(b);
                }
            }
            return;
        }
        let g1 = self.granules[1][gr];
        let lsf_tab = g1.sf_compress & 1;
        let sf_max = if h.lsf { 16 } else { 7 };
        let is_gain = |sf: usize| -> (i64, i64) {
            if !h.lsf {
                (MP3_IS_RATIO[sf * 2] as i64, MP3_IS_RATIO[sf * 2 + 1] as i64)
            } else {
                let e = -((lsf_tab as i32 + 1) * ((sf as i32 + 1) >> 1));
                let f = (POW2_QUARTER[(e & 3) as usize] as i64) >> (-(e >> 2)) as u32;
                if sf & 1 == 1 { (f, 1 << 30) } else { (1 << 30, f) }
            }
        };
        let ms_band = |xr: &mut [[i32; 576]; 2], start: usize, len: usize| {
            if ms {
                for j in start..start + len {
                    let (a, b) = (xr[0][j] as i64, xr[1][j] as i64);
                    xr[0][j] = (((a + b) * ISQRT2) >> 30) as i32;
                    xr[1][j] = (((a - b) * ISQRT2) >> 30) as i32;
                }
            }
        };
        let mut pos = 576;
        let mut nz_short = [false; 3];
        let mut k = (13 - g1.short_start) * 3 + g1.long_end - 3;
        let mut i = 12isize;
        while i >= g1.short_start as isize {
            if i != 11 {
                k -= 3;
            }
            let len = MP3_BAND_SIZE_SHORT[h.sr_index * 13 + i as usize] as usize;
            for l in (0..3).rev() {
                pos -= len;
                let mut handled = false;
                if !nz_short[l] {
                    if xr[1][pos..pos + len].iter().any(|&v| v != 0) {
                        nz_short[l] = true;
                    } else {
                        let sf = g1.scale_factors[(k + l).min(39)] as usize;
                        if sf < sf_max {
                            let (v1, v2) = is_gain(sf);
                            for j in pos..pos + len {
                                let t = xr[0][j] as i64;
                                xr[0][j] = ((t * v1) >> 30) as i32;
                                xr[1][j] = ((t * v2) >> 30) as i32;
                            }
                            handled = true;
                        }
                    }
                }
                if !handled {
                    ms_band(xr, pos, len);
                }
            }
            i -= 1;
        }
        let mut nz = nz_short[0] || nz_short[1] || nz_short[2];
        for i in (0..g1.long_end).rev() {
            let len = MP3_BAND_SIZE_LONG[h.sr_index * 22 + i] as usize;
            pos -= len;
            let mut handled = false;
            if !nz {
                if xr[1][pos..pos + len].iter().any(|&v| v != 0) {
                    nz = true;
                } else {
                    let kk = if i == 21 { 20 } else { i };
                    let sf = g1.scale_factors[kk] as usize;
                    if sf < sf_max {
                        let (v1, v2) = is_gain(sf);
                        for j in pos..pos + len {
                            let t = xr[0][j] as i64;
                            xr[0][j] = ((t * v1) >> 30) as i32;
                            xr[1][j] = ((t * v2) >> 30) as i32;
                        }
                        handled = true;
                    }
                }
            }
            if !handled {
                ms_band(xr, pos, len);
            }
        }
    }

    /// IMDCT, windowing and overlap-add into 18 x 32 subband samples.
    fn hybrid(&mut self, g: &Granule, ch: usize, xr: &[i32; 576], out: &mut [[i32; 32]; 18]) {
        // Last non-zero subband.
        let mut sblimit = 0;
        for sbi in (0..32).rev() {
            if xr[sbi * 18..sbi * 18 + 18].iter().any(|&v| v != 0) {
                sblimit = sbi + 1;
                break;
            }
        }
        let long_end = if g.block_type == 2 { if g.switch_point { 2 } else { 0 } } else { 32 };
        for sb in 0..32 {
            let ov = &mut self.overlap[ch][sb];
            let mut y = [0i64; 36];
            if sb < sblimit {
                let x = &xr[sb * 18..sb * 18 + 18];
                if sb < long_end {
                    let bt = if g.switch_point && sb < 2 { 0 } else { g.block_type as usize };
                    for i in 0..36 {
                        let mut acc = 0i64;
                        for k in 0..18 {
                            acc += x[k] as i64 * MP3_IMDCT36[i * 18 + k] as i64;
                        }
                        y[i] = ((acc >> 30) * MP3_WINDOWS[bt * 36 + i] as i64) >> 30;
                    }
                } else {
                    for w in 0..3 {
                        for i in 0..12 {
                            let mut acc = 0i64;
                            for k in 0..6 {
                                acc += x[3 * k + w] as i64 * MP3_IMDCT12[i * 6 + k] as i64;
                            }
                            y[6 + 6 * w + i] += ((acc >> 30) * MP3_WINDOWS[2 * 36 + i] as i64) >> 30;
                        }
                    }
                }
            }
            for i in 0..18 {
                let mut v = y[i] + ov[i] as i64;
                if sb & 1 == 1 && i & 1 == 1 {
                    v = -v;
                }
                out[i][sb] = v.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
                ov[i] = y[18 + i].clamp(i32::MIN as i64, i32::MAX as i64) as i32;
            }
        }
    }

    fn synth(&mut self, ch: usize, s: &[i32; 32], out: &mut [i32; 32]) {
        let off = (self.voff[ch] + 1024 - 64) & 1023;
        self.voff[ch] = off;
        let v = &mut self.v[ch];
        for i in 0..64 {
            let row = &MP3_SYNTH_COS[i * 32..i * 32 + 32];
            let mut acc = 0i64;
            for k in 0..32 {
                acc += s[k] as i64 * row[k] as i64;
            }
            v[(off + i) & 1023] = (acc >> 30) as i32;
        }
        for j in 0..32 {
            let mut sum = 0i64;
            for i in 0..8 {
                sum += v[(off + i * 128 + j) & 1023] as i64 * self.window[i * 64 + j] as i64;
                sum += v[(off + i * 128 + 96 + j) & 1023] as i64 * self.window[i * 64 + 32 + j] as i64;
            }
            out[j] = (sum >> 16) as i32;
        }
    }
}

fn reorder(h: &Header, g: &Granule, xr: &mut [i32; 576]) {
    if g.block_type != 2 {
        return;
    }
    let mut p = if g.switch_point { if h.sr_index != 8 { 36 } else { 72 } } else { 0 };
    let mut tmp = [0i32; 576];
    for i in g.short_start..13 {
        let len = MP3_BAND_SIZE_SHORT[h.sr_index * 13 + i] as usize;
        if p + 3 * len > 576 {
            break;
        }
        for j in 0..len {
            tmp[3 * j] = xr[p + j];
            tmp[3 * j + 1] = xr[p + len + j];
            tmp[3 * j + 2] = xr[p + 2 * len + j];
        }
        xr[p..p + 3 * len].copy_from_slice(&tmp[..3 * len]);
        p += 3 * len;
    }
}

fn antialias(g: &Granule, xr: &mut [i32; 576]) {
    let n = if g.block_type == 2 {
        if !g.switch_point {
            return;
        }
        1
    } else {
        31
    };
    for sb in 1..=n {
        let b = sb * 18;
        for i in 0..8 {
            let a0 = xr[b - 1 - i] as i64;
            let b0 = xr[b + i] as i64;
            xr[b - 1 - i] = ((a0 * MP3_AA_CS[i] as i64 - b0 * MP3_AA_CA[i] as i64) >> 30) as i32;
            xr[b + i] = ((b0 * MP3_AA_CS[i] as i64 + a0 * MP3_AA_CA[i] as i64) >> 30) as i32;
        }
    }
}

mod f64_free_cbrt {
    pub struct Q(pub u64);
    impl From<u64> for Q {
        fn from(v: u64) -> Q {
            Q(v)
        }
    }
    impl Q {
        /// q^(4/3) * 2^13, rounded.
        pub fn pow43(self) -> u32 {
            let target = (self.0 as u128).pow(4) << 39;
            let (mut lo, mut hi) = (0u64, 1u64 << 32);
            while lo < hi {
                let mid = (lo + hi + 1) / 2;
                if (mid as u128).pow(3) <= target {
                    lo = mid;
                } else {
                    hi = mid - 1;
                }
            }
            lo as u32
        }
    }
}
