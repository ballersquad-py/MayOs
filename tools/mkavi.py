#!/usr/bin/env python3
"""Write a short Motion-JPEG AVI with PCM audio (sample video for MayOS).

usage: mkavi.py out.avi [seconds] [width] [height] [fps]
"""
import io, math, struct, sys
from PIL import Image, ImageDraw


def chunk(fourcc, data):
    pad = b"\0" if len(data) % 2 else b""
    return fourcc + struct.pack("<I", len(data)) + data + pad


def lst(kind, payload):
    return b"LIST" + struct.pack("<I", len(payload) + 4) + kind + payload


def main():
    out = sys.argv[1]
    secs = float(sys.argv[2]) if len(sys.argv) > 2 else 4
    w = int(sys.argv[3]) if len(sys.argv) > 3 else 320
    h = int(sys.argv[4]) if len(sys.argv) > 4 else 180
    fps = int(sys.argv[5]) if len(sys.argv) > 5 else 15
    n = int(secs * fps)
    frames = []
    for i in range(n):
        t = i / fps
        im = Image.new("RGB", (w, h))
        d = ImageDraw.Draw(im)
        for y in range(0, h, 4):
            c = int(40 + 60 * y / h)
            d.rectangle((0, y, w, y + 4), fill=(c // 2, c // 3, c))
        cx = w / 2 + math.cos(t * 2) * w / 3
        cy = h / 2 + math.sin(t * 3) * h / 4
        d.ellipse((cx - 22, cy - 22, cx + 22, cy + 22), fill=(250, 190, 60))
        d.text((10, 10), "MayOS video  %.1f s" % t, fill=(255, 255, 255))
        buf = io.BytesIO()
        im.save(buf, "JPEG", quality=80)
        frames.append(buf.getvalue())
    rate = 22050
    audio = bytearray()
    for i in range(int(secs * rate)):
        t = i / rate
        note = 440 * 2 ** (int(t * 2) % 5 / 12)
        env = 0.5 * (1 - (t * 2) % 1)
        v = int(8000 * env * math.sin(2 * math.pi * note * t))
        audio += struct.pack("<hh", v, v)

    avih = struct.pack("<IIIIIIIIII", 1000000 // fps, 0, 0, 0x10, n, 0, 2, 0, w, h) + b"\0" * 16
    vstrh = b"vids" + b"MJPG" + struct.pack("<IHHIIIIIIII", 0, 0, 0, 0, 1, fps, 0, n, 0, 0xFFFFFFFF, 0) + struct.pack("<hhhh", 0, 0, w, h)
    vstrf = struct.pack("<IiiHH4sIiiII", 40, w, h, 1, 24, b"MJPG", w * h * 3, 0, 0, 0, 0)
    astrh = b"auds" + b"\0\0\0\0" + struct.pack("<IHHIIIIIIII", 0, 0, 0, 0, 1, rate, 0, len(audio) // 4, 0, 0xFFFFFFFF, 4) + struct.pack("<hhhh", 0, 0, 0, 0)
    astrf = struct.pack("<HHIIHH", 1, 2, rate, rate * 4, 4, 16)
    hdrl = lst(b"hdrl", chunk(b"avih", avih) + lst(b"strl", chunk(b"strh", vstrh) + chunk(b"strf", vstrf))
               + lst(b"strl", chunk(b"strh", astrh) + chunk(b"strf", astrf)))
    movi = bytearray()
    per_frame = len(audio) // n // 4 * 4
    for i, f in enumerate(frames):
        movi += chunk(b"00dc", f)
        movi += chunk(b"01wb", bytes(audio[i * per_frame:(i + 1) * per_frame]))
    body = b"AVI " + hdrl + lst(b"movi", bytes(movi))
    with open(out, "wb") as fh:
        fh.write(b"RIFF" + struct.pack("<I", len(body)) + body)


if __name__ == "__main__":
    main()
