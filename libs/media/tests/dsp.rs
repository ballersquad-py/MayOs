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
