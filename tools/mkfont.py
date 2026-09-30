#!/usr/bin/env python3
"""Pre-render TrueType fonts into MayOS glyph atlases (.mfnt).

This is a stop-gap until MayOS has its own TrueType rasterizer (Phase 2):
it bakes antialiased 8-bit coverage bitmaps at fixed pixel sizes.

Format (little endian):
  header: b"MFNT", u16 version=1, u16 glyph_count,
          i16 ascent, i16 descent, i16 line_height, u16 reserved
  glyph:  u32 codepoint, u16 advance (1/16 px), i16 bearing_x,
          i16 bearing_top (pixels above the baseline), u16 w, u16 h,
          u32 bitmap_offset (from the start of the bitmap block)
  bitmaps: w*h bytes of coverage per glyph, row-major
"""
import struct
import sys
from PIL import Image, ImageDraw, ImageFont

CHARS = (
    list(range(0x20, 0x7F))
    + list(range(0xA0, 0x100))
    + [0x2013, 0x2014, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2026,
       0x2190, 0x2191, 0x2192, 0x2193, 0x2713, 0x2715, 0x25B2, 0x25BC, 0x25B6, 0x25C0, 0x203A,
       0x2500, 0x2502, 0x250C, 0x2510, 0x2514, 0x2518, 0x251C, 0x2524, 0x252C, 0x2534, 0x253C, 0x2588]
)


SS = 4  # supersampling: glyphs are drawn unhinted at 4x and averaged down


def render(font, size, ch):
    """Return (bearing_x, bearing_top, w, h, coverage bytes) for one glyph.

    `font` is the 4x font: outlines unhinted (no snapping, so spacing and
    x-height stay true to the design), box-filtered down to `size`."""
    pad = size * 2 * SS
    img = Image.new("L", (pad * 3, pad * 3), 0)
    ox, oy = pad, pad * 2
    ImageDraw.Draw(img).text((ox, oy), ch, font=font, fill=255, anchor="ls")
    bbox = img.getbbox()
    if bbox is None:
        return (0, 0, 0, 0, b"")
    l, t, r, b = bbox
    # Snap the box to whole output pixels around the origin.
    l = ox + ((l - ox) // SS) * SS
    t = oy + ((t - oy) // SS) * SS
    r = ox + -(-(r - ox) // SS) * SS
    b = oy + -(-(b - oy) // SS) * SS
    small = img.crop((l, t, r, b)).resize(((r - l) // SS, (b - t) // SS), Image.BOX)
    return ((l - ox) // SS, (oy - t) // SS, small.width, small.height, small.tobytes())


def load(ttf, size, weight):
    font = ImageFont.truetype(ttf, size * SS, layout_engine=ImageFont.Layout.BASIC)
    if weight:
        try:
            axes = font.get_variation_axes()
            font.set_variation_by_axes([weight if a.get("name", b"") in (b"Weight", "Weight") else a["default"] for a in axes])
        except Exception:
            pass
    return font


def build(ttf, size, out, weight=None):
    font = load(ttf, size, weight)
    ascent, descent = font.getmetrics()
    ascent, descent = -(-ascent // SS), -(-descent // SS)
    notdef = render(font, size, "\uffff")
    glyphs = []
    bitmaps = bytearray()
    for cp in CHARS:
        ch = chr(cp)
        # Skip characters the font lacks (they would render as .notdef).
        if cp > 0x7F and cp != 0xA0 and render(font, size, ch) == notdef:
            continue
        advance = font.getlength(ch) / SS
        bx, by, w, h, data = render(font, size, ch)
        glyphs.append((cp, round(advance * 16), bx, by, w, h, len(bitmaps)))
        bitmaps += data
    with open(out, "wb") as f:
        f.write(b"MFNT")
        f.write(struct.pack("<HHhhhH", 1, len(glyphs), ascent, descent, ascent + descent + 2, 0))
        for g in glyphs:
            f.write(struct.pack("<IHhhHHI", *g))
        f.write(bitmaps)
    print(f"{out}: {len(glyphs)} glyphs, {len(bitmaps)} bytes of bitmaps")


if __name__ == "__main__":
    if len(sys.argv) not in (4, 5):
        sys.exit("usage: mkfont.py <font.ttf> <pixel size> <out.mfnt> [weight]")
    build(sys.argv[1], int(sys.argv[2]), sys.argv[3], int(sys.argv[4]) if len(sys.argv) == 5 else None)
