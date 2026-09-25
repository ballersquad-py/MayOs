//! Time decoding an image: `bench <file>`.
fn main() {
    let path = std::env::args().nth(1).expect("usage: bench <file>");
    let data = std::fs::read(&path).unwrap();
    let t = std::time::Instant::now();
    let img = image::decode(&data).unwrap();
    let d = t.elapsed();
    let t2 = std::time::Instant::now();
    let _small = img.cover(320, 320, 0);
    println!("{}x{} decoded in {:?}, cover(320) in {:?}", img.width, img.height, d, t2.elapsed());
}
