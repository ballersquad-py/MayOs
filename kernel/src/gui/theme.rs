//! Colours, metrics and fonts shared by the desktop and its apps.

use gfx::{rgb, rgba, Color, Font};

use crate::sync::Once;

pub const ACCENT: Color = rgb(0x2f, 0x7c, 0xf6);
pub const ACCENT_DARK: Color = rgb(0x1f, 0x63, 0xd6);
pub const SELECTION: Color = rgb(0xd6, 0xe6, 0xff);
pub const TEXT: Color = rgb(0x1d, 0x1f, 0x24);
pub const TEXT_DIM: Color = rgb(0x6b, 0x71, 0x7e);
pub const TEXT_ON_ACCENT: Color = rgb(0xff, 0xff, 0xff);
pub const WINDOW_BG: Color = rgb(0xff, 0xff, 0xff);
pub const PANEL_BG: Color = rgb(0xf5, 0xf6, 0xf8);
pub const SIDEBAR_BG: Color = rgb(0xee, 0xf0, 0xf4);
pub const SEPARATOR: Color = rgb(0xe1, 0xe4, 0xe9);
pub const HOVER: Color = rgb(0xec, 0xf2, 0xfe);
pub const DANGER: Color = rgb(0xe5, 0x48, 0x4d);
pub const TITLEBAR: Color = rgb(0xf4, 0xf5, 0xf7);
pub const TITLEBAR_INACTIVE: Color = rgb(0xea, 0xeb, 0xee);
pub const BORDER: Color = rgba(0, 0, 0, 38);
pub const SHADOW: Color = rgba(8, 14, 30, 110);
pub const SHADOW_INACTIVE: Color = rgba(8, 14, 30, 60);

pub const TITLEBAR_H: i32 = 36;
pub const WINDOW_RADIUS: i32 = 12;
pub const SHADOW_BLUR: i32 = 26;
pub const SHADOW_OFFSET: i32 = 8;
pub const TOPBAR_H: i32 = 28;
pub const DOCK_ICON: i32 = 48;
pub const DOCK_PAD: i32 = 10;

pub struct Fonts {
    pub ui: Font,
    pub bold: Font,
    pub mono: Font,
    pub large: Font,
    pub small_bold: Font,
}

static FONTS: Once<Fonts> = Once::new();

pub fn init() {
    FONTS.set(Fonts {
        ui: Font::parse(include_bytes!("../../../assets/fonts/sans-13.mfnt")).expect("sans font"),
        bold: Font::parse(include_bytes!("../../../assets/fonts/sans-bold-13.mfnt")).expect("bold font"),
        mono: Font::parse(include_bytes!("../../../assets/fonts/mono-13.mfnt")).expect("mono font"),
        large: Font::parse(include_bytes!("../../../assets/fonts/sans-24.mfnt")).expect("large font"),
        small_bold: Font::parse(include_bytes!("../../../assets/fonts/sans-bold-11.mfnt")).expect("small font"),
    });
}

pub fn fonts() -> &'static Fonts {
    FONTS.get().expect("fonts not initialised")
}

pub fn try_fonts() -> Option<&'static Fonts> {
    FONTS.get()
}
