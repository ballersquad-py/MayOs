#!/usr/bin/env python3
"""Generate the sample pictures shipped in /pictures (needs Pillow).

usage: mkpictures.py <out dir>
"""
import math, os, random, sys
from PIL import Image, ImageChops, ImageDraw, ImageFilter

W, H = 1920, 1080


def lerp(a, b, t):
    return tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(3))


def gradient(top, bottom):
    im = Image.new("RGB", (W, H))
    d = ImageDraw.Draw(im)
    for y in range(H):
        d.line([(0, y), (W, y)], fill=lerp(top, bottom, y / (H - 1)))
    return im


def ridge(rng, base, amp, rough):
    pts = []
    phase = [rng.random() * 6.28 for _ in range(4)]
    for x in range(0, W + 8, 8):
        y = base
        for k, f in enumerate((1.3, 2.9, 6.1, 13.0)):
            y += math.sin(x / W * f * 6.28 + phase[k]) * amp / (1 + k * rough)
        pts.append((x, y))
    return pts + [(W, H), (0, H)]


def mountains(path):
    rng = random.Random(7)
    im = gradient((255, 170, 120), (120, 90, 170))
    glow = Image.new("L", (W, H), 0)
    ImageDraw.Draw(glow).ellipse([W * 0.62 - 120, 330, W * 0.62 + 120, 570], fill=255)
    glow = glow.filter(ImageFilter.GaussianBlur(60))
    im.paste((255, 236, 200), mask=glow)
    ImageDraw.Draw(im).ellipse([W * 0.62 - 80, 370, W * 0.62 + 80, 530], fill=(255, 244, 222))
    layers = [((150, 95, 150), 560, 120), ((105, 70, 125), 660, 110), ((70, 48, 95), 760, 90), ((38, 28, 60), 880, 70)]
    for col, base, amp in layers:
        layer = Image.new("RGBA", (W, H), (0, 0, 0, 0))
        ImageDraw.Draw(layer).polygon(ridge(rng, base, amp, 1.4), fill=col + (255,))
        im.paste(layer, mask=layer)
    im.save(path, quality=90)


def aurora(path):
    rng = random.Random(3)
    im = gradient((6, 12, 30), (12, 40, 60))
    d = ImageDraw.Draw(im)
    for _ in range(500):
        x, y = rng.randrange(W), rng.randrange(int(H * 0.7))
        b = rng.randrange(120, 255)
        d.point((x, y), fill=(b, b, b))
    band = Image.new("RGB", (W, H), (0, 0, 0))
    bd = ImageDraw.Draw(band)
    for i, col in enumerate([(60, 255, 170), (80, 200, 255), (170, 110, 255)]):
        for x in range(0, W, 4):
            y = 300 + i * 70 + math.sin(x / 260 + i) * 90 + math.sin(x / 90) * 20
            bd.line([(x, y - 140), (x, y + 30)], fill=col, width=4)
    band = band.filter(ImageFilter.GaussianBlur(35))
    im = ImageChops.add(im, Image.eval(band, lambda v: int(v * 0.8)))
    d = ImageDraw.Draw(im)
    d.polygon(ridge(rng, 960, 40, 1.0), fill=(4, 8, 14))
    im.save(path, quality=90)


def shapes(path):
    im = Image.new("RGBA", (800, 500), (0, 0, 0, 0))
    d = ImageDraw.Draw(im)
    d.rounded_rectangle([20, 20, 780, 480], 48, fill=(250, 250, 252, 255))
    d.ellipse([90, 110, 370, 390], fill=(255, 94, 138, 255))
    d.rounded_rectangle([300, 160, 560, 420], 36, fill=(73, 198, 229, 220))
    d.polygon([(560, 90), (720, 380), (400, 380)], fill=(255, 200, 87, 210))
    im.save(path, optimize=True)


def main():
    out = sys.argv[1]
    os.makedirs(out, exist_ok=True)
    mountains(os.path.join(out, "Mountains.jpg"))
    aurora(os.path.join(out, "Aurora.jpg"))
    shapes(os.path.join(out, "Shapes.png"))


if __name__ == "__main__":
    main()
