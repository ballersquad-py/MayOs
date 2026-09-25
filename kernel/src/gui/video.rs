//! Video player for Motion-JPEG AVI files with PCM audio.
//!
//! A background thread decodes upcoming frames into a small queue; the
//! app shows each frame when its time comes. The clock starts together
//! with the audio, so picture and sound stay in sync.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use gfx::icons::Icon;
use gfx::{rgb, with_alpha, Canvas, Rect, Surface};

use super::app::{App, AppEvent, Ctx};
use super::theme::{self, fonts};
use crate::fs;
use crate::input::Key;
use crate::sync::Spin;
use crate::time::uptime_ms;

const BAR_H: i32 = 54;
const BG: u32 = rgb(0x0c, 0x0d, 0x10);
const QUEUE: usize = 6;

pub fn is_video_name(name: &str) -> bool {
    matches!(fs::extension(name).as_deref(), Some("avi" | "mjpg" | "mjpeg"))
}

struct Shared {
    frames: VecDeque<(usize, image::Image)>,
    next: usize,
    stop: bool,
    error: Option<String>,
}

struct Media {
    data: Vec<u8>,
    /// (offset, length) of each frame in `data`.
    frames: Vec<(usize, usize)>,
    frame_us: u64,
    width: u32,
    height: u32,
    audio: Option<Arc<Vec<i16>>>,
}

type Loaded = Arc<Spin<Option<Result<Arc<Media>, String>>>>;

struct LoadJob {
    path: String,
    out: Loaded,
}

extern "C" fn load_thread(arg: usize) {
    let job = unsafe { alloc::boxed::Box::from_raw(arg as *mut LoadJob) };
    let result = (|| -> Result<Arc<Media>, String> {
        let data = fs::read_file(&job.path).map_err(|e| e.to_string())?;
        let avi = image::avi::parse(&data).map_err(|_| {
            String::from("not a Motion-JPEG AVI file (convert it with ffmpeg: see Getting Started)")
        })?;
        let base = data.as_ptr() as usize;
        let frames = avi.frames.iter().map(|f| (f.as_ptr() as usize - base, f.len())).collect();
        let audio = avi.audio.as_ref().map(|(ch, rate, bits, pcm)| Arc::new(crate::audio::wav::resample_pcm(*ch, *rate, *bits, pcm)));
        let (frame_us, width, height) = (avi.frame_us.max(1000), avi.width, avi.height);
        Ok(Arc::new(Media { data, frames, frame_us, width, height, audio }))
    })();
    *job.out.lock() = Some(result);
}

struct DecodeJob {
    media: Arc<Media>,
    shared: Arc<Spin<Shared>>,
}

extern "C" fn decode_thread(arg: usize) {
    let job = unsafe { alloc::boxed::Box::from_raw(arg as *mut DecodeJob) };
    loop {
        let next = {
            let s = job.shared.lock();
            if s.stop {
                return;
            }
            if s.frames.len() >= QUEUE || s.next >= job.media.frames.len() {
                None
            } else {
                Some(s.next)
            }
        };
        let Some(i) = next else {
            crate::proc::sched::sleep_ms(4);
            continue;
        };
        let (off, len) = job.media.frames[i];
        let decoded = image::decode(&job.media.data[off..off + len]);
        let mut s = job.shared.lock();
        if s.next != i {
            continue; // a seek happened meanwhile
        }
        s.next = i + 1;
        match decoded {
            Ok(img) => s.frames.push_back((i, img)),
            Err(e) => {
                if s.error.is_none() {
                    s.error = Some(format!("frame {}: {}", i, e));
                }
            }
        }
    }
}

pub struct VideoPlayer {
    path: String,
    loading: Option<Loaded>,
    media: Option<Arc<Media>>,
    shared: Option<Arc<Spin<Shared>>>,
    error: Option<String>,
    playing: bool,
    /// Wall-clock time at which frame 0 would have been shown.
    clock_start: u64,
    position: usize,
    frame: Option<Surface>,
    voice: Option<u64>,
    size: (i32, i32),
    hover_bar: bool,
    ended: bool,
}

impl VideoPlayer {
    pub fn open(path: &str) -> VideoPlayer {
        let out: Loaded = Arc::new(Spin::new(None));
        let job = alloc::boxed::Box::new(LoadJob { path: String::from(path), out: out.clone() });
        crate::proc::sched::spawn_kernel("video-load", load_thread, alloc::boxed::Box::into_raw(job) as usize);
        VideoPlayer {
            path: String::from(path),
            loading: Some(out),
            media: None,
            shared: None,
            error: None,
            playing: false,
            clock_start: 0,
            position: 0,
            frame: None,
            voice: None,
            size: (800, 520),
            hover_bar: false,
            ended: false,
        }
    }

    fn total(&self) -> usize {
        self.media.as_ref().map(|m| m.frames.len()).unwrap_or(0)
    }

    fn frame_us(&self) -> u64 {
        self.media.as_ref().map(|m| m.frame_us).unwrap_or(40_000)
    }

    fn start_audio(&mut self, from_frame: usize) {
        self.stop_audio();
        if let Some(a) = self.media.as_ref().and_then(|m| m.audio.clone()) {
            let start = (from_frame as u64 * self.frame_us() * 48 / 1000) as usize * 2;
            if start < a.len() {
                let slice: Vec<i16> = a[start..].to_vec();
                self.voice = crate::audio::play(Arc::new(slice));
            }
        }
    }

    fn stop_audio(&mut self) {
        if let Some(v) = self.voice.take() {
            crate::audio::stop(v);
        }
    }

    fn play(&mut self) {
        if self.media.is_none() {
            return;
        }
        if self.ended || self.position + 1 >= self.total() {
            self.seek(0);
        }
        self.ended = false;
        self.playing = true;
        self.clock_start = uptime_ms() - self.position as u64 * self.frame_us() / 1000;
        self.start_audio(self.position);
    }

    fn pause(&mut self) {
        self.playing = false;
        self.stop_audio();
    }

    fn seek(&mut self, frame: usize) {
        let frame = frame.min(self.total().saturating_sub(1));
        self.position = frame;
        if let Some(s) = &self.shared {
            let mut s = s.lock();
            s.frames.clear();
            s.next = frame;
        }
        if self.playing {
            self.clock_start = uptime_ms() - frame as u64 * self.frame_us() / 1000;
            self.start_audio(frame);
        }
    }

    fn video_rect(&self) -> Rect {
        Rect::new(0, 0, self.size.0, self.size.1 - BAR_H)
    }

    fn seek_rect(&self) -> Rect {
        Rect::new(64, self.size.1 - BAR_H + 22, self.size.0 - 200, 10)
    }

    fn play_rect(&self) -> Rect {
        Rect::new(14, self.size.1 - BAR_H + 9, 36, 36)
    }

    fn time_text(&self) -> String {
        let fus = self.frame_us();
        let cur = self.position as u64 * fus / 1_000_000;
        let tot = self.total() as u64 * fus / 1_000_000;
        format!("{}:{:02} / {}:{:02}", cur / 60, cur % 60, tot / 60, tot % 60)
    }
}

impl App for VideoPlayer {
    fn title(&self) -> String {
        format!("{} \u{2014} Video Player", fs::file_name(&self.path))
    }
    fn icon(&self) -> Icon {
        Icon::Video
    }
    fn initial_size(&self) -> (i32, i32) {
        (800, 520)
    }

    fn render(&mut self, c: &mut Canvas, (w, h): (i32, i32), _focused: bool) {
        self.size = (w, h);
        let f = fonts();
        let v = self.video_rect();
        c.fill_rect(v, BG);
        if let Some(frame) = &self.frame {
            // Fit, keeping the aspect ratio.
            let (fw, fh) = (frame.w as i64, frame.h as i64);
            let s = (v.w as i64 * 1024 / fw).min(v.h as i64 * 1024 / fh);
            let (dw, dh) = ((fw * s / 1024) as i32, (fh * s / 1024) as i32);
            let r = Rect::new(v.x + (v.w - dw) / 2, v.y + (v.h - dh) / 2, dw, dh);
            if (dw, dh) == (frame.w, frame.h) {
                c.blit(frame, r.x, r.y);
            } else {
                c.blit_scaled(frame, r, 255, 0);
            }
        } else {
            let msg = match (&self.error, &self.media) {
                (Some(e), _) => format!("Cannot play this video: {}", e),
                (None, None) => String::from("Loading\u{2026}"),
                _ => String::new(),
            };
            c.draw_text_centered(&f.ui, v, &msg, with_alpha(0xffffff, 190));
        }
        // Control bar.
        let bar = Rect::new(0, h - BAR_H, w, BAR_H);
        c.fill_rect(bar, rgb(0x1b, 0x1d, 0x23));
        let pr = self.play_rect();
        c.fill_circle(pr.x + 18, pr.y + 18, 18, theme::accent());
        let (cx, cy) = (pr.x + 18, pr.y + 18);
        if self.playing {
            c.fill_rect(Rect::new(cx - 6, cy - 7, 4, 14), rgb(255, 255, 255));
            c.fill_rect(Rect::new(cx + 2, cy - 7, 4, 14), rgb(255, 255, 255));
        } else {
            for i in 0..14 {
                let len = if i < 7 { i + 1 } else { 14 - i };
                c.fill_rect(Rect::new(cx - 4, cy - 7 + i, len * 3 / 2, 1), rgb(255, 255, 255));
            }
        }
        let sr = self.seek_rect();
        c.fill_rounded_rect(sr, 5, rgb(0x3a, 0x3e, 0x48));
        let total = self.total().max(1);
        let pos = (sr.w as i64 * self.position as i64 / total as i64) as i32;
        c.fill_rounded_rect(Rect::new(sr.x, sr.y, pos.max(10), sr.h), 5, theme::accent());
        if self.hover_bar {
            c.fill_circle(sr.x + pos, sr.y + 5, 8, rgb(255, 255, 255));
        }
        c.draw_text(&f.mono, sr.right() + 16, h - BAR_H + 32, &self.time_text(), rgb(0xd0, 0xd4, 0xdc));
    }

    fn event(&mut self, ev: &AppEvent, ctx: &mut Ctx) {
        match ev {
            AppEvent::MouseDown { x, y, button: 0, .. } => {
                if self.play_rect().contains(*x, *y) || (self.video_rect().contains(*x, *y) && self.media.is_some()) {
                    if self.playing { self.pause() } else { self.play() }
                } else if self.seek_rect().inset(-8).contains(*x, *y) {
                    let sr = self.seek_rect();
                    let f = ((*x - sr.x).clamp(0, sr.w) as i64 * self.total() as i64 / sr.w.max(1) as i64) as usize;
                    self.seek(f);
                }
                ctx.redraw();
            }
            AppEvent::MouseMove { x, y, buttons } => {
                let over = self.seek_rect().inset(-8).contains(*x, *y);
                if over && buttons & 1 != 0 {
                    let sr = self.seek_rect();
                    let f = ((*x - sr.x).clamp(0, sr.w) as i64 * self.total() as i64 / sr.w.max(1) as i64) as usize;
                    self.seek(f);
                    ctx.redraw();
                }
                if over != self.hover_bar {
                    self.hover_bar = over;
                    ctx.redraw();
                }
            }
            AppEvent::Key(k) if k.pressed => {
                let step = (5_000_000 / self.frame_us()) as usize;
                match k.key {
                    Key::Char(' ') | Key::Enter => {
                        if self.playing { self.pause() } else { self.play() }
                    }
                    Key::Left => self.seek(self.position.saturating_sub(step)),
                    Key::Right => self.seek(self.position + step),
                    Key::Home => self.seek(0),
                    Key::Escape => ctx.close(),
                    _ => return,
                }
                ctx.redraw();
            }
            AppEvent::Resized { .. } | AppEvent::Focus(_) => ctx.redraw(),
            _ => {}
        }
    }

    fn tick(&mut self, ctx: &mut Ctx) {
        if let Some(l) = &self.loading {
            let done = l.lock().take();
            if let Some(r) = done {
                self.loading = None;
                match r {
                    Ok(m) => {
                        let shared = Arc::new(Spin::new(Shared { frames: VecDeque::new(), next: 0, stop: false, error: None }));
                        let job = alloc::boxed::Box::new(DecodeJob { media: m.clone(), shared: shared.clone() });
                        crate::proc::sched::spawn_kernel("video-decode", decode_thread, alloc::boxed::Box::into_raw(job) as usize);
                        crate::kprintln!("video: {} {}x{} {} frames", self.path, m.width, m.height, m.frames.len());
                        self.media = Some(m);
                        self.shared = Some(shared);
                        self.play();
                    }
                    Err(e) => self.error = Some(e),
                }
                ctx.redraw();
            }
            return;
        }
        let Some(shared) = self.shared.clone() else { return };
        // Which frame should be on screen now?
        let target = if self.playing {
            ((uptime_ms() - self.clock_start) * 1000 / self.frame_us()) as usize
        } else {
            self.position
        };
        let mut show = None;
        {
            let mut s = shared.lock();
            if let Some(e) = s.error.take() {
                crate::kprintln!("video: {}", e);
            }
            while let Some((i, _)) = s.frames.front() {
                if *i <= target {
                    show = s.frames.pop_front();
                } else {
                    break;
                }
            }
        }
        if let Some((i, img)) = show {
            let mut surf = Surface::new(img.width as i32, img.height as i32, BG);
            for (d, p) in surf.data.iter_mut().zip(img.pixels.iter()) {
                *d = *p | 0xff00_0000;
            }
            self.frame = Some(surf);
            self.position = i;
            ctx.redraw();
        }
        if self.playing && target + 1 >= self.total() && self.position + 1 >= self.total() {
            self.playing = false;
            self.ended = true;
            self.stop_audio();
            ctx.redraw();
        }
    }

    fn request_close(&mut self, _ctx: &mut Ctx) -> bool {
        self.stop_audio();
        if let Some(s) = &self.shared {
            s.lock().stop = true;
        }
        true
    }
}
