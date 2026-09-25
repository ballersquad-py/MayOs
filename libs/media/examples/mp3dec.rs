//! Decode an MP3 file to raw s16le PCM.
use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&args[1]).unwrap();
    let mut out = std::fs::File::create(&args[2]).unwrap();
    let mut p = 0;
    if data.len() > 10 && &data[..3] == b"ID3" {
        let sz = ((data[6] as usize & 0x7f) << 21) | ((data[7] as usize & 0x7f) << 14) | ((data[8] as usize & 0x7f) << 7) | (data[9] as usize & 0x7f);
        p = 10 + sz;
    }
    let mut dec = media::mp3::Mp3Decoder::new();
    let mut pcm = Vec::new();
    let t = std::time::Instant::now();
    let (mut frames, mut errors) = (0, 0);
    while p + 4 <= data.len() {
        let Some(h) = media::mp3::Header::parse(&data[p..]) else { p += 1; continue };
        if p + h.frame_len > data.len() {
            break;
        }
        if let Err(e) = dec.decode_frame(&data[p..p + h.frame_len], &mut pcm) {
            errors += 1;
            eprintln!("frame {}: {:?}", frames, e);
        }
        frames += 1;
        p += h.frame_len;
    }
    for s in &pcm {
        out.write_all(&s.to_le_bytes()).unwrap();
    }
    eprintln!("{} frames in {:?}, {} errors, {} ch", frames, t.elapsed(), errors, dec.channels);
}
