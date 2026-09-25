//! AAC-LC decoder (ISO/IEC 14496-3), fixed point. HE-AAC streams are
//! decoded through their AAC-LC core (at half the sample rate).

use alloc::vec;
use alloc::vec::Vec;

use crate::bits::BitReader;
use crate::dsp::{imdct, isqrt, mul30, pow43_table};
use crate::tables::*;
use crate::vlc::Vlc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Invalid(&'static str),
    Unsupported(&'static str),
}

pub type Result<T> = core::result::Result<T, Error>;

pub const RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

/// Fractional bits of spectral and time-domain values.
const FRAC: i32 = 4;

const LONG_START: u8 = 1;
const EIGHT_SHORT: u8 = 2;
const LONG_STOP: u8 = 3;

const ZERO_HCB: u8 = 0;
const ESC_HCB: u8 = 11;
const NOISE_HCB: u8 = 13;
const INTENSITY_HCB2: u8 = 14;
const INTENSITY_HCB: u8 = 15;

#[derive(Clone, Copy, Default)]
struct IcsInfo {
    window_sequence: u8,
    window_shape: u8,
    max_sfb: usize,
    num_groups: usize,
    group_len: [usize; 8],
}

#[derive(Clone)]
struct Tns {
    present: bool,
    n_filt: [usize; 8],
    /// Per window and filter: (length, order, direction, lpc coefficients Q24).
    filt: [[(usize, usize, bool, [i32; 20]); 3]; 8],
}

impl Default for Tns {
    fn default() -> Tns {
        Tns { present: false, n_filt: [0; 8], filt: [[(0, 0, false, [0; 20]); 3]; 8] }
    }
}

struct Ics {
    info: IcsInfo,
    band_type: [[u8; 64]; 8],
    sf: [[i32; 64]; 8],
    tns: Tns,
    /// Dequantised spectrum (Q4); short windows at 128 * w.
    coef: [i32; 1024],
}

impl Ics {
    fn new() -> Ics {
        Ics { info: IcsInfo::default(), band_type: [[0; 64]; 8], sf: [[0; 64]; 8], tns: Tns::default(), coef: [0; 1024] }
    }
}

struct ChannelState {
    overlap: Vec<i32>,
    prev_shape: u8,
}

pub struct AacDecoder {
    pub sample_rate: u32,
    /// Channels in the output (1 or 2).
    pub channels: usize,
    sf_index: usize,
    spectral: Vec<Vlc>,
    sf_vlc: Vlc,
    pow43: Vec<u32>,
    state: Vec<ChannelState>,
    rand: u32,
}

fn swb_long(sf_index: usize) -> &'static [u16] {
    match sf_index {
        0 | 1 => &AAC_SWB_1024_96,
        2 => &AAC_SWB_1024_64,
        3 | 4 => &AAC_SWB_1024_48,
        5 => &AAC_SWB_1024_32,
        6 | 7 => &AAC_SWB_1024_24,
        8..=10 => &AAC_SWB_1024_16,
        _ => &AAC_SWB_1024_8,
    }
}

fn swb_short(sf_index: usize) -> &'static [u16] {
    match sf_index {
        0..=2 => &AAC_SWB_128_96,
        3..=5 => &AAC_SWB_128_48,
        6 | 7 => &AAC_SWB_128_24,
        8..=10 => &AAC_SWB_128_16,
        _ => &AAC_SWB_128_8,
    }
}

impl AacDecoder {
    /// From an AudioSpecificConfig (MP4 esds / MKV CodecPrivate).
    pub fn from_config(asc: &[u8]) -> Result<AacDecoder> {
        let mut br = BitReader::new(asc);
        let mut aot = br.bits(5);
        if aot == 31 {
            aot = 32 + br.bits(6);
        }
        let mut sf_index = br.bits(4) as usize;
        if sf_index == 15 {
            let rate = br.bits(24);
            sf_index = RATES.iter().position(|&r| r == rate).unwrap_or(4);
        }
        let channel_config = br.bits(4);
        if aot == 5 || aot == 29 {
            // HE-AAC: skip the extension rate, use the core object.
            let ext = br.bits(4);
            if ext == 15 {
                br.bits(24);
            }
            aot = br.bits(5);
        }
        if !matches!(aot, 1 | 2 | 4) {
            return Err(Error::Unsupported("AAC object type (only AAC-LC)"));
        }
        if br.flag() {
            return Err(Error::Unsupported("960-sample AAC frames"));
        }
        AacDecoder::new(sf_index, channel_config as usize)
    }

    pub fn new(sf_index: usize, channel_config: usize) -> Result<AacDecoder> {
        if sf_index >= 12 {
            return Err(Error::Invalid("sample rate index"));
        }
        let mut spectral = Vec::new();
        let books: [(&[u16], &[u8]); 11] = [
            (&AAC_CODES1, &AAC_BITS1),
            (&AAC_CODES2, &AAC_BITS2),
            (&AAC_CODES3, &AAC_BITS3),
            (&AAC_CODES4, &AAC_BITS4),
            (&AAC_CODES5, &AAC_BITS5),
            (&AAC_CODES6, &AAC_BITS6),
            (&AAC_CODES7, &AAC_BITS7),
            (&AAC_CODES8, &AAC_BITS8),
            (&AAC_CODES9, &AAC_BITS9),
            (&AAC_CODES10, &AAC_BITS10),
            (&AAC_CODES11, &AAC_BITS11),
        ];
        for (codes, bits) in books {
            let v: Vec<(u32, u8, i32)> = codes.iter().zip(bits.iter()).enumerate().map(|(i, (&c, &b))| (c as u32, b, i as i32)).collect();
            spectral.push(Vlc::new(&v));
        }
        let sfv: Vec<(u32, u8, i32)> = AAC_SF_CODES.iter().zip(AAC_SF_BITS.iter()).enumerate().map(|(i, (&c, &b))| (c, b, i as i32)).collect();
        Ok(AacDecoder {
            sample_rate: RATES[sf_index],
            channels: if channel_config == 1 { 1 } else { 2 },
            sf_index,
            spectral,
            sf_vlc: Vlc::new(&sfv),
            pow43: pow43_table(),
            state: Vec::new(),
            rand: 0x1f2e3d4c,
        })
    }

    /// Parse an ADTS header; returns (header length, frame length,
    /// sample rate index, channel config).
    pub fn parse_adts(data: &[u8]) -> Option<(usize, usize, usize, usize)> {
        if data.len() < 7 || data[0] != 0xff || data[1] & 0xf6 != 0xf0 {
            return None;
        }
        let protection_absent = data[1] & 1;
        let sf_index = ((data[2] >> 2) & 0xf) as usize;
        let ch = (((data[2] & 1) << 2) | (data[3] >> 6)) as usize;
        let len = (((data[3] & 3) as usize) << 11) | ((data[4] as usize) << 3) | ((data[5] >> 5) as usize);
        let hdr = if protection_absent == 1 { 7 } else { 9 };
        if len < hdr {
            return None;
        }
        Some((hdr, len, sf_index, ch))
    }

    fn channel(&mut self, i: usize) -> &mut ChannelState {
        while self.state.len() <= i {
            self.state.push(ChannelState { overlap: vec![0; 1024], prev_shape: 0 });
        }
        &mut self.state[i]
    }

    /// Decode one raw_data_block into 1024 interleaved samples per output
    /// channel (appended to `out`).
    pub fn decode_frame(&mut self, data: &[u8], out: &mut Vec<i16>) -> Result<()> {
        let mut br = BitReader::new(data);
        let mut chans: Vec<Vec<i32>> = Vec::new();
        let mut roles: Vec<u8> = Vec::new(); // 0 = mono element, 1/2 = pair left/right, 3 = LFE
        loop {
            if br.bits_left() < 3 {
                break;
            }
            let id = br.bits(3);
            match id {
                0 | 3 => {
                    br.bits(4);
                    let mut ics = Ics::new();
                    self.decode_ics(&mut br, &mut ics, false)?;
                    let idx = chans.len();
                    chans.push(self.synthesize(&mut ics, idx));
                    roles.push(if id == 3 { 3 } else { 0 });
                }
                1 => {
                    br.bits(4);
                    let common = br.flag();
                    let mut a = Ics::new();
                    let mut b = Ics::new();
                    let mut ms = [[false; 64]; 8];
                    let mut ms_present = 0;
                    if common {
                        a.info = self.ics_info(&mut br)?;
                        b.info = a.info;
                        ms_present = br.bits(2);
                        if ms_present == 1 {
                            for g in 0..a.info.num_groups {
                                for sfb in 0..a.info.max_sfb {
                                    ms[g][sfb] = br.flag();
                                }
                            }
                        } else if ms_present == 2 {
                            ms = [[true; 64]; 8];
                        }
                    }
                    self.decode_ics(&mut br, &mut a, common)?;
                    self.decode_ics(&mut br, &mut b, common)?;
                    if common {
                        self.stereo(&mut a, &mut b, &ms, ms_present != 0);
                    }
                    let idx = chans.len();
                    chans.push(self.synthesize(&mut a, idx));
                    chans.push(self.synthesize(&mut b, idx + 1));
                    roles.push(1);
                    roles.push(2);
                }
                2 => return Err(Error::Unsupported("AAC coupling channels")),
                4 => {
                    br.bits(4);
                    let align = br.flag();
                    let mut count = br.bits(8) as usize;
                    if count == 255 {
                        count += br.bits(8) as usize;
                    }
                    if align {
                        br.align();
                    }
                    br.skip(count as u32 * 8);
                }
                5 => self.skip_pce(&mut br),
                6 => {
                    let mut count = br.bits(4) as usize;
                    if count == 15 {
                        count += br.bits(8) as usize - 1;
                    }
                    br.skip(count as u32 * 8);
                }
                _ => break,
            }
            if br.bits_left() < 0 {
                return Err(Error::Invalid("AAC frame truncated"));
            }
        }
        if chans.is_empty() {
            return Err(Error::Invalid("empty AAC frame"));
        }
        // Mix to the output layout.
        let to16 = |v: i64| ((v + (1 << (FRAC - 1))) >> FRAC).clamp(-32768, 32767) as i16;
        if self.channels == 1 {
            for i in 0..1024 {
                out.push(to16(chans[0][i] as i64));
            }
        } else {
            let left = roles.iter().position(|&r| r == 1);
            let center = roles.iter().position(|&r| r == 0);
            // Surround pairs after the first one fold into the front.
            let extra: Vec<usize> = roles.iter().enumerate().filter(|&(i, &r)| r == 1 && Some(i) != left).map(|(i, _)| i).collect();
            for i in 0..1024 {
                let (mut l, mut r) = match left {
                    Some(li) => (chans[li][i] as i64, chans[li + 1][i] as i64),
                    None => (chans[0][i] as i64, chans[0][i] as i64),
                };
                if left.is_some() {
                    if let Some(c) = center {
                        let cv = chans[c][i] as i64 * 181 >> 8;
                        l += cv;
                        r += cv;
                    }
                    for &e in &extra {
                        l += chans[e][i] as i64 * 181 >> 8;
                        r += chans[e + 1][i] as i64 * 181 >> 8;
                    }
                }
                out.push(to16(l));
                out.push(to16(r));
            }
        }
        Ok(())
    }

    fn skip_pce(&mut self, br: &mut BitReader) {
        br.bits(4);
        br.bits(2);
        br.bits(4);
        let front = br.bits(4);
        let side = br.bits(4);
        let back = br.bits(4);
        let lfe = br.bits(2);
        let assoc = br.bits(3);
        let cc = br.bits(4);
        if br.flag() {
            br.bits(4);
        }
        if br.flag() {
            br.bits(4);
        }
        if br.flag() {
            br.bits(3);
        }
        br.skip((front + side + back) * 5 + lfe * 4 + assoc * 4 + cc * 5);
        br.align();
        let n = br.bits(8);
        br.skip(n * 8);
    }

    fn ics_info(&self, br: &mut BitReader) -> Result<IcsInfo> {
        br.bit();
        let mut i = IcsInfo { window_sequence: br.bits(2) as u8, window_shape: br.bits(1) as u8, ..Default::default() };
        if i.window_sequence == EIGHT_SHORT {
            i.max_sfb = br.bits(4) as usize;
            let grouping = br.bits(7);
            i.num_groups = 1;
            i.group_len[0] = 1;
            for w in 0..7 {
                if grouping & (1 << (6 - w)) != 0 {
                    i.group_len[i.num_groups - 1] += 1;
                } else {
                    i.num_groups += 1;
                    i.group_len[i.num_groups - 1] = 1;
                }
            }
            if i.max_sfb > swb_short(self.sf_index).len() - 1 {
                return Err(Error::Invalid("max_sfb"));
            }
        } else {
            i.max_sfb = br.bits(6) as usize;
            i.num_groups = 1;
            i.group_len[0] = 1;
            if br.flag() {
                return Err(Error::Unsupported("AAC prediction"));
            }
            if i.max_sfb > swb_long(self.sf_index).len() - 1 {
                return Err(Error::Invalid("max_sfb"));
            }
        }
        Ok(i)
    }

    fn decode_ics(&mut self, br: &mut BitReader, ics: &mut Ics, common: bool) -> Result<()> {
        let global_gain = br.bits(8) as i32;
        if !common {
            ics.info = self.ics_info(br)?;
        }
        let info = ics.info;
        let short = info.window_sequence == EIGHT_SHORT;
        let swb = if short { swb_short(self.sf_index) } else { swb_long(self.sf_index) };
        // Section data.
        let sect_bits = if short { 3 } else { 5 };
        let esc = (1 << sect_bits) - 1;
        for g in 0..info.num_groups {
            let mut k = 0;
            while k < info.max_sfb {
                let cb = br.bits(4) as u8;
                if cb == 12 {
                    return Err(Error::Invalid("reserved codebook"));
                }
                let mut len = 0;
                loop {
                    let v = br.bits(sect_bits);
                    len += v as usize;
                    if v != esc {
                        break;
                    }
                    if br.bits_left() < 0 {
                        return Err(Error::Invalid("section data"));
                    }
                }
                if k + len > info.max_sfb {
                    return Err(Error::Invalid("section length"));
                }
                for sfb in k..k + len {
                    ics.band_type[g][sfb] = cb;
                }
                k += len;
            }
        }
        // Scale factors.
        let mut sf = global_gain;
        let mut is_pos = 0i32;
        let mut noise = global_gain - 90;
        let mut noise_first = true;
        for g in 0..info.num_groups {
            for sfb in 0..info.max_sfb {
                match ics.band_type[g][sfb] {
                    ZERO_HCB => ics.sf[g][sfb] = 0,
                    INTENSITY_HCB | INTENSITY_HCB2 => {
                        is_pos += self.sf_vlc.read(br).ok_or(Error::Invalid("scalefactor"))? - 60;
                        ics.sf[g][sfb] = is_pos;
                    }
                    NOISE_HCB => {
                        if noise_first {
                            noise_first = false;
                            noise += br.bits(9) as i32 - 256;
                        } else {
                            noise += self.sf_vlc.read(br).ok_or(Error::Invalid("scalefactor"))? - 60;
                        }
                        ics.sf[g][sfb] = noise;
                    }
                    _ => {
                        sf += self.sf_vlc.read(br).ok_or(Error::Invalid("scalefactor"))? - 60;
                        if !(0..=255).contains(&sf) {
                            return Err(Error::Invalid("scalefactor range"));
                        }
                        ics.sf[g][sfb] = sf;
                    }
                }
            }
        }
        // Pulse data.
        let mut pulses: Vec<(usize, i32)> = Vec::new();
        if br.flag() {
            if short {
                return Err(Error::Invalid("pulse data in short window"));
            }
            let n = br.bits(2) as usize + 1;
            let start = br.bits(6) as usize;
            if start >= swb.len() - 1 {
                return Err(Error::Invalid("pulse start"));
            }
            let mut k = swb[start] as usize;
            for _ in 0..n {
                k += br.bits(5) as usize;
                let amp = br.bits(4) as i32;
                if k >= 1024 {
                    return Err(Error::Invalid("pulse offset"));
                }
                pulses.push((k, amp));
            }
        }
        // TNS.
        ics.tns = Tns::default();
        if br.flag() {
            self.tns_data(br, ics, short)?;
        }
        if br.flag() {
            return Err(Error::Unsupported("AAC gain control"));
        }
        // Spectral data.
        let mut q = [0i32; 1024];
        let mut win = 0;
        for g in 0..info.num_groups {
            for sfb in 0..info.max_sfb {
                let cb = ics.band_type[g][sfb];
                let (start, end) = (swb[sfb] as usize, swb[sfb + 1] as usize);
                if cb == ZERO_HCB || cb >= NOISE_HCB {
                    continue;
                }
                for w in 0..info.group_len[g] {
                    let base = (win + w) * 128;
                    let mut k = start;
                    while k < end {
                        k += self.spectral_values(br, cb, &mut q[base + k..])?;
                    }
                }
            }
            win += info.group_len[g];
        }
        for (k, amp) in pulses {
            q[k] += if q[k] > 0 { amp } else { -amp };
        }
        // Dequantise.
        ics.coef = [0; 1024];
        let mut win = 0;
        for g in 0..info.num_groups {
            for sfb in 0..info.max_sfb {
                let cb = ics.band_type[g][sfb];
                let (start, end) = (swb[sfb] as usize, swb[sfb + 1] as usize);
                let scale = ics.sf[g][sfb];
                for w in 0..info.group_len[g] {
                    let base = (win + w) * 128;
                    if cb == NOISE_HCB {
                        self.noise_band(&mut ics.coef[base + start..base + end], scale);
                    } else if cb != ZERO_HCB && cb < NOISE_HCB {
                        for k in start..end {
                            ics.coef[base + k] = self.dequant(q[base + k], scale);
                        }
                    }
                }
            }
            win += info.group_len[g];
        }
        Ok(())
    }

    /// Decode one codeword (4 or 2 values) into `out`; returns the count.
    fn spectral_values(&self, br: &mut BitReader, cb: u8, out: &mut [i32]) -> Result<usize> {
        let idx = self.spectral[cb as usize - 1].read(br).ok_or(Error::Invalid("spectral codeword"))?;
        let (dim, unsigned, lav) = match cb {
            1 | 2 => (4, false, 1),
            3 | 4 => (4, true, 2),
            5 | 6 => (2, false, 4),
            7 | 8 => (2, true, 7),
            9 | 10 => (2, true, 12),
            _ => (2, true, 16),
        };
        let modulo = if unsigned { lav + 1 } else { 2 * lav + 1 };
        let mut vals = [0i32; 4];
        let mut v = idx;
        for i in (0..dim).rev() {
            vals[i] = v % modulo;
            v /= modulo;
            if !unsigned {
                vals[i] -= lav;
            }
        }
        if unsigned {
            for val in vals.iter_mut().take(dim) {
                if *val != 0 && br.bit() == 1 {
                    *val = -*val;
                }
            }
        }
        if cb == ESC_HCB {
            for val in vals.iter_mut().take(dim) {
                if val.abs() == 16 {
                    let mut n = 4;
                    while br.bit() == 1 {
                        n += 1;
                        if n > 12 {
                            return Err(Error::Invalid("escape"));
                        }
                    }
                    let e = (1 << n) + br.bits(n) as i32;
                    *val = if *val < 0 { -e } else { e };
                }
            }
        }
        out[..dim].copy_from_slice(&vals[..dim]);
        Ok(dim)
    }

    #[inline]
    fn dequant(&self, q: i32, sf: i32) -> i32 {
        if q == 0 {
            return 0;
        }
        let a = q.unsigned_abs().min(8191) as usize;
        let e = sf - 100;
        let m = mul30(self.pow43[a] as i32, POW2_QUARTER[(e & 3) as usize]) as i64;
        let shift = (e >> 2) + FRAC - 13;
        let v = if shift >= 0 { (m << shift.min(40)).min(1 << 30) } else { (m + (1i64 << (-shift - 1).min(62))) >> (-shift).min(63) };
        let v = v as i32;
        if q < 0 { -v } else { v }
    }

    fn noise_band(&mut self, band: &mut [i32], nrg: i32) {
        let mut energy: u64 = 0;
        for v in band.iter_mut() {
            self.rand = self.rand.wrapping_mul(1664525).wrapping_add(1013904223);
            *v = (self.rand as i32) >> 16;
            energy += (*v as i64 * *v as i64) as u64;
        }
        if energy == 0 {
            return;
        }
        // Scale so that sqrt(sum v^2) = 2^(nrg / 4) (in Q4 units).
        let root = isqrt(energy).max(1) as i64;
        let e = nrg;
        let gain = POW2_QUARTER[(e & 3) as usize] as i64; // Q30
        let shift = (e >> 2) + FRAC;
        for v in band.iter_mut() {
            let x = (*v as i64 * gain) / root; // Q30
            let y = if shift >= 30 { x << (shift - 30).min(30) } else { x >> (30 - shift).min(62) };
            *v = y.clamp(-(1 << 30), 1 << 30) as i32;
        }
    }

    fn tns_data(&self, br: &mut BitReader, ics: &mut Ics, short: bool) -> Result<()> {
        let windows = if short { 8 } else { 1 };
        ics.tns.present = true;
        for w in 0..windows {
            let n_filt = br.bits(if short { 1 } else { 2 }) as usize;
            ics.tns.n_filt[w] = n_filt;
            if n_filt == 0 {
                continue;
            }
            let coef_res = br.bits(1) as usize;
            for f in 0..n_filt {
                let length = br.bits(if short { 4 } else { 6 }) as usize;
                let order = br.bits(if short { 3 } else { 5 }) as usize;
                if order > 12 {
                    return Err(Error::Invalid("TNS order"));
                }
                let mut lpc = [0i32; 20];
                let mut dir = false;
                if order > 0 {
                    dir = br.flag();
                    let compress = br.bits(1) as usize;
                    let bits = coef_res as u32 + 3 - compress as u32;
                    let map: &[i32] = match (compress, coef_res) {
                        (0, 0) => &TNS_TMP2_MAP_0_3,
                        (0, _) => &TNS_TMP2_MAP_0_4,
                        (_, 0) => &TNS_TMP2_MAP_1_3,
                        _ => &TNS_TMP2_MAP_1_4,
                    };
                    let mut refl = [0i32; 20];
                    for r in refl.iter_mut().take(order) {
                        *r = map[br.bits(bits) as usize] >> 6; // Q30 -> Q24
                    }
                    // Reflection -> LPC (step-up recursion), Q24.
                    for i in 0..order {
                        let r = -(refl[i] as i64);
                        lpc[i] = r as i32;
                        let prev = lpc;
                        for j in 0..(i + 1) / 2 {
                            let fj = prev[j] as i64;
                            let bj = prev[i - 1 - j] as i64;
                            lpc[j] = (fj + ((r * bj) >> 24)) as i32;
                            lpc[i - 1 - j] = (bj + ((r * fj) >> 24)) as i32;
                        }
                    }
                }
                ics.tns.filt[w][f] = (length, order, dir, lpc);
            }
        }
        Ok(())
    }

    fn apply_tns(&self, ics: &mut Ics) {
        let info = ics.info;
        let short = info.window_sequence == EIGHT_SHORT;
        let swb = if short { swb_short(self.sf_index) } else { swb_long(self.sf_index) };
        let nswb = swb.len() - 1;
        let max_bands = if short { AAC_TNS_MAX_BANDS_128[self.sf_index] } else { AAC_TNS_MAX_BANDS_1024[self.sf_index] } as usize;
        let mmm = max_bands.min(info.max_sfb).min(nswb);
        let windows = if short { 8 } else { 1 };
        for w in 0..windows {
            let mut bottom = nswb;
            for f in 0..ics.tns.n_filt[w] {
                let (length, order, dir, lpc) = ics.tns.filt[w][f];
                let top = bottom;
                bottom = top.saturating_sub(length);
                if order == 0 {
                    continue;
                }
                let start = swb[bottom.min(mmm)] as usize;
                let end = swb[top.min(mmm)] as usize;
                if end <= start {
                    continue;
                }
                let size = end - start;
                let base = w * 128;
                let c = &mut ics.coef[base..base + if short { 128 } else { 1024 }];
                let (first, inc): (isize, isize) = if dir { (end as isize - 1, -1) } else { (start as isize, 1) };
                let mut hist = [0i64; 20];
                for m in 0..size {
                    let pos = (first + m as isize * inc) as usize;
                    let mut acc = (c[pos] as i64) << 24;
                    for i in 0..order.min(m) {
                        acc -= hist[i] * lpc[i] as i64;
                    }
                    let y = (acc >> 24).clamp(-(1 << 30), 1 << 30);
                    c[pos] = y as i32;
                    for i in (1..order).rev() {
                        hist[i] = hist[i - 1];
                    }
                    hist[0] = y;
                }
            }
        }
    }

    fn stereo(&self, a: &mut Ics, b: &mut Ics, ms: &[[bool; 64]; 8], ms_present: bool) {
        let info = a.info;
        let short = info.window_sequence == EIGHT_SHORT;
        let swb = if short { swb_short(self.sf_index) } else { swb_long(self.sf_index) };
        let mut win = 0;
        for g in 0..info.num_groups {
            for sfb in 0..info.max_sfb {
                let (start, end) = (swb[sfb] as usize, swb[sfb + 1] as usize);
                let bt = b.band_type[g][sfb];
                for w in 0..info.group_len[g] {
                    let base = (win + w) * 128;
                    if bt == INTENSITY_HCB || bt == INTENSITY_HCB2 {
                        let mut sign: i64 = if bt == INTENSITY_HCB { 1 } else { -1 };
                        if ms_present && ms[g][sfb] {
                            sign = -sign;
                        }
                        let e = -b.sf[g][sfb];
                        let gain = POW2_QUARTER[(e & 3) as usize] as i64;
                        let sh = e >> 2;
                        for k in start..end {
                            let v = (a.coef[base + k] as i64 * gain) >> 30;
                            let v = if sh >= 0 { v << sh.min(30) } else { v >> (-sh).min(62) };
                            b.coef[base + k] = (sign * v).clamp(-(1 << 30), 1 << 30) as i32;
                        }
                    } else if ms_present && ms[g][sfb] && a.band_type[g][sfb] < NOISE_HCB && bt < NOISE_HCB {
                        for k in start..end {
                            let (m, s) = (a.coef[base + k], b.coef[base + k]);
                            a.coef[base + k] = m.saturating_add(s);
                            b.coef[base + k] = m.saturating_sub(s);
                        }
                    }
                }
            }
            win += info.group_len[g];
        }
    }

    /// TNS, inverse MDCT, windowing and overlap-add: 1024 output samples.
    fn synthesize(&mut self, ics: &mut Ics, ch: usize) -> Vec<i32> {
        if ics.tns.present {
            self.apply_tns(ics);
        }
        let info = ics.info;
        let st = self.channel(ch);
        let prev_shape = st.prev_shape;
        let (long_prev, short_prev): (&[i32], &[i32]) = if prev_shape == 1 { (&AAC_KBD_LONG, &AAC_KBD_SHORT) } else { (&AAC_SINE_LONG, &AAC_SINE_SHORT) };
        let (long_cur, short_cur): (&[i32], &[i32]) = if info.window_shape == 1 { (&AAC_KBD_LONG, &AAC_KBD_SHORT) } else { (&AAC_SINE_LONG, &AAC_SINE_SHORT) };
        let mut buf = vec![0i32; 2048];
        if info.window_sequence == EIGHT_SHORT {
            let mut x = [0i32; 256];
            for w in 0..8 {
                imdct(&ics.coef[w * 128..w * 128 + 128], &mut x);
                let rise = if w == 0 { short_prev } else { short_cur };
                for i in 0..128 {
                    buf[448 + w * 128 + i] += mul30(x[i], rise[i]);
                    buf[448 + w * 128 + 128 + i] += mul30(x[128 + i], short_cur[127 - i]);
                }
            }
        } else {
            imdct(&ics.coef, &mut buf);
            // Left half.
            match info.window_sequence {
                LONG_STOP => {
                    for i in 0..448 {
                        buf[i] = 0;
                    }
                    for i in 0..128 {
                        buf[448 + i] = mul30(buf[448 + i], short_prev[i]);
                    }
                }
                _ => {
                    for i in 0..1024 {
                        buf[i] = mul30(buf[i], long_prev[i]);
                    }
                }
            }
            // Right half.
            match info.window_sequence {
                LONG_START => {
                    for i in 0..128 {
                        buf[1472 + i] = mul30(buf[1472 + i], short_cur[127 - i]);
                    }
                    for v in buf[1600..2048].iter_mut() {
                        *v = 0;
                    }
                }
                _ => {
                    for i in 0..1024 {
                        buf[1024 + i] = mul30(buf[1024 + i], long_cur[1023 - i]);
                    }
                }
            }
        }
        let st = self.channel(ch);
        let mut out = vec![0i32; 1024];
        for i in 0..1024 {
            out[i] = st.overlap[i].saturating_add(buf[i]);
            st.overlap[i] = buf[1024 + i];
        }
        st.prev_shape = info.window_shape;
        out
    }
}
