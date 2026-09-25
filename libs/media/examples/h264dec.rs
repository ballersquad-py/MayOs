//! Decode an Annex B H.264 stream to raw planar YUV 4:2:0 (for testing
//! against a reference decoder).
//!
//! usage: h264dec in.264 out.yuv

use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&args[1]).unwrap();
    let mut out = std::fs::File::create(&args[2]).unwrap();
    let mut dec = media::h264::Decoder::new();
    let t = std::time::Instant::now();
    // Feed the stream in pieces to exercise picture boundary detection.
    let mut frames = 0;
    let write = |f: media::h264::Frame, out: &mut std::fs::File| {
        let b = &f.buf;
        for y in 0..f.height {
            let o = (f.crop_y + y) * b.width + f.crop_x;
            out.write_all(&b.y[o..o + f.width]).unwrap();
        }
        for plane in [&b.cb, &b.cr] {
            for y in 0..f.height / 2 {
                let o = (f.crop_y / 2 + y) * (b.width / 2) + f.crop_x / 2;
                out.write_all(&plane[o..o + f.width / 2]).unwrap();
            }
        }
    };
    for nal in media::h264::AnnexB::new(&data) {
        dec.decode_nal(nal);
        while let Some(f) = dec.next_frame() {
            write(f, &mut out);
            frames += 1;
        }
    }
    dec.flush();
    while let Some(f) = dec.next_frame() {
        write(f, &mut out);
        frames += 1;
    }
    eprintln!("{} frames in {:?}, {} slice errors {:?}", frames, t.elapsed(), dec.errors, dec.last_error);
}
