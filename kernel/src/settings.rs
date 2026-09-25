//! System settings, stored as `key = value` lines in /config/settings.ini.

use alloc::format;
use alloc::string::{String, ToString};
use core::sync::atomic::{AtomicU64, Ordering};

use crate::sync::Spin;

pub const PATH: &str = "/config/settings.ini";

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub resolution: Option<(u32, u32)>,
    pub wallpaper: usize,
    /// Path of a picture used as the wallpaper; empty = built-in one.
    pub wallpaper_image: String,
    pub accent: usize,
    pub animations: bool,
    pub volume: u8,
    pub muted: bool,
    pub system_sounds: bool,
    pub pointer_speed: u8,
    pub double_click_ms: u32,
    pub natural_scroll: bool,
    pub key_repeat: u8,
    pub clock_24h: bool,
    pub show_seconds: bool,
    pub tz_offset_min: i32,
    pub dhcp: bool,
    pub static_ip: String,
    pub static_mask: String,
    pub static_gateway: String,
    pub static_dns: String,
    pub hostname: String,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            resolution: None,
            wallpaper: 0,
            wallpaper_image: String::new(),
            accent: 0,
            animations: true,
            volume: 70,
            muted: false,
            system_sounds: true,
            pointer_speed: 5,
            double_click_ms: 450,
            natural_scroll: false,
            key_repeat: 3,
            clock_24h: true,
            show_seconds: false,
            tz_offset_min: 0,
            dhcp: true,
            static_ip: String::from("10.0.2.15"),
            static_mask: String::from("255.255.255.0"),
            static_gateway: String::from("10.0.2.2"),
            static_dns: String::from("10.0.2.3"),
            hostname: String::from("mayos"),
        }
    }
}

static CURRENT: Spin<Option<Settings>> = Spin::new(None);
static GENERATION: AtomicU64 = AtomicU64::new(0);

pub fn get() -> Settings {
    CURRENT.lock().clone().unwrap_or_default()
}

/// Changes whenever a setting changes, so views can refresh cheaply.
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

fn parse_bool(v: &str) -> Option<bool> {
    match v {
        "true" | "yes" | "1" | "on" => Some(true),
        "false" | "no" | "0" | "off" => Some(false),
        _ => None,
    }
}

pub fn parse(text: &str) -> Settings {
    let mut s = Settings::default();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        let num = |d: u32| v.parse::<u32>().unwrap_or(d);
        match k {
            "resolution" => {
                s.resolution = v.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)));
            }
            "wallpaper" => s.wallpaper = num(0) as usize,
            "wallpaper_image" => s.wallpaper_image = v.to_string(),
            "accent" => s.accent = num(0) as usize,
            "animations" => s.animations = parse_bool(v).unwrap_or(true),
            "volume" => s.volume = num(70).min(100) as u8,
            "muted" => s.muted = parse_bool(v).unwrap_or(false),
            "system_sounds" => s.system_sounds = parse_bool(v).unwrap_or(true),
            "pointer_speed" => s.pointer_speed = num(5).clamp(1, 10) as u8,
            "double_click_ms" => s.double_click_ms = num(450).clamp(200, 900),
            "natural_scroll" => s.natural_scroll = parse_bool(v).unwrap_or(false),
            "key_repeat" => s.key_repeat = num(3).clamp(1, 5) as u8,
            "clock_24h" => s.clock_24h = parse_bool(v).unwrap_or(true),
            "show_seconds" => s.show_seconds = parse_bool(v).unwrap_or(false),
            "tz_offset_min" => s.tz_offset_min = v.parse::<i32>().unwrap_or(0).clamp(-12 * 60, 14 * 60),
            "dhcp" => s.dhcp = parse_bool(v).unwrap_or(true),
            "static_ip" => s.static_ip = v.to_string(),
            "static_mask" => s.static_mask = v.to_string(),
            "static_gateway" => s.static_gateway = v.to_string(),
            "static_dns" => s.static_dns = v.to_string(),
            "hostname" => s.hostname = v.to_string(),
            _ => {}
        }
    }
    s
}

pub fn serialize(s: &Settings) -> String {
    let res = match s.resolution {
        Some((w, h)) => format!("{}x{}", w, h),
        None => String::from("auto"),
    };
    format!(
        "# MayOS settings (edited by the Settings app)\n\
         resolution = {}\nwallpaper = {}\nwallpaper_image = {}\naccent = {}\nanimations = {}\n\
         volume = {}\nmuted = {}\nsystem_sounds = {}\n\
         pointer_speed = {}\ndouble_click_ms = {}\nnatural_scroll = {}\nkey_repeat = {}\n\
         clock_24h = {}\nshow_seconds = {}\ntz_offset_min = {}\n\
         dhcp = {}\nstatic_ip = {}\nstatic_mask = {}\nstatic_gateway = {}\nstatic_dns = {}\nhostname = {}\n",
        res,
        s.wallpaper,
        s.wallpaper_image,
        s.accent,
        s.animations,
        s.volume,
        s.muted,
        s.system_sounds,
        s.pointer_speed,
        s.double_click_ms,
        s.natural_scroll,
        s.key_repeat,
        s.clock_24h,
        s.show_seconds,
        s.tz_offset_min,
        s.dhcp,
        s.static_ip,
        s.static_mask,
        s.static_gateway,
        s.static_dns,
        s.hostname
    )
}

/// Load from disk (or defaults) and apply everything.
pub fn load() {
    let s = crate::fs::read_file(PATH).map(|d| parse(&String::from_utf8_lossy(&d))).unwrap_or_default();
    *CURRENT.lock() = Some(s.clone());
    apply(&s, None);
}

/// Change settings, apply the side effects and save.
pub fn update(f: impl FnOnce(&mut Settings)) {
    let (old, new) = {
        let mut g = CURRENT.lock();
        let cur = g.get_or_insert_with(Settings::default);
        let old = cur.clone();
        f(cur);
        (old, cur.clone())
    };
    if old == new {
        return;
    }
    GENERATION.fetch_add(1, Ordering::Relaxed);
    apply(&new, Some(&old));
    save(&new);
}

/// Like `update`, but without writing to disk (used while dragging a
/// slider; call `save` when the drag ends).
pub fn update_live(f: impl FnOnce(&mut Settings)) {
    let (old, new) = {
        let mut g = CURRENT.lock();
        let cur = g.get_or_insert_with(Settings::default);
        let old = cur.clone();
        f(cur);
        (old, cur.clone())
    };
    if old != new {
        GENERATION.fetch_add(1, Ordering::Relaxed);
        apply(&new, Some(&old));
    }
}

pub fn save(s: &Settings) {
    if !crate::fs::is_dir("/config") {
        let _ = crate::fs::create_dir("/config");
    }
    if let Err(e) = crate::fs::write_file(PATH, serialize(s).as_bytes()) {
        crate::kprintln!("settings: cannot save: {}", e);
    }
}

fn network_changed(a: &Settings, b: &Settings) -> bool {
    a.dhcp != b.dhcp
        || a.static_ip != b.static_ip
        || a.static_mask != b.static_mask
        || a.static_gateway != b.static_gateway
        || a.static_dns != b.static_dns
}

/// Push settings into the subsystems that need them.
pub fn apply(s: &Settings, old: Option<&Settings>) {
    crate::gui::theme::set_accent(s.accent);
    crate::audio::set_volume(s.volume, s.muted);
    if old.map(|o| o.key_repeat != s.key_repeat).unwrap_or(true) {
        crate::drivers::ps2::set_repeat(s.key_repeat);
    }
    if old.map(|o| network_changed(o, s)).unwrap_or(true) {
        apply_network(s);
    }
}

pub fn apply_network(s: &Settings) {
    if !crate::network::is_present() {
        return;
    }
    if s.dhcp {
        if crate::network::status().map(|st| st.dhcp == crate::network::DhcpState::Disabled).unwrap_or(false) {
            crate::network::use_dhcp();
        }
    } else {
        let p = |v: &str| net::Ipv4::parse(v).unwrap_or_default();
        let dns = s.static_dns.split(',').filter_map(net::Ipv4::parse).collect();
        crate::network::set_static(p(&s.static_ip), p(&s.static_mask), p(&s.static_gateway), dns);
    }
}

/// Wall-clock time adjusted by the configured time-zone offset.
pub fn local_time() -> crate::arch::rtc::DateTime {
    let t = crate::arch::rtc::now();
    let off = get().tz_offset_min;
    shift_minutes(t, off)
}

fn days_in_month(y: u16, m: u8) -> u8 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

pub fn shift_minutes(mut t: crate::arch::rtc::DateTime, minutes: i32) -> crate::arch::rtc::DateTime {
    let mut total = t.hour as i32 * 60 + t.minute as i32 + minutes;
    let mut day_delta = 0;
    while total < 0 {
        total += 24 * 60;
        day_delta -= 1;
    }
    while total >= 24 * 60 {
        total -= 24 * 60;
        day_delta += 1;
    }
    t.hour = (total / 60) as u8;
    t.minute = (total % 60) as u8;
    if day_delta > 0 {
        t.day += 1;
        if t.day > days_in_month(t.year, t.month) {
            t.day = 1;
            t.month += 1;
            if t.month > 12 {
                t.month = 1;
                t.year += 1;
            }
        }
    } else if day_delta < 0 {
        if t.day > 1 {
            t.day -= 1;
        } else {
            t.month = if t.month == 1 { 12 } else { t.month - 1 };
            if t.month == 12 {
                t.year -= 1;
            }
            t.day = days_in_month(t.year, t.month);
        }
    }
    t
}
