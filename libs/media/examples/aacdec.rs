//! Decode an ADTS AAC file to raw s16le PCM.
use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&args[1]).unwrap();
    let mut out = std::fs::File::create(&args[2]).unwrap();
    let mut p = 0;
    let mut dec: Option<media::aac::AacDecoder> = None;
    let mut pcm = Vec::new();
    let t = std::time::Instant::now();
    let mut frames = 0;
    let mut errors = 0;
    while p < data.len() {
        let Some((hdr, len, sfi, ch)) = media::aac::AacDecoder::parse_adts(&data[p..]) else { break };
        if dec.is_none() {
            dec = Some(media::aac::AacDecoder::new(sfi, ch).unwrap());
        }
        if let Err(e) = dec.as_mut().unwrap().decode_frame(&data[p + hdr..p + len], &mut pcm) {
            errors += 1;
            eprintln!("frame {}: {:?}", frames, e);
        }
        frames += 1;
        p += len;
    }
    for s in &pcm {
        out.write_all(&s.to_le_bytes()).unwrap();
    }
    eprintln!("{} frames in {:?}, {} errors, {} ch", frames, t.elapsed(), errors, dec.map(|d| d.channels).unwrap_or(0));
}
