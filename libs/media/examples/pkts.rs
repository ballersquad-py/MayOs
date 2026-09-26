//! Print the first packets a demuxer produces: `pkts <file> [n]`.
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&a[1]).unwrap();
    let n: usize = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(12);
    let mut dm = media::demux::open(Box::new(data)).unwrap();
    let mut i = 0;
    let mut count = 0;
    while let Some(p) = dm.next_packet() {
        let p = p.unwrap();
        count += 1;
        if i < n {
            println!("track {} pts {:>9} dts {:>9} key {} size {}", p.track, p.pts, p.dts, p.key, p.data.len());
            i += 1;
        }
    }
    println!("{} packets", count);
}
