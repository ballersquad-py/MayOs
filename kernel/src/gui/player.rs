//! Media player: video (H.264, Motion JPEG) and music (MP3, AAC, WAV) in
//! MP4, MOV, MKV, AVI and plain audio files.
//!
//! A decoding thread keeps half a second of audio queued in the mixer and a
//! few video frames ready; the window shows the frame whose time has come
//! according to the audio clock, so picture and sound stay in sync.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use gfx::icons::Icon;
use gfx::{mix, rgb, rgba, with_alpha, Canvas, Color, Rect, Surface};
use media::demux;
use media::pipeline::{AudioDecoder, VideoDecoder, VideoFrame};

use super::app::{App, AppEvent, Ctx};
use super::theme::{self, fonts};
use crate::audio::{self, AudioStream};
use crate::fs;
use crate::input::Key;
use crate::sync::Spin;
use crate::time::uptime_ms;

const VIDEO_EXT: &[&str] = &["mp4", "m4v", "mov", "mkv", "webm", "avi", "3gp", "mjpg", "mjpeg"];
const AUDIO_EXT: &[&str] = &["mp3", "m4a", "aac", "wav", "mka"];

pub fn is_video_name(name: &str) -> bool {
    fs::extension(name).map(|e| VIDEO_EXT.contains(&e.as_str())).unwrap_or(false)
}

pub fn is_audio_name(name: &str) -> bool {
    fs::extension(name).map(|e| AUDIO_EXT.contains(&e.as_str())).unwrap_or(false)
}

pub fn is_media_name(name: &str) -> bool {
    is_video_name(name) || is_audio_name(name)
}

fn fmt_time(us: i64) -> String {
    let s = (us.max(0) / 1_000_000) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

// ---------------------------------------------------------------------
// Decoding engine (runs on its own kernel thread)
// ---------------------------------------------------------------------

struct Summary {
    duration_us: i64,
    has_video: bool,
    has_audio: bool,
    width: u32,
    height: u32,
    description: String,
    title: String,
    artist: String,
    album: String,
    cover: Option<Surface>,
    /// Shown when only part of the file can be played.
    warning: Option<String>,
}

#[derive(Clone, PartialEq)]
enum Status {
    Loading,
    Ready,
    Failed(String),
}

const VIZ_RING: usize = 1 << 15;

struct Shared {
    status: Status,
    summary: Option<Arc<Summary>>,
    frames: VecDeque<VideoFrame>,
    seek: Option<i64>,
    stop: bool,
    late: bool,
    /// Media time at audio frame 0 (changes on seek).
    base_us: i64,
    /// Bumped on every seek.
    epoch: u32,
    /// Demuxer exhausted and decoders flushed.
    eof: bool,
    /// Mono copy of the queued audio (48 kHz) for the visualiser, indexed
    /// by audio frame number since the last seek.
    viz: Vec<i16>,
    viz_written: u64,
}

struct EngineArgs {
    path: String,
    shared: Arc<Spin<Shared>>,
    stream: Arc<AudioStream>,
}

fn cover_surface(bytes: &[u8]) -> Option<Surface> {
    let img = image::decode(bytes).ok()?;
    let fitted = img.cover(320, 320, 0xff20_2020);
    let mut s = Surface::new(320, 320, 0);
    for (d, p) in s.data.iter_mut().zip(fitted.pixels.iter()) {
        *d = *p | 0xff00_0000;
    }
    Some(s)
}

extern "C" fn engine_thread(arg: usize) {
    let args = unsafe { Box::from_raw(arg as *mut EngineArgs) };
    if let Err(e) = engine(&args) {
        args.shared.lock().status = Status::Failed(e);
    }
    // Let the window know the thread has finished.
    let mut s = args.shared.lock();
    s.eof = true;
}

fn engine(args: &EngineArgs) -> Result<(), String> {
    let shared = &args.shared;
    let stream = &args.stream;
    let opened = uptime_ms();
    let file = fs::open(&args.path).map_err(|e| e.to_string())?;
    let mut src: Box<dyn demux::Source + Send> = Box::new(file);
    let tags = media::tags::read(&mut *src);
    let mut dm = demux::open(src).map_err(|e| match e {
        demux::Error::Unsupported(m) => m,
        demux::Error::Invalid(m) => String::from(m),
        demux::Error::Io => String::from("could not read the file"),
    })?;
    let info = dm.info().clone();
    let (vi, ai) = media::pipeline::choose_tracks(&info.tracks);
    let mut warning = None;
    let mut vdec = None;
    if let Some(i) = vi {
        match VideoDecoder::new(&info.tracks[i]) {
            Ok(d) => vdec = Some(d),
            Err(e) => {
                if ai.is_none() {
                    return Err(e);
                }
                warning = Some(e);
            }
        }
    }
    let mut adec = None;
    if let Some(i) = ai {
        match AudioDecoder::new(&info.tracks[i]) {
            Ok(d) => adec = Some(d),
            Err(e) => {
                if vdec.is_none() {
                    return Err(e);
                }
                warning = Some(e);
            }
        }
    }
    let vtrack = vi.filter(|_| vdec.is_some());
    let atrack = ai.filter(|_| adec.is_some());
    let mut description = String::from(info.format);
    if let Some(i) = vi {
        let t = &info.tracks[i];
        description = format!("{} \u{00b7} {} {}\u{00d7}{}", description, t.codec.name(), t.width, t.height);
    }
    if let Some(i) = ai {
        let t = &info.tracks[i];
        description = format!("{} \u{00b7} {} {} Hz", description, t.codec.name(), t.sample_rate);
    }
    let (w, h) = vi.map(|i| (info.tracks[i].width, info.tracks[i].height)).unwrap_or((0, 0));
    let summary = Summary {
        duration_us: info.duration_us,
        has_video: vtrack.is_some(),
        has_audio: atrack.is_some(),
        width: w,
        height: h,
        description,
        title: if tags.title.is_empty() { String::from(fs::file_name(&args.path)) } else { tags.title.clone() },
        artist: tags.artist.clone(),
        album: tags.album.clone(),
        cover: tags.cover.as_deref().and_then(cover_surface),
        warning,
    };
    crate::kprintln!("player: {} ({}), ready in {} ms", args.path, summary.description, uptime_ms() - opened);
    {
        let mut s = shared.lock();
        s.summary = Some(Arc::new(summary));
        s.status = Status::Ready;
    }
    let ai_rate = atrack.map(|i| info.tracks[i].sample_rate).unwrap_or(48000);
    let mut resampler = media::resample::Resampler::new(ai_rate.max(1), 48000);
    let mut skip_until: i64 = 0;
    let mut audio_next: Option<i64> = None;
    let mut eof = false;
    let mut pcm48: Vec<i16> = Vec::new();
    // Compressed video waiting to be decoded. Audio always comes first:
    // video is decoded only while enough sound is queued, and when the CPU
    // can't keep up, video skips ahead to the next keyframe instead of
    // making the sound stutter.
    let mut vq: VecDeque<demux::Packet> = VecDeque::new();
    let mut skipping = false;
    let mut flushed = false;
    loop {
        let (stop, seek, late, frames, base) = {
            let mut s = shared.lock();
            (s.stop, s.seek.take(), s.late, s.frames.len(), s.base_us)
        };
        if stop {
            return Ok(());
        }
        if let Some(t) = seek {
            let t = t.clamp(0, info.duration_us.max(0));
            let _ = dm.seek(t);
            if let Some(v) = vdec.as_mut() {
                v.reset();
            }
            if let Some(i) = atrack {
                adec = AudioDecoder::new(&info.tracks[i]).ok();
            }
            resampler.reset();
            stream.reset(0);
            skip_until = t;
            audio_next = None;
            eof = false;
            flushed = false;
            vq.clear();
            skipping = false;
            let mut s = shared.lock();
            s.frames.clear();
            s.base_us = t;
            s.epoch = s.epoch.wrapping_add(1);
            s.eof = false;
            s.viz_written = 0;
            continue;
        }
        if let Some(v) = vdec.as_mut() {
            v.set_skip_nonref(late || skipping);
        }
        let queued = stream.queued();
        let audio_low = atrack.is_some() && queued < 14_400; // 0.3 s
        let audio_full = atrack.is_none() || queued >= 24_000; // 0.5 s
        let video_wanted = vtrack.is_some() && frames < 8;
        let mut worked = false;

        // 1. Read the next packet: while sound is short, or video needs one.
        if !eof && (!audio_full || (video_wanted && vq.is_empty())) && vq.len() < 3000 {
            worked = true;
            match dm.next_packet() {
                Some(Ok(pkt)) => {
                    if Some(pkt.track) == vtrack {
                        vq.push_back(pkt);
                    } else if Some(pkt.track) == atrack {
                        if let Some(a) = adec.as_mut() {
                            let samples = a.decode(&pkt);
                            let ch = a.channels.max(1);
                            let rate = a.sample_rate.max(1);
                            if resampler.in_rate() != rate {
                                resampler = media::resample::Resampler::new(rate, 48000);
                            }
                            let frames_in = samples.len() / ch;
                            // Drop audio before the seek target (and encoder priming).
                            let mut start = 0usize;
                            if pkt.pts < skip_until {
                                start = (((skip_until - pkt.pts) as i128 * rate as i128 / 1_000_000) as usize).min(frames_in);
                            }
                            let pts0 = pkt.pts + (start as i64 * 1_000_000 / rate as i64);
                            // Fill gaps (audio starting late, or missing packets) with silence.
                            let expected = audio_next.unwrap_or(skip_until);
                            pcm48.clear();
                            if pts0 > expected + 30_000 && start < frames_in {
                                let gap = ((pts0 - expected) as i128 * 48 / 1000).min(48_000 * 5) as usize;
                                pcm48.resize(gap * 2, 0);
                            }
                            resampler.process(&samples[start * ch..], ch, &mut pcm48);
                            if start < frames_in {
                                audio_next = Some(pts0 + ((frames_in - start) as i64 * 1_000_000 / rate as i64));
                            }
                            stream.push(&pcm48);
                            let mut s = shared.lock();
                            if s.viz.len() != VIZ_RING {
                                s.viz = vec![0; VIZ_RING];
                            }
                            let mut w = s.viz_written;
                            for pair in pcm48.chunks_exact(2) {
                                let m = ((pair[0] as i32 + pair[1] as i32) / 2) as i16;
                                s.viz[(w as usize) & (VIZ_RING - 1)] = m;
                                w += 1;
                            }
                            s.viz_written = w;
                        }
                    }
                }
                Some(Err(_)) => {}
                None => eof = true,
            }
        }

        // 2. Decode one video packet, unless the sound needs the CPU first.
        if video_wanted && !vq.is_empty() && (!audio_low || vq.len() >= 3000) {
            worked = true;
            let pkt = vq.pop_front().unwrap();
            // What the listener hears now (the same clock the window uses).
            let clock = base + (stream.played().saturating_sub(audio::latency_frames()) as i64 * 1_000_000 / 48_000);
            let behind = atrack.is_some() && pkt.pts + 250_000 < clock;
            let v = vdec.as_mut().unwrap();
            let decode = if skipping {
                if pkt.key {
                    skipping = false;
                    v.reset();
                    true
                } else {
                    false
                }
            } else if (behind || vq.len() >= 3000) && !pkt.key && vq.iter().any(|q| q.key) {
                // Far behind and a keyframe is waiting: jump to it.
                skipping = true;
                false
            } else {
                true
            };
            if decode {
                v.decode(&pkt);
                let mut s = shared.lock();
                while let Some(f) = v.next_frame() {
                    if f.pts + 20_000 >= skip_until {
                        s.frames.push_back(f);
                    }
                }
            }
        }

        // 3. End of file: flush the decoder once everything is decoded.
        if eof && vq.is_empty() && !flushed {
            flushed = true;
            if let Some(v) = vdec.as_mut() {
                v.flush();
                let mut s = shared.lock();
                while let Some(f) = v.next_frame() {
                    s.frames.push_back(f);
                }
            }
            shared.lock().eof = true;
        }

        if !worked {
            crate::proc::sched::sleep_ms(6);
        }
    }
}

// ---------------------------------------------------------------------
// Window
// ---------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Btn {
    Prev,
    Play,
    Next,
    Mute,
    Fullscreen,
    Replay,
}

pub struct Player {
    path: String,
    playlist: Vec<String>,
    shared: Arc<Spin<Shared>>,
    stream: Arc<AudioStream>,
    summary: Option<Arc<Summary>>,
    current: Option<VideoFrame>,
    epoch: u32,
    paused: bool,
    /// Clock for files without audio: media time at `wall_start`.
    wall_base: i64,
    wall_start: u64,
    /// Smoothed audio clock: (media time, uptime in us) of the last
    /// reading. The sound card's position moves in 21 ms steps and in
    /// bursts; following it directly shows frames for uneven times.
    smooth: core::cell::Cell<Option<(i64, u64, u32)>>,
    position: i64,
    ended: bool,
    size: (i32, i32),
    fullscreen: bool,
    volume: u8,
    muted: bool,
    last_activity: u64,
    pointer: (i32, i32),
    hover: Option<Btn>,
    hover_seek: Option<i32>,
    dragging_seek: bool,
    dragging_volume: bool,
    controls_alpha: u32,
    bars: [u8; 32],
    last_bars: u64,
    /// Music view without the visualiser, cached: (size, song, pixels).
    music_bg: Option<((i32, i32), usize, Surface)>,
    /// Where the visualiser bars were last drawn.
    viz_rect: Rect,
    opened_at: u64,
}

impl Player {
    pub fn open(path: &str) -> Player {
        let stream = audio::open_stream();
        let cfg = crate::settings::get();
        let mut p = Player {
            path: String::from(path),
            playlist: Vec::new(),
            shared: Arc::new(Spin::new(new_shared())),
            stream,
            summary: None,
            current: None,
            epoch: 0,
            paused: false,
            wall_base: 0,
            wall_start: uptime_ms(),
            smooth: core::cell::Cell::new(None),
            position: 0,
            ended: false,
            size: (900, 560),
            fullscreen: false,
            volume: cfg.player_volume,
            muted: false,
            last_activity: uptime_ms(),
            pointer: (0, 0),
            hover: None,
            hover_seek: None,
            dragging_seek: false,
            dragging_volume: false,
            controls_alpha: 255,
            bars: [0; 32],
            last_bars: 0,
            music_bg: None,
            viz_rect: Rect::default(),
            opened_at: uptime_ms(),
        };
        p.apply_volume();
        p.start(path);
        p
    }

    fn start(&mut self, path: &str) {
        // Stop the previous engine, if any.
        self.shared.lock().stop = true;
        self.stream.close();
        self.stream = audio::open_stream();
        self.apply_volume();
        self.path = String::from(path);
        self.shared = Arc::new(Spin::new(new_shared()));
        self.summary = None;
        self.current = None;
        self.epoch = 0;
        self.paused = false;
        self.ended = false;
        self.position = 0;
        self.wall_base = 0;
        self.wall_start = uptime_ms();
        self.opened_at = uptime_ms();
        let dir = fs::parent(path);
        self.playlist = fs::read_dir(&dir)
            .map(|es| es.into_iter().filter(|e| !e.is_dir && is_media_name(&e.name)).map(|e| fs::join(&dir, &e.name)).collect())
            .unwrap_or_default();
        let args = Box::new(EngineArgs { path: String::from(path), shared: self.shared.clone(), stream: self.stream.clone() });
        crate::proc::sched::spawn_kernel("player", engine_thread, Box::into_raw(args) as usize);
    }

    fn apply_volume(&self) {
        let v = if self.muted { 0 } else { self.volume as u32 * 256 / 100 };
        self.stream.set_volume(v);
    }

    fn has_video(&self) -> bool {
        self.summary.as_ref().map(|s| s.has_video).unwrap_or(false)
    }

    fn duration(&self) -> i64 {
        self.summary.as_ref().map(|s| s.duration_us).unwrap_or(0)
    }

    /// Current media time.
    fn clock(&self) -> i64 {
        let s = self.shared.lock();
        let (base, epoch) = (s.base_us, s.epoch);
        drop(s);
        let has_audio = self.summary.as_ref().map(|s| s.has_audio).unwrap_or(false);
        if has_audio {
            let played = self.stream.played().saturating_sub(audio::latency_frames());
            let a = base + (played as i64 * 1_000_000 / 48_000);
            if self.paused {
                self.smooth.set(None);
                return a;
            }
            let now = crate::time::uptime_us();
            let t = match self.smooth.get() {
                // (Not across a seek: the clock restarts there.)
                Some((m, w, e)) if e == epoch => {
                    let p = m + (now - w) as i64;
                    let err = a - p;
                    if err > 150_000 {
                        a // far behind the sound (a stall): jump
                    } else if err < -60_000 {
                        m // sound is not moving (starting, starved): wait for it
                    } else {
                        (p + err / 16).max(m) // follow the sound gently
                    }
                }
                _ => a,
            };
            self.smooth.set(Some((t, now, epoch)));
            t
        } else if self.paused {
            self.wall_base
        } else {
            self.wall_base + (uptime_ms() - self.wall_start) as i64 * 1000
        }
    }

    fn set_paused(&mut self, p: bool) {
        if self.ended && !p {
            self.seek(0);
            self.ended = false;
        }
        if p == self.paused {
            return;
        }
        if p {
            self.wall_base = self.clock();
        } else {
            self.wall_start = uptime_ms();
        }
        self.paused = p;
        self.stream.set_paused(p);
        self.poke();
    }

    fn seek(&mut self, t: i64) {
        let t = t.clamp(0, self.duration().max(0));
        self.shared.lock().seek = Some(t);
        self.wall_base = t;
        self.wall_start = uptime_ms();
        self.position = t;
        self.ended = false;
        self.poke();
    }

    fn poke(&mut self) {
        self.last_activity = uptime_ms();
    }

    fn step(&mut self, delta: isize) {
        if self.playlist.is_empty() {
            return;
        }
        let i = self.playlist.iter().position(|p| p.eq_ignore_ascii_case(&self.path)).unwrap_or(0) as isize;
        let n = self.playlist.len() as isize;
        let next = ((i + delta) % n + n) % n;
        let p = self.playlist[next as usize].clone();
        self.start(&p);
    }

    fn controls_wanted(&self) -> bool {
        if !self.has_video() || self.paused || self.ended || self.dragging_seek || self.dragging_volume {
            return true;
        }
        let bar = Rect::new(0, self.size.1 - 96, self.size.0, 96);
        uptime_ms() - self.last_activity < 2500 || bar.contains(self.pointer.0, self.pointer.1)
    }

    // Layout -----------------------------------------------------------

    fn seek_rect(&self) -> Rect {
        Rect::new(20, self.size.1 - 74, self.size.0 - 40, 6)
    }

    fn btn_rect(&self, b: Btn) -> Rect {
        let (w, h) = self.size;
        let cy = h - 34;
        match b {
            Btn::Prev => Rect::new(18, cy - 16, 32, 32),
            Btn::Play => Rect::new(56, cy - 20, 40, 40),
            Btn::Next => Rect::new(102, cy - 16, 32, 32),
            Btn::Fullscreen => Rect::new(w - 50, cy - 16, 32, 32),
            Btn::Mute => Rect::new(w - 198, cy - 16, 32, 32),
            Btn::Replay => Rect::new(w / 2 - 36, h / 2 - 36, 72, 72),
        }
    }

    fn volume_rect(&self) -> Rect {
        let (w, h) = self.size;
        Rect::new(w - 162, h - 37, 96, 6)
    }

    fn video_rect(&self) -> Rect {
        let (w, h) = self.size;
        let Some(s) = &self.summary else { return Rect::new(0, 0, w, h) };
        let (vw, vh) = match &self.current {
            Some(f) => (f.width as i64, f.height as i64),
            None => (s.width.max(1) as i64, s.height.max(1) as i64),
        };
        let scale = ((w as i64) << 16) / vw.max(1);
        let scale = scale.min(((h as i64) << 16) / vh.max(1));
        let dw = ((vw * scale) >> 16).clamp(1, w as i64) as i32;
        let dh = ((vh * scale) >> 16).clamp(1, h as i64) as i32;
        Rect::new((w - dw) / 2, (h - dh) / 2, dw, dh)
    }

    fn hit(&self, x: i32, y: i32) -> Option<Btn> {
        if self.ended && self.has_video() && self.btn_rect(Btn::Replay).contains(x, y) {
            return Some(Btn::Replay);
        }
        if self.controls_alpha == 0 {
            return None;
        }
        [Btn::Prev, Btn::Play, Btn::Next, Btn::Mute, Btn::Fullscreen].into_iter().find(|&b| self.btn_rect(b).contains(x, y))
    }

    fn seek_to_x(&mut self, x: i32) {
        let r = self.seek_rect();
        let d = self.duration();
        if d <= 0 {
            return;
        }
        let t = ((x - r.x).clamp(0, r.w) as i64) * d / r.w.max(1) as i64;
        self.seek(t);
    }

    fn volume_to_x(&mut self, x: i32) {
        let r = self.volume_rect();
        self.volume = (((x - r.x).clamp(0, r.w) * 100) / r.w.max(1)) as u8;
        self.muted = false;
        self.apply_volume();
        crate::settings::update(|s| s.player_volume = self.volume);
    }

    // Drawing ----------------------------------------------------------

    fn draw_icon(c: &mut Canvas, b: Btn, r: Rect, playing: bool, col: Color, fullscreen: bool, volume: u8) {
        let (cx, cy) = (r.x + r.w / 2, r.y + r.h / 2);
        match b {
            Btn::Play | Btn::Replay => {
                if b == Btn::Play && playing {
                    c.fill_rounded_rect(Rect::new(cx - 7, cy - 9, 5, 18), 2, col);
                    c.fill_rounded_rect(Rect::new(cx + 2, cy - 9, 5, 18), 2, col);
                } else {
                    let s = if b == Btn::Replay { 16 } else { 10 };
                    triangle(c, cx - s / 2 + 2, cy, s, col);
                }
            }
            Btn::Prev | Btn::Next => {
                let dir = if b == Btn::Next { 1 } else { -1 };
                for k in 0..2 {
                    let x0 = cx + dir * (k * 7 - 4);
                    for i in 0..8 {
                        let hh = 8 - i;
                        c.fill_rect(Rect::new(x0 + dir * i, cy - hh, 1, hh * 2), col);
                    }
                }
                c.fill_rect(Rect::new(cx + dir * 10 - if dir > 0 { 0 } else { 1 }, cy - 8, 2, 16), col);
            }
            Btn::Mute => {
                c.fill_rect(Rect::new(cx - 9, cy - 4, 5, 8), col);
                for k in 0..6 {
                    c.fill_rect(Rect::new(cx - 4 + k, cy - 4 - k, 1, 8 + 2 * k), col);
                }
                if volume == 0 {
                    for d in -4..=4 {
                        c.blend_pixel(cx + 7 + d, cy + d, col, 255);
                        c.blend_pixel(cx + 7 + d, cy - d, col, 255);
                    }
                } else {
                    let arcs = if volume > 66 { 3 } else if volume > 33 { 2 } else { 1 };
                    for a in 0..arcs {
                        c.fill_rect(Rect::new(cx + 5 + a * 4, cy - 3 - a * 2, 2, 6 + a * 4), col);
                    }
                }
            }
            Btn::Fullscreen => {
                let s = 7;
                let corners = [(-1, -1), (1, -1), (-1, 1), (1, 1)];
                for (dx, dy) in corners {
                    let (ox, oy) = if fullscreen { (cx + dx * 2, cy + dy * 2) } else { (cx + dx * 9, cy + dy * 9) };
                    let hx = if fullscreen { dx } else { -dx };
                    let hy = if fullscreen { dy } else { -dy };
                    let x0 = if hx > 0 { ox } else { ox - s + 1 };
                    let y0 = if hy > 0 { oy } else { oy - s + 1 };
                    c.fill_rect(Rect::new(x0, oy - if dy > 0 { 1 } else { 0 }, s, 2), col);
                    c.fill_rect(Rect::new(ox - if dx > 0 { 1 } else { 0 }, y0, 2, s), col);
                }
            }
        }
    }

    fn draw_controls(&mut self, c: &mut Canvas, a: u32) {
        let (w, h) = self.size;
        let f = fonts();
        let white = rgb(255, 255, 255);
        // Bottom gradient.
        for i in 0..110 {
            let alpha = (i * i * 180 / (110 * 110)) as u8;
            c.fill_rect(Rect::new(0, h - 110 + i, w, 1), rgba(0, 0, 0, (alpha as u32 * a / 255) as u8));
        }
        // Seek bar.
        let sr = self.seek_rect();
        let d = self.duration().max(1);
        let pos = self.position.clamp(0, d);
        let hovered = self.hover_seek.is_some() || self.dragging_seek;
        let bar = if hovered { sr.inset(-1) } else { sr };
        c.fill_rounded_rect(bar, bar.h / 2, rgba(255, 255, 255, (70 * a / 255) as u8));
        let px = (sr.w as i64 * pos / d) as i32;
        c.fill_rounded_rect(Rect::new(bar.x, bar.y, px.max(bar.h), bar.h), bar.h / 2, with_alpha(theme::accent(), a as u8));
        if hovered || self.paused {
            c.fill_circle(sr.x + px, sr.y + sr.h / 2, 7, with_alpha(white, a as u8));
        }
        if let Some(hx) = self.hover_seek {
            let t = ((hx - sr.x).clamp(0, sr.w) as i64) * d / sr.w.max(1) as i64;
            let label = fmt_time(t);
            let tw = f.ui.measure(&label) + 16;
            let tip = Rect::new((hx - tw / 2).clamp(4, w - tw - 4), sr.y - 34, tw, 24);
            c.fill_rounded_rect(tip, 7, rgba(20, 22, 28, 230));
            c.draw_text_centered(&f.ui, tip, &label, white);
        }
        // Buttons.
        let playing = !self.paused && !self.ended;
        for b in [Btn::Prev, Btn::Play, Btn::Next, Btn::Mute, Btn::Fullscreen] {
            let r = self.btn_rect(b);
            if b == Btn::Play {
                c.fill_circle(r.x + r.w / 2, r.y + r.h / 2, 20, with_alpha(if self.hover == Some(b) { theme::accent_dark() } else { theme::accent() }, a as u8));
            } else if self.hover == Some(b) {
                c.fill_circle(r.x + r.w / 2, r.y + r.h / 2, 16, rgba(255, 255, 255, (40 * a / 255) as u8));
            }
            let vol = if self.muted { 0 } else { self.volume };
            Self::draw_icon(c, b, r, playing, with_alpha(white, a as u8), self.fullscreen, vol);
        }
        let label = format!("{} / {}", fmt_time(pos), fmt_time(self.duration()));
        let base = h - 34 + (f.ui.ascent - f.ui.descent) / 2;
        c.draw_text(&f.ui, 148, base, &label, with_alpha(white, a as u8));
        // Volume slider.
        let vr = self.volume_rect();
        c.fill_rounded_rect(vr, 3, rgba(255, 255, 255, (70 * a / 255) as u8));
        let vol = if self.muted { 0 } else { self.volume as i32 };
        let vx = vr.w * vol / 100;
        c.fill_rounded_rect(Rect::new(vr.x, vr.y, vx.max(6), vr.h), 3, with_alpha(white, a as u8));
        c.fill_circle(vr.x + vx, vr.y + 3, 6, with_alpha(white, a as u8));
    }

    fn draw_music(&mut self, c: &mut Canvas, s: &Arc<Summary>) {
        // The background, cover and text only change with the song or the
        // window size; draw them once and reuse them.
        let key = (self.size, Arc::as_ptr(s) as usize ^ ((theme::accent() as usize) << 48));
        if self.music_bg.as_ref().map(|(sz, id, _)| (*sz, *id) != key).unwrap_or(true) {
            let mut bg = Surface::new(self.size.0, self.size.1, 0);
            let viz = self.draw_music_static(&mut bg.canvas(), s);
            self.viz_rect = viz;
            self.music_bg = Some((key.0, key.1, bg));
        }
        if let Some((_, _, bg)) = &self.music_bg {
            c.blit(bg, 0, 0);
        }
        let accent = theme::accent();
        let vz = self.viz_rect;
        let n = self.bars.len() as i32;
        let bw = (vz.w / n).max(3);
        for (i, &v) in self.bars.iter().enumerate() {
            let bh = (v as i32 * vz.h / 255).max(3);
            let r = Rect::new(vz.x + i as i32 * bw, vz.bottom() - bh, (bw - 3).max(2), bh);
            let col = mix(accent, rgb(255, 255, 255), (i as u32 * 120 / n as u32).min(255));
            c.fill_rounded_rect(r, 2, with_alpha(col, 230));
        }
    }

    /// Everything in the music view except the visualiser; returns the
    /// visualiser's rectangle.
    fn draw_music_static(&self, c: &mut Canvas, s: &Summary) -> Rect {
        let (w, h) = self.size;
        let f = fonts();
        let accent = theme::accent();
        c.fill_gradient_v(Rect::new(0, 0, w, h), mix(accent, rgb(10, 12, 20), 200), rgb(8, 9, 14));
        let art = (h - 200).min(w / 2 - 40).clamp(80, 300);
        let ax = 40;
        let ay = (h - 110 - art) / 2 + 10;
        let ar = Rect::new(ax, ay.max(20), art, art);
        c.draw_shadow(ar, 16, 24, rgba(0, 0, 0, 140));
        match &s.cover {
            Some(cov) => c.blit_scaled(cov, ar, 255, 16),
            None => {
                c.fill_rounded_rect(ar, 16, mix(accent, rgb(255, 255, 255), 60));
                let iz = art / 2;
                gfx::icons::draw(c, Icon::Music, ar.x + (art - iz) / 2, ar.y + (art - iz) / 2, iz);
            }
        }
        let tx = ar.right() + 36;
        let tw = w - tx - 30;
        let white = rgb(255, 255, 255);
        let mut y = ar.y + 40;
        c.draw_text_clipped(&f.large, tx, y, &s.title, tw, white);
        y += 34;
        if !s.artist.is_empty() {
            c.draw_text_clipped(&f.bold, tx, y, &s.artist, tw, rgba(255, 255, 255, 220));
            y += 24;
        }
        if !s.album.is_empty() {
            c.draw_text_clipped(&f.ui, tx, y, &s.album, tw, rgba(255, 255, 255, 170));
            y += 24;
        }
        c.draw_text_clipped(&f.ui, tx, y + 8, &s.description, tw, rgba(255, 255, 255, 110));
        // Visualiser below the text, down to the bottom of the cover.
        let top = (y + 30).max(ar.bottom() - 140);
        Rect::new(tx, top, tw.max(40), (ar.bottom() - top).max(30))
    }

    /// Advance the visualiser (about 30 times a second); true if it changed.
    fn update_bars(&mut self, clock: i64) -> bool {
        let now = uptime_ms();
        if now - self.last_bars < 33 {
            return false;
        }
        self.last_bars = now;
        let before = self.bars;
        let mut frame = [0i16; 512];
        {
            let s = self.shared.lock();
            if s.viz.len() != VIZ_RING {
                return false;
            }
            let pos = ((clock - s.base_us).max(0) as i128 * 48 / 1000) as u64;
            if pos + 512 > s.viz_written || s.viz_written - pos > (VIZ_RING - 1024) as u64 {
                return false;
            }
            for (i, v) in frame.iter_mut().enumerate() {
                *v = s.viz[((pos + i as u64) as usize) & (VIZ_RING - 1)];
            }
        }
        let mut levels = [0u8; 32];
        media::viz::spectrum(&frame, &mut levels);
        for (b, l) in self.bars.iter_mut().zip(levels.iter()) {
            // Fast attack, slow release.
            let target = (*l as u32).saturating_sub(40) * 255 / 215;
            *b = if target as u8 > *b { target as u8 } else { b.saturating_sub(10).max(target as u8) };
        }
        self.bars != before
    }
}

fn new_shared() -> Shared {
    Shared {
        status: Status::Loading,
        summary: None,
        frames: VecDeque::new(),
        seek: None,
        stop: false,
        late: false,
        base_us: 0,
        epoch: 0,
        eof: false,
        viz: Vec::new(),
        viz_written: 0,
    }
}

/// Right-pointing "play" triangle `s` wide, centred vertically on `cy`.
fn triangle(c: &mut Canvas, x: i32, cy: i32, s: i32, col: Color) {
    for dx in 0..s {
        // Half height shrinks linearly; 1/4-pixel steps give smooth edges.
        let half4 = (s - dx) * 4 * 9 / 8;
        let full = half4 / 4;
        c.fill_rect(Rect::new(x + dx, cy - full, 1, full * 2), col);
        let frac = (half4 % 4) as u32 * 64;
        if frac > 0 {
            c.blend_pixel(x + dx, cy - full - 1, col, frac);
            c.blend_pixel(x + dx, cy + full, col, frac);
        }
    }
}

impl App for Player {
    fn title(&self) -> String {
        format!("{} \u{2014} Media Player", fs::file_name(&self.path))
    }

    fn icon(&self) -> Icon {
        if is_audio_name(&self.path) { Icon::Music } else { Icon::Video }
    }

    fn initial_size(&self) -> (i32, i32) {
        if is_audio_name(&self.path) { (820, 420) } else { (960, 580) }
    }

    fn min_size(&self) -> (i32, i32) {
        (480, 300)
    }

    fn render(&mut self, c: &mut Canvas, (w, h): (i32, i32), _focused: bool) {
        self.size = (w, h);
        let f = fonts();
        let status = self.shared.lock().status.clone();
        let white = rgb(255, 255, 255);
        match &status {
            Status::Failed(msg) => {
                c.fill_rect(Rect::new(0, 0, w, h), rgb(18, 19, 24));
                let mut y = h / 2 - 50;
                c.draw_text_centered(&f.large, Rect::new(0, y - 30, w, 30), "Can't play this file", white);
                y += 10;
                for line in msg.lines() {
                    c.draw_text_centered(&f.ui, Rect::new(20, y, w - 40, 24), line, rgba(255, 255, 255, 190));
                    y += 24;
                }
                return;
            }
            Status::Loading => {
                c.fill_rect(Rect::new(0, 0, w, h), rgb(10, 11, 14));
                spinner(c, w / 2, h / 2, uptime_ms());
                return;
            }
            Status::Ready => {}
        }
        let Some(s) = self.summary.clone() else { return };
        if s.has_video {
            let vr = self.video_rect();
            // Letterbox bars.
            c.fill_rect(Rect::new(0, 0, w, vr.y), rgb(0, 0, 0));
            c.fill_rect(Rect::new(0, vr.bottom(), w, h - vr.bottom()), rgb(0, 0, 0));
            c.fill_rect(Rect::new(0, vr.y, vr.x, vr.h), rgb(0, 0, 0));
            c.fill_rect(Rect::new(vr.right(), vr.y, w - vr.right(), vr.h), rgb(0, 0, 0));
            match &self.current {
                Some(frame) => {
                    if let Some((buf, stride)) = c.raw_region(vr) {
                        frame.render(buf, vr.w as usize, vr.h as usize, stride);
                    }
                }
                None => {
                    c.fill_rect(vr, rgb(0, 0, 0));
                    spinner(c, w / 2, h / 2, uptime_ms());
                }
            }
        } else {
            self.draw_music(c, &s);
        }
        if let Some(wm) = &s.warning {
            let first = wm.lines().next().unwrap_or("");
            let tw = f.ui.measure(first).min(w - 40) + 24;
            let r = Rect::new((w - tw) / 2, 14, tw, 30);
            c.fill_rounded_rect(r, 8, rgba(0, 0, 0, 170));
            c.draw_text_centered(&f.ui, r, first, rgb(255, 214, 140));
        }
        if self.ended && s.has_video {
            let r = self.btn_rect(Btn::Replay);
            c.fill_circle(r.x + r.w / 2, r.y + r.h / 2, 36, rgba(0, 0, 0, 150));
            triangle(c, r.x + r.w / 2 - 6, r.y + r.h / 2, 16, white);
        } else if self.paused && s.has_video && self.controls_alpha > 0 {
            let (cx, cy) = (w / 2, h / 2);
            c.fill_circle(cx, cy, 34, rgba(0, 0, 0, (130 * self.controls_alpha / 255) as u8));
            triangle(c, cx - 5, cy, 14, with_alpha(white, self.controls_alpha as u8));
        }
        if self.controls_alpha > 0 {
            if s.has_video && (self.fullscreen || self.controls_alpha > 0) {
                for i in 0..60 {
                    let alpha = ((60 - i) * (60 - i) * 120 / 3600) as u32 * self.controls_alpha / 255;
                    c.fill_rect(Rect::new(0, i, w, 1), rgba(0, 0, 0, alpha as u8));
                }
                c.draw_text_clipped(&f.bold, 20, 30, &s.title, w - 40, with_alpha(white, self.controls_alpha as u8));
            }
            let a = self.controls_alpha;
            self.draw_controls(c, a);
        }
    }

    fn event(&mut self, ev: &AppEvent, ctx: &mut Ctx) {
        match ev {
            AppEvent::MouseMove { x, y, buttons } => {
                let (x, y) = (*x, *y);
                let moved = (x, y) != self.pointer;
                self.pointer = (x, y);
                if moved {
                    self.poke();
                }
                if self.dragging_seek && buttons & 1 != 0 {
                    self.seek_to_x(x);
                }
                if self.dragging_volume && buttons & 1 != 0 {
                    self.volume_to_x(x);
                }
                self.hover = self.hit(x, y);
                self.hover_seek = if self.seek_rect().inset(-8).contains(x, y) && self.controls_alpha > 0 { Some(x) } else { None };
                ctx.redraw();
            }
            AppEvent::MouseLeave => {
                self.hover = None;
                self.hover_seek = None;
                ctx.redraw();
            }
            AppEvent::MouseDown { x, y, button: 0, clicks } => {
                let (x, y) = (*x, *y);
                self.poke();
                if let Some(b) = self.hit(x, y) {
                    match b {
                        Btn::Play | Btn::Replay => {
                            let p = !self.paused && !self.ended;
                            self.set_paused(p);
                        }
                        Btn::Prev => {
                            if self.position > 3_000_000 {
                                self.seek(0);
                            } else {
                                self.step(-1);
                            }
                        }
                        Btn::Next => self.step(1),
                        Btn::Mute => {
                            self.muted = !self.muted;
                            self.apply_volume();
                        }
                        Btn::Fullscreen => {
                            self.fullscreen = !self.fullscreen;
                            ctx.set_fullscreen(self.fullscreen);
                        }
                    }
                } else if self.seek_rect().inset(-8).contains(x, y) && self.controls_alpha > 0 {
                    self.dragging_seek = true;
                    self.seek_to_x(x);
                } else if self.volume_rect().inset(-8).contains(x, y) && self.controls_alpha > 0 {
                    self.dragging_volume = true;
                    self.volume_to_x(x);
                } else if self.has_video() && y < self.size.1 - 100 {
                    if *clicks >= 2 {
                        self.fullscreen = !self.fullscreen;
                        ctx.set_fullscreen(self.fullscreen);
                        // The first click of the pair toggled pause; undo it.
                        let p = !self.paused;
                        self.set_paused(p);
                    } else {
                        let p = !self.paused && !self.ended;
                        self.set_paused(p);
                    }
                }
                ctx.redraw();
            }
            AppEvent::MouseUp { .. } => {
                self.dragging_seek = false;
                self.dragging_volume = false;
                ctx.redraw();
            }
            AppEvent::Wheel { delta, .. } => {
                self.volume = (self.volume as i32 - delta * 5).clamp(0, 100) as u8;
                self.muted = false;
                self.apply_volume();
                crate::settings::update(|s| s.player_volume = self.volume);
                self.poke();
                ctx.redraw();
            }
            AppEvent::Key(k) if k.pressed => {
                self.poke();
                match k.key {
                    Key::Char(' ') | Key::Char('k') => {
                        let p = !self.paused && !self.ended;
                        self.set_paused(p);
                    }
                    Key::Left => self.seek(self.position - if k.shift { 30_000_000 } else { 5_000_000 }),
                    Key::Right => self.seek(self.position + if k.shift { 30_000_000 } else { 5_000_000 }),
                    Key::Char('j') => self.seek(self.position - 10_000_000),
                    Key::Char('l') => self.seek(self.position + 10_000_000),
                    Key::Home => self.seek(0),
                    Key::Up | Key::Down => {
                        let d = if k.key == Key::Up { 5 } else { -5 };
                        self.volume = (self.volume as i32 + d).clamp(0, 100) as u8;
                        self.muted = false;
                        self.apply_volume();
                        crate::settings::update(|s| s.player_volume = self.volume);
                    }
                    Key::Char('m') => {
                        self.muted = !self.muted;
                        self.apply_volume();
                    }
                    Key::Char('f') => {
                        self.fullscreen = !self.fullscreen;
                        ctx.set_fullscreen(self.fullscreen);
                    }
                    Key::Escape if self.fullscreen => {
                        self.fullscreen = false;
                        ctx.set_fullscreen(false);
                    }
                    Key::Char('n') => self.step(1),
                    Key::Char('p') => self.step(-1),
                    Key::Char(c @ '0'..='9') => {
                        let d = self.duration();
                        self.seek(d * (c as i64 - '0' as i64) / 10);
                    }
                    _ => return,
                }
                ctx.redraw();
            }
            AppEvent::Resized { .. } | AppEvent::Focus(_) => ctx.redraw(),
            _ => {}
        }
    }

    fn tick(&mut self, ctx: &mut Ctx) {
        let (status, summary, epoch, eof) = {
            let s = self.shared.lock();
            (s.status.clone(), s.summary.clone(), s.epoch, s.eof)
        };
        if self.summary.is_none() && summary.is_some() {
            self.summary = summary;
            ctx.redraw();
        }
        if status == Status::Loading {
            if uptime_ms() - self.opened_at > 300 {
                ctx.redraw();
            }
            return;
        }
        if let Status::Failed(_) = status {
            return;
        }
        // Controls fade.
        let want = if self.controls_wanted() { 255 } else { 0 };
        if want != self.controls_alpha {
            self.controls_alpha = if want > self.controls_alpha { (self.controls_alpha + 60).min(255) } else { self.controls_alpha.saturating_sub(30) };
            ctx.redraw();
        }
        if self.paused || self.ended {
            return;
        }
        let clock = self.clock();
        let has_video = self.has_video();
        // Pick the frame whose time has come.
        if has_video {
            let mut s = self.shared.lock();
            if epoch != self.epoch {
                self.epoch = epoch;
            }
            let mut newest = None;
            while let Some(f) = s.frames.front() {
                if f.pts <= clock {
                    newest = s.frames.pop_front();
                } else {
                    break;
                }
            }
            let behind = newest.as_ref().map(|f| clock - f.pts > 120_000).unwrap_or(false);
            if behind {
                s.late = true;
            } else if !s.frames.is_empty() {
                s.late = false;
            }
            drop(s);
            if let Some(f) = newest {
                self.current = Some(f);
                // The client area only: no title bar or shadow work.
                ctx.redraw_rect(Rect::new(0, 0, self.size.0, self.size.1));
            }
        } else if self.update_bars(clock) {
            ctx.redraw_rect(self.viz_rect);
        }
        let new_pos = clock.clamp(0, self.duration().max(clock));
        if new_pos / 250_000 != self.position / 250_000 && self.controls_alpha > 0 {
            // Seek bar and time.
            ctx.redraw_rect(Rect::new(0, self.size.1 - 110, self.size.0, 110));
        }
        self.position = new_pos;
        // End of the file: everything played.
        let drained = self.stream.queued() == 0 && self.shared.lock().frames.is_empty();
        if eof && drained && uptime_ms() - self.opened_at > 500 {
            let near_end = self.duration() <= 0 || clock + 500_000 >= self.duration() || !self.summary.as_ref().map(|s| s.has_audio).unwrap_or(false);
            if near_end {
                self.ended = true;
                self.paused = false;
                self.position = self.duration();
                if !has_video && self.playlist.len() > 1 {
                    // Music: carry on with the next song in the folder.
                    let last = self.playlist.iter().position(|p| p.eq_ignore_ascii_case(&self.path)) == Some(self.playlist.len() - 1);
                    if !last {
                        self.step(1);
                    }
                }
                ctx.redraw();
            }
        }
    }

    fn request_close(&mut self, _ctx: &mut Ctx) -> bool {
        self.shared.lock().stop = true;
        self.stream.close();
        true
    }
}

fn spinner(c: &mut Canvas, cx: i32, cy: i32, now: u64) {
    let n = 12;
    let head = ((now / 80) % n as u64) as i32;
    for i in 0..n {
        let ang = i * 30;
        let (s, co) = sin_cos_deg(ang);
        let x = cx + (co * 22 / 1024);
        let y = cy + (s * 22 / 1024);
        let age = (head - i + n) % n;
        let a = 255 - age * 18;
        c.fill_circle(x, y, 3, rgba(255, 255, 255, a.clamp(40, 255) as u8));
    }
}

/// sin and cos of whole degrees in 1/1024 units (for the spinner).
fn sin_cos_deg(deg: i32) -> (i32, i32) {
    const S: [i32; 13] = [0, 265, 512, 724, 887, 989, 1024, 989, 887, 724, 512, 265, 0];
    let d = deg.rem_euclid(360);
    let idx = |a: i32| -> i32 {
        let a = a.rem_euclid(360);
        let v = S[((a % 180) / 15) as usize];
        if a >= 180 { -v } else { v }
    };
    (idx(d), idx(d + 90))
}
