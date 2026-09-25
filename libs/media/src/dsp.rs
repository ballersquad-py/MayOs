//! Fixed-point DSP shared by the audio decoders: inverse MDCT via FFT,
//! |x|^(4/3) table and helpers.

use alloc::vec;
use alloc::vec::Vec;

use crate::tables::{FFT512_COS, FFT512_SIN, IMDCT256_COS, IMDCT256_SIN, IMDCT2048_COS, IMDCT2048_SIN};

#[inline(always)]
pub fn mul30(a: i32, b: i32) -> i32 {
    ((a as i64 * b as i64 + (1 << 29)) >> 30) as i32
}

#[inline(always)]
pub fn mul30_64(a: i64, b: i32) -> i64 {
    (a * b as i64 + (1 << 29)) >> 30
}

/// |q|^(4/3) * 2^13 for q in 0..8192.
pub fn pow43_table() -> Vec<u32> {
    let mut t = vec![0u32; 8192];
    for (q, v) in t.iter_mut().enumerate() {
        let target = (q as u128).pow(4) << 39;
        let (mut lo, mut hi) = (0u64, 1u64 << 31);
        while lo < hi {
            let mid = (lo + hi + 1) / 2;
            if (mid as u128).pow(3) <= target {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        // Round to nearest.
        let a = (lo as u128).pow(3);
        let b = ((lo + 1) as u128).pow(3);
        *v = if target - a > b - target { (lo + 1) as u32 } else { lo as u32 };
    }
    t
}

/// In-place complex FFT (e^{+i}) of size n (power of two, <= 512) on
/// bit-reversed input; each stage halves the values (total 1/n scaling).
fn fft(re: &mut [i32], im: &mut [i32], n: usize) {
    let mut size = 2;
    while size <= n {
        let half = size / 2;
        let step = 512 / size;
        let mut start = 0;
        while start < n {
            for k in 0..half {
                let (c, s) = (FFT512_COS[k * step], FFT512_SIN[k * step]);
                let a = start + k;
                let b = a + half;
                let tr = mul30(re[b], c) - mul30(im[b], s);
                let ti = mul30(re[b], s) + mul30(im[b], c);
                let (ar, ai) = (re[a], im[a]);
                re[a] = (ar + tr) >> 1;
                im[a] = (ai + ti) >> 1;
                re[b] = (ar - tr) >> 1;
                im[b] = (ai - ti) >> 1;
            }
            start += size;
        }
        size *= 2;
    }
}

fn bitrev(x: usize, bits: u32) -> usize {
    x.reverse_bits() >> (usize::BITS - bits)
}

/// Inverse MDCT: `input` has n/2 coefficients, `out` receives n samples of
/// (2/n) * sum(X[k] cos(pi/(2n) (2i + 1 + n/2)(2k + 1))).
pub fn imdct(input: &[i32], out: &mut [i32]) {
    let n = out.len();
    let (tcos, tsin): (&[i32], &[i32]) = match n {
        2048 => (&IMDCT2048_COS, &IMDCT2048_SIN),
        256 => (&IMDCT256_COS, &IMDCT256_SIN),
        _ => panic!("imdct size"),
    };
    let n2 = n / 2;
    let n4 = n / 4;
    let n8 = n / 8;
    let bits = n4.trailing_zeros();
    let mut re = [0i32; 512];
    let mut im = [0i32; 512];
    for k in 0..n4 {
        let j = bitrev(k, bits);
        let a = input[n2 - 1 - 2 * k];
        let b = input[2 * k];
        re[j] = mul30(a, tcos[k]) - mul30(b, tsin[k]);
        im[j] = mul30(a, tsin[k]) + mul30(b, tcos[k]);
    }
    fft(&mut re[..n4], &mut im[..n4], n4);
    let mut half = vec![0i32; n2];
    for k in 0..n8 {
        let (a, b) = (n8 - k - 1, n8 + k);
        let r0 = mul30(im[a], tsin[a]) - mul30(re[a], tcos[a]);
        let i1 = mul30(im[a], tcos[a]) + mul30(re[a], tsin[a]);
        let r1 = mul30(im[b], tsin[b]) - mul30(re[b], tcos[b]);
        let i0 = mul30(im[b], tcos[b]) + mul30(re[b], tsin[b]);
        half[2 * a] = r0;
        half[2 * a + 1] = i0;
        half[2 * b] = r1;
        half[2 * b + 1] = i1;
    }
    // The FFT scaled by 1/n4; the definition wants 2/n = 1/(2 n4), and
    // this factorisation yields the negated sum.
    for k in 0..n2 {
        out[n4 + k] = -(half[k] >> 1);
    }
    for k in 0..n4 {
        out[k] = -out[n2 - k - 1];
        out[n - k - 1] = out[n2 + k];
    }
}

/// Integer square root of a u64.
pub fn isqrt(v: u64) -> u64 {
    if v < 2 {
        return v;
    }
    let mut x = f64_free::F::from(v).sqrt_approx();
    loop {
        let y = (x + v / x) / 2;
        if y >= x {
            break;
        }
        x = y;
    }
    while x * x > v {
        x -= 1;
    }
    while (x + 1) * (x + 1) <= v {
        x += 1;
    }
    x
}

mod f64_free {
    /// Rough starting point for Newton's method without floating point.
    pub struct F(u64);
    impl From<u64> for F {
        fn from(v: u64) -> F {
            F(v)
        }
    }
    impl F {
        pub fn sqrt_approx(self) -> u64 {
            let bits = 64 - self.0.leading_zeros();
            (1u64 << bits.div_ceil(2)).max(1)
        }
    }
}
