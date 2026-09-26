//! ALSA sound devices for Linux programs: `/dev/snd/controlC0` and the
//! playback PCM `/dev/snd/pcmC0D0p`, speaking the kernel's ioctl ABI so
//! the unmodified alsa-lib (and Firefox's cubeb on top of it) can play
//! sound. Samples are converted (S16/S32/float, mono/stereo, any rate) to
//! the 48 kHz stereo 16-bit stream of MayOS's mixer.
//!
//! alsa-lib's status/control mmap is refused, so it uses SYNC_PTR, and
//! only interleaved read/write access is offered.

use alloc::sync::Arc;
use alloc::vec::Vec;

use super::{sched, usermem};
use crate::audio::{self, AudioStream};
use crate::sync::Spin;

const EINVAL: i64 = 22;
const EFAULT: i64 = 14;
const ENOTTY: i64 = 25;
const EAGAIN: i64 = 11;
const EBADFD: i64 = 77;
const ENOENT: i64 = 2;
const EINTR: i64 = 4;

// Parameter indexes (masks 0..2, intervals 8..19).
const ACCESS: usize = 0;
const FORMAT: usize = 1;
const SUBFORMAT: usize = 2;
const SAMPLE_BITS: usize = 8;
const FRAME_BITS: usize = 9;
const CHANNELS: usize = 10;
const RATE: usize = 11;
const PERIOD_TIME: usize = 12;
const PERIOD_SIZE: usize = 13;
const PERIOD_BYTES: usize = 14;
const PERIODS: usize = 15;
const BUFFER_TIME: usize = 16;
const BUFFER_SIZE: usize = 17;
const BUFFER_BYTES: usize = 18;
const TICK_TIME: usize = 19;

const FMT_S16_LE: u32 = 2;
const FMT_S32_LE: u32 = 10;
const FMT_FLOAT_LE: u32 = 14;
const ACCESS_RW_INTERLEAVED: u32 = 3;

// PCM states
const OPEN: i32 = 0;
const SETUP: i32 = 1;
const PREPARED: i32 = 2;
const RUNNING: i32 = 3;
const DRAINING: i32 = 5;
const PAUSED: i32 = 6;

/// Hardware parameter block (struct snd_pcm_hw_params, 608 bytes).
const HWP_SIZE: usize = 608;
const MASKS: usize = 4;
const INTERVALS: usize = 260;
const RMASK: usize = 512;
const CMASK: usize = 516;
const INFO: usize = 520;
const MSBITS: usize = 524;
const RATE_NUM: usize = 528;
const RATE_DEN: usize = 532;

/// Frames Linux programs have played (all PCMs, for tests and stats).
pub static FRAMES_WRITTEN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub struct Pcm {
    st: Spin<State>,
}

struct State {
    state: i32,
    format: u32,
    channels: u32,
    rate: u32,
    buffer: u64,
    period: u64,
    appl: u64,
    avail_min: u64,
    start_threshold: u64,
    boundary: u64,
    stream: Option<Arc<AudioStream>>,
    /// Resampler position (fraction of an input frame, 16.16) and the last
    /// input frame, for linear interpolation.
    phase: u64,
    last: (i32, i32),
}

impl Drop for Pcm {
    fn drop(&mut self) {
        if let Some(s) = self.st.lock().stream.take() {
            s.close();
        }
    }
}

pub enum Dev {
    Ctl,
    Pcm(Arc<Pcm>),
}

/// The device for a path under /dev/snd, if it is one of ours.
pub fn open(path: &str) -> Option<Dev> {
    if !audio::is_present() {
        return None;
    }
    match path {
        "/dev/snd/controlC0" => Some(Dev::Ctl),
        "/dev/snd/pcmC0D0p" => Some(Dev::Pcm(Arc::new(Pcm {
            st: Spin::new(State {
                state: OPEN,
                format: FMT_S16_LE,
                channels: 2,
                rate: 48000,
                buffer: 0,
                period: 0,
                appl: 0,
                avail_min: 1,
                start_threshold: 1,
                boundary: 1 << 62,
                stream: None,
                phase: 0,
                last: (0, 0),
            }),
        }))),
        _ => None,
    }
}

pub fn names() -> &'static [&'static str] {
    &["controlC0", "pcmC0D0p"]
}

/// /etc/asound.conf: the default device is our card, without plugins
/// (conversions happen here; dmix would need SysV shared memory).
pub const ASOUND_CONF: &str = "pcm.!default { type hw card 0 device 0 }\nctl.!default { type hw card 0 }\n";

fn put(pml4: u64, addr: u64, b: &[u8]) -> i64 {
    if usermem::write_bytes(pml4, addr, b) { 0 } else { -EFAULT }
}

fn str_at(b: &mut [u8], off: usize, len: usize, s: &str) {
    let n = s.len().min(len - 1);
    b[off..off + n].copy_from_slice(&s.as_bytes()[..n]);
}

pub fn ctl_ioctl(pml4: u64, cmd: u64, arg: u64) -> i64 {
    match cmd {
        0x8004_5500 => put(pml4, arg, &0x0002_0007u32.to_le_bytes()), // PVERSION
        0x8178_5501 => {
            // CARD_INFO
            let mut b = [0u8; 376];
            str_at(&mut b, 8, 16, "AC97");
            str_at(&mut b, 24, 16, "MayOS");
            str_at(&mut b, 40, 32, "MayOS AC'97");
            str_at(&mut b, 72, 80, "MayOS AC'97 sound");
            str_at(&mut b, 168, 80, "MayOS mixer");
            put(pml4, arg, &b)
        }
        0xc004_5530 => {
            // PCM_NEXT_DEVICE: device 0 only
            let Some(v) = usermem::read_bytes(pml4, arg, 4) else { return -EFAULT };
            let cur = i32::from_le_bytes(v.try_into().unwrap());
            put(pml4, arg, &(if cur < 0 { 0i32 } else { -1i32 }).to_le_bytes())
        }
        0xc120_5531 => {
            // PCM_INFO
            let Some(v) = usermem::read_bytes(pml4, arg, 12) else { return -EFAULT };
            let dev = u32::from_le_bytes(v[0..4].try_into().unwrap());
            let stream = i32::from_le_bytes(v[8..12].try_into().unwrap());
            if dev != 0 || stream != 0 {
                return -ENOENT;
            }
            put(pml4, arg, &pcm_info())
        }
        0x4004_5532 | 0xc004_5516 => 0, // PCM_PREFER_SUBDEVICE, SUBSCRIBE_EVENTS
        0xc050_5510 => {
            // ELEM_LIST: no mixer controls
            let Some(mut v) = usermem::read_bytes(pml4, arg, 16) else { return -EFAULT };
            v[8..16].fill(0);
            put(pml4, arg, &v)
        }
        _ => -ENOTTY,
    }
}

fn pcm_info() -> [u8; 288] {
    let mut b = [0u8; 288];
    str_at(&mut b, 16, 64, "MayOS");
    str_at(&mut b, 80, 80, "MayOS AC'97");
    str_at(&mut b, 160, 32, "subdevice #0");
    b[200..204].copy_from_slice(&1u32.to_le_bytes());
    b[204..208].copy_from_slice(&1u32.to_le_bytes());
    b
}

// --- hw_params refinement ------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
struct Iv {
    min: u64,
    max: u64,
    integer: bool,
}

impl Iv {
    fn empty(&self) -> bool {
        self.min > self.max
    }

    fn refine(&mut self, o: Iv) -> bool {
        let before = *self;
        self.min = self.min.max(o.min);
        self.max = self.max.min(o.max);
        *self != before
    }
}

fn mul(a: Iv, b: Iv) -> Iv {
    Iv { min: a.min.saturating_mul(b.min), max: a.max.saturating_mul(b.max), integer: true }
}

fn div(a: Iv, b: Iv) -> Iv {
    if b.max == 0 {
        return Iv { min: 0, max: u64::MAX, integer: true };
    }
    Iv { min: a.min / b.max.max(1), max: if b.min == 0 { u64::MAX } else { a.max.div_ceil(b.min) }, integer: true }
}

/// a * b / k
fn muldivk(a: Iv, b: Iv, k: u64) -> Iv {
    Iv { min: a.min.saturating_mul(b.min) / k, max: a.max.saturating_mul(b.max).div_ceil(k), integer: false }
}

/// a * k / b
fn mulkdiv(a: Iv, k: u64, b: Iv) -> Iv {
    Iv {
        min: a.min.saturating_mul(k) / b.max.max(1),
        max: if b.min == 0 { u64::MAX } else { a.max.saturating_mul(k).div_ceil(b.min) },
        integer: false,
    }
}

struct Params {
    raw: Vec<u8>,
}

impl Params {
    fn mask(&self, p: usize) -> u32 {
        let o = MASKS + p * 32;
        u32::from_le_bytes(self.raw[o..o + 4].try_into().unwrap())
    }

    fn set_mask(&mut self, p: usize, v: u32) {
        let o = MASKS + p * 32;
        self.raw[o..o + 4].copy_from_slice(&v.to_le_bytes());
        self.raw[o + 4..o + 32].fill(0);
    }

    fn iv(&self, p: usize) -> Iv {
        let o = INTERVALS + (p - SAMPLE_BITS) * 12;
        let g = |k: usize| u32::from_le_bytes(self.raw[o + k..o + k + 4].try_into().unwrap());
        let flags = g(8);
        let (mut min, mut max) = (g(0) as u64, g(4) as u64);
        // Open bounds: exclude the end (integers).
        if flags & 1 != 0 {
            min += 1;
        }
        if flags & 2 != 0 && max > 0 {
            max -= 1;
        }
        if flags & 8 != 0 {
            return Iv { min: 1, max: 0, integer: true };
        }
        Iv { min, max, integer: flags & 4 != 0 }
    }

    fn set_iv(&mut self, p: usize, v: Iv) {
        let o = INTERVALS + (p - SAMPLE_BITS) * 12;
        let min = v.min.min(u32::MAX as u64) as u32;
        let max = v.max.min(u32::MAX as u64) as u32;
        let flags: u32 = if v.integer { 4 } else { 0 } | if v.empty() { 8 } else { 0 };
        self.raw[o..o + 4].copy_from_slice(&min.to_le_bytes());
        self.raw[o + 4..o + 8].copy_from_slice(&max.to_le_bytes());
        self.raw[o + 8..o + 12].copy_from_slice(&flags.to_le_bytes());
    }

    fn u32_at(&self, o: usize) -> u32 {
        u32::from_le_bytes(self.raw[o..o + 4].try_into().unwrap())
    }

    fn set_u32(&mut self, o: usize, v: u32) {
        self.raw[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }
}

fn sample_bits(format_mask: u32) -> Iv {
    let mut min = u64::MAX;
    let mut max = 0;
    for (f, bits) in [(FMT_S16_LE, 16u64), (FMT_S32_LE, 32), (FMT_FLOAT_LE, 32)] {
        if format_mask & (1 << f) != 0 {
            min = min.min(bits);
            max = max.max(bits);
        }
    }
    Iv { min, max, integer: true }
}

/// Narrow every parameter to what we support and to what the others
/// allow (the kernel's snd_pcm_hw_refine, reduced to closed ranges).
fn refine(p: &mut Params) -> Result<(), i64> {
    let access = p.mask(ACCESS) & (1 << ACCESS_RW_INTERLEAVED);
    let format = p.mask(FORMAT) & ((1 << FMT_S16_LE) | (1 << FMT_S32_LE) | (1 << FMT_FLOAT_LE));
    let subformat = p.mask(SUBFORMAT) & 1;
    if access == 0 || format == 0 || subformat == 0 {
        return Err(-EINVAL);
    }
    p.set_mask(ACCESS, access);
    p.set_mask(FORMAT, format);
    p.set_mask(SUBFORMAT, subformat);
    let mut iv: [Iv; 20] = [Iv { min: 0, max: u64::MAX, integer: false }; 20];
    for (k, slot) in iv.iter_mut().enumerate().skip(SAMPLE_BITS) {
        *slot = p.iv(k);
    }
    // What the device can do.
    let limits = [
        (SAMPLE_BITS, 16, 32),
        (FRAME_BITS, 16, 64),
        (CHANNELS, 1, 2),
        (RATE, 8000, 192000),
        (PERIOD_TIME, 1000, 1_000_000),
        (PERIOD_SIZE, 64, 65536),
        (PERIOD_BYTES, 128, 1 << 20),
        (PERIODS, 2, 64),
        (BUFFER_TIME, 2000, 4_000_000),
        (BUFFER_SIZE, 128, 1 << 20),
        (BUFFER_BYTES, 256, 4 << 20),
        (TICK_TIME, 0, 1_000_000),
    ];
    for (k, lo, hi) in limits {
        iv[k].refine(Iv { min: lo, max: hi, integer: false });
    }
    iv[SAMPLE_BITS].refine(sample_bits(format));
    for k in [SAMPLE_BITS, FRAME_BITS, CHANNELS, PERIOD_SIZE, PERIOD_BYTES, PERIODS, BUFFER_SIZE, BUFFER_BYTES] {
        iv[k].integer = true;
    }
    for _ in 0..16 {
        let mut changed = false;
        let v = iv;
        changed |= iv[FRAME_BITS].refine(mul(v[SAMPLE_BITS], v[CHANNELS]));
        changed |= iv[SAMPLE_BITS].refine(div(v[FRAME_BITS], v[CHANNELS]));
        changed |= iv[CHANNELS].refine(div(v[FRAME_BITS], v[SAMPLE_BITS]));
        changed |= iv[PERIOD_BYTES].refine(muldivk(v[PERIOD_SIZE], v[FRAME_BITS], 8));
        changed |= iv[PERIOD_SIZE].refine(mulkdiv(v[PERIOD_BYTES], 8, v[FRAME_BITS]));
        changed |= iv[BUFFER_BYTES].refine(muldivk(v[BUFFER_SIZE], v[FRAME_BITS], 8));
        changed |= iv[BUFFER_SIZE].refine(mulkdiv(v[BUFFER_BYTES], 8, v[FRAME_BITS]));
        changed |= iv[BUFFER_SIZE].refine(mul(v[PERIOD_SIZE], v[PERIODS]));
        changed |= iv[PERIOD_SIZE].refine(div(v[BUFFER_SIZE], v[PERIODS]));
        changed |= iv[PERIODS].refine(div(v[BUFFER_SIZE], v[PERIOD_SIZE]));
        // Times and frame counts: a time narrower than one frame picks the
        // nearest whole frame count; times never fail on rounding.
        for (time, size) in [(PERIOD_TIME, PERIOD_SIZE), (BUFFER_TIME, BUFFER_SIZE)] {
            let (t, r) = (iv[time], iv[RATE]);
            if r.min == r.max && t.max.saturating_sub(t.min).saturating_mul(r.min) < 1_000_000 {
                let mid = (t.min + t.max) / 2;
                let n = (mid * r.min + 500_000) / 1_000_000;
                changed |= iv[size].refine(Iv { min: n.max(1), max: n.max(1), integer: true });
            } else {
                changed |= iv[size].refine(muldivk(t, r, 1_000_000));
            }
            let mut tt = iv[time];
            if tt.refine(mulkdiv(iv[size], 1_000_000, r)) && !tt.empty() {
                iv[time] = tt;
                changed = true;
            }
        }
        if iv.iter().enumerate().skip(SAMPLE_BITS).any(|(k, x)| x.empty() && !matches!(k, PERIOD_TIME | BUFFER_TIME | TICK_TIME)) {
            return Err(-EINVAL);
        }
        if !changed {
            break;
        }
    }
    // Keep the format mask consistent with the sample size.
    let bits = iv[SAMPLE_BITS];
    let mut fm = format;
    for (f, b) in [(FMT_S16_LE, 16u64), (FMT_S32_LE, 32), (FMT_FLOAT_LE, 32)] {
        if b < bits.min || b > bits.max {
            fm &= !(1 << f);
        }
    }
    if fm == 0 {
        return Err(-EINVAL);
    }
    p.set_mask(FORMAT, fm);
    for (k, v) in iv.iter().enumerate().skip(SAMPLE_BITS) {
        p.set_iv(k, *v);
    }
    let rmask = p.u32_at(RMASK);
    p.set_u32(CMASK, if rmask == 0 { u32::MAX } else { rmask });
    // INFO_INTERLEAVED | BLOCK_TRANSFER | PAUSE | RESUME
    p.set_u32(INFO, 0x100 | 0x10000 | 0x80000 | 0x40000);
    if bits.min == bits.max {
        p.set_u32(MSBITS, bits.min as u32);
    }
    let rate = iv[RATE];
    if rate.min == rate.max {
        p.set_u32(RATE_NUM, rate.min as u32);
        p.set_u32(RATE_DEN, 1);
    }
    Ok(())
}

// --- the PCM -----------------------------------------------------------

impl State {
    fn frame_bytes(&self) -> usize {
        let sb = if self.format == FMT_S16_LE { 2 } else { 4 };
        sb * self.channels as usize
    }

    /// Frames of ours not yet played (converted back from 48 kHz).
    fn queued(&self) -> u64 {
        match &self.stream {
            Some(s) => s.queued() as u64 * self.rate as u64 / 48000,
            None => 0,
        }
    }

    fn hw(&self) -> u64 {
        self.appl.saturating_sub(self.queued())
    }

    fn avail(&self) -> u64 {
        self.buffer.saturating_sub(self.appl - self.hw())
    }

    /// One input sample as i32 in 16-bit range.
    fn sample(&self, b: &[u8]) -> i32 {
        match self.format {
            FMT_S16_LE => i16::from_le_bytes([b[0], b[1]]) as i32,
            FMT_S32_LE => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) >> 16,
            _ => {
                let f = f32::from_bits(u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
                let v = f * 32767.0;
                if v != v {
                    0
                } else if v > 32767.0 {
                    32767
                } else if v < -32768.0 {
                    -32768
                } else {
                    v as i32
                }
            }
        }
    }

    /// Convert frames to 48 kHz stereo i16 and queue them.
    fn push(&mut self, data: &[u8]) {
        let fb = self.frame_bytes();
        let sb = fb / self.channels as usize;
        let step = ((self.rate as u64) << 16) / 48000; // input frames per output frame, 16.16
        let mut out: Vec<i16> = Vec::with_capacity(data.len() / fb * 2 * 48000 / self.rate as usize + 4);
        for f in data.chunks_exact(fb) {
            let l = self.sample(&f[..sb]);
            let r = if self.channels > 1 { self.sample(&f[sb..2 * sb]) } else { l };
            // Emit the output frames that fall between the last input frame
            // and this one.
            while self.phase < (1 << 16) {
                let t = self.phase as i64;
                let li = self.last.0 as i64 + ((l - self.last.0) as i64 * t >> 16);
                let ri = self.last.1 as i64 + ((r - self.last.1) as i64 * t >> 16);
                out.push(li as i16);
                out.push(ri as i16);
                self.phase += step.max(1);
            }
            self.phase -= 1 << 16;
            self.last = (l, r);
        }
        if let Some(s) = &self.stream {
            s.push(&out);
        }
    }
}

impl Pcm {
    pub fn ready_to_write(&self) -> bool {
        let s = self.st.lock();
        s.state != RUNNING && s.state != PREPARED || s.avail() >= s.avail_min
    }

    /// Play `frames` frames from user memory at `buf`.
    fn write(&self, pml4: u64, buf: u64, frames: u64, nonblock: bool) -> i64 {
        let mut done = 0u64;
        loop {
            let seen = sched::events();
            {
                let mut s = self.st.lock();
                if !matches!(s.state, PREPARED | RUNNING) {
                    return if done > 0 { done as i64 } else { -EBADFD };
                }
                let n = s.avail().min(frames - done);
                if n > 0 {
                    let fb = s.frame_bytes() as u64;
                    let Some(data) = usermem::read_bytes(pml4, buf + done * fb, n * fb) else { return -EFAULT };
                    s.push(&data);
                    s.appl += n;
                    FRAMES_WRITTEN.fetch_add(n, core::sync::atomic::Ordering::Relaxed);
                    done += n;
                    if s.state == PREPARED && s.appl >= s.start_threshold {
                        s.state = RUNNING;
                        if let Some(st) = &s.stream {
                            st.set_paused(false);
                        }
                    }
                }
                if done == frames {
                    return done as i64;
                }
            }
            if nonblock {
                return if done > 0 { done as i64 } else { -EAGAIN };
            }
            if sched::current_process().is_some_and(|p| super::signal::interrupted(&p)) {
                return if done > 0 { done as i64 } else { -EINTR };
            }
            sched::wait_event(seen, 5);
        }
    }

    fn status(&self) -> [u8; 152] {
        let s = self.st.lock();
        let mut b = [0u8; 152];
        b[0..4].copy_from_slice(&s.state.to_le_bytes());
        let now = crate::time::uptime_us();
        b[24..32].copy_from_slice(&(now / 1_000_000).to_le_bytes());
        b[32..40].copy_from_slice(&((now % 1_000_000) * 1000).to_le_bytes());
        b[40..48].copy_from_slice(&s.appl.to_le_bytes());
        b[48..56].copy_from_slice(&s.hw().to_le_bytes());
        b[56..64].copy_from_slice(&s.queued().to_le_bytes());
        b[64..72].copy_from_slice(&s.avail().to_le_bytes());
        b[72..80].copy_from_slice(&s.avail().to_le_bytes());
        b
    }
}

pub fn pcm_ioctl(pcm: &Arc<Pcm>, pml4: u64, cmd: u64, arg: u64, nonblock: bool) -> i64 {
    match cmd {
        0x8004_4100 => put(pml4, arg, &0x0002_000fu32.to_le_bytes()), // PVERSION 2.0.15
        0x8120_4101 => put(pml4, arg, &pcm_info()),
        0x4004_4102 | 0x4004_4103 | 0x4004_4104 => 0, // TSTAMP, TTSTAMP, USER_PVERSION
        0xc260_4110 | 0xc260_4111 => {
            // HW_REFINE / HW_PARAMS
            let Some(raw) = usermem::read_bytes(pml4, arg, HWP_SIZE as u64) else { return -EFAULT };
            let mut p = Params { raw };
            if let Err(e) = refine(&mut p) {
                return e;
            }
            if cmd == 0xc260_4111 {
                // HW_PARAMS: every parameter must be down to one value.
                let one = |k: usize| {
                    let v = p.iv(k);
                    v.min
                };
                let fm = p.mask(FORMAT);
                let mut s = pcm.st.lock();
                s.format = if fm & (1 << FMT_S16_LE) != 0 { FMT_S16_LE } else if fm & (1 << FMT_FLOAT_LE) != 0 { FMT_FLOAT_LE } else { FMT_S32_LE };
                s.channels = one(CHANNELS) as u32;
                s.rate = one(RATE) as u32;
                s.period = one(PERIOD_SIZE);
                s.buffer = one(BUFFER_SIZE).max(s.period);
                s.state = SETUP;
                s.appl = 0;
                if let Some(old) = s.stream.take() {
                    old.close();
                }
                // Every parameter down to exactly one value, consistent
                // with the chosen frame counts (alsa-lib reads them back).
                let sb = if s.format == FMT_S16_LE { 16u64 } else { 32 };
                let fb = sb * s.channels as u64;
                let rate = s.rate as u64;
                let single = |v: u64, integer: bool| Iv { min: v, max: v, integer };
                let (period, buffer) = (s.period, s.buffer);
                p.set_iv(SAMPLE_BITS, single(sb, true));
                p.set_iv(FRAME_BITS, single(fb, true));
                p.set_iv(CHANNELS, single(s.channels as u64, true));
                p.set_iv(RATE, single(rate, true));
                p.set_iv(PERIOD_SIZE, single(period, true));
                p.set_iv(PERIOD_BYTES, single(period * fb / 8, true));
                p.set_iv(PERIODS, single((buffer / period.max(1)).max(1), true));
                p.set_iv(BUFFER_SIZE, single(buffer, true));
                p.set_iv(BUFFER_BYTES, single(buffer * fb / 8, true));
                p.set_iv(PERIOD_TIME, single(period * 1_000_000 / rate, false));
                p.set_iv(BUFFER_TIME, single(buffer * 1_000_000 / rate, false));
                p.set_iv(TICK_TIME, single(0, false));
                let fmt_mask = 1u32 << s.format;
                p.set_mask(FORMAT, fmt_mask);
                p.set_mask(ACCESS, 1 << ACCESS_RW_INTERLEAVED);
                p.set_mask(SUBFORMAT, 1);
                p.set_u32(MSBITS, sb as u32);
                p.set_u32(RATE_NUM, s.rate);
                p.set_u32(RATE_DEN, 1);
            }
            put(pml4, arg, &p.raw)
        }
        0x4112 => {
            // HW_FREE
            let mut s = pcm.st.lock();
            if let Some(old) = s.stream.take() {
                old.close();
            }
            s.state = OPEN;
            0
        }
        0xc088_4113 => {
            // SW_PARAMS
            let Some(b) = usermem::read_bytes(pml4, arg, 136) else { return -EFAULT };
            let g = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
            let mut s = pcm.st.lock();
            s.avail_min = g(16).max(1);
            s.start_threshold = g(32).max(1);
            if g(64) != 0 {
                s.boundary = g(64);
            }
            0
        }
        0x8098_4120 | 0xc098_4124 => put(pml4, arg, &pcm.status()), // STATUS, STATUS_EXT
        0x8008_4121 => {
            // DELAY
            let d = pcm.st.lock().queued();
            put(pml4, arg, &d.to_le_bytes())
        }
        0x4122 => 0, // HWSYNC
        0xc088_4123 => {
            // SYNC_PTR
            let Some(mut b) = usermem::read_bytes(pml4, arg, 136) else { return -EFAULT };
            let flags = u32::from_le_bytes(b[0..4].try_into().unwrap());
            let mut s = pcm.st.lock();
            if flags & 2 == 0 {
                // The program's appl_ptr (it may have moved it with rewind).
                let appl = u64::from_le_bytes(b[72..80].try_into().unwrap());
                if appl <= s.appl {
                    s.appl = s.appl.max(appl);
                }
            }
            if flags & 4 == 0 {
                s.avail_min = u64::from_le_bytes(b[80..88].try_into().unwrap()).max(1);
            }
            b[8..12].copy_from_slice(&s.state.to_le_bytes());
            b[16..24].copy_from_slice(&s.hw().to_le_bytes());
            b[72..80].copy_from_slice(&s.appl.to_le_bytes());
            b[80..88].copy_from_slice(&s.avail_min.to_le_bytes());
            drop(s);
            put(pml4, arg, &b)
        }
        0x4140 | 0x4141 => {
            // PREPARE / RESET
            let mut s = pcm.st.lock();
            if s.state == OPEN {
                return -EBADFD;
            }
            if s.stream.is_none() {
                let st = audio::open_stream();
                st.set_paused(true);
                s.stream = Some(st);
            } else if let Some(st) = &s.stream {
                st.reset(0);
                st.set_paused(true);
            }
            s.appl = 0;
            s.phase = 0;
            s.state = PREPARED;
            0
        }
        0x4142 => {
            // START
            let mut s = pcm.st.lock();
            if s.state != PREPARED {
                return -EBADFD;
            }
            s.state = RUNNING;
            if let Some(st) = &s.stream {
                st.set_paused(false);
            }
            0
        }
        0x4143 => {
            // DROP
            let mut s = pcm.st.lock();
            if let Some(st) = &s.stream {
                st.reset(0);
                st.set_paused(true);
            }
            s.appl = 0;
            s.state = SETUP;
            0
        }
        0x4144 => {
            // DRAIN: wait until everything queued has played.
            {
                let mut s = pcm.st.lock();
                if s.state == PREPARED {
                    s.state = SETUP;
                    return 0;
                }
                if s.state != RUNNING {
                    return 0;
                }
                s.state = DRAINING;
                if let Some(st) = &s.stream {
                    st.set_paused(false);
                }
            }
            loop {
                if pcm.st.lock().queued() == 0 || nonblock {
                    break;
                }
                sched::sleep_ms(5);
            }
            let mut s = pcm.st.lock();
            s.state = SETUP;
            s.appl = 0;
            if nonblock { -EAGAIN } else { 0 }
        }
        0x4004_4145 => {
            // PAUSE (arg is the value, not a pointer)
            let mut s = pcm.st.lock();
            let on = arg != 0;
            s.state = if on { PAUSED } else { RUNNING };
            if let Some(st) = &s.stream {
                st.set_paused(on);
            }
            0
        }
        0x4147 => 0, // RESUME
        0x4008_4146 | 0x4008_4149 => 0, // REWIND / FORWARD: nothing moved
        0x4018_4150 => {
            // WRITEI_FRAMES: struct snd_xferi { result, buf, frames }
            let Some(b) = usermem::read_bytes(pml4, arg, 24) else { return -EFAULT };
            let buf = u64::from_le_bytes(b[8..16].try_into().unwrap());
            let frames = u64::from_le_bytes(b[16..24].try_into().unwrap());
            let r = pcm.write(pml4, buf, frames, nonblock);
            if r >= 0 {
                usermem::write_u64(pml4, arg, r as u64);
                0
            } else {
                r
            }
        }
        0x4004_4160 | 0x4161 => -EINVAL, // LINK / UNLINK
        _ => -ENOTTY,
    }
}

/// write() on the PCM: interleaved frames.
pub fn pcm_write(pcm: &Arc<Pcm>, pml4: u64, buf: u64, len: u64, nonblock: bool) -> i64 {
    let fb = pcm.st.lock().frame_bytes() as u64;
    let r = pcm.write(pml4, buf, len / fb.max(1), nonblock);
    if r > 0 { r * fb as i64 } else { r }
}

pub fn is_running_state(pcm: &Pcm) -> bool {
    matches!(pcm.st.lock().state, RUNNING | DRAINING)
}
