# MayOS

A from-scratch x86_64 operating system written in Rust: its own kernel,
file system driver, window manager and apps. See
[docs/ROADMAP.md](docs/ROADMAP.md) for the full plan (graphics, media,
networking, browser).

## What works today (Phase 1)

- **Boot**: UEFI or legacy BIOS via the Limine bootloader, from one ISO.
- **Kernel**: GDT/TSS/IDT, physical and virtual memory, kernel heap,
  ACPI + APIC timer, preemptive scheduler, `syscall`-based user programs
  loaded from ELF files, shutdown/reboot.
- **Drivers**: PCI, virtio block / GPU / tablet, PS/2 keyboard and mouse
  (with wheel), boot framebuffer fallback, CMOS clock.
- **File system**: FAT32 read/write with long file names, tested against
  `mkfs.fat`, `fsck.fat` and mtools. Uses the virtio disk (persistent)
  when present, otherwise a RAM disk built into the ISO.
- **Desktop**: antialiased rounded windows with soft shadows, a dock,
  top bar with clock and system menu, a hardware cursor on virtio-gpu,
  drag, resize, minimise and maximise.
- **Apps**: file explorer (browse, open, new file/folder, rename,
  duplicate, copy/cut/paste, delete), terminal with a shell (~30
  commands), text editor (selection, clipboard, save / save as), and an
  About window.
- **User programs** in `/bin`: `hello`, `count`, `cat`, `write`, `ls`,
  `guess`, `sysinfo`.

## Running it on Windows

Use `mayos.iso` with any of these:

**VirtualBox**: New VM → Type *Other*, Version *Other/Unknown (64-bit)*,
1024 MB of RAM. Under *Storage*, attach `mayos.iso` to the optical drive.
Under *System → Motherboard*, set **Pointing Device: PS/2 Mouse**. EFI can
be on or off. Start the VM, then click inside it to capture the mouse
(the right Ctrl key releases it).

**VMware Workstation Player**: Create a VM → "I will install the operating
system later" → *Other 64-bit*, then point the CD drive at `mayos.iso`.

**QEMU for Windows** (the best experience, with a persistent disk):
build the data disk on Linux or WSL (`make disk`), then run
```
qemu-system-x86_64 -M q35 -m 512M -vga none -device virtio-gpu-pci -device virtio-tablet-pci ^
  -drive file=disk.img,if=none,id=d0,format=raw -device virtio-blk-pci,drive=d0 -cdrom mayos.iso
```

**Real PC**: write the ISO to a USB stick with
[Rufus](https://rufus.ie) (DD mode) and boot from it. Keyboard and mouse
must be PS/2 or emulated by the firmware ("USB legacy support"); native
USB drivers are future work. Hyper-V is not supported (it has no PS/2
devices).

Without a virtio disk, files live in a RAM disk: every change works but is
lost at power off.

## Building (Linux or WSL)

Requirements: Rust (stable) with `rustup target add x86_64-unknown-none`,
`qemu-system-x86`, `ovmf`, `xorriso`, `mtools`, `dosfstools`, `git`, `make`.

```
make run          # build everything and boot in QEMU (window)
make iso          # build/mayos.iso only
make test         # host tests + boot-time self-test + fsck of the disk
make disk-reset   # recreate the persistent data disk
```

`tools/mkfont.py` regenerates the font atlases in `assets/fonts` (needs Pillow).

## Layout

```
kernel/      kernel: boot, memory, scheduler, syscalls, drivers, desktop
libs/fat32   FAT32 driver (no_std, host-tested)
libs/gfx     2D graphics: AA shapes, shadows, text, icons (no_std, host-tested)
userspace/   mstd (user standard library) and programs in src/bin
tools/       disk builder, font baker, self-test runner
assets/      fonts and the starter files copied onto the disk
```

Fonts are pre-rendered from DejaVu (see `assets/fonts/DEJAVU-LICENSE.txt`).
