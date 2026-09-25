//! Audio output: a software mixer feeding the AC'97 controller.
//!
//! Sounds are 48 kHz stereo 16-bit sample vectors. `play` queues one; the
//! audio thread mixes all active sounds into the DMA ring a few buffers
//! ahead of the hardware.

pub mod ac97;
pub mod sounds;
pub mod wav;

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use ac97::Ac97;

use crate::sync::{Once, Spin};

struct Voice {
    id: u64,
    samples: Arc<Vec<i16>>,
    pos: usize,
}

struct Mixer {
    dev: Ac97,
    voices: Vec<Voice>,
    idle_since: u64,
}

static MIXER: Spin<Option<Mixer>> = Spin::new(None);
static STREAMS: Spin<Vec<Arc<AudioStream>>> = Spin::new(Vec::new());

/// A continuous sound fed piece by piece (music and video players).
/// Samples are 48 kHz stereo.
pub struct AudioStream {
    queue: Spin<VecDeque<i16>>,
    /// Frames the mixer has taken from the queue.
    played: AtomicU64,
    paused: AtomicBool,
    closed: AtomicBool,
    /// 0..=256
    volume: AtomicU32,
}

impl AudioStream {
    pub fn push(&self, samples: &[i16]) {
        self.queue.lock().extend(samples.iter().copied());
    }

    /// Frames waiting to be played.
    pub fn queued(&self) -> usize {
        self.queue.lock().len() / 2
    }

    /// Frames handed to the sound card so far.
    pub fn played(&self) -> u64 {
        self.played.load(Ordering::Relaxed)
    }

    /// Drop everything queued (after a seek); `played` restarts at `frames`.
    pub fn reset(&self, frames: u64) {
        self.queue.lock().clear();
        self.played.store(frames, Ordering::Relaxed);
    }

    pub fn set_paused(&self, p: bool) {
        self.paused.store(p, Ordering::Relaxed);
    }

    pub fn set_volume(&self, v: u32) {
        self.volume.store(v.min(256), Ordering::Relaxed);
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }
}

pub fn open_stream() -> Arc<AudioStream> {
    let s = Arc::new(AudioStream {
        queue: Spin::new(VecDeque::new()),
        played: AtomicU64::new(0),
        paused: AtomicBool::new(false),
        closed: AtomicBool::new(false),
        volume: AtomicU32::new(256),
    });
    STREAMS.lock().push(s.clone());
    s
}

/// Frames between being mixed and being heard (the DMA queue depth).
pub fn latency_frames() -> u64 {
    (AHEAD as u64) * ac97::FRAMES as u64
}
static DEVICE_NAME: Once<String> = Once::new();
static VOLUME: AtomicU32 = AtomicU32::new(80);
static MUTED: AtomicBool = AtomicBool::new(false);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
/// Frames mixed so far (lets the self-test see that audio is flowing).
pub static FRAMES_MIXED: AtomicU64 = AtomicU64::new(0);

const AHEAD: u8 = 4;

pub fn init(dev: Ac97, name: &str) {
    DEVICE_NAME.set(String::from(name));
    *MIXER.lock() = Some(Mixer { dev, voices: Vec::new(), idle_since: 0 });
    apply_volume();
    crate::proc::sched::spawn_kernel("audio", audio_thread, 0);
}

pub fn is_present() -> bool {
    MIXER.lock().is_some()
}

pub fn device_name() -> String {
    DEVICE_NAME.get().cloned().unwrap_or_else(|| String::from("No sound card found"))
}

pub fn set_volume(percent: u8, muted: bool) {
    VOLUME.store(percent.min(100) as u32, Ordering::Relaxed);
    MUTED.store(muted, Ordering::Relaxed);
    apply_volume();
}

fn apply_volume() {
    if let Some(m) = MIXER.lock().as_mut() {
        m.dev.set_volume(VOLUME.load(Ordering::Relaxed) as u8, MUTED.load(Ordering::Relaxed));
    }
}

/// Queue a sound; returns an id usable with `stop`.
pub fn play(samples: Arc<Vec<i16>>) -> Option<u64> {
    let mut g = MIXER.lock();
    let m = g.as_mut()?;
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    if m.voices.len() < 16 {
        m.voices.push(Voice { id, samples, pos: 0 });
    }
    Some(id)
}

pub fn stop(id: u64) {
    if let Some(m) = MIXER.lock().as_mut() {
        m.voices.retain(|v| v.id != id);
    }
}

pub fn is_playing(id: u64) -> bool {
    MIXER.lock().as_ref().map(|m| m.voices.iter().any(|v| v.id == id)).unwrap_or(false)
}

/// Which system sound to play; respects the "system sounds" setting.
#[derive(Clone, Copy)]
pub enum SystemSound {
    Startup,
    Notify,
    Error,
    Click,
    Test,
}

pub fn play_system(s: SystemSound) {
    static CACHE: Spin<Vec<(u8, Arc<Vec<i16>>)>> = Spin::new(Vec::new());
    if !is_present() {
        return;
    }
    if !matches!(s, SystemSound::Test) && !crate::settings::get().system_sounds {
        return;
    }
    let key = s as u8;
    let cached = CACHE.lock().iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone());
    let samples = match cached {
        Some(v) => v,
        None => {
            let v = Arc::new(match s {
                SystemSound::Startup => sounds::startup(),
                SystemSound::Notify => sounds::notify(),
                SystemSound::Error => sounds::error(),
                SystemSound::Click => sounds::click(),
                SystemSound::Test => sounds::test(),
            });
            CACHE.lock().push((key, v.clone()));
            v
        }
    };
    play(samples);
}

fn fill(m: &mut Mixer, buf: usize) {
    let frames = ac97::FRAMES;
    let mut acc = [0i32; ac97::FRAMES * 2];
    for v in m.voices.iter_mut() {
        let n = (v.samples.len() - v.pos).min(frames * 2);
        for (a, s) in acc[..n].iter_mut().zip(&v.samples[v.pos..v.pos + n]) {
            *a += *s as i32;
        }
        v.pos += n;
    }
    m.voices.retain(|v| v.pos < v.samples.len());
    {
        let mut streams = STREAMS.lock();
        streams.retain(|s| !s.closed.load(Ordering::Relaxed));
        for s in streams.iter() {
            if s.paused.load(Ordering::Relaxed) {
                continue;
            }
            let vol = s.volume.load(Ordering::Relaxed) as i32;
            let mut q = s.queue.lock();
            let n = q.len().min(frames * 2) & !1;
            for (a, v) in acc[..n].iter_mut().zip(q.drain(..n)) {
                *a += (v as i32 * vol) >> 8;
            }
            s.played.fetch_add((n / 2) as u64, Ordering::Relaxed);
        }
    }
    let out = m.dev.buffer(buf);
    for (o, a) in out.iter_mut().zip(acc.iter()) {
        *o = (*a).clamp(-32768, 32767) as i16;
    }
    FRAMES_MIXED.fetch_add(frames as u64, Ordering::Relaxed);
}

extern "C" fn audio_thread(_: usize) {
    loop {
        {
            let mut g = MIXER.lock();
            if let Some(m) = g.as_mut() {
                let now = crate::time::uptime_ms();
                let streaming = STREAMS.lock().iter().any(|s| !s.paused.load(Ordering::Relaxed) && s.queued() > 0);
                if m.voices.is_empty() && !streaming && m.dev.is_running() {
                    // Stop the DMA engine after a quiet second.
                    if m.idle_since == 0 {
                        m.idle_since = now;
                    } else if now - m.idle_since > 1000 {
                        m.dev.stop();
                    }
                } else if !m.voices.is_empty() || streaming {
                    m.idle_since = 0;
                    if !m.dev.is_running() {
                        // Restart: queue from the buffer the engine points at.
                        let civ = m.dev.current();
                        m.dev.set_last_valid(civ.wrapping_sub(1) & 31);
                    }
                }
                if !m.voices.is_empty() || streaming || m.dev.is_running() {
                    let civ = m.dev.current();
                    let mut lvi = m.dev.last_valid();
                    let mut filled = false;
                    while lvi.wrapping_sub(civ) & 31 < AHEAD {
                        lvi = (lvi + 1) & 31;
                        fill(m, lvi as usize);
                        filled = true;
                    }
                    if filled {
                        m.dev.set_last_valid(lvi);
                    }
                    m.dev.kick();
                }
            }
        }
        crate::proc::sched::sleep_ms(5);
    }
}
