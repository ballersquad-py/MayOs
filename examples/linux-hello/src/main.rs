use std::io::{Read, Write};

fn main() {
    println!("Hello from a Linux program running on MayOS!");
    let args: Vec<String> = std::env::args().collect();
    println!("args: {:?}", args);
    println!("HOME = {:?}", std::env::var("HOME"));
    println!("time since 1970: {:?}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()));
    let t0 = std::time::Instant::now();
    // Random numbers (getrandom) through a HashMap's hasher.
    let mut m = std::collections::HashMap::new();
    for i in 0..1000 { m.insert(i, i * i); }
    println!("hashmap: {} entries, 999^2 = {}", m.len(), m[&999]);
    // Files.
    std::fs::write("/home/linux-test.txt", "written by a Linux program\n").unwrap();
    let back = std::fs::read_to_string("/home/linux-test.txt").unwrap();
    print!("file says: {}", back);
    let n = std::fs::read_dir("/").unwrap().count();
    println!("/ has {} entries", n);
    // Big allocation (mmap).
    let v = vec![7u8; 32 * 1024 * 1024];
    println!("allocated {} MB, sum of a slice = {}", v.len() >> 20, v[..1000].iter().map(|&x| x as u32).sum::<u32>());
    // Threads.
    let handles: Vec<_> = (0..4).map(|i| std::thread::spawn(move || (0..1_000_000u64).map(|x| x * i).sum::<u64>())).collect();
    let sums: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    println!("threads: {:?}", sums);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || { for i in 0..3 { tx.send(i).unwrap(); std::thread::sleep(std::time::Duration::from_millis(50)); } });
    println!("channel: {:?}", rx.iter().collect::<Vec<_>>());
    // Network: DNS + TCP.
    match std::net::TcpStream::connect("example.com:80") {
        Ok(mut s) => {
            s.write_all(b"GET / HTTP/1.0\r\nHost: example.com\r\n\r\n").unwrap();
            let mut r = String::new();
            let _ = s.read_to_string(&mut r);
            println!("http: {} ({} bytes)", r.lines().next().unwrap_or(""), r.len());
        }
        Err(e) => println!("network error: {}", e),
    }
    println!("float math: sqrt(2) = {:.6}, sin(1) = {:.6}", 2f64.sqrt(), 1f64.sin());
    println!("done in {:?}", t0.elapsed());
}
