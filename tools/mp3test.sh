#!/bin/bash
# usage: aactest.sh name "ffmpeg encode args" [source]
S=${OUT:-/tmp/mediatest}; mkdir -p $S
name=$1; enc=$2; src=${3:-"aevalsrc=0.4*sin(2*PI*440*t)+0.2*sin(2*PI*3000*t)*sin(2*PI*0.5*t)|0.3*sin(2*PI*660*t)+0.1*random(0):s=44100:d=5"}
ffmpeg -v error -y -f lavfi -i "$src" $enc -f mp3 $S/$name.mp3 || exit 1
ffmpeg -v error -y -i $S/$name.mp3 -f s16le $S/$name.ref.pcm
CH=$(ffprobe -v error -show_entries stream=channels -of csv=p=0 $S/$name.mp3)
$(dirname "$0")/../libs/target/release/examples/mp3dec $S/$name.mp3 $S/$name.out.pcm 2> $S/$name.log
python3 - $S/$name $CH <<'PY'
import sys, struct, math
b, ch = sys.argv[1], int(sys.argv[2])
ref = open(b+'.ref.pcm','rb').read(); out = open(b+'.out.pcm','rb').read()
r = struct.unpack('<%dh' % (len(ref)//2), ref); o = struct.unpack('<%dh' % (len(out)//2), out)
outch = 1 if ch == 1 else 2
if outch != ch: print("channel mismatch", ch); 
n = min(len(r), len(o))
best = None
for shift in list(range(0, 2049*outch, outch)) + [2257*outch, 1105*outch, 1152*outch, 529*outch]:
    m = min(len(r), len(o) - shift)
    if m <= 0: break
    sig = sum(x*x for x in r[:m:7]); err = sum((r[i]-o[i+shift])**2 for i in range(0, m, 7))
    snr = 10*math.log10(sig/max(err,1e-9)) if sig else 0
    if best is None or snr > best[0]: best = (snr, shift//outch)
    if snr > 60: break
log = open(b+'.log').read().strip().splitlines()[-1]
print(f"{b.split('/')[-1]}: SNR {best[0]:.1f} dB at offset {best[1]}, ref {len(r)//outch} out {len(o)//outch} samples  [{log}]")
PY
