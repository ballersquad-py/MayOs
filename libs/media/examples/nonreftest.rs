//! Toggle skipping of non-reference pictures (what the player does when
//! it falls behind) and check that frames come out in time order with
//! the right picture for their timestamp: `nonreftest <file> <ref.yuv>`
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&a[1]).unwrap();
    let reference = std::fs::read(&a[2]).unwrap();
    let mut dm = media::demux::open(Box::new(data)).unwrap();
    let info = dm.info().clone();
    let (vi, _) = media::pipeline::choose_tracks(&info.tracks);
    let vi = vi.unwrap();
    let t = &info.tracks[vi];
    let (w, h) = (t.width as usize, t.height as usize);
    let fs = w * h * 3 / 2;
    let frame_us = if t.frame_us > 0 { t.frame_us as i64 } else { 40_000 };
    let mut dec = media::pipeline::VideoDecoder::new(t).unwrap();
    let (mut last, mut back, mut bad, mut shown, mut n) = (i64::MIN, 0, 0, 0, 0);
    let mut check = |f: media::pipeline::VideoFrame| {
        if f.pts <= last { back += 1; println!("pts went back: {} after {}", f.pts, last); }
        last = f.pts;
        let idx = ((f.pts + frame_us / 2) / frame_us) as usize;
        let mut out = vec![0u32; w * h];
        f.render(&mut out, w, h, w);
        let r = &reference[idx * fs..idx * fs + w * h];
        let mut diff = 0u64;
        for i in (0..w * h).step_by(97) {
            diff += (((out[i] >> 8) & 255) as i64 - r[i] as i64).unsigned_abs();
        }
        let avg = diff as f64 / ((w * h / 97) as f64);
        shown += 1;
        if avg > 20.0 { bad += 1; println!("pts {:.3}s wrong picture (diff {:.1})", f.pts as f64 / 1e6, avg); }
    };
    while let Some(Ok(p)) = dm.next_packet() {
        if p.track != vi { continue; }
        n += 1;
        dec.set_skip_nonref((n / 7) % 2 == 1);
        dec.decode(&p);
        while let Some(f) = dec.next_frame() { check(f); }
    }
    dec.flush();
    while let Some(f) = dec.next_frame() { check(f); }
    println!("{} shown, {} wrong, {} backwards", shown, bad, back);
}
