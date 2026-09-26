# MayOS

A from-scratch x86_64 operating system written in Rust: its own kernel,
file system driver, window manager and apps. See
[docs/ROADMAP.md](docs/ROADMAP.md) for the full plan (graphics, media,
networking, browser).

## What works today

Phase 1 (kernel, file system, desktop) is complete. From the later phases,
networking with TCP, audio, media playback (H.264/AAC/MP3) and the
Settings app are done.

- **Boot**: UEFI or legacy BIOS via the Limine bootloader, from one ISO.
- **Kernel**: GDT/TSS/IDT, physical and virtual memory, kernel heap,
  ACPI + APIC timer, preemptive scheduler, `syscall`-based user programs
  loaded from ELF files, shutdown/reboot.
- **Drivers**: PCI, virtio block / GPU / tablet, VMware SVGA II
  (VirtualBox VMSVGA/VBoxSVGA), Bochs VBE (VirtualBox VBoxVGA, QEMU std VGA),
  PS/2 keyboard and mouse (with wheel), boot framebuffer fallback, CMOS clock, Intel e1000-family
  network adapters, Intel AC'97 audio, and the VirtualBox guest device
  (the desktop follows the window size; the mouse moves in and out freely).
- **Networking**: Ethernet, ARP, IPv4, ICMP, UDP, TCP, DHCP client and DNS
  resolver (`ping`, `nslookup`, `ifconfig`, `dhcp` in the terminal).
- **Web browser**: HTTP and HTTPS (TLS 1.3 via the `embedded-tls` crate),
  HTML and CSS (engine in `libs/web`, built from Matt Brubeck's
  [robinson](https://github.com/mbrubeck/robinson), MIT) with text layout,
  links, pictures, lists, tables, rounded corners and web fonts
  (`@font-face`, TrueType via `ab_glyph`); JavaScript through
  [QuickJS](https://bellard.org/quickjs/) (MIT) with a DOM, events, timers,
  `<canvas>` 2D, cookies and `localStorage`. Try `/docs/snake.html`.
  Certificates are not checked yet; `fetch`/XHR, floats, grid and WOFF2
  fonts are still missing.
- **Linux programs**: MayOS speaks the Linux system-call ABI, so
  statically linked Linux x86_64 programs run unchanged: Rust programs
  built with `--target x86_64-unknown-linux-musl` (full `std`: files,
  threads, networking, time) or C programs built with `musl-gcc -static`.
  Supported: files and directories, `mmap`/`brk` with memory on demand,
  threads (`clone`, `futex`, TLS), TCP/UDP sockets and DNS, pipes, `poll`.
  Graphics use the Linux framebuffer: a program that opens `/dev/fb0`
  gets a window, maps its pixels with `mmap` and reads the keyboard and
  mouse from `/dev/input/event0`, `event1` (evdev) or `/dev/input/mice`.
  Try `linux-paint` in the Terminal. Processes work too: `fork`,
  `vfork`/`posix_spawn`, `execve` (including `#!` scripts), `wait4` and
  pipes, so shells run pipelines. Dynamically linked programs load their
  dynamic linker: copy e.g. Alpine Linux's `/lib/ld-musl-x86_64.so.1`
  (also as `/lib/libc.musl-x86_64.so.1`) and the libraries a program
  needs into `/lib`. Alpine's BusyBox runs as is. Signals work: handlers
  (`sigaction` with `SA_SIGINFO`, `SA_ONSTACK`, `sigaltstack`), masks,
  `kill`/`tgkill`, `SIGCHLD`, `SIGSEGV` handlers for faults and guard
  pages, `sigsuspend`/`sigtimedwait`, and `EINTR` from blocking calls.
  Also `timerfd`, `select`/`pselect6`, `pwrite`/`preadv`, `mremap`,
  symbolic links (stored as MayOS link files), `close_range`,
  `sendmmsg` and `/proc/self/{maps,status,stat,fd,...}`. Mapped files are
  read on first touch, so large libraries only cost what they use. `linuxtrace on` logs every
  Linux system call to the serial port. See `examples/linux-hello` and
  `examples/linux-paint`.
- **Linux software from Alpine Linux**: `pkg install <name>` in the
  Terminal downloads Alpine packages (with their dependencies), unpacks
  them and runs their setup steps; `pkg search`, `pkg list`. For
  example `pkg install netsurf` then `netsurf` gives you the NetSurf web
  browser (GTK 3). `pkg install firefox` then `firefox` runs Mozilla
  Firefox (software WebRender, no sandbox; the first start takes a
  while). A D-Bus session bus (Alpine's `dbus-daemon`) starts on its own
  for Linux programs. Give the VM 2 GB of RAM or more and about 1 GB of
  free disk. Firefox's `libEGL missing` / `No GPUs detected via PCI`
  messages are expected: there is no GPU driver for Linux programs, so it
  draws in software. Programs with windows use MayOS's built-in **Wayland
  compositor** (`wl_shm`, `xdg_shell`, keyboard and pointer), so GTK and
  SDL programs appear as normal MayOS windows. Needs about 150 MB of
  disk for NetSurf.
- **File sharing**: a built-in web server. Open it in a browser on your
  PC to drag files onto MayOS, download files, and create, rename or
  delete them (`share` in the terminal, or Settings → Network).
- **Audio**: a software mixer, synthesised system sounds (startup chime,
  alerts) and WAV playback (`play`, or double-click a `.wav` file).
- **File system**: FAT32 read/write with long file names, tested against
  `mkfs.fat`, `fsck.fat` and mtools, on virtio, SATA (AHCI) and IDE disks
  with MBR or GPT partitions. Extra disks mount as `/disk1`, `/disk2`, …;
  any disk can be set up as the main MayOS disk from Settings → Storage.
- **Pictures and video**: our own PNG (all colour types, interlaced),
  JPEG (baseline and progressive) and BMP decoders, an image viewer with
  zoom and pan, and picture wallpapers. Files shows previews of pictures,
  videos and album covers.
- **Media player**: our own H.264 video decoder (CAVLC and CABAC, B-frames,
  up to High profile), AAC-LC and MP3 audio decoders, and MP4 / MOV / M4A,
  MKV / WebM, AVI, MP3, AAC and WAV containers. It has seeking, volume,
  fullscreen, a playlist of the folder, and a music view with cover art,
  tags and a visualiser.
- **Desktop**: antialiased rounded windows with soft shadows, a dock
  (bottom, left or right, three sizes, optional auto-hide),
  top bar with clock, network and volume indicators, a system menu, a
  hardware cursor on virtio-gpu, drag, resize, minimise and maximise.
- **Animations**: windows grow in from the dock, fade and scale when
  opening and closing, fly into the dock when minimised and glide when
  maximised; dock icons bounce on launch; menus, toggles and settings pages
  animate; the desktop fades in at boot. Can be turned off in Settings.
- **Settings app**: resolution (virtio-gpu, VMware SVGA, Bochs VBE), six wallpapers or your own pictures, eight accent
  colours, animations; volume, mute, system sounds, test sound; network
  status, DHCP or static IP, ping and DNS tests; pointer speed,
  double-click speed, natural scrolling, key repeat; 12/24-hour clock,
  seconds, time zone; storage usage; system information. Saved to
  `/config/settings.ini`.
- **Apps**: file explorer (browse, open, new file/folder, rename,
  duplicate, copy/cut/paste, delete), terminal with a shell (~40
  commands), text editor (selection, clipboard, save / save as), Settings
  and an About window.
- **User programs** in `/bin`: `hello`, `count`, `cat`, `write`, `ls`,
  `guess`, `sysinfo`.

## Running it on Windows

Use `mayos.iso` with any of these:

**VirtualBox**: New VM → Type *Other*, Version *Other/Unknown (64-bit)*,
1024 MB of RAM. Under *Storage*, attach `mayos.iso` to the optical drive.
Under *System → Motherboard*, set **Pointing Device: PS/2 Mouse**. EFI can
be on or off. Under *Display*, keep the **VMSVGA** controller, give it
**64 MB or more** of video memory, and leave **3D acceleration off**
(MayOS renders in software). The desktop resizes itself to fit the
VirtualBox window, and the mouse moves in and out without being captured.
The resolution can also be set in Settings → Display or with the
`resolution` terminal command. If the
screen stays black, pick *MayOS (safe graphics)* in the boot menu. For sound, set *Audio → Audio Controller* to **ICH AC97**.
For internet, set *Network → Adapter 1* to **NAT** with adapter type
**Intel PRO/1000 MT Desktop**. To copy files to and from MayOS, click
*Advanced → Port Forwarding* there and add a rule with host port **8080**
and guest port **80**.

**VMware Workstation Player**: Create a VM → "I will install the operating
system later" → *Other 64-bit*, then point the CD drive at `mayos.iso`.

**QEMU for Windows** (the best experience, with a persistent disk):
build the data disk on Linux or WSL (`make disk`), then run
```
qemu-system-x86_64 -M q35 -m 512M -vga none -device virtio-gpu-pci -device virtio-tablet-pci ^
  -drive file=disk.img,if=none,id=d0,format=raw -device virtio-blk-pci,drive=d0 ^
  -netdev user,id=n0,hostfwd=tcp::8080-:80 -device e1000,netdev=n0 -audiodev dsound,id=a0 -device AC97,audiodev=a0 ^
  -cdrom mayos.iso
```

**Real PC**: write the ISO to a USB stick with
[Rufus](https://rufus.ie) (DD mode) and boot from it. Keyboard and mouse
must be PS/2 or emulated by the firmware ("USB legacy support"); native
USB drivers are future work. Hyper-V is not supported (it has no PS/2
devices).

Without a disk set up for MayOS, files live in a RAM disk: every change
works but is lost at power off. To keep settings and files in
VirtualBox, add an empty disk (*Storage → SATA controller → Add hard disk
→ Create*, VDI, 1 GB or more), boot, and click **Set up for MayOS** in
Settings → Storage (or run `setupdisk sata0` in the terminal).

**If it is slow or the sound stutters**: run `perf` in the terminal. A
"device access" time above ~8 µs means VirtualBox is running on top of
Hyper-V (a green turtle in its status bar), which makes every virtual
device many times slower. Turn off Hyper-V, Virtual Machine Platform,
Windows Hypervisor Platform and Core isolation → Memory integrity (or run
`bcdedit /set hypervisorlaunchtype off` as administrator) and restart
Windows. Set *Audio → Host Audio Driver* to *Windows Audio Session*.

**Updating MayOS**: the system always starts from the ISO; the disk only
holds your files and settings. To update, shut MayOS down, put the new
`mayos.iso` in the VM's optical drive (*Settings → Storage*, click the
disc, choose the new file) and start it again. Nothing needs
reinstalling: your files and settings stay, and the programs in `/bin`
and the guides in `/docs` are refreshed from the new ISO automatically.

If the dock is ever out of view, move it with Settings → Personalization →
Dock (or `dock left` in the terminal), or just resize the VirtualBox
window: MayOS follows it.

### Your own pictures, videos and files

The easiest way: with MayOS running and the port-forwarding rule above,
open **http://localhost:8080** in your web browser on Windows. Drag files
onto the page and they are copied into MayOS; click a file to download
it. (With a *Bridged* network adapter, open `http://<MayOS IP address>/`
instead; the `share` command shows it.) Combine this with a MayOS disk
(below) to keep the files.

For lots of files, you can also make a FAT32 virtual disk on Windows and
attach it to the VM:

1. *Disk Management* (Win+X) → *Action → Create VHD*, 1 GB or more,
   fixed size. Right-click the new disk → *Initialize Disk* (MBR), then
   *New Simple Volume* formatted as **FAT32**.
2. Copy files onto the new drive. A `pictures` folder is picked up by
   Settings → Personalization automatically.
3. Right-click the disk → *Detach VHD*.
4. VirtualBox: *Storage → SATA controller → Add hard disk → Add*, choose
   the `.vhd`. It shows up in Files (mounted at `/disk1`).

Pictures can be PNG, JPEG or BMP; right-click one → *Set as Wallpaper*.
Videos play as MP4, MOV, M4V, MKV or AVI with H.264 video (the format
phones, cameras and most downloads use) and AAC, MP3 or PCM sound; music
as MP3, M4A/AAC or WAV. HEVC/H.265, VP9 and AV1 video are not supported
yet; convert such files with
```
ffmpeg -i input.mkv -c:v libx264 -crf 20 -preset slow -c:a aac output.mp4
```

## Building (Linux or WSL)

Requirements: Rust (stable) with `rustup target add x86_64-unknown-none`,
`qemu-system-x86`, `ovmf`, `xorriso`, `mtools`, `dosfstools`, `git`, `make`.

```
make run          # build everything and boot in QEMU (window; AUDIO=pa by default)
make iso          # build/mayos.iso only
make test         # host tests + boot-time self-test + fsck of the disk
make disk-reset   # recreate the persistent data disk
```

`tools/mkfont.py` regenerates the font atlases in `assets/fonts` (needs Pillow).

## Layout

```
kernel/      kernel: boot, memory, scheduler, syscalls, drivers, network,
             audio, settings, desktop
libs/fat32   FAT32 driver (no_std, host-tested)
libs/net     Ethernet/ARP/IPv4/ICMP/UDP/DHCP/DNS encoding (no_std, host-tested)
libs/gfx     2D graphics: AA shapes, shadows, text, icons (no_std, host-tested)
libs/image   PNG/JPEG/BMP decoders and AVI parser (no_std, tested against Pillow)
userspace/   mstd (user standard library) and programs in src/bin
tools/       disk builder, font baker, self-test runner
assets/      fonts and the starter files copied onto the disk
```

## What is ours and what isn't

Everything that makes up the OS is written in this repository, with no
third-party crates. The only outside pieces are the Limine bootloader
(it loads the kernel and hands over the memory map and screen), the Rust
compiler with its `core`/`alloc` language libraries, and font glyphs
pre-rendered from DejaVu (see `assets/fonts/DEJAVU-LICENSE.txt`) until
MayOS has its own TrueType renderer.
