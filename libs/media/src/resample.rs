//! Streaming sample-rate conversion to 48 kHz stereo (cubic Hermite
//! interpolation, fixed point).

use alloc::vec::Vec;

pub struct Resampler {
    in_rate: u32,
    out_rate: u32,
    /// Input frames per output frame, 32.32 fixed point.
    step: u64,
    /// Position between hist[1] and hist[2], 32.32 fixed point (< 1.0).
    pos: u64,
    /// Last four input frames per channel (oldest first).
    hist: [[i32; 4]; 2],
    primed: usize,
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Resampler {
        let step = ((in_rate.max(1) as u64) << 32) / out_rate.max(1) as u64;
        Resampler { in_rate, out_rate, step, pos: 0, hist: [[0; 4]; 2], primed: 0 }
    }

    pub fn in_rate(&self) -> u32 {
        self.in_rate
    }

    pub fn reset(&mut self) {
        self.pos = 0;
        self.hist = [[0; 4]; 2];
        self.primed = 0;
    }

    /// Convert interleaved input (`channels` 1 or 2) and append stereo
    /// output samples.
    pub fn process(&mut self, input: &[i16], channels: usize, out: &mut Vec<i16>) {
        let ch = channels.clamp(1, 2);
        let frames = input.len() / ch;
        if self.in_rate == self.out_rate {
            out.reserve(frames * 2);
            for f in 0..frames {
                let l = input[f * ch];
                let r = input[f * ch + ch - 1];
                out.push(l);
                out.push(r);
            }
            return;
        }
        out.reserve((frames as u64 * self.out_rate as u64 / self.in_rate as u64) as usize * 2 + 4);
        let one = 1u64 << 32;
        for f in 0..frames {
            for c in 0..2 {
                let h = &mut self.hist[c];
                h[0] = h[1];
                h[1] = h[2];
                h[2] = h[3];
                h[3] = input[f * ch + c.min(ch - 1)] as i32;
            }
            if self.primed < 3 {
                self.primed += 1;
                continue;
            }
            // Emit outputs while the position lies between hist[1] and hist[2].
            while self.pos < one {
                let t = (self.pos >> 16) as i64; // 16-bit fraction
                for c in 0..2 {
                    let [p0, p1, p2, p3] = self.hist[c].map(|v| v as i64);
                    // Catmull-Rom spline.
                    let a = -p0 + 3 * p1 - 3 * p2 + p3;
                    let b = 2 * p0 - 5 * p1 + 4 * p2 - p3;
                    let cc = -p0 + p2;
                    let d = 2 * p1;
                    let v = ((((a * t >> 16) + b) * t >> 16) + cc) * t >> 16;
                    let s = (v + d) >> 1;
                    out.push(s.clamp(-32768, 32767) as i16);
                }
                self.pos += self.step;
            }
            self.pos -= one;
        }
    }
}
