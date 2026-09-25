//! Demux + decode a whole media file: writes video frames (raw YUV, in
//! display order) and audio (s16le) for comparison with ffmpeg.
use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&args[1]).unwrap();
    let mut vout = std::fs::File::create(&args[2]).unwrap();
    let mut aout = std::fs::File::create(&args[3]).unwrap();
    let t = std::time::Instant::now();
    let mut dm = media::demux::open(Box::new(data)).unwrap_or_else(|e| panic!("open: {:?}", e));
    let info = dm.info().clone();
    eprintln!("{} {:.2}s", info.format, info.duration_us as f64 / 1e6);
    for tr in &info.tracks {
        eprintln!("  {:?} {} {}x{} {}Hz {}ch", tr.kind, tr.codec.name(), tr.width, tr.height, tr.sample_rate, tr.channels);
    }
    let (vi, ai) = media::pipeline::choose_tracks(&info.tracks);
    let mut vd = vi.map(|i| media::pipeline::VideoDecoder::new(&info.tracks[i]).unwrap());
    let mut ad = ai.map(|i| media::pipeline::AudioDecoder::new(&info.tracks[i]).unwrap());
    let (mut vframes, mut apackets, mut asamples) = (0, 0, 0usize);
    let mut last_pts = i64::MIN;
    let mut write_frame = |f: media::pipeline::VideoFrame, out: &mut std::fs::File| {
        if let media::pipeline::FrameData::Yuv(fr) = &f.data {
            let b = &fr.buf;
            for y in 0..fr.height {
                let o = (fr.crop_y + y) * b.width + fr.crop_x;
                out.write_all(&b.y[o..o + fr.width]).unwrap();
            }
            for plane in [&b.cb, &b.cr] {
                for y in 0..fr.height / 2 {
                    let o = (fr.crop_y / 2 + y) * (b.width / 2) + fr.crop_x / 2;
                    out.write_all(&plane[o..o + fr.width / 2]).unwrap();
                }
            }
        }
        if f.pts < last_pts {
            eprintln!("pts went backwards: {} after {}", f.pts, last_pts);
        }
        last_pts = f.pts;
    };
    while let Some(p) = dm.next_packet() {
        let p = p.unwrap();
        if Some(p.track) == vi {
            let d = vd.as_mut().unwrap();
            d.decode(&p);
            while let Some(f) = d.next_frame() {
                write_frame(f, &mut vout);
                vframes += 1;
            }
        } else if Some(p.track) == ai {
            let s = ad.as_mut().unwrap().decode(&p);
            apackets += 1;
            asamples += s.len();
            for v in s {
                aout.write_all(&v.to_le_bytes()).unwrap();
            }
        }
    }
    if let Some(d) = vd.as_mut() {
        d.flush();
        while let Some(f) = d.next_frame() {
            write_frame(f, &mut vout);
            vframes += 1;
        }
    }
    eprintln!("{} video frames, {} audio packets ({} samples, {} ch) in {:?}", vframes, apackets, asamples, ad.map(|a| a.channels).unwrap_or(0), t.elapsed());
}
