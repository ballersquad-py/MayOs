//! Colours, metrics and fonts shared by the desktop and its apps.

use gfx::{rgb, rgba, Color, Font};

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::sync::Once;

/// Accent colours offered in Settings: (name, colour).
pub const ACCENTS: &[(&str, Color)] = &[
    ("Mint", rgb(0x35, 0xa8, 0x54)),
    ("Blue", rgb(0x2f, 0x7c, 0xf6)),
    ("Purple", rgb(0x8e, 0x5c, 0xe6)),
    ("Pink", rgb(0xe0, 0x4f, 0x92)),
    ("Red", rgb(0xe5, 0x48, 0x4d)),
    ("Orange", rgb(0xf0, 0x8a, 0x24)),
    ("Green", rgb(0x2f, 0xa8, 0x5a)),
    ("Teal", rgb(0x14, 0x9e, 0xa8)),
    ("Graphite", rgb(0x6e, 0x74, 0x80)),
];

static ACCENT_INDEX: AtomicUsize = AtomicUsize::new(0);

pub fn set_accent(i: usize) {
    ACCENT_INDEX.store(i.min(ACCENTS.len() - 1), Ordering::Relaxed);
}

pub fn accent() -> Color {
    ACCENTS[ACCENT_INDEX.load(Ordering::Relaxed)].1
}

/// A darker shade of the accent (hover / pressed states).
pub fn accent_dark() -> Color {
    gfx::mix(accent(), rgb(0, 0, 0), 40)
}

/// A pale tint of the accent (text selection).
pub fn selection() -> Color {
    gfx::mix(rgb(0xff, 0xff, 0xff), accent(), 55)
}

pub const TEXT_ON_ACCENT: Color = rgb(0xff, 0xff, 0xff);

/// Dark mode (Settings > Appearance): the colours below follow it.
static DARK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn set_dark(on: bool) {
    DARK.store(on, Ordering::Relaxed);
}

pub fn is_dark() -> bool {
    DARK.load(Ordering::Relaxed)
}

macro_rules! themed {
    ($($name:ident: $light:expr, $dark:expr;)*) => {
        $(
            #[inline]
            pub fn $name() -> Color {
                if is_dark() { $dark } else { $light }
            }
        )*
    };
}

themed! {
    text: rgb(0x1d, 0x1f, 0x24), rgb(0xe8, 0xea, 0xee);
    text_dim: rgb(0x6b, 0x71, 0x7e), rgb(0x9a, 0xa1, 0xad);
    window_bg: rgb(0xff, 0xff, 0xff), rgb(0x1f, 0x21, 0x26);
    panel_bg: rgb(0xf5, 0xf6, 0xf8), rgb(0x27, 0x2a, 0x30);
    sidebar_bg: rgb(0xee, 0xf0, 0xf4), rgb(0x23, 0x26, 0x2b);
    separator: rgb(0xe1, 0xe4, 0xe9), rgb(0x36, 0x3a, 0x42);
    hover: rgb(0xec, 0xf2, 0xfe), rgb(0x30, 0x36, 0x44);
    titlebar: rgb(0xf4, 0xf5, 0xf7), rgb(0x2a, 0x2d, 0x33);
    titlebar_inactive: rgb(0xea, 0xeb, 0xee), rgb(0x24, 0x27, 0x2c);
    control_off: rgb(0xd5, 0xd9, 0xe0), rgb(0x4a, 0x4f, 0x59);
    card_bg: rgb(0xff, 0xff, 0xff), rgb(0x2b, 0x2e, 0x35);
    border: rgba(0, 0, 0, 38), rgba(255, 255, 255, 34);
}

/// Kept for the few places that need a constant.
pub fn text_on_accent() -> Color {
    TEXT_ON_ACCENT
}
pub const DANGER: Color = rgb(0xe5, 0x48, 0x4d);
pub const SHADOW: Color = rgba(6, 10, 24, 105);
pub const SHADOW_INACTIVE: Color = rgba(6, 10, 24, 58);

pub const TITLEBAR_H: i32 = 36;
pub const WINDOW_RADIUS: i32 = 12;
pub const SHADOW_BLUR: i32 = 30;
pub const SHADOW_OFFSET: i32 = 6;
pub const TOPBAR_H: i32 = 28;
pub const DOCK_ICON: i32 = 48;
pub const DOCK_PAD: i32 = 10;

pub struct Fonts {
    pub ui: Font,
    pub bold: Font,
    pub mono: Font,
    pub large: Font,
    pub small_bold: Font,
    /// Medium 15 px (panel clock, headings).
    pub medium: Font,
}

static FONTS: Once<Fonts> = Once::new();

pub fn init() {
    FONTS.set(Fonts {
        // Noto Sans, and Inter for the clock (SIL Open Font License, assets/fonts/).
        ui: Font::parse(include_bytes!("../../../assets/fonts/noto-13.mfnt")).expect("sans font"),
        bold: Font::parse(include_bytes!("../../../assets/fonts/noto-semibold-13.mfnt")).expect("bold font"),
        mono: Font::parse(include_bytes!("../../../assets/fonts/mono-13.mfnt")).expect("mono font"),
        large: Font::parse(include_bytes!("../../../assets/fonts/noto-semibold-24.mfnt")).expect("large font"),
        small_bold: Font::parse(include_bytes!("../../../assets/fonts/noto-semibold-11.mfnt")).expect("small font"),
        medium: Font::parse(include_bytes!("../../../assets/fonts/inter-medium-15.mfnt")).expect("medium font"),
    });
}

pub fn fonts() -> &'static Fonts {
    FONTS.get().expect("fonts not initialised")
}

pub fn try_fonts() -> Option<&'static Fonts> {
    FONTS.get()
}

/// A light overlay (hover, pressed, outlines) that shows on either theme.
pub fn shade(a: u8) -> Color {
    if is_dark() {
        gfx::with_alpha(0xffffff, (a as u32 * 3 / 2).min(255) as u8)
    } else {
        gfx::with_alpha(0x000000, a)
    }
}
