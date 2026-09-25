use media::dsp;

fn reference(input: &[f64], n: usize) -> Vec<f64> {
    (0..n)
        .map(|i| {
            let mut s = 0.0;
            for (k, &x) in input.iter().enumerate() {
                s += x * (std::f64::consts::PI / (2.0 * n as f64) * (2.0 * i as f64 + 1.0 + n as f64 / 2.0) * (2.0 * k as f64 + 1.0)).cos();
            }
            s * 2.0 / n as f64
        })
        .collect()
}

#[test]
fn imdct_matches_definition() {
    for &n in &[256usize, 2048] {
        let mut seed = 12345u32;
        let input: Vec<i32> = (0..n / 2)
            .map(|k| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                let r = (seed >> 8) as i32 % 2_000_000 - 1_000_000;
                r / (1 + k as i32 / 16)
            })
            .collect();
        let mut out = vec![0i32; n];
        dsp::imdct(&input, &mut out);
        let want = reference(&input.iter().map(|&v| v as f64).collect::<Vec<_>>(), n);
        let mut max_err = 0f64;
        let mut peak = 0f64;
        for i in 0..n {
            max_err = max_err.max((out[i] as f64 - want[i]).abs());
            peak = peak.max(want[i].abs());
        }
        println!("n={} peak {:.0} max error {:.2}", n, peak, max_err);
        assert!(max_err < 4.0, "imdct error {}", max_err);
    }
}

#[test]
fn pow43_is_accurate() {
    let t = dsp::pow43_table();
    for q in [0usize, 1, 2, 7, 100, 1000, 8191] {
        let want = (q as f64).powf(4.0 / 3.0) * 8192.0;
        assert!((t[q] as f64 - want).abs() <= 1.0, "{} {} {}", q, t[q], want);
    }
}

#[test]
fn resampler_keeps_a_sine_clean() {
    let rate = 44100;
    let input: Vec<i16> = (0..rate).map(|i| ((i as f64 * 2.0 * std::f64::consts::PI * 1000.0 / rate as f64).sin() * 20000.0) as i16).collect();
    let mut r = media::resample::Resampler::new(rate, 48000);
    let mut out = Vec::new();
    for chunk in input.chunks(1000) {
        r.process(chunk, 1, &mut out);
    }
    let frames = out.len() / 2;
    assert!((frames as i64 - 48000).abs() < 10, "{} frames", frames);
    // Compare with an ideal 1 kHz sine, allowing for the filter delay.
    let mut best = 0.0f64;
    for delay in 0..32 {
        let off = delay as f64 * 0.125;
        let (mut sig, mut err) = (0.0, 0.0);
        for i in 100..frames - 100 {
            let t_in = i as f64 * 44100.0 / 48000.0 + off;
            let want = (t_in * 2.0 * std::f64::consts::PI * 1000.0 / 44100.0).sin() * 20000.0;
            let got = out[i * 2] as f64;
            sig += want * want;
            err += (want - got) * (want - got);
        }
        best = f64::max(best, 10.0 * (sig / err).log10());
    }
    println!("resampler SNR {:.1} dB", best);
    assert!(best > 40.0);
}
