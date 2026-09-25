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
