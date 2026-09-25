//! Spectrum bars for a music visualiser (integer DFT at log-spaced bins).

use crate::tables::{HANN512_Q14, SIN512_Q14};

/// Band levels 0..=255 for `bands` log-spaced bands from 512 mono samples.
pub fn spectrum(samples: &[i16; 512], out: &mut [u8]) {
    let bands = out.len().max(1);
    for (b, o) in out.iter_mut().enumerate() {
        // Log-spaced DFT bins (about 190 Hz .. 18 kHz at 48 kHz).
        let k = bin_for(b, bands);
        let (mut re, mut im) = (0i64, 0i64);
        for (n, &s) in samples.iter().enumerate() {
            let x = (s as i64 * HANN512_Q14[n] as i64) >> 14;
            let idx = (k * n) & 511;
            re += x * SIN512_Q14[(idx + 128) & 511] as i64;
            im += x * SIN512_Q14[idx] as i64;
        }
        let re = (re >> 14) as i64;
        let im = (im >> 14) as i64;
        let mag = crate::dsp::isqrt((re * re + im * im) as u64) as u64;
        // Rough log scale: 16 steps per octave above a noise floor.
        let lg = if mag < 64 { 0 } else { (63 - mag.leading_zeros() as u64 - 6) * 24 + ((mag << (mag.leading_zeros())) >> 59 & 15) * 24 / 16 };
        *o = lg.min(255) as u8;
    }
}

/// Log-spaced DFT bins for 32 bands (512-point DFT at 48 kHz).
const BINS: [usize; 32] = [2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 17, 19, 23, 26, 31, 36, 42, 48, 56, 65, 76, 89, 103, 120, 140, 162, 189, 220];

fn bin_for(b: usize, bands: usize) -> usize {
    BINS[(b * 32 / bands.max(1)).min(31)]
}
