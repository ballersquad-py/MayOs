#!/bin/sh
# Generate the sample video and song shipped on the MayOS disk (needs ffmpeg
# with libx264 and libmp3lame). Output: assets/disk/videos, assets/disk/music.
set -e
ROOT=$(cd "$(dirname "$0")/.." && pwd)
V=$ROOT/assets/disk/videos
M=$ROOT/assets/disk/music
mkdir -p "$V" "$M"
FONT=/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf
# 12 s of 720p: flowing colour gradients with a title and a timer.
ffmpeg -v error -y \
  -f lavfi -i "gradients=s=1280x720:r=30:c0=0x1b2a5e:c1=0x5a2d7a:c2=0x0b7a8f:c3=0xf07a4a:x0=0:y0=0:x1=1280:y1=720:speed=0.012:n=4" \
  -f lavfi -i "aevalsrc=0.25*sin(2*PI*(220+110*floor(mod(t\,4)))*t)*exp(-2*mod(t\,0.5))+0.15*sin(2*PI*440*t)*exp(-3*mod(t+0.25\,0.5)):s=48000:c=stereo" \
  -t 12 \
  -vf "drawtext=fontfile=$FONT:text='MayOS':fontsize=120:fontcolor=white@0.92:x=(w-text_w)/2:y=(h-text_h)/2-40:shadowcolor=black@0.35:shadowx=4:shadowy=4,drawtext=fontfile=$FONT:text='H.264 video played by the MayOS media player':fontsize=30:fontcolor=white@0.85:x=(w-text_w)/2:y=(h/2)+70,drawtext=fontfile=$FONT:text='%{pts\:hms}':fontsize=26:fontcolor=white@0.7:x=40:y=h-60" \
  -c:v libx264 -preset slow -crf 21 -profile:v high -pix_fmt yuv420p -movflags +faststart \
  -c:a aac -b:a 160k "$V/Welcome to MayOS.mp4"
# A short song: chords with a soft envelope, tagged, with cover art.
ffmpeg -v error -y \
  -f lavfi -i "aevalsrc='0.18*(sin(2*PI*261.6*t)+sin(2*PI*329.6*t)+sin(2*PI*392*t))*(0.6+0.4*exp(-3*mod(t\,2)))*if(lt(mod(t\,8)\,4)\,1\,0)+0.18*(sin(2*PI*220*t)+sin(2*PI*261.6*t)+sin(2*PI*329.6*t))*(0.6+0.4*exp(-3*mod(t\,2)))*if(lt(mod(t\,8)\,4)\,0\,1)+0.12*sin(2*PI*(523.3+130.8*floor(mod(t*2\,4)))*t)*exp(-6*mod(t\,0.5))':s=44100:c=stereo" \
  -i "$ROOT/assets/disk/pictures/Aurora.jpg" \
  -t 24 -map 0 -map 1 -c:a libmp3lame -b:a 192k -c:v mjpeg -vf scale=500:-1 -disposition:v attached_pic -id3v2_version 3 \
  -metadata title="Northern Lights" -metadata artist="MayOS Band" -metadata album="Sounds of MayOS" \
  "$M/Northern Lights.mp3"
ls -la "$V" "$M"
