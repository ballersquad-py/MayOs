//! Thumbnail service for the file explorer.
//!
//! Pictures are decoded and shrunk, videos contribute a frame from early in
//! the clip, and music files show their embedded cover art. The work runs
//! on a background thread; `get` returns what is ready and queues the rest.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use gfx::Surface;
use media::demux;
use media::pipeline::VideoDecoder;

use crate::fs;
use crate::sync::Spin;

/// Largest thumbnail edge; tiles draw them scaled to fit.
pub const MAX_W: u32 = 128;
pub const MAX_H: u32 = 96;
/// Edge of the small version shown in the list view.
pub const SMALL: u32 = 22;
/// Don't read enormous pictures just for a preview.
const MAX_IMAGE_BYTES: u64 = 24 * 1024 * 1024;
const CACHE_LIMIT: usize = 300;

pub struct Thumb {
    /// At most MAX_W x MAX_H.
    pub big: Surface,
    /// At most SMALL x SMALL.
    pub small: Surface,
}

enum Entry {
    Pending,
    Ready(Arc<Thumb>, u64),
    Failed,
}

struct State {
    cache: BTreeMap<String, Entry>,
    queue: VecDeque<String>,
    stamp: u64,
}

static STATE: Spin<State> = Spin::new(State { cache: BTreeMap::new(), queue: VecDeque::new(), stamp: 0 });
static WORKER: AtomicBool = AtomicBool::new(false);
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Changes whenever a new thumbnail becomes ready.
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

/// Whether this file type can have a thumbnail.
pub fn supported(name: &str) -> bool {
    super::imageview::is_image_name(name) || super::player::is_media_name(name)
}

/// Cache key: the path plus the size, so edited files get a fresh preview.
fn key(path: &str, size: u64) -> String {
    alloc::format!("{}\u{0}{}", path, size)
}

/// The thumbnail for `path` if it is ready; otherwise queue it (most
/// recent requests first, so what is on screen wins).
pub fn get(path: &str, size: u64) -> Option<Arc<Thumb>> {
    let k = key(path, size);
    let mut s = STATE.lock();
    s.stamp += 1;
    let stamp = s.stamp;
    match s.cache.get_mut(&k) {
        Some(Entry::Ready(img, used)) => {
            *used = stamp;
            return Some(img.clone());
        }
        Some(Entry::Pending) => {
            // Move it to the front of the queue.
            if let Some(i) = s.queue.iter().position(|q| *q == k) {
                let q = s.queue.remove(i).unwrap();
                s.queue.push_front(q);
            }
            return None;
        }
        Some(Entry::Failed) => return None,
        None => {}
    }
    s.cache.insert(k.clone(), Entry::Pending);
    s.queue.push_front(k);
    drop(s);
    if !WORKER.swap(true, Ordering::AcqRel) {
        crate::proc::sched::spawn_kernel("thumbnails", worker, 0);
    }
    None
}

extern "C" fn worker(_: usize) {
    loop {
        let job = STATE.lock().queue.pop_front();
        let Some(k) = job else {
            crate::proc::sched::sleep_ms(30);
            continue;
        };
        let path = k.split('\u{0}').next().unwrap_or("");
        let result = make(path);
        let mut s = STATE.lock();
        s.stamp += 1;
        let stamp = s.stamp;
        s.cache.insert(k, match result {
            Some(img) => Entry::Ready(Arc::new(thumb_of(&img)), stamp),
            None => Entry::Failed,
        });
        // Forget the least recently used previews when the cache is full.
        while s.cache.len() > CACHE_LIMIT {
            let oldest = s
                .cache
                .iter()
                .filter_map(|(k, e)| if let Entry::Ready(_, used) = e { Some((*used, k.clone())) } else { None })
                .min();
            match oldest {
                Some((_, k)) => {
                    s.cache.remove(&k);
                }
                None => break,
            }
        }
        drop(s);
        GENERATION.fetch_add(1, Ordering::Relaxed);
    }
}

/// Size that fits `w`x`h` inside the thumbnail box, keeping the aspect.
fn fit(w: u32, h: u32, mw: u32, mh: u32) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (1, 1);
    }
    let s = (mw as f32 / w as f32).min(mh as f32 / h as f32).min(1.0);
    (((w as f32 * s) as u32).max(1), ((h as f32 * s) as u32).max(1))
}

fn surface(img: &image::Image) -> Surface {
    let mut s = Surface::new(img.width as i32, img.height as i32, 0);
    for (d, p) in s.data.iter_mut().zip(img.pixels.iter()) {
        // Show transparent pixels over white, like the image viewer.
        *d = image::over(*p, 0xffff_ffff) | 0xff00_0000;
    }
    s
}

fn thumb_of(img: &image::Image) -> Thumb {
    let (sw, sh) = fit(img.width, img.height, SMALL, SMALL);
    Thumb { big: surface(img), small: surface(&img.resized(sw, sh)) }
}

/// Shrink to the big thumbnail size.
fn from_image(img: &image::Image) -> image::Image {
    let (w, h) = fit(img.width, img.height, MAX_W, MAX_H);
    img.resized(w, h)
}

fn make(path: &str) -> Option<image::Image> {
    let name = fs::file_name(path);
    if super::imageview::is_image_name(name) {
        let f = fs::open(path).ok()?;
        if f.size > MAX_IMAGE_BYTES {
            return None;
        }
        let data = fs::read_file(path).ok()?;
        return image::decode(&data).ok().map(|img| from_image(&img));
    }
    if super::player::is_media_name(name) {
        return media_thumb(path);
    }
    None
}

fn media_thumb(path: &str) -> Option<image::Image> {
    let file = fs::open(path).ok()?;
    let mut src: Box<dyn demux::Source + Send> = Box::new(file);
    let tags = media::tags::read(&mut *src);
    if let Some(cover) = &tags.cover
        && let Ok(img) = image::decode(cover)
    {
        return Some(from_image(&img));
    }
    let mut dm = demux::open(src).ok()?;
    let info = dm.info().clone();
    let (vi, _) = media::pipeline::choose_tracks(&info.tracks);
    let vi = vi?;
    let mut dec = VideoDecoder::new(&info.tracks[vi]).ok()?;
    // Skip the (often black) opening: take a frame a little way in.
    let target = if info.duration_us > 4_000_000 { (info.duration_us / 10).min(10_000_000) } else { 0 };
    if target > 0 && dm.seek(target).is_err() {
        dm.seek(0).ok()?;
    }
    let mut frame = None;
    for _ in 0..400 {
        match dm.next_packet() {
            Some(Ok(p)) if p.track == vi => dec.decode(&p),
            Some(Ok(_)) => continue,
            _ => {
                dec.flush();
                frame = dec.next_frame();
                break;
            }
        }
        // First frame at or after the seek target (keyframes may be earlier).
        while let Some(f) = dec.next_frame() {
            if f.pts + 100_000 >= target || frame.is_none() {
                frame = Some(f);
            }
            if frame.as_ref().map(|f| f.pts + 100_000 >= target).unwrap_or(false) {
                break;
            }
        }
        if frame.as_ref().map(|f| f.pts + 100_000 >= target).unwrap_or(false) {
            break;
        }
    }
    let f = frame?;
    // Render at twice the size, then shrink smoothly.
    let (w, h) = fit(f.width as u32, f.height as u32, MAX_W * 2, MAX_H * 2);
    let mut img = image::Image::new(w, h);
    f.render(&mut img.pixels, w as usize, h as usize, w as usize);
    for p in img.pixels.iter_mut() {
        *p |= 0xff00_0000;
    }
    Some(from_image(&img))
}
