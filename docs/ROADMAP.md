# MayOS Implementation Plan

This is the plan for building MayOS from scratch. It covers a kernel, a desktop
with antialiased rounded windows, image and video support, GPU-accelerated
drawing, networking (starting with `ping`), and a web browser with its own HTML,
CSS and JavaScript engines.

The plan is split into **phases**. Each phase ends with a **milestone**: a
concrete, demoable result. Don't start a phase until the one before it has
reached its milestone, because every layer here depends on the layer below
working reliably.

---

## 0. Key decisions (made up front)

| Decision | Choice | Why |
|---|---|---|
| Language | **Rust** (`no_std` kernel, plus a small amount of assembly) | Memory safety removes most of the bugs that kill hobby OSes (use-after-free, buffer overruns in parsers). It has a good ecosystem for `no_std` work. C is the alternative: simpler to start, but harder to keep correct at browser scale. |
| Architecture | **x86_64** first | Best documented, and the architecture QEMU emulates best. AArch64 can come later. |
| Boot | **UEFI** through the **Limine** bootloader | Hands us a framebuffer, a memory map, 64-bit long mode and higher-half mapping, so we skip BIOS/real-mode work. |
| Dev loop | **QEMU** (+ KVM when available), with GDB attached | Fast iteration. Real hardware only after QEMU works. |
| Kernel design | **Hybrid / modular monolith** | Drivers live in the kernel for speed and simplicity, behind clean trait interfaces so they can move to userspace later. |
| "From scratch" rule | Everything is written in-tree: no libc, no ported engines | That's the point of the project. Where scratch-building is extremely hard (H.264, TLS crypto), the plan notes it explicitly. |

### Repository layout

```
MayOs/
├── kernel/            # boot, memory, scheduling, syscalls, drivers
├── libs/
│   ├── mstd/          # our userspace std (syscall wrappers, alloc, collections)
│   ├── gfx/           # 2D rasterizer, AA paths, compositor primitives
│   ├── font/          # TrueType/OpenType parser + rasterizer
│   ├── image/         # PNG, JPEG, GIF, BMP, WebP decoders
│   ├── media/         # containers + video/audio codecs
│   ├── net/           # TCP/IP stack (usable from kernel or userspace)
│   ├── crypto/        # hashes, ciphers, bignum, X.509 (for TLS)
│   ├── html/  css/  layout/  js/   # browser engine crates
├── userspace/
│   ├── init/  shell/  compositor/  terminal/  imageview/  player/  browser/
├── tools/             # build scripts, disk image creation, test runners
└── docs/
```

Build with `cargo` plus a `Makefile`/`xtask` that produces a bootable `.iso`/`.img`
and runs it in QEMU with one command (`cargo xtask run`).

---

## Phase 1: Boot and kernel core

**Goal:** a kernel that boots, manages memory, runs several processes and has
a shell.

1. **Toolchain and boot**
   - Custom target JSON (`x86_64-mayos`), `#![no_std]`, `#![no_main]`, panic handler.
   - Limine config; kernel entry receives the framebuffer and memory map.
   - Serial port (COM1) logging first, because it works before any graphics.
2. **CPU setup:** GDT, TSS, IDT, exception handlers (page fault and double
   fault get their own IST stacks).
3. **Memory**
   - Physical frame allocator (bitmap first, buddy allocator later).
   - 4-level paging, higher-half kernel, per-process address spaces.
   - Kernel heap (linked-list, then slab allocator) → enables `alloc` (Vec, Box).
4. **Interrupts and time:** parse ACPI (RSDP → MADT/HPET), set up the Local APIC
   and IO-APIC, APIC timer, HPET/TSC calibration.
5. **Multitasking**
   - Kernel threads, context switch, preemptive round-robin scheduler with priorities.
   - SMP: start the other cores, per-CPU data, spinlocks → then mutexes and wait queues.
6. **Userspace:** ring 3, `syscall`/`sysret`, ELF64 loader, `mstd` syscall
   library. The first syscalls are `exit, write, read, mmap, spawn, wait, yield, sleep`.
7. **Storage and files**
   - PCI/PCIe enumeration (ECAM from the MCFG table).
   - Block drivers: **virtio-blk** (QEMU), then **AHCI** (SATA) and **NVMe** (real hardware).
   - VFS layer (inodes, mounts, file descriptors), plus FAT32 (boot partition) and
     ext2 read/write, or a simple custom FS.
8. **Input:** PS/2 keyboard and mouse first; **xHCI USB + HID** later for real hardware.
9. **IPC:** message-passing channels plus shared memory. The compositor and
   apps will use these.

**Milestone 1:** MayOS boots in QEMU and drops into a text shell (drawn on the
framebuffer) that can `ls`, `cat` and launch user programs from disk.

---

## Phase 2: Graphics stack (rounded windows, AA, GPU)

**Goal:** a modern-looking desktop with smooth rounded, shadowed and
antialiased windows.

### 2a. Software renderer (build this first, then keep it as the fallback)
- **Surfaces:** 32-bit BGRA/premultiplied-alpha buffers, clipping rects, blits.
- **Antialiased rasterizer:** analytic coverage computation (the approach used
  by font-rs and tiny-skia). It handles lines, Bézier paths (quadratic and
  cubic, flattened), and fills with non-zero and even-odd rules.
- **Rounded rectangles:** a signed-distance-function per pixel gives
  exact, cheap AA corners for the most common shape on screen.
- **Compositing:** Porter-Duff `over` in premultiplied space, gamma-correct
  (linear-light) blending, gradients (linear and radial), box blur approximating
  Gaussian (3 passes) for shadows and frosted glass.
- **Performance:** SIMD (SSE2/AVX2) inner loops, tile-based rendering, damage
  tracking so only changed regions are redrawn.

### 2b. Fonts
- TrueType/OpenType parser (`cmap`, `glyf`, `loca`, `hmtx`, `kern`/`GPOS` basic).
- Glyph outlines → the same AA path rasterizer; optional subpixel (LCD) AA.
- Glyph cache (atlas), basic text shaping (kerning, ligatures later).
- Bundle an open font (e.g. Inter / Noto Sans) on the disk image.

### 2c. Compositor and window manager (userspace process)
- Each app gets a shared-memory surface. The compositor composes all windows
  every frame, synced to vblank when the driver supports it.
- **Real rounded windows:** each window is clipped by an SDF rounded-rect mask
  with AA edges, plus a soft drop shadow and optional backdrop blur.
- Window operations: move, resize, focus, z-order, minimize/maximize, animations
  (spring/ease curves at 60 fps).
- A widget toolkit library (`libs/ui`): buttons, text fields, scroll views,
  layout (flex-like), theming.
- Desktop shell: taskbar/dock, launcher, terminal app.

### 2d. GPU acceleration
Be realistic here: native drivers for modern NVIDIA or AMD GPUs are
multi-year efforts even for large teams. The staged approach:
1. **virtio-gpu 2D** (QEMU): proper mode setting, resolution changes, hardware cursor.
2. **virtio-gpu 3D (virgl / Venus)**: send Gallium/Vulkan-style command streams
   to the host GPU. That gives **real GPU-drawn** compositing inside a VM.
3. Design a small **rendering API** (`gfx` backend trait) with two backends,
   `SoftwareBackend` and `GpuBackend`. The compositor and browser only talk to the trait.
4. On the GPU backend: windows are textures, and rounded corners, shadows and
   blur are fragment shaders. Write a tiny shader compiler, or precompile
   shaders to TGSI/SPIR-V at build time.
5. *Stretch goal:* **Intel integrated graphics** (Gen9+/Xe) native driver.
   It's the best documented real-hardware GPU (public PRMs).

**Milestone 2:** a desktop at native resolution with draggable,
antialiased rounded windows, shadows, blur, smooth text, a terminal, and
60 fps compositing (software), with GPU compositing under QEMU/virgl.

---

## Phase 3: Media (pictures and video)

### 3a. Image formats (in `libs/image`, in this order)
1. **BMP / PPM / TGA**: trivial, used to test the pipeline.
2. **DEFLATE/zlib inflate**: needed by PNG (and later by HTTP gzip and fonts).
3. **PNG**: all color types, interlacing, alpha, gamma.
4. **JPEG**: baseline Huffman + IDCT + YCbCr → RGB, then progressive, then
   SIMD IDCT.
5. **GIF**: LZW + animation frames.
6. **WebP**: lossless (VP8L), then lossy (VP8 intra). This is needed for the modern web.
7. *Later:* AVIF (depends on AV1 decoder), SVG (reuses the path rasterizer + a
   subset of the XML parser from Phase 5).

App: **Image Viewer**, with zoom/pan and high-quality scaling (bilinear → Lanczos).

### 3b. Audio (video needs sound and A/V sync)
- Drivers: **Intel HDA** (QEMU `-device intel-hda` and most real PCs), AC'97 as a fallback.
- Mixer/audio server in userspace, ring buffers, sample-rate conversion.
- Formats: WAV/PCM → **Vorbis/Opus** or **MP3** decoder.

### 3c. Video
- **Containers:** AVI/MJPEG first (reuses the JPEG decoder, which is the easiest
  win), then **WebM/Matroska** and **MP4 (ISO BMFF)** demuxers.
- **Codecs**, in increasing difficulty:
  1. **MJPEG**: already have it.
  2. **VP8**: moderately sized, well-specified (RFC 6386).
  3. **VP9** / **H.264 (AVC)**: large. Budget months. H.264 also has patent/licensing concerns.
  4. **AV1**: the modern web codec; very large.
- Playback pipeline: demux thread → decode thread(s) → frame queue →
  presented on the audio clock (A/V sync), YUV → RGB conversion done on the GPU
  when available.
- *Stretch:* hardware decode via virtio-video or Intel QuickSync.

App: **Media Player**.

**Milestone 3:** open PNG/JPEG/GIF/WebP files in the image viewer; play a
WebM (VP8 + Vorbis) or MJPEG AVI file with synced audio.

---

## Phase 4: Networking (start with `ping`)

### 4a. Base: reach `ping`
1. **NIC drivers:** **virtio-net** (QEMU), **Intel e1000/e1000e** (QEMU and many
   real machines), **Realtek RTL8139/8169**. Use DMA descriptor rings and interrupts.
2. **Network device abstraction:** `trait NetDevice { send(frame), recv() }`,
   packet buffers (zero-copy where practical).
3. **Ethernet II** framing.
4. **ARP**: request/reply, cache with timeouts.
5. **IPv4**: header parsing, checksum, routing table (default gateway), TTL.
   Fragmentation reassembly can come later.
6. **ICMP**: echo request/reply → **`ping` command** in the shell.
   Static IP config at first (QEMU user-net: `10.0.2.15`, gateway `10.0.2.2`).

**Milestone 4a:** `ping 10.0.2.2` gets replies with RTT times, and MayOS
answers pings sent to it.

### 4b. Full network stack
7. **UDP** → **DHCP client** (automatic IP) → **DNS resolver** (so `ping example.com` works).
8. **TCP**: state machine, 3-way handshake, retransmission timers, sliding window,
   congestion control (Reno → CUBIC), proper close. This is the hardest part of the stack.
9. **Sockets API** in `mstd` (BSD-like: `socket/bind/connect/listen/accept/send/recv`,
   plus `poll`/`epoll`-like readiness).
10. Tools: `ifconfig`, `nslookup`, `nc`, a simple HTTP server.
11. **IPv6** + ICMPv6 + NDP (later; many sites are dual-stack).

### 4c. HTTP and TLS (needed for real websites)
12. **HTTP/1.1 client**: requests, headers, chunked encoding, keep-alive,
    redirects, gzip/deflate (reuses inflate), cookies.
13. **Crypto library** (`libs/crypto`): SHA-256/384, HMAC, HKDF, AES-GCM,
    ChaCha20-Poly1305, X25519, P-256 ECDSA, RSA (bignum), secure RNG
    (RDRAND + ChaCha20 CSPRNG). Must be **constant-time** and tested
    against official test vectors. Be aware that a from-scratch TLS stack is
    only as trustworthy as its test coverage.
14. **TLS 1.3** (and TLS 1.2 for compatibility), X.509 parsing, certificate chain
    validation against a bundled root CA store.
15. Later: HTTP/2 (HPACK, multiplexing).

**Milestone 4:** `curl https://example.com` prints the page's HTML.

---

## Phase 5: Browser engine: HTML and CSS

Follow the **WHATWG HTML** and **W3C CSS** specs closely. The spec is written as
algorithms, which makes it a good implementation guide.

### 5a. HTML
1. **Tokenizer**: the full WHATWG state machine (≈80 states), character
   references, and error recovery. Real sites are full of malformed HTML.
2. **Tree builder**: insertion modes, active formatting elements, adoption agency
   algorithm, implied tags.
3. **DOM**: Node/Element/Text/Document tree, attributes, `getElementById`,
   querySelector (shares the selector engine with CSS).
4. Encoding sniffing (UTF-8 first, then windows-1252 etc.).

### 5b. CSS
5. **Tokenizer and parser** (CSS Syntax Level 3): rules, at-rules, declarations.
6. **Selectors**: type/class/id, attribute, combinators, pseudo-classes
   (`:hover`, `:nth-child`, …), specificity.
7. **Cascade and computed style**: user-agent stylesheet, origin/importance,
   inheritance, `initial`/`inherit`, `var()` custom properties, units (px, em,
   rem, %, vw/vh), `calc()`, colors.

### 5c. Layout and painting
8. **Box tree** from DOM + styles (`display` handling, anonymous boxes).
9. **Block and inline formatting contexts**: line breaking (Unicode line-break
   rules, simple version first), text shaping via `libs/font`, margins with
   collapsing, padding, borders.
10. Positioning: `relative/absolute/fixed/sticky`, `float`, `z-index` stacking contexts.
11. **Flexbox**, then **Grid** (modern sites depend heavily on both).
12. **Painting**: build a display list, then render through the `gfx` backend
    (GPU when available). Handles `border-radius`, `box-shadow`, gradients, opacity,
    `transform`, images (Phase 3 decoders), and web fonts (`@font-face` → `libs/font`).
13. **Browser app**: tabs, URL bar, back/forward, scrolling, links, basic forms,
    resource loader (HTML → CSS/images fetched in parallel), cache.

**Milestone 5:** the browser renders static sites (e.g. a Wikipedia article or a
blog) with correct layout, styles, images and working links.

---

## Phase 6: JavaScript engine

Build a small, correct interpreter first. Worry about performance later.

1. **Lexer and parser** → AST (ES5 first, then ES2015+: `let/const`, arrows,
   classes, template literals, destructuring, spread, modules).
2. **Bytecode compiler + register- or stack-based VM**, with scopes and closures.
3. **Runtime objects**: property maps → hidden classes/shapes later,
   prototype chains, property descriptors, getters/setters.
4. **Garbage collector**: precise mark-and-sweep → incremental → generational.
5. **Built-ins**: Object, Function, Array, String (UTF-16 semantics), Number,
   Math, JSON, RegExp (own regex engine), Map/Set, Symbol, Error, Date, Proxy/Reflect.
6. **Async**: Promises, microtask queue, `async/await`, generators.
7. **Browser integration**
   - Event loop (tasks, microtasks, rendering steps, `requestAnimationFrame`).
   - Web IDL-style bindings: DOM manipulation, events (`addEventListener`,
     bubbling/capture), timers, `fetch`/XHR, `localStorage`, `console`.
   - Mutations from JS trigger style recalculation → relayout → repaint (incremental).
8. **Performance** (later): inline caches, a baseline JIT (x86_64 codegen),
   string interning, NaN-boxing.
9. **Conformance**: run the **Test262** suite continuously and track the pass rate.

**Milestone 6:** interactive sites work: menus, JS-rendered content,
simple web apps (e.g. a TodoMVC implementation) and basic canvas drawing.

---

## Cross-cutting: testing and quality

- **Host-side unit tests:** keep `libs/*` platform-independent so decoders,
  parsers, the network stack, crypto and the JS engine run under `cargo test`
  on Linux. This is where most bugs get caught.
- **Fuzzing** (`cargo fuzz`) for every parser: PNG, JPEG, TCP input, HTML, CSS, JS.
- **Kernel tests** run in QEMU headless (`-display none`), report over serial,
  and exit through `isa-debug-exit`.
- **Conformance suites:** web-platform-tests (HTML/CSS), Test262 (JS), image
  test suites (PngSuite), crypto test vectors (NIST/Wycheproof).
- **Screenshot regression tests** for the compositor and browser rendering.
- **CI** (GitHub Actions): build, host tests, QEMU boot test on every push.

---

## Suggested order and rough scale

| Phase | Depends on | Relative size |
|---|---|---|
| 1. Kernel core | nothing | Large |
| 2. Graphics (software) | 1 | Large |
| 2d. GPU (virgl) | 2 | Large |
| 3a. Images | 2 (can start in parallel as host-side libs) | Medium |
| 3b/c. Audio + video | 1, 2, 3a | Very large (codecs) |
| 4a. Ping | 1 | Small–medium |
| 4b/c. TCP, HTTP, TLS | 4a | Large |
| 5. HTML/CSS/layout | 2, 3a, 4c | Very large |
| 6. JavaScript | 5 | Very large |

Because `libs/*` are host-testable, **image decoders, the HTML/CSS parsers and
the JS engine can be developed in parallel** with kernel work and then integrated.
This is the single biggest schedule win.

## Main risks and mitigations

- **Real-hardware drivers** (GPU, Wi-Fi, USB): target QEMU devices first, and pick
  well-documented real hardware (Intel NIC, Intel iGPU, AHCI/NVMe, HDA).
- **Codec and TLS complexity:** ship easier formats first (MJPEG, VP8), and test
  crypto against published vectors. Treat the TLS stack as experimental
  until it has been audited.
- **Web compatibility is endless:** measure progress with WPT and Test262 pass
  rates and a fixed list of target sites, not "the whole web".

---

## Immediate next steps (first PRs)

1. Cargo workspace + custom target + `xtask` that builds an ISO with Limine and runs QEMU.
2. Kernel prints "Hello from MayOS" over serial **and** to the framebuffer.
3. GDT/IDT + exception handlers with a readable panic screen.
4. Physical frame allocator + paging + heap → `Vec`/`Box` work in the kernel.
5. GitHub Actions CI: build + headless QEMU boot test.
