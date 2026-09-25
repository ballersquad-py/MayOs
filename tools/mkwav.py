#!/usr/bin/env python3
"""Generate a short sample tune as a 16-bit stereo WAV (for /music)."""
import math, struct, sys, wave

RATE = 44100
NOTES = [(523, 0.0), (659, 0.25), (784, 0.5), (1047, 0.75), (784, 1.0), (880, 1.25), (1047, 1.5)]
LENGTH = 2.6

frames = bytearray()
for i in range(int(RATE * LENGTH)):
    t = i / RATE
    l = r = 0.0
    for k, (f, start) in enumerate(NOTES):
        dt = t - start
        if 0 <= dt < 1.1:
            env = min(1.0, dt * 200) * math.exp(-dt * 3.5)
            s = math.sin(2 * math.pi * f * dt) * 0.7 + math.sin(4 * math.pi * f * dt) * 0.2
            pan = (k / (len(NOTES) - 1)) * 2 - 1
            l += s * env * 0.25 * (1 - max(0, pan))
            r += s * env * 0.25 * (1 + min(0, pan))
    frames += struct.pack("<hh", int(max(-1, min(1, l)) * 32767), int(max(-1, min(1, r)) * 32767))

with wave.open(sys.argv[1], "wb") as w:
    w.setnchannels(2)
    w.setsampwidth(2)
    w.setframerate(RATE)
    w.writeframes(bytes(frames))
