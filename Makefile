# MayOS build. `make run` builds everything and boots it in QEMU.

BUILD      := build
LIMINE     := $(BUILD)/limine
KERNEL     := kernel/target/x86_64-unknown-none/release/kernel
ISO        := $(BUILD)/mayos.iso
TEST_ISO   := $(BUILD)/mayos-test.iso
DISK       := $(BUILD)/disk.img
DISK_MB    := 128
RAMDISK    := $(BUILD)/ramdisk.img
OVMF       ?= $(firstword $(wildcard /usr/share/ovmf/OVMF.fd /usr/share/qemu/OVMF.fd /usr/share/OVMF/OVMF_CODE.fd /opt/homebrew/share/qemu/edk2-x86_64-code.fd))
USER_BINS  := hello count cat write ls guess sysinfo
USER_DIR   := userspace/target/x86_64-unknown-none/release
# Linux demo programs (examples/), included when Rust's musl target is
# installed (`rustup target add x86_64-unknown-linux-musl`).
LINUX_BINS := $(shell rustup target list --installed 2>/dev/null | grep -q x86_64-unknown-linux-musl && echo linux-hello linux-paint)
USER_BINS  += $(LINUX_BINS)
# C test of the Linux layer (signals, timerfd, ...), built when musl-gcc is installed.
LINUX_C    := $(shell command -v musl-gcc >/dev/null 2>&1 && echo linux-signals)
USER_BINS  += $(LINUX_C)

QEMU       ?= qemu-system-x86_64
# Sound backend for QEMU: pa (PulseAudio), pipewire, alsa, sdl, dsound
# (Windows), coreaudio (macOS) or none.
AUDIO      ?= pa
QEMU_BASE  := -M q35 -m 512M -cpu max -smp 1 \
              -accel kvm -accel tcg \
              -bios $(OVMF) \
              -vga none -device virtio-gpu-pci \
              -device virtio-tablet-pci \
              -drive file=$(DISK),if=none,id=disk0,format=raw -device virtio-blk-pci,drive=disk0 \
              -netdev user,id=net0,hostfwd=tcp::8080-:80 -device e1000,netdev=net0 \
              -audiodev $(AUDIO),id=snd0 -device AC97,audiodev=snd0 \
              -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
              -no-reboot

.PHONY: all kernel userspace ramdisk iso disk disk-reset run run-headless test test-libs clean

all: iso disk

$(LIMINE)/limine:
	mkdir -p $(BUILD)
	git clone --depth=1 --branch=v9.x-binary https://github.com/limine-bootloader/limine.git $(LIMINE)
	$(MAKE) -C $(LIMINE)

kernel:
	cd kernel && cargo build --release

userspace:
	cd userspace && cargo build --release
	for b in $(LINUX_BINS); do \
		(cd examples/$$b && cargo build --release --target x86_64-unknown-linux-musl) && \
		cp examples/$$b/target/x86_64-unknown-linux-musl/release/$$b $(USER_DIR)/ || exit 1; \
	done
	for b in $(LINUX_C); do \
		musl-gcc -static -O2 -o $(USER_DIR)/$$b examples/$$b/signals.c -lpthread || exit 1; \
	done

define make_iso
	rm -rf $(BUILD)/iso_root
	mkdir -p $(BUILD)/iso_root/boot/limine $(BUILD)/iso_root/EFI/BOOT
	cp $(KERNEL) $(BUILD)/iso_root/boot/kernel
	cp $(RAMDISK) $(BUILD)/iso_root/boot/disk.img
	cp $(1) $(BUILD)/iso_root/boot/limine/limine.conf
	cp $(LIMINE)/limine-bios.sys $(LIMINE)/limine-bios-cd.bin $(LIMINE)/limine-uefi-cd.bin $(BUILD)/iso_root/boot/limine/
	cp $(LIMINE)/BOOTX64.EFI $(BUILD)/iso_root/EFI/BOOT/
	xorriso -as mkisofs -R -r -J -b boot/limine/limine-bios-cd.bin \
		-no-emul-boot -boot-load-size 4 -boot-info-table -hfsplus \
		-apm-block-size 2048 --efi-boot boot/limine/limine-uefi-cd.bin \
		-efi-boot-part --efi-boot-image --protective-msdos-label \
		$(BUILD)/iso_root -o $(2) 2>/dev/null
	$(LIMINE)/limine bios-install $(2) 2>/dev/null
endef

# The ISO carries its own FAT32 image as a RAM disk, so it works on
# machines without a virtio disk (VirtualBox, VMware, real PCs).
ramdisk: userspace
	rm -f $(RAMDISK)
	CLUSTER=1 ./tools/mkdisk.sh $(RAMDISK) 40 $(USER_DIR) $(USER_BINS)

iso: $(LIMINE)/limine kernel ramdisk
	$(call make_iso,limine.conf,$(ISO))

$(TEST_ISO): $(LIMINE)/limine kernel ramdisk
	$(call make_iso,tools/limine-test.conf,$(TEST_ISO))

# The data disk persists between runs; only the programs in /bin are refreshed.
disk: userspace
	./tools/mkdisk.sh $(DISK) $(DISK_MB) $(USER_DIR) $(USER_BINS)

disk-reset:
	rm -f $(DISK)
	$(MAKE) disk

run: iso disk
	$(QEMU) $(QEMU_BASE) -cdrom $(ISO) -serial stdio

run-headless: iso disk
	$(QEMU) $(QEMU_BASE) -cdrom $(ISO) -serial stdio -display none

# Boots a kernel that runs its self-tests against a scratch copy of the disk
# and powers off with a pass/fail exit code.
test: $(TEST_ISO) disk test-libs
	./tools/run-selftest.sh "$(QEMU)" "$(OVMF)" $(TEST_ISO) $(DISK)

test-libs:
	cd libs && cargo test --quiet

clean:
	cd kernel && cargo clean
	cd userspace && cargo clean
	cd libs && cargo clean
	rm -rf $(BUILD)/iso_root $(ISO) $(TEST_ISO)
