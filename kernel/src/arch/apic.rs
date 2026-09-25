//! Local APIC, I/O APIC, legacy PIC shutdown and PIT-based calibration.

use core::sync::atomic::{AtomicU64, Ordering};

use super::cpu::{inb, outb};
use crate::acpi::AcpiInfo;
use crate::mem::paging;

static LAPIC: AtomicU64 = AtomicU64::new(0);
static IOAPIC: AtomicU64 = AtomicU64::new(0);
static IOAPIC_GSI_BASE: AtomicU64 = AtomicU64::new(0);

const LAPIC_ID: usize = 0x20;
const LAPIC_TPR: usize = 0x80;
const LAPIC_EOI: usize = 0xb0;
const LAPIC_SVR: usize = 0xf0;
const LAPIC_LVT_TIMER: usize = 0x320;
const LAPIC_TIMER_INIT: usize = 0x380;
const LAPIC_TIMER_CUR: usize = 0x390;
const LAPIC_TIMER_DIV: usize = 0x3e0;

fn lapic_read(reg: usize) -> u32 {
    unsafe { core::ptr::read_volatile((LAPIC.load(Ordering::Relaxed) as usize + reg) as *const u32) }
}

fn lapic_write(reg: usize, v: u32) {
    unsafe { core::ptr::write_volatile((LAPIC.load(Ordering::Relaxed) as usize + reg) as *mut u32, v) }
}

pub fn eoi() {
    if LAPIC.load(Ordering::Relaxed) != 0 {
        lapic_write(LAPIC_EOI, 0);
    }
}

/// Remap the 8259 PICs away from the exception vectors and mask them.
fn disable_pic() {
    unsafe {
        outb(0x20, 0x11);
        outb(0xa0, 0x11);
        outb(0x21, 0xf0);
        outb(0xa1, 0xf8);
        outb(0x21, 0x04);
        outb(0xa1, 0x02);
        outb(0x21, 0x01);
        outb(0xa1, 0x01);
        outb(0x21, 0xff);
        outb(0xa1, 0xff);
    }
}

/// Busy-wait for `ms` milliseconds using PIT channel 2 (max ~50 ms).
fn pit_wait_ms(ms: u32) {
    let count = (1_193_182u32 * ms / 1000) as u16;
    unsafe {
        let p = inb(0x61);
        outb(0x61, (p & 0xfd) | 1);
        outb(0x43, 0xb0);
        outb(0x42, count as u8);
        outb(0x42, (count >> 8) as u8);
        let p = inb(0x61) & 0xfe;
        outb(0x61, p);
        outb(0x61, p | 1);
        while inb(0x61) & 0x20 == 0 {}
    }
}

fn ioapic_write(reg: u32, v: u32) {
    let base = IOAPIC.load(Ordering::Relaxed) as usize;
    unsafe {
        core::ptr::write_volatile(base as *mut u32, reg);
        core::ptr::write_volatile((base + 0x10) as *mut u32, v);
    }
}

pub fn init(acpi: &AcpiInfo) {
    disable_pic();
    let lapic = paging::map_mmio(acpi.lapic_phys, 0x1000);
    LAPIC.store(lapic, Ordering::Relaxed);
    lapic_write(LAPIC_TPR, 0);
    lapic_write(LAPIC_SVR, 0x100 | super::idt::VEC_SPURIOUS as u32);

    if let Some(io) = acpi.ioapics.first() {
        let v = paging::map_mmio(io.phys, 0x1000);
        IOAPIC.store(v, Ordering::Relaxed);
        IOAPIC_GSI_BASE.store(io.gsi_base as u64, Ordering::Relaxed);
    }
}

pub fn lapic_id() -> u32 {
    lapic_read(LAPIC_ID) >> 24
}

/// Route a legacy ISA IRQ to `vector` on this CPU, honouring ACPI overrides.
pub fn route_irq(acpi: &AcpiInfo, irq: u8, vector: u8) {
    if IOAPIC.load(Ordering::Relaxed) == 0 {
        return;
    }
    let mut gsi = irq as u32;
    let mut flags = 0u16;
    if let Some(o) = acpi.overrides.iter().find(|o| o.irq == irq) {
        gsi = o.gsi;
        flags = o.flags;
    }
    let mut low = vector as u32;
    if flags & 0x3 == 0x3 {
        low |= 1 << 13; // active low
    }
    if flags & 0xc == 0xc {
        low |= 1 << 15; // level triggered
    }
    let idx = gsi - IOAPIC_GSI_BASE.load(Ordering::Relaxed) as u32;
    ioapic_write(0x10 + idx * 2 + 1, lapic_id() << 24);
    ioapic_write(0x10 + idx * 2, low);
}

/// Calibrate the TSC and the LAPIC timer against the PIT, then start the
/// timer in periodic mode at `hz`. Returns TSC ticks per millisecond.
pub fn start_timer(hz: u32) -> u64 {
    lapic_write(LAPIC_TIMER_DIV, 0x3); // divide by 16
    lapic_write(LAPIC_LVT_TIMER, 1 << 16); // masked while calibrating
    lapic_write(LAPIC_TIMER_INIT, u32::MAX);
    let t0 = super::cpu::rdtsc();
    pit_wait_ms(20);
    let t1 = super::cpu::rdtsc();
    let elapsed = u32::MAX - lapic_read(LAPIC_TIMER_CUR);
    let per_ms = (elapsed / 20).max(1);
    lapic_write(LAPIC_LVT_TIMER, super::idt::VEC_TIMER as u32 | (1 << 17));
    lapic_write(LAPIC_TIMER_INIT, per_ms * 1000 / hz);
    ((t1 - t0) / 20).max(1)
}
