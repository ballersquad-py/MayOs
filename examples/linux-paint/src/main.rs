//! Paint on a Linux framebuffer: drag with the left mouse button to draw,
//! right button to erase, keys 1-6 pick a colour, C clears, Esc quits.
//! The background animates to show smooth updates.

use std::fs::OpenOptions;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

const FBIOGET_VSCREENINFO: u64 = 0x4600;
const FBIOGET_FSCREENINFO: u64 = 0x4602;
const FBIO_WAITFORVSYNC: u64 = 0x4004_4620;

fn main() {
    let fb = OpenOptions::new().read(true).write(true).open("/dev/fb0").expect("open /dev/fb0");
    let mut var = [0u32; 40];
    let mut fix = [0u8; 80];
    unsafe {
        assert_eq!(libc::ioctl(fb.as_raw_fd(), FBIOGET_VSCREENINFO as _, var.as_mut_ptr()), 0);
        assert_eq!(libc::ioctl(fb.as_raw_fd(), FBIOGET_FSCREENINFO as _, fix.as_mut_ptr()), 0);
    }
    let (w, h, bpp) = (var[0] as usize, var[1] as usize, var[6]);
    let stride = u32::from_le_bytes(fix[48..52].try_into().unwrap()) as usize / 4;
    println!("framebuffer {}x{} {} bpp, stride {}", w, h, bpp, stride);
    let len = stride * h * 4;
    let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fb.as_raw_fd(), 0) };
    assert!(ptr != libc::MAP_FAILED, "mmap failed");
    let px = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u32, stride * h) };

    let open = |p: &str| OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(p).expect(p);
    let mut kbd = open("/dev/input/event0");
    let mut mouse = open("/dev/input/event1");
    let mut name = [0u8; 64];
    unsafe { libc::ioctl(mouse.as_raw_fd(), 0x8040_4506u64 as _, name.as_mut_ptr()) };
    println!("pointer: {}", String::from_utf8_lossy(&name).trim_end_matches('\0'));

    let colours = [0xffffff, 0xe04040, 0x40c040, 0x4080ff, 0xffd040, 0x000000];
    let mut colour = colours[0];
    let mut canvas = vec![0u32; w * h]; // painted strokes, 0 = empty
    let (mut mx, mut my, mut buttons) = (w as i32 / 2, h as i32 / 2, 0u8);
    let mut last: Option<(i32, i32)> = None;
    let mut t = 0u32;
    let mut ev = [0u8; 24 * 64];
    'main: loop {
        // Keyboard
        while let Ok(n) = kbd.read(&mut ev) {
            if n == 0 { break; }
            for e in ev[..n].chunks_exact(24) {
                let (ty, code, val) = (u16::from_le_bytes([e[16], e[17]]), u16::from_le_bytes([e[18], e[19]]), i32::from_le_bytes(e[20..24].try_into().unwrap()));
                if ty != 1 || val != 1 { continue; }
                match code {
                    1 => break 'main,                       // Esc
                    2..=7 => colour = colours[code as usize - 2], // 1-6
                    46 => canvas.fill(0),                    // C
                    _ => {}
                }
            }
        }
        // Mouse
        while let Ok(n) = mouse.read(&mut ev) {
            if n == 0 { break; }
            for e in ev[..n].chunks_exact(24) {
                let (ty, code, val) = (u16::from_le_bytes([e[16], e[17]]), u16::from_le_bytes([e[18], e[19]]), i32::from_le_bytes(e[20..24].try_into().unwrap()));
                match (ty, code) {
                    (3, 0) => mx = val,
                    (3, 1) => my = val,
                    (1, 0x110) => buttons = if val != 0 { buttons | 1 } else { buttons & !1 },
                    (1, 0x111) => buttons = if val != 0 { buttons | 2 } else { buttons & !2 },
                    (0, 0) => {
                        if buttons != 0 {
                            let c = if buttons & 2 != 0 { 0 } else { colour | 0x0100_0000 };
                            let (x0, y0) = last.unwrap_or((mx, my));
                            let steps = (mx - x0).abs().max((my - y0).abs()).max(1);
                            for s in 0..=steps {
                                let x = x0 + (mx - x0) * s / steps;
                                let y = y0 + (my - y0) * s / steps;
                                for dy in -4..=4 {
                                    for dx in -4..=4 {
                                        if dx * dx + dy * dy > 16 { continue; }
                                        let (px_, py_) = (x + dx, y + dy);
                                        if px_ >= 0 && py_ >= 0 && (px_ as usize) < w && (py_ as usize) < h {
                                            canvas[py_ as usize * w + px_ as usize] = c;
                                        }
                                    }
                                }
                            }
                            last = Some((mx, my));
                        } else {
                            last = None;
                        }
                    }
                    _ => {}
                }
            }
        }
        // Draw: animated gradient, strokes on top, a palette bar.
        for y in 0..h {
            let row = &mut px[y * stride..y * stride + w];
            let src = &canvas[y * w..y * w + w];
            for x in 0..w {
                row[x] = if src[x] != 0 {
                    src[x] & 0xffffff
                } else {
                    let r = ((x as u32 + t) & 255) / 3;
                    let g = ((y as u32 + t / 2) & 255) / 3;
                    let b = 60 + (((x + y) as u32 / 4 + t) & 63);
                    (r << 16) | (g << 8) | b
                };
            }
        }
        for (i, &c) in colours.iter().enumerate() {
            for y in 8..32 {
                for x in 8 + i * 32..8 + i * 32 + 24 {
                    px[y * stride + x] = if c == colour && (y < 10 || y > 29) { 0xffffff } else { c };
                }
            }
        }
        // Pointer cross
        for d in -6i32..=6 {
            for (x, y) in [(mx + d, my), (mx, my + d)] {
                if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
                    px[y as usize * stride + x as usize] ^= 0xffffff;
                }
            }
        }
        t = t.wrapping_add(2);
        unsafe { libc::ioctl(fb.as_raw_fd(), FBIO_WAITFORVSYNC as _, &0u32) };
    }
    println!("bye");
}
