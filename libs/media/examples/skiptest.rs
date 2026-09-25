//! Reproduce the player's "skip to the next keyframe" and compare the
//! frames that come out with a reference decode (raw yuv420p file):
//! `skiptest <file.mp4> <ref.yuv> <skip_at_seconds>`
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&a[1]).unwrap();
    let reference = std::fs::read(&a[2]).unwrap();
    let skip_at: i64 = (a[3].parse::<f64>().unwrap() * 1e6) as i64;
    let mut dm = media::demux::open(Box::new(data)).unwrap();
    let info = dm.info().clone();
    let (vi, _) = media::pipeline::choose_tracks(&info.tracks);
    let vi = vi.unwrap();
    let t = &info.tracks[vi];
    let (w, h) = (t.width as usize, t.height as usize);
    let fs = w * h * 3 / 2;
    let frame_us = if t.frame_us > 0 { t.frame_us as i64 } else { 40_000 };
    let mut dec = media::pipeline::VideoDecoder::new(t).unwrap();
    let mut skipping = false;
    let mut done_skip = false;
    let mut bad = 0;
    let mut shown = 0;
    let mut check = |f: media::pipeline::VideoFrame, bad: &mut i32, shown: &mut i32| {
        let idx = ((f.pts + frame_us / 2) / frame_us) as usize;
        let mut out = vec![0u32; w * h];
        f.render(&mut out, w, h, w);
        let r = &reference[idx * fs..idx * fs + w * h];
        // Compare luma via the ARGB green channel (roughly luma).
        let mut diff = 0u64;
        for i in (0..w * h).step_by(97) {
            let g = ((out[i] >> 8) & 255) as i64;
            diff += (g - r[i] as i64).unsigned_abs();
        }
        let avg = diff as f64 / ((w * h / 97) as f64);
        *shown += 1;
        if avg > 20.0 {
            *bad += 1;
            println!("frame pts {:.3}s looks corrupt (avg diff {:.1})", f.pts as f64 / 1e6, avg);
        }
    };
    while let Some(Ok(p)) = dm.next_packet() {
        if p.track != vi {
            continue;
        }
        if !done_skip && p.pts >= skip_at - 1_500_000 && !p.key {
            // Pretend we fell behind: drop packets until the next keyframe.
            skipping = true;
            continue;
        }
        if skipping {
            if !p.key {
                continue;
            }
            skipping = false;
            done_skip = true;
            dec.reset();
            println!("resumed at keyframe pts {:.3}s", p.pts as f64 / 1e6);
        }
        dec.decode(&p);
        while let Some(f) = dec.next_frame() {
            check(f, &mut bad, &mut shown);
        }
    }
    dec.flush();
    while let Some(f) = dec.next_frame() {
        check(f, &mut bad, &mut shown);
    }
    println!("{} frames shown, {} corrupt", shown, bad);
}
