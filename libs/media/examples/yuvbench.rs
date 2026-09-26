//! Time YUV 4:2:0 -> ARGB conversion with scaling: `yuvbench`
fn main() {
    let (w, h) = (1280usize, 720usize);
    let y: Vec<u8> = (0..w * h).map(|i| (i * 7) as u8).collect();
    let u: Vec<u8> = (0..w * h / 4).map(|i| (i * 3) as u8).collect();
    let v = u.clone();
    for (dw, dh) in [(1280usize, 720usize), (1920, 1080), (960, 540)] {
        let mut dst = vec![0u32; dw * dh];
        let t = std::time::Instant::now();
        for _ in 0..20 {
            media::yuv::scale_to_argb(&y, &u, &v, w, w / 2, 0, 0, w, h, true, false, &mut dst, dw, dh, dw);
        }
        println!("{}x{} -> {}x{}: {:.2} ms (checksum {})", w, h, dw, dh, t.elapsed().as_secs_f64() * 1000.0 / 20.0, dst.iter().fold(0u32, |a, &b| a.wrapping_mul(31).wrapping_add(b)));
    }
}
