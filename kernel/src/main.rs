#![no_std]
#![no_main]

extern crate alloc;

#[macro_use]
mod log;

mod acpi;
mod arch;
mod boot;
mod drivers;
mod interrupts;
mod mem;
mod proc;
mod serial;
mod sync;
mod time;

use arch::{apic, cpu, gdt, idt};

#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    serial::init();
    kprintln!("MayOS booting...");
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
    let acpi = acpi::parse(rsdp);
    kprintln!("acpi: {} cpu(s), {} io-apic(s)", acpi.cpu_count, acpi.ioapics.len());
    apic::init(&acpi);
    let tsc_per_ms = apic::start_timer(100);
    time::init(tsc_per_ms);
    kprintln!("timer: tsc {} kHz", tsc_per_ms);
    idt::init_syscall();

    proc::sched::init();
    proc::sched::spawn_kernel("test-a", test_thread, 1);
    proc::sched::spawn_kernel("test-b", test_thread, 2);
    proc::sched::start();
    cpu::sti();
    loop {
        cpu::sti_hlt();
    }
}

extern "C" fn test_thread(n: usize) {
    for i in 0..3 {
        kprintln!("thread {} tick {} t={}ms", n, i, time::uptime_ms());
        proc::sched::sleep_ms(100);
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    cpu::cli();
    serial::write_fmt_unlocked(format_args!("\n*** KERNEL PANIC ***\n{}\n", info));
    cpu::halt_forever();
}
