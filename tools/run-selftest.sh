#!/bin/sh
# Boot MayOS in self-test mode on a scratch copy of the data disk, then
# verify the disk with fsck.fat.
#   run-selftest.sh <qemu> <ovmf> <iso> <disk>
QEMU=$1; OVMF=$2; ISO=$3; DISK=$4
DIR=$(dirname "$DISK")
SCRATCH="$DIR/selftest-disk.img"
LOG="$DIR/selftest-serial.log"
cp "$DISK" "$SCRATCH"
timeout 300 $QEMU -M q35 -m 512M -cpu max -smp 1 -accel kvm -accel tcg -bios "$OVMF" \
    -vga none -device virtio-gpu-pci -device virtio-tablet-pci \
    -drive file="$SCRATCH",if=none,id=disk0,format=raw -device virtio-blk-pci,drive=disk0 \
    -netdev user,id=net0 -device e1000,netdev=net0 \
    -audiodev none,id=snd0 -device AC97,audiodev=snd0 \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
    -cdrom "$ISO" -display none -serial file:"$LOG" -no-reboot 2>/dev/null
CODE=$?
grep -a "selftest\|PANIC\|panicked" "$LOG" | tr -d '\r'
# isa-debug-exit: 0x10 -> 33 (pass), 0x11 -> 35 (fail)
if [ "$CODE" -ne 33 ]; then
    echo "run-selftest: FAILED (qemu exit code $CODE, log in $LOG)"
    exit 1
fi
if ! fsck.fat -n "$SCRATCH" >"$DIR/selftest-fsck.log" 2>&1; then
    cat "$DIR/selftest-fsck.log"
    echo "run-selftest: FAILED (fsck.fat found problems on the disk)"
    exit 1
fi
if grep -q -E "FATs differ|Reclaimed|Free cluster summary wrong|orphan" "$DIR/selftest-fsck.log"; then
    cat "$DIR/selftest-fsck.log"
    echo "run-selftest: FAILED (fsck.fat warnings)"
    exit 1
fi
echo "run-selftest: PASSED (disk verified by fsck.fat)"
