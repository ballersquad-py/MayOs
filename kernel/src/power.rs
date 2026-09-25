//! Shutdown and reboot.

use crate::arch::cpu::{self, inb, outb, outw};

pub fn shutdown() -> ! {
    crate::kprintln!("power: shutting down");
    if let Some(acpi) = crate::acpi::info()
        && let (Some(port), Some(typ)) = (acpi.pm1a_cnt, acpi.slp_typ_s5)
    {
        unsafe { outw(port, (typ << 10) | (1 << 13)) };
    }
    // Emulator fallbacks (QEMU q35/piix, Bochs).
    unsafe {
        outw(0x604, 0x2000);
        outw(0xb004, 0x2000);
    }
    cpu::halt_forever();
}

pub fn reboot() -> ! {
    crate::kprintln!("power: rebooting");
    unsafe {
        outb(0xcf9, 0x02);
        outb(0xcf9, 0x06);
        for _ in 0..100_000 {
            if inb(0x64) & 2 == 0 {
                break;
            }
        }
        outb(0x64, 0xfe);
    }
    // Last resort: triple fault with an empty IDT.
    #[repr(C, packed)]
    struct Idtr(u16, u64);
    let empty = Idtr(0, 0);
    unsafe { core::arch::asm!("lidt [{}]; int3", in(reg) &empty) };
    cpu::halt_forever();
}

/// Exit QEMU with a status code via the isa-debug-exit device (tests).
pub fn qemu_exit(success: bool) -> ! {
    // QEMU exits with (value << 1) | 1.
    unsafe { outb(0xf4, if success { 0x10 } else { 0x11 }) };
    shutdown();
}
