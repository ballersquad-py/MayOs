//! Decode images written by Pillow and compare with Pillow's own decoding.

use std::path::PathBuf;
use std::process::Command;

fn python_ok() -> bool {
    Command::new("python3").args(["-c", "import PIL"]).output().map(|o| o.status.success()).unwrap_or(false)
}

/// Run a Python snippet that writes files into `dir`.
fn py(dir: &PathBuf, code: &str) {
    let out = Command::new("python3").arg("-c").arg(code).current_dir(dir).output().unwrap();
    assert!(out.status.success(), "python failed: {}", String::from_utf8_lossy(&out.stderr));
}

fn workdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mayos-image-{}-{}", name, std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Reference RGBA pixels from Pillow, as raw bytes.
fn reference(dir: &PathBuf, file: &str) -> (u32, u32, Vec<u8>) {
    let code = format!(
        "from PIL import Image; im=Image.open('{file}').convert('RGBA'); open('{file}.rgba','wb').write(im.tobytes()); print(im.size[0], im.size[1])"
    );
    let out = Command::new("python3").arg("-c").arg(code).current_dir(dir).output().unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    let mut it = s.split_whitespace().map(|v| v.parse::<u32>().unwrap());
    let (w, h) = (it.next().unwrap(), it.next().unwrap());
    (w, h, std::fs::read(dir.join(format!("{file}.rgba"))).unwrap())
}

/// Mean absolute difference per channel between our decode and Pillow's.
fn compare(dir: &PathBuf, file: &str) -> f64 {
    let data = std::fs::read(dir.join(file)).unwrap();
    let img = image::decode(&data).unwrap_or_else(|e| panic!("{file}: {e}"));
    let (w, h, rgba) = reference(dir, file);
    assert_eq!((img.width, img.height), (w, h), "{file}: size");
    let mut total = 0u64;
    for (i, px) in img.pixels.iter().enumerate() {
        let ours = [(px >> 16) as u8, (px >> 8) as u8, *px as u8, (px >> 24) as u8];
        for c in 0..4 {
            total += (ours[c] as i32 - rgba[i * 4 + c] as i32).unsigned_abs() as u64;
        }
    }
    total as f64 / (w as f64 * h as f64 * 4.0)
}

const SCENE: &str = "
from PIL import Image, ImageDraw
im = Image.new('RGB', (173, 117))
d = ImageDraw.Draw(im)
for y in range(117):
    for x in range(173):
        im.putpixel((x, y), ((x * 3) % 256, (y * 5) % 256, (x * y) % 256))
d.ellipse((20, 20, 120, 100), fill=(250, 200, 40), outline=(0, 0, 0))
d.text((10, 5), 'MayOS', fill=(255, 255, 255))
";

#[test]
fn png_variants_match_pillow() {
    if !python_ok() {
        eprintln!("skipping: Pillow not available");
        return;
    }
    let dir = workdir("png");
    py(&dir, &format!("{SCENE}
im.save('rgb.png')
im.convert('RGBA').save('rgba.png')
im.convert('L').save('gray.png')
im.convert('LA').save('graya.png')
im.convert('P', palette=Image.ADAPTIVE, colors=200).save('pal.png', transparency=5)
im.convert('1').save('bw.png')
im.convert('P', palette=Image.ADAPTIVE, colors=16).save('pal4.png', bits=4)
im.convert('I;16').save('gray16.png')
a = im.convert('RGBA'); a.putalpha(128); a.save('half.png', optimize=True)
"));
    for f in ["rgb.png", "rgba.png", "gray.png", "graya.png", "pal.png", "bw.png", "pal4.png", "half.png"] {
        let d = compare(&dir, f);
        assert!(d < 0.01, "{f}: mean difference {d}");
    }
    // 16-bit grayscale: Pillow's RGBA conversion clips rather than scales,
    // so just check it decodes to the right size.
    let data = std::fs::read(dir.join("gray16.png")).unwrap();
    let img = image::decode(&data).unwrap();
    assert_eq!((img.width, img.height), (173, 117));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn interlaced_png_matches() {
    if !python_ok() {
        return;
    }
    let dir = workdir("adam7");
    // Pillow cannot write Adam7, so encode it here in Python.
    py(&dir, &format!("{SCENE}
import zlib, struct
w, h = im.size
px = im.load()
passes = [(0,0,8,8),(4,0,8,8),(0,4,4,8),(2,0,4,4),(0,2,2,4),(1,0,2,2),(0,1,1,2)]
raw = bytearray()
for x0, y0, dx, dy in passes:
    for y in range(y0, h, dy):
        xs = list(range(x0, w, dx))
        if not xs: continue
        raw.append(0)
        for x in xs: raw.extend(px[x, y])
def chunk(t, d): return struct.pack('>I', len(d)) + t + d + struct.pack('>I', zlib.crc32(t + d) & 0xffffffff)
data = b'\\x89PNG\\r\\n\\x1a\\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', w, h, 8, 2, 0, 0, 1)) + chunk(b'IDAT', zlib.compress(bytes(raw), 9)) + chunk(b'IEND', b'')
open('adam7.png', 'wb').write(data)
"));
    let d = compare(&dir, "adam7.png");
    assert!(d < 0.01, "adam7: {d}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn jpeg_variants_match_pillow() {
    if !python_ok() {
        return;
    }
    let dir = workdir("jpeg");
    py(&dir, &format!("{SCENE}
im.save('base420.jpg', quality=90)
im.save('base444.jpg', quality=90, subsampling=0)
im.save('base422.jpg', quality=90, subsampling=1)
im.save('prog.jpg', quality=85, progressive=True)
im.save('prog444.jpg', quality=85, progressive=True, subsampling=0)
im.convert('L').save('gray.jpg', quality=90)
im.convert('L').save('grayprog.jpg', quality=90, progressive=True)
im.save('low.jpg', quality=20)
im.convert('CMYK').save('cmyk.jpg', quality=92)
big = im.resize((1031, 777)); big.save('big.jpg', quality=80, progressive=True)
"));
    for f in ["base420.jpg", "base444.jpg", "base422.jpg", "prog.jpg", "prog444.jpg", "gray.jpg", "grayprog.jpg", "low.jpg", "big.jpg", "cmyk.jpg"] {
        let d = compare(&dir, f);
        // Different IDCT and chroma upsampling: allow small differences.
        eprintln!("{f}: {d:.3}");
        assert!(d < 1.5, "{f}: mean difference {d}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn jpeg_without_huffman_tables_uses_defaults() {
    if !python_ok() {
        return;
    }
    let dir = workdir("mjpeg");
    // Pillow writes the standard tables when optimize=False; strip the DHT
    // segments like many Motion-JPEG encoders do.
    py(&dir, &format!("{SCENE}
im.save('full.jpg', quality=80, optimize=False)
d = open('full.jpg','rb').read()
out = bytearray(d[:2]); i = 2
while i < len(d):
    assert d[i] == 0xff
    m = d[i+1]
    if m == 0xda:
        out += d[i:]; break
    n = (d[i+2] << 8) | d[i+3]
    if m != 0xc4: out += d[i:i+2+n]
    i += 2 + n
open('nodht.jpg','wb').write(bytes(out))
"));
    let a = image::decode(&std::fs::read(dir.join("full.jpg")).unwrap()).unwrap();
    let b = image::decode(&std::fs::read(dir.join("nodht.jpg")).unwrap()).unwrap();
    assert!(a.pixels == b.pixels, "default tables must reproduce the image");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn bmp_variants_match_pillow() {
    if !python_ok() {
        return;
    }
    let dir = workdir("bmp");
    py(&dir, &format!("{SCENE}
im.save('rgb.bmp')
im.convert('P', palette=Image.ADAPTIVE, colors=256).save('pal.bmp')
im.convert('1').save('bw.bmp')
im.convert('RGBA').save('rgba.bmp')
"));
    for f in ["rgb.bmp", "pal.bmp", "bw.bmp", "rgba.bmp"] {
        let d = compare(&dir, f);
        assert!(d < 0.01, "{f}: mean difference {d}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn inflate_matches_zlib() {
    if !python_ok() {
        return;
    }
    let dir = workdir("zlib");
    py(&dir, "
import zlib, random
random.seed(7)
parts = [bytes(random.randrange(256) for _ in range(5000)), b'abc' * 20000, bytes(range(256)) * 50, b'']
data = b''.join(parts)
open('plain', 'wb').write(data)
for level in (0, 1, 6, 9):
    open(f'z{level}', 'wb').write(zlib.compress(data, level))
");
    let plain = std::fs::read(dir.join("plain")).unwrap();
    for level in [0, 1, 6, 9] {
        let z = std::fs::read(dir.join(format!("z{level}"))).unwrap();
        assert_eq!(image::inflate::zlib_decompress(&z, 1 << 24).unwrap(), plain, "level {level}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn garbage_is_rejected_not_panicking() {
    for len in [0usize, 1, 10, 100, 1000] {
        let junk: Vec<u8> = (0..len).map(|i| (i * 37 % 251) as u8).collect();
        let _ = image::decode(&junk);
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        png.extend_from_slice(&junk);
        let _ = image::decode(&png);
        let mut jpg = vec![0xff, 0xd8, 0xff];
        jpg.extend_from_slice(&junk);
        let _ = image::decode(&jpg);
    }
}

#[test]
fn resizing_keeps_colours() {
    let mut img = image::Image::new(64, 32);
    for p in img.pixels.iter_mut() {
        *p = 0xff33_6699;
    }
    let small = img.resized(10, 5);
    assert!(small.pixels.iter().all(|&p| p == 0xff33_6699));
    let big = img.cover(200, 200, 0xff000000);
    assert_eq!((big.width, big.height), (200, 200));
    assert!(big.pixels.iter().all(|&p| p == 0xff33_6699));
    let fit = img.fit(100, 100);
    assert_eq!((fit.width, fit.height), (100, 50));
}

#[test]
fn avi_mjpeg_parses_and_frames_decode() {
    if !python_ok() {
        return;
    }
    let dir = workdir("avi");
    let tool = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/mkavi.py");
    let out = Command::new("python3").arg(&tool).arg(dir.join("clip.avi")).args(["2", "160", "90", "10"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let data = std::fs::read(dir.join("clip.avi")).unwrap();
    let avi = image::avi::parse(&data).unwrap();
    assert_eq!((avi.width, avi.height, avi.frames.len()), (160, 90, 20));
    assert_eq!(avi.frame_us, 100_000);
    let (ch, rate, bits, pcm) = avi.audio.as_ref().unwrap();
    assert_eq!((*ch, *rate, *bits), (2, 22050, 16));
    assert!(pcm.len() > 22050 * 4 * 3 / 2);
    for f in &avi.frames {
        let img = image::decode(f).unwrap();
        assert_eq!((img.width, img.height), (160, 90));
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
