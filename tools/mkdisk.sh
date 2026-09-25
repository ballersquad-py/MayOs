#!/bin/sh
# Create (if missing) the persistent FAT32 data disk and refresh /bin.
#   mkdisk.sh <image> <size MiB> <program dir> <program>...
set -e
IMG=$1; SIZE=$2; BINDIR=$3; shift 3
ROOT=$(cd "$(dirname "$0")/.." && pwd)
export MTOOLS_SKIP_CHECK=1

if [ ! -f "$IMG" ]; then
    echo "mkdisk: creating $IMG ($SIZE MiB FAT32)"
    mkdir -p "$(dirname "$IMG")"
    mkfs.fat -F 32 -s "${CLUSTER:-2}" -n MAYOS -C "$IMG" $((SIZE * 1024)) >/dev/null
    mmd -i "$IMG" ::/bin ::/docs ::/docs/notes ::/pictures ::/home ::/music ::/config
    mcopy -i "$IMG" "$ROOT/assets/disk/Welcome.txt" ::/Welcome.txt
    mcopy -i "$IMG" "$ROOT/assets/disk/docs/Getting Started.txt" "::/docs/Getting Started.txt"
    mcopy -i "$IMG" "$ROOT/assets/disk/docs/notes/todo.txt" ::/docs/notes/todo.txt
    mcopy -i "$IMG" "$ROOT/docs/ROADMAP.md" ::/docs/Roadmap.md
    if command -v python3 >/dev/null; then
        TMPWAV=$(mktemp)
        python3 "$ROOT/tools/mkwav.py" "$TMPWAV" && mcopy -i "$IMG" "$TMPWAV" "::/music/Welcome Tune.wav"
        rm -f "$TMPWAV"
    fi
fi

for p in "$@"; do
    mcopy -o -i "$IMG" "$BINDIR/$p" "::/bin/$p"
done
echo "mkdisk: $IMG ready ($(echo "$@" | wc -w) programs in /bin)"
