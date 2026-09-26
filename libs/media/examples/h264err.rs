//! Decode an MP4's video and report decoder errors: `h264err <file>`.
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&a[1]).unwrap();
    let mut dm = media::demux::open(Box::new(data)).unwrap();
    let info = dm.info().clone();
    let (vi, _) = media::pipeline::choose_tracks(&info.tracks);
    let vi = vi.unwrap();
    let media::demux::Codec::H264(avcc) = &info.tracks[vi].codec else { panic!("not h264") };
    let mut d = media::h264::Decoder::new();
    d.configure_avcc(avcc).unwrap();
    let (mut n, mut out, mut last) = (0, 0, 0);
    while let Some(Ok(p)) = dm.next_packet() {
        if p.track != vi { continue; }
        let _ = d.decode(&p.data);
        n += 1;
        while d.next_frame().is_some() { out += 1; }
        if d.errors != last {
            println!("packet {} (key {}): {} errors, last {:?}", n, p.key, d.errors - last, d.last_error);
            last = d.errors;
        }
    }
    d.flush();
    while d.next_frame().is_some() { out += 1; }
    println!("{} packets, {} frames out, {} errors", n, out, d.errors);
}
