//! Monotonic time based on the calibrated TSC.

use core::sync::atomic::{AtomicU64, Ordering};

static TSC_PER_MS: AtomicU64 = AtomicU64::new(1_000_000);
static TSC_BOOT: AtomicU64 = AtomicU64::new(0);

pub fn init(tsc_per_ms: u64) {
    TSC_BOOT.store(crate::arch::cpu::rdtsc(), Ordering::Relaxed);
    TSC_PER_MS.store(tsc_per_ms, Ordering::Relaxed);
}

pub fn uptime_ms() -> u64 {
    let now = crate::arch::cpu::rdtsc();
    now.saturating_sub(TSC_BOOT.load(Ordering::Relaxed)) / TSC_PER_MS.load(Ordering::Relaxed)
}

pub fn uptime_us() -> u64 {
    let now = crate::arch::cpu::rdtsc();
    now.saturating_sub(TSC_BOOT.load(Ordering::Relaxed)) * 1000 / TSC_PER_MS.load(Ordering::Relaxed)
}

/// Busy-wait about `us` microseconds without touching any device.
pub fn delay_us(us: u64) {
    let end = uptime_us() + us;
    while uptime_us() < end {
        core::hint::spin_loop();
    }
}

/// Polling a device politely. Every register read in a virtual machine is
/// a trip out to the hypervisor, so a tight polling loop slows the device
/// down and the host with it. `Backoff` spaces the reads out and, after
/// half a millisecond, lets other threads (audio!) run between them.
pub struct Backoff {
    start: u64,
    pause: u64,
}

impl Backoff {
    pub fn new() -> Backoff {
        Backoff { start: uptime_us(), pause: 2 }
    }

    pub fn wait(&mut self) {
        let waited = uptime_us() - self.start;
        if waited < 500 || !crate::arch::cpu::interrupts_enabled() {
            delay_us(self.pause);
            self.pause = (self.pause * 2).min(64);
        } else {
            crate::proc::sched::sleep_ms(1);
        }
    }
}

static IO_COST_NS: AtomicU64 = AtomicU64::new(0);

/// Measure what one device register access costs. On real hardware or a
/// well-accelerated VM it is around a microsecond; when VirtualBox runs on
/// top of Hyper-V ("turtle mode") it can be tens of microseconds, and
/// everything that talks to devices gets that much slower.
pub fn measure_io_cost() -> u64 {
    let t0n = crate::arch::cpu::rdtsc();
    for _ in 0..64 {
        let _ = crate::drivers::pci::read32(0, 0, 0, 0);
    }
    let ticks = crate::arch::cpu::rdtsc() - t0n;
    // Each config read is two port accesses.
    let ns = ticks * 1_000_000 / TSC_PER_MS.load(Ordering::Relaxed).max(1) / 128;
    IO_COST_NS.store(ns, Ordering::Relaxed);
    ns
}

/// Nanoseconds per device register access, measured at boot.
pub fn io_cost_ns() -> u64 {
    IO_COST_NS.load(Ordering::Relaxed)
}
