#![no_std]
#![no_main]

extern crate alloc;

#[macro_use]
mod log;

mod acpi;
mod arch;
mod audio;
mod boot;
mod drivers;
mod fs;
mod gui;
mod input;
mod interrupts;
mod mem;
mod network;
mod power;
mod proc;
mod selftest;
mod serial;
mod settings;
mod storage;
mod sync;
mod time;

use arch::{apic, cpu, gdt, idt};
use drivers::{pci, ps2, virtio};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    serial::init();
    kprintln!("MayOS {} booting", VERSION);
    if !boot::BASE_REVISION.is_supported() {
        kprintln!("Limine base revision 3 not supported by bootloader");
        cpu::halt_forever();
    }
    gdt::init();
    idt::init();
    mem::init();
    let (free, total) = mem::pmm::stats();
    kprintln!("memory: {} MiB free of {} MiB", free * 4 / 1024, total * 4 / 1024);

    let rsdp = boot::RSDP.response().expect("no RSDP").address;
    let acpi = acpi::store(acpi::parse(rsdp));
    kprintln!("acpi: {} cpu(s), {} io-apic(s)", acpi.cpu_count, acpi.ioapics.len());
    apic::init(acpi);
    let tsc_per_ms = apic::start_timer(1000);
    time::init(tsc_per_ms);
    idt::init_syscall();

    let wheel = ps2::init();
    apic::route_irq(acpi, 1, idt::VEC_KEYBOARD);
    apic::route_irq(acpi, 12, idt::VEC_MOUSE);
    kprintln!("ps2: keyboard + mouse{}", if wheel { " (wheel)" } else { "" });

    gui::theme::init();
    init_devices();

    proc::sched::init();
    // Drivers that start their own threads come after the scheduler.
    init_threaded_devices();
    storage::init();
    settings::load();
    let selftest = boot::cmdline().contains("selftest");
    if selftest {
        proc::sched::spawn_kernel("selftest", selftest::run, 0);
    }
    proc::sched::spawn_kernel("desktop", gui::desktop_main, 0);
    proc::sched::start();
    kprintln!("boot complete in {} ms", time::uptime_ms());
    loop {
        cpu::sti_hlt();
    }
}

fn init_threaded_devices() {
    for d in pci::devices() {
        if d.vendor == 0x8086 && drivers::e1000::DEVICE_IDS.contains(&d.device) && !network::is_present() {
            match drivers::e1000::E1000::new(&d) {
                Some(nic) => {
                    kprintln!("net: {} mac {}, link {}", nic.model, nic.mac, if nic.link_up() { "up" } else { "down" });
                    network::init(nic);
                }
                None => kprintln!("net: e1000 init failed"),
            }
        }
        if d.vendor == audio::ac97::VENDOR_INTEL && audio::ac97::DEVICE_IDS.contains(&d.device) && !audio::is_present() {
            match audio::ac97::Ac97::new(&d) {
                Some(dev) => {
                    kprintln!("audio: Intel AC'97 at {:02x}:{:02x}.{}", d.bus, d.slot, d.func);
                    audio::init(dev, "Intel AC'97 Audio");
                }
                None => kprintln!("audio: AC'97 init failed"),
            }
        }
    }
}

fn init_devices() {
    pci::scan();
    let devices = pci::devices();
    kprintln!("pci: {} devices", devices.len());
    let io = time::measure_io_cost();
    kprintln!("cpu: device access costs {}.{} us{}", io / 1000, io / 100 % 10, if io > 8000 { " (slow virtualisation)" } else { "" });
    if let Some(d) = devices.iter().find(|d| d.vendor == 0x80ee && d.device == 0xcafe) {
        drivers::vmmdev::init(d);
    }
    for d in devices.iter().filter(|d| d.vendor == virtio::VENDOR) {
        match d.device {
            virtio::DEVICE_BLOCK | virtio::DEVICE_BLOCK_TRANSITIONAL => match drivers::virtio_blk::VirtioBlk::new(d) {
                Some(blk) if !fs::is_mounted() => {
                    let mb = blk.capacity / 2048;
                    match fs::mount(fs::Disk::Virtio(blk)) {
                        Ok(()) => kprintln!("disk: virtio-blk {} MiB, FAT32 \"{}\" mounted at /", mb, fs::volume_label()),
                        Err(e) => kprintln!("disk: virtio-blk {} MiB: cannot mount: {}", mb, e),
                    }
                }
                Some(_) => kprintln!("disk: ignoring additional virtio-blk device"),
                None => kprintln!("disk: virtio-blk init failed"),
            },
            virtio::DEVICE_GPU => match drivers::virtio_gpu::VirtioGpu::new(d) {
                Some(gpu) => {
                    kprintln!("gpu: virtio-gpu {}x{}", gpu.width, gpu.height);
                    gui::set_display(gui::display::Display::Virtio(gpu));
                }
                None => kprintln!("gpu: virtio-gpu init failed"),
            },
            virtio::DEVICE_INPUT => match drivers::virtio_input::VirtioInput::new(d) {
                Some(inp) => {
                    kprintln!("input: virtio \"{}\"", inp.name);
                    gui::add_input(inp);
                }
                None => kprintln!("input: virtio-input init failed"),
            },
            _ => {}
        }
    }
    if !fs::is_mounted()
        && let Some(ram) = drivers::ramdisk::RamDisk::from_boot_module()
    {
        let mb = ram.sectors / 2048;
        match fs::mount(fs::Disk::Ram(ram)) {
            Ok(()) => kprintln!("disk: no virtio disk, using the {} MiB RAM disk from the boot image", mb),
            Err(e) => kprintln!("disk: RAM disk cannot be mounted: {}", e),
        }
    }
    // Display adapters we can drive ourselves (resolution switching).
    // "safegraphics" on the kernel command line keeps the firmware screen.
    if gui::display_description() == "none" && !boot::cmdline().contains("safegraphics") {
        for d in devices.iter() {
            let is_svga = (d.vendor == 0x15ad && d.device == 0x0405)
                || (d.vendor == 0x80ee && d.device == 0xbeef && d.read32(0x10) & 1 != 0);
            let is_bochs = (d.vendor == 0x1234 && d.device == 0x1111)
                || (d.vendor == 0x80ee && d.device == 0xbeef && d.read32(0x10) & 1 == 0);
            let display = if is_svga {
                drivers::vmware_svga::VmwareSvga::new(d).and_then(gui::display::Display::from_svga)
            } else if is_bochs {
                drivers::bochs_vga::BochsVga::new(d).and_then(gui::display::Display::from_bochs)
            } else {
                continue;
            };
            match display {
                Some(disp) => {
                    let (w, h) = disp.size();
                    kprintln!("gpu: {} {}x{}", disp.name(), w, h);
                    gui::set_display(disp);
                    break;
                }
                None => kprintln!("gpu: {:04x}:{:04x} could not be initialised", d.vendor, d.device),
            }
        }
    }
    if gui::display_description() == "none" {
        match gui::display::Display::from_boot_framebuffer() {
            Some(d) => {
                kprintln!("gpu: no virtio-gpu, using the boot framebuffer");
                gui::set_display(d);
            }
            None => kprintln!("gpu: no display found"),
        }
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    cpu::cli();
    serial::write_fmt_unlocked(format_args!("\n*** KERNEL PANIC ***\n{}\n", info));
    panic_screen(info);
    if boot::cmdline().contains("selftest") {
        power::qemu_exit(false);
    }
    cpu::halt_forever();
}

/// Draw the panic message on the boot framebuffer (visible when the
/// desktop uses it, e.g. in VirtualBox or VMware where there is no serial
/// console to read).
fn panic_screen(info: &core::panic::PanicInfo) {
    use core::fmt::Write;
    let Some(fb) = boot::FRAMEBUFFER.response().and_then(|r| r.first()) else { return };
    let Some(fonts) = gui::theme::try_fonts() else { return };
    if fb.bpp != 32 {
        return;
    }
    let (w, h) = (fb.width as i32, fb.height as i32);
    let stride = fb.pitch as usize / 4;
    let buf = unsafe { core::slice::from_raw_parts_mut(fb.address as *mut u32, stride * h as usize) };
    let mut c = gfx::Canvas::new(buf, w, h, stride);
    let box_r = gfx::Rect::new(w / 2 - 380, h / 2 - 230, 760, 460);
    c.fill_rounded_rect(box_r, 14, gfx::rgb(0xb3, 0x26, 0x2d));
    c.draw_text(&fonts.large, box_r.x + 24, box_r.y + 44, "MayOS has stopped", gfx::rgb(255, 255, 255));
    struct Buf([u8; 1024], usize);
    impl Write for Buf {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            for &b in s.as_bytes() {
                if self.1 < self.0.len() {
                    self.0[self.1] = b;
                    self.1 += 1;
                }
            }
            Ok(())
        }
    }
    let mut msg = Buf([0; 1024], 0);
    let _ = write!(msg, "{}", info);
    let text = core::str::from_utf8(&msg.0[..msg.1]).unwrap_or("panic");
    let mut y = box_r.y + 80;
    for line in text.lines() {
        let mut rest = line;
        while !rest.is_empty() && y < box_r.bottom() - 40 {
            let n = rest.char_indices().nth(90).map(|(i, _)| i).unwrap_or(rest.len());
            c.draw_text(&fonts.mono, box_r.x + 24, y, &rest[..n], gfx::rgb(255, 235, 235));
            rest = &rest[n..];
            y += 18;
        }
    }
    // The last kernel log lines usually explain what led up to the crash.
    let log = log::tail(8);
    let mut ly = y.max(box_r.y + 190);
    c.draw_text(&fonts.bold, box_r.x + 24, ly, "Recent kernel log:", gfx::rgb(255, 220, 220));
    ly += 20;
    for line in log.lines() {
        if ly > box_r.bottom() - 40 {
            break;
        }
        let n = line.char_indices().nth(95).map(|(i, _)| i).unwrap_or(line.len());
        c.draw_text(&fonts.mono, box_r.x + 24, ly, &line[..n], gfx::rgb(255, 210, 210));
        ly += 17;
    }
    c.draw_text(&fonts.ui, box_r.x + 24, box_r.bottom() - 20, "Please restart the computer.", gfx::rgb(255, 220, 220));
}
