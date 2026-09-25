fn main() {
    for f in std::env::args().skip(1) {
        let mut src: Box<dyn media::demux::Source> = Box::new(std::fs::read(&f).unwrap());
        let t = media::tags::read(&mut *src);
        println!("{}: title={:?} artist={:?} album={:?} cover={:?}", f, t.title, t.artist, t.album, t.cover.as_ref().map(|c| (c.len(), image::decode(c).map(|i| (i.width, i.height)).ok())));
    }
}
