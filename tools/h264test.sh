#!/bin/bash
# usage: h264test.sh name "x264 options" [extra ffmpeg args]
S=${OUT:-/tmp/h264test}
mkdir -p $S
name=$1; opts=$2; size=${3:-352x288}; frames=${4:-30}
src="testsrc2=size=$size:rate=25,noise=alls=20:allf=t"
ffmpeg -v error -y -f lavfi -i "$src" -frames:v $frames -pix_fmt yuv420p -c:v libx264 $opts $S/$name.264 || exit 1
ffmpeg -v error -y -i $S/$name.264 -f rawvideo -pix_fmt yuv420p $S/$name.ref.yuv
$(dirname "$0")/../libs/target/release/examples/h264dec $S/$name.264 $S/$name.out.yuv 2> $S/$name.log
python3 - "$S/$name" "$size" <<'PY'
import sys
base, size = sys.argv[1], sys.argv[2]
w, h = map(int, size.split('x'))
a = open(base + '.ref.yuv', 'rb').read(); b = open(base + '.out.yuv', 'rb').read()
fs = w * h * 3 // 2
log = open(base + '.log').read().strip()
if a == b:
    print(f"PASS {base.split('/')[-1]}: {len(a)//fs} frames identical  [{log}]")
else:
    n = min(len(a), len(b)) // fs
    bad = None
    for i in range(n):
        if a[i*fs:(i+1)*fs] != b[i*fs:(i+1)*fs]:
            bad = i; break
    msg = f"FAIL {base.split('/')[-1]}: ref {len(a)//fs} frames, ours {len(b)//fs}; first differing frame {bad}"
    if bad is not None:
        fa, fb = a[bad*fs:(bad+1)*fs], b[bad*fs:(bad+1)*fs]
        for k in range(fs):
            if fa[k] != fb[k]:
                plane = 'Y' if k < w*h else ('U' if k < w*h*5//4 else 'V')
                if plane == 'Y': x, y = k % w, k // w
                else:
                    kk = (k - w*h) % (w*h//4); x, y = (kk % (w//2))*2, (kk // (w//2))*2
                msg += f"; first diff {plane} at luma ({x},{y}) mb ({x//16},{y//16})"
                break
    print(msg + f"  [{log}]")
PY
