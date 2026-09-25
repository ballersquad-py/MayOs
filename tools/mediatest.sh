#!/bin/bash
# usage: cttest.sh file   (compares our decode with ffmpeg's)
S=${OUT:-/tmp/mediatest}
f=$1; base=$S/$(basename $f)
$(dirname "$0")/../libs/target/release/examples/playtest $f $base.v.yuv $base.a.pcm 2> $base.log || { tail -3 $base.log; exit 1; }
ffmpeg -v error -y -i $f -map 0:v:0? -fps_mode passthrough -f rawvideo -pix_fmt yuv420p $base.vref.yuv 2>/dev/null
ffmpeg -v error -y -i $f -map 0:a:0? -ac 2 -f s16le $base.aref.pcm 2>/dev/null
python3 - $base <<'PY'
import sys, struct, math, os
b = sys.argv[1]
log = open(b + '.log').read().strip().splitlines()
v = open(b + '.v.yuv','rb').read(); vr = open(b + '.vref.yuv','rb').read() if os.path.exists(b+'.vref.yuv') else b''
res = 'video identical' if v == vr else f'VIDEO DIFFERS ({len(v)} vs {len(vr)} bytes)'
if not vr: res = 'no video'
a = open(b + '.a.pcm','rb').read(); ar = open(b + '.aref.pcm','rb').read() if os.path.exists(b+'.aref.pcm') else b''
if ar and a:
    x = struct.unpack('<%dh' % (len(a)//2), a); y = struct.unpack('<%dh' % (len(ar)//2), ar)
    stereo = 2
    best = -99
    for sh in range(0, 3000*2, 2):
        n = min(len(y), len(x) - sh)
        if n <= 0: break
        sig = sum(t*t for t in y[:n:11]); err = sum((y[i]-x[i+sh])**2 for i in range(0, n, 11))
        snr = 10*math.log10(sig/max(err,1)) if sig else 0
        if snr > best: best = snr
        if snr > 60: break
    res += f', audio SNR {best:.1f} dB ({len(x)} vs {len(y)} samples)'
print(b.split('/')[-1] + ': ' + res + '   [' + ' | '.join(l.strip() for l in log) + ']')
PY
