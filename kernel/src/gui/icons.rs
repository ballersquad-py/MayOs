//! App and file icons: the Papirus icon theme (GPL-3.0, see
//! assets/icons/LICENSE-Papirus), embedded as 32/64/128 px PNGs, drawn
//! smoothly at any size (box-filtered from the next larger picture, cached
//! per size) with their alpha. Falls back to the drawn icons in gfx.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use gfx::icons::Icon;
use gfx::Canvas;

use crate::sync::Spin;

static PNGS: &[(&str, [&[u8]; 3])] = &[
    ("files", [include_bytes!("../../../assets/icons/files-32.png"), include_bytes!("../../../assets/icons/files-64.png"), include_bytes!("../../../assets/icons/files-128.png")]),
    ("terminal", [include_bytes!("../../../assets/icons/terminal-32.png"), include_bytes!("../../../assets/icons/terminal-64.png"), include_bytes!("../../../assets/icons/terminal-128.png")]),
    ("editor", [include_bytes!("../../../assets/icons/editor-32.png"), include_bytes!("../../../assets/icons/editor-64.png"), include_bytes!("../../../assets/icons/editor-128.png")]),
    ("browser", [include_bytes!("../../../assets/icons/browser-32.png"), include_bytes!("../../../assets/icons/browser-64.png"), include_bytes!("../../../assets/icons/browser-128.png")]),
    ("firefox", [include_bytes!("../../../assets/icons/firefox-32.png"), include_bytes!("../../../assets/icons/firefox-64.png"), include_bytes!("../../../assets/icons/firefox-128.png")]),
    ("settings", [include_bytes!("../../../assets/icons/settings-32.png"), include_bytes!("../../../assets/icons/settings-64.png"), include_bytes!("../../../assets/icons/settings-128.png")]),
    ("tasks", [include_bytes!("../../../assets/icons/tasks-32.png"), include_bytes!("../../../assets/icons/tasks-64.png"), include_bytes!("../../../assets/icons/tasks-128.png")]),
    ("set-display", [include_bytes!("../../../assets/icons/set-display-32.png"), include_bytes!("../../../assets/icons/set-display-64.png"), include_bytes!("../../../assets/icons/set-display-128.png")]),
    ("set-theme", [include_bytes!("../../../assets/icons/set-theme-32.png"), include_bytes!("../../../assets/icons/set-theme-64.png"), include_bytes!("../../../assets/icons/set-theme-128.png")]),
    ("set-sound", [include_bytes!("../../../assets/icons/set-sound-32.png"), include_bytes!("../../../assets/icons/set-sound-64.png"), include_bytes!("../../../assets/icons/set-sound-128.png")]),
    ("set-network", [include_bytes!("../../../assets/icons/set-network-32.png"), include_bytes!("../../../assets/icons/set-network-64.png"), include_bytes!("../../../assets/icons/set-network-128.png")]),
    ("set-mouse", [include_bytes!("../../../assets/icons/set-mouse-32.png"), include_bytes!("../../../assets/icons/set-mouse-64.png"), include_bytes!("../../../assets/icons/set-mouse-128.png")]),
    ("set-time", [include_bytes!("../../../assets/icons/set-time-32.png"), include_bytes!("../../../assets/icons/set-time-64.png"), include_bytes!("../../../assets/icons/set-time-128.png")]),
    ("about", [include_bytes!("../../../assets/icons/about-32.png"), include_bytes!("../../../assets/icons/about-64.png"), include_bytes!("../../../assets/icons/about-128.png")]),
    ("minecraft", [include_bytes!("../../../assets/icons/minecraft-32.png"), include_bytes!("../../../assets/icons/minecraft-64.png"), include_bytes!("../../../assets/icons/minecraft-128.png")]),
    ("player", [include_bytes!("../../../assets/icons/player-32.png"), include_bytes!("../../../assets/icons/player-64.png"), include_bytes!("../../../assets/icons/player-128.png")]),
    ("weather", [include_bytes!("../../../assets/icons/weather-32.png"), include_bytes!("../../../assets/icons/weather-64.png"), include_bytes!("../../../assets/icons/weather-128.png")]),
    ("clock", [include_bytes!("../../../assets/icons/clock-32.png"), include_bytes!("../../../assets/icons/clock-64.png"), include_bytes!("../../../assets/icons/clock-128.png")]),
    ("software", [include_bytes!("../../../assets/icons/software-32.png"), include_bytes!("../../../assets/icons/software-64.png"), include_bytes!("../../../assets/icons/software-128.png")]),
    ("folder", [include_bytes!("../../../assets/icons/folder-32.png"), include_bytes!("../../../assets/icons/folder-64.png"), include_bytes!("../../../assets/icons/folder-128.png")]),
    ("home", [include_bytes!("../../../assets/icons/home-32.png"), include_bytes!("../../../assets/icons/home-64.png"), include_bytes!("../../../assets/icons/home-128.png")]),
    ("trash", [include_bytes!("../../../assets/icons/trash-32.png"), include_bytes!("../../../assets/icons/trash-64.png"), include_bytes!("../../../assets/icons/trash-128.png")]),
    ("videos", [include_bytes!("../../../assets/icons/videos-32.png"), include_bytes!("../../../assets/icons/videos-64.png"), include_bytes!("../../../assets/icons/videos-128.png")]),
    ("pictures", [include_bytes!("../../../assets/icons/pictures-32.png"), include_bytes!("../../../assets/icons/pictures-64.png"), include_bytes!("../../../assets/icons/pictures-128.png")]),
    ("music", [include_bytes!("../../../assets/icons/music-32.png"), include_bytes!("../../../assets/icons/music-64.png"), include_bytes!("../../../assets/icons/music-128.png")]),
    ("documents", [include_bytes!("../../../assets/icons/documents-32.png"), include_bytes!("../../../assets/icons/documents-64.png"), include_bytes!("../../../assets/icons/documents-128.png")]),
    ("downloads", [include_bytes!("../../../assets/icons/downloads-32.png"), include_bytes!("../../../assets/icons/downloads-64.png"), include_bytes!("../../../assets/icons/downloads-128.png")]),
    ("drive", [include_bytes!("../../../assets/icons/drive-32.png"), include_bytes!("../../../assets/icons/drive-64.png"), include_bytes!("../../../assets/icons/drive-128.png")]),
    ("computer", [include_bytes!("../../../assets/icons/computer-32.png"), include_bytes!("../../../assets/icons/computer-64.png"), include_bytes!("../../../assets/icons/computer-128.png")]),
    ("text", [include_bytes!("../../../assets/icons/text-32.png"), include_bytes!("../../../assets/icons/text-64.png"), include_bytes!("../../../assets/icons/text-128.png")]),
    ("image", [include_bytes!("../../../assets/icons/image-32.png"), include_bytes!("../../../assets/icons/image-64.png"), include_bytes!("../../../assets/icons/image-128.png")]),
    ("video", [include_bytes!("../../../assets/icons/video-32.png"), include_bytes!("../../../assets/icons/video-64.png"), include_bytes!("../../../assets/icons/video-128.png")]),
    ("audio", [include_bytes!("../../../assets/icons/audio-32.png"), include_bytes!("../../../assets/icons/audio-64.png"), include_bytes!("../../../assets/icons/audio-128.png")]),
    ("program", [include_bytes!("../../../assets/icons/program-32.png"), include_bytes!("../../../assets/icons/program-64.png"), include_bytes!("../../../assets/icons/program-128.png")]),
    ("html", [include_bytes!("../../../assets/icons/html-32.png"), include_bytes!("../../../assets/icons/html-64.png"), include_bytes!("../../../assets/icons/html-128.png")]),
    ("mayos", [include_bytes!("../../../assets/icons/mayos-32.png"), include_bytes!("../../../assets/icons/mayos-64.png"), include_bytes!("../../../assets/icons/mayos-128.png")]),
    ("net-wired", [include_bytes!("../../../assets/icons/net-wired-32.png"), include_bytes!("../../../assets/icons/net-wired-64.png"), include_bytes!("../../../assets/icons/net-wired-128.png")]),
    ("net-off", [include_bytes!("../../../assets/icons/net-off-32.png"), include_bytes!("../../../assets/icons/net-off-64.png"), include_bytes!("../../../assets/icons/net-off-128.png")]),
    ("net-phone", [include_bytes!("../../../assets/icons/net-phone-32.png"), include_bytes!("../../../assets/icons/net-phone-64.png"), include_bytes!("../../../assets/icons/net-phone-128.png")]),
    ("vol-high", [include_bytes!("../../../assets/icons/vol-high-32.png"), include_bytes!("../../../assets/icons/vol-high-64.png"), include_bytes!("../../../assets/icons/vol-high-128.png")]),
    ("vol-med", [include_bytes!("../../../assets/icons/vol-med-32.png"), include_bytes!("../../../assets/icons/vol-med-64.png"), include_bytes!("../../../assets/icons/vol-med-128.png")]),
    ("vol-low", [include_bytes!("../../../assets/icons/vol-low-32.png"), include_bytes!("../../../assets/icons/vol-low-64.png"), include_bytes!("../../../assets/icons/vol-low-128.png")]),
    ("vol-mute", [include_bytes!("../../../assets/icons/vol-mute-32.png"), include_bytes!("../../../assets/icons/vol-mute-64.png"), include_bytes!("../../../assets/icons/vol-mute-128.png")]),
];

fn name(i: Icon) -> &'static str {
    match i {
        Icon::Folder => "folder",
        Icon::File => "text",
        Icon::TextFile => "text",
        Icon::Program => "program",
        Icon::Image => "image",
        Icon::Terminal => "terminal",
        Icon::Explorer => "files",
        Icon::Editor => "editor",
        Icon::Info => "about",
        Icon::Drive => "drive",
        Icon::Home => "home",
        Icon::Settings => "settings",
        Icon::Video => "video",
        Icon::Music => "audio",
        Icon::Browser => "browser",
        Icon::Tasks => "tasks",
        Icon::Firefox => "firefox",
        Icon::Minecraft => "minecraft",
        Icon::Trash => "trash",
        Icon::Documents => "documents",
        Icon::Downloads => "downloads",
        Icon::Pictures => "pictures",
        Icon::Computer => "computer",
        Icon::Audio => "audio",
        Icon::Html => "html",
        Icon::Weather => "weather",
        Icon::Clock => "clock",
        Icon::Software => "software",
        Icon::Player => "player",
        Icon::MayOS => "mayos",
        Icon::NetWired => "net-wired",
        Icon::NetOff => "net-off",
        Icon::NetPhone => "net-phone",
        Icon::VolHigh => "vol-high",
        Icon::VolMed => "vol-med",
        Icon::VolLow => "vol-low",
        Icon::VolMute => "vol-mute",
        Icon::SetDisplay => "set-display",
        Icon::SetTheme => "set-theme",
        Icon::SetSound => "set-sound",
        Icon::SetNetwork => "set-network",
        Icon::SetMouse => "set-mouse",
        Icon::SetTime => "set-time",
    }
}

/// Decoded, resized pictures: (name, size) -> ARGB pixels.
static CACHE: Spin<BTreeMap<(&'static str, i32), Option<alloc::sync::Arc<Vec<u32>>>>> = Spin::new(BTreeMap::new());

fn picture(n: &'static str, s: i32) -> Option<alloc::sync::Arc<Vec<u32>>> {
    if let Some(p) = CACHE.lock().get(&(n, s)) {
        return p.clone();
    }
    let made = make(n, s).map(alloc::sync::Arc::new);
    CACHE.lock().insert((n, s), made.clone());
    made
}

fn make(n: &str, s: i32) -> Option<Vec<u32>> {
    let (_, pngs) = PNGS.iter().find(|(k, _)| *k == n)?;
    let src = if s <= 32 { pngs[0] } else if s <= 64 { pngs[1] } else { pngs[2] };
    let img = image::decode(src).ok()?;
    let (w, h) = (img.width as i32, img.height as i32);
    if w == s && h == s {
        return Some(img.pixels);
    }
    // Box filter with premultiplied alpha (smooth edges at any size).
    let mut out = alloc::vec![0u32; (s * s) as usize];
    for oy in 0..s {
        let (y0, y1) = (oy * h / s, ((oy + 1) * h / s).max(oy * h / s + 1).min(h));
        for ox in 0..s {
            let (x0, x1) = (ox * w / s, ((ox + 1) * w / s).max(ox * w / s + 1).min(w));
            let (mut a, mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32, 0u32);
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = img.pixels[(y * w + x) as usize];
                    let pa = p >> 24;
                    a += pa;
                    r += (p >> 16 & 0xff) * pa;
                    g += (p >> 8 & 0xff) * pa;
                    b += (p & 0xff) * pa;
                    n += 1;
                }
            }
            let px = if a == 0 { 0 } else { (a / n) << 24 | (r / a) << 16 | (g / a) << 8 | (b / a) };
            out[(oy * s + ox) as usize] = px;
        }
    }
    Some(out)
}

pub fn draw(c: &mut Canvas, icon: Icon, x: i32, y: i32, s: i32) {
    let Some(p) = picture(name(icon), s) else {
        return gfx::icons::draw(c, icon, x, y, s);
    };
    for j in 0..s {
        for i in 0..s {
            let px = p[(j * s + i) as usize];
            let a = px >> 24;
            if a != 0 {
                c.blend_pixel(x + i, y + j, px | 0xff00_0000, a);
            }
        }
    }
}
