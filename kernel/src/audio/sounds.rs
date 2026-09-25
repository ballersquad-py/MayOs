//! Synthesised system sounds (48 kHz stereo).

use alloc::vec::Vec;

const RATE: u32 = 48000;

/// 1024-entry sine table scaled to +-32767.
fn sine_table() -> [i16; 1024] {
    let mut t = [0i16; 1024];
    for (i, v) in t.iter_mut().enumerate() {
        let x = i as f64 / 1024.0 * 2.0 * core::f64::consts::PI;
        *v = (sin(x) * 32767.0) as i16;
    }
    t
}

fn sin(x: f64) -> f64 {
    use core::f64::consts::PI;
    // Reduce to [-pi, pi], then to [-pi/2, pi/2].
    let mut x = x % (2.0 * PI);
    if x > PI {
        x -= 2.0 * PI;
    }
    if x > PI / 2.0 {
        x = PI - x;
    } else if x < -PI / 2.0 {
        x = -PI - x;
    }
    let x2 = x * x;
    x * (1.0 - x2 / 6.0 * (1.0 - x2 / 20.0 * (1.0 - x2 / 42.0 * (1.0 - x2 / 72.0))))
}

/// A note: frequency in Hz, start and length in ms, amplitude 0..=1000,
/// pan -100 (left) ..= 100 (right).
struct Note {
    freq: u32,
    start_ms: u32,
    len_ms: u32,
    amp: i32,
    pan: i32,
}

fn render(notes: &[Note]) -> Vec<i16> {
    let table = sine_table();
    let end = notes.iter().map(|n| n.start_ms + n.len_ms).max().unwrap_or(0);
    let frames = (end * RATE / 1000) as usize;
    let mut mix = alloc::vec![0i32; frames * 2];
    for n in notes {
        let start = (n.start_ms * RATE / 1000) as usize;
        let len = (n.len_ms * RATE / 1000) as usize;
        let attack = (RATE / 200) as usize; // 5 ms
        let step = ((n.freq as u64) << 32) / RATE as u64 * 1024;
        let mut phase: u64 = 0;
        for i in 0..len {
            if start + i >= frames {
                break;
            }
            // Fast attack, exponential-ish (quadratic) decay.
            let env = if i < attack {
                (i * 1000 / attack) as i32
            } else {
                let rem = (len - i) as i64 * 1000 / len as i64;
                (rem * rem / 1000) as i32
            };
            let idx = ((phase >> 32) & 1023) as usize;
            // Add a quiet octave overtone for a bell-like timbre.
            let idx2 = ((phase >> 31) & 1023) as usize;
            let s = (table[idx] as i32 * 3 + table[idx2] as i32) / 4;
            let v = s * env / 1000 * n.amp / 1000;
            let l = v * (100 - n.pan).min(100) / 100;
            let r = v * (100 + n.pan).min(100) / 100;
            mix[(start + i) * 2] += l;
            mix[(start + i) * 2 + 1] += r;
            phase = phase.wrapping_add(step);
        }
    }
    mix.into_iter().map(|v| v.clamp(-32767, 32767) as i16).collect()
}

pub fn startup() -> Vec<i16> {
    // C major arpeggio: C5 E5 G5 C6.
    render(&[
        Note { freq: 523, start_ms: 0, len_ms: 900, amp: 380, pan: -30 },
        Note { freq: 659, start_ms: 120, len_ms: 900, amp: 340, pan: -10 },
        Note { freq: 784, start_ms: 240, len_ms: 900, amp: 320, pan: 10 },
        Note { freq: 1047, start_ms: 360, len_ms: 1200, amp: 300, pan: 30 },
    ])
}

pub fn click() -> Vec<i16> {
    render(&[Note { freq: 1800, start_ms: 0, len_ms: 18, amp: 250, pan: 0 }])
}

pub fn notify() -> Vec<i16> {
    render(&[
        Note { freq: 880, start_ms: 0, len_ms: 250, amp: 350, pan: 0 },
        Note { freq: 1320, start_ms: 110, len_ms: 400, amp: 300, pan: 0 },
    ])
}

pub fn error() -> Vec<i16> {
    render(&[
        Note { freq: 330, start_ms: 0, len_ms: 180, amp: 400, pan: 0 },
        Note { freq: 247, start_ms: 150, len_ms: 300, amp: 400, pan: 0 },
    ])
}

/// Left channel, then right channel, so speakers can be checked.
pub fn test() -> Vec<i16> {
    render(&[
        Note { freq: 440, start_ms: 0, len_ms: 600, amp: 500, pan: -100 },
        Note { freq: 660, start_ms: 700, len_ms: 600, amp: 500, pan: 100 },
    ])
}
