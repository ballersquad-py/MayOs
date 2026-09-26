//! Locking primitives.
//!
//! MayOS currently runs on a single CPU, so `Spin` disables interrupts while
//! held: that makes it safe to share data with interrupt handlers. `Mutex`
//! yields to the scheduler while contended and is meant for long critical
//! sections (file system, GUI state) that must never be touched from an IRQ.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::cpu;

pub struct Spin<T: ?Sized> {
    locked: AtomicBool,
    /// Where the current holder took the lock (a `&'static Location`), for
    /// the deadlock report.
    holder: core::sync::atomic::AtomicUsize,
    data: UnsafeCell<T>,
}

/// Write straight to COM1 without any lock (the deadlock report must not
/// need the locks that may be stuck).
fn raw_serial(s: &str) {
    for b in s.bytes() {
        unsafe {
            let mut n = 0;
            while cpu::inb(0x3f8 + 5) & 0x20 == 0 && n < 100_000 {
                n += 1;
            }
            cpu::outb(0x3f8, b);
        }
    }
}

struct RawSerial;

impl core::fmt::Write for RawSerial {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        raw_serial(s);
        Ok(())
    }
}
static REPORTED: AtomicBool = AtomicBool::new(false);

unsafe impl<T: ?Sized + Send> Sync for Spin<T> {}
unsafe impl<T: ?Sized + Send> Send for Spin<T> {}

impl<T> Spin<T> {
    pub const fn new(v: T) -> Self {
        Spin { locked: AtomicBool::new(false), holder: core::sync::atomic::AtomicUsize::new(0), data: UnsafeCell::new(v) }
    }
}

impl<T: ?Sized> Spin<T> {
    /// Raw access without locking; only for the panic path.
    pub fn data_ptr(&self) -> *const T {
        self.data.get()
    }

    #[track_caller]
    pub fn lock(&self) -> SpinGuard<'_, T> {
        let irq = cpu::interrupts_enabled();
        cpu::cli();
        let here = core::panic::Location::caller();
        let mut start = 0u64;
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
            // Waiting more than a few seconds with interrupts off means a
            // deadlock: say where, once, on the serial port.
            let now = cpu::rdtsc();
            if start == 0 {
                start = now;
            } else if now - start > 8_000_000_000 && !REPORTED.swap(true, Ordering::AcqRel) {
                use core::fmt::Write;
                let h = self.holder.load(Ordering::Relaxed) as *const core::panic::Location<'static>;
                let _ = write!(RawSerial, "\nDEADLOCK: cpu {} waits at {} for a spin lock ", crate::arch::percpu::index(), here);
                if h.is_null() {
                    let _ = writeln!(RawSerial, "(holder unknown)");
                } else {
                    let _ = writeln!(RawSerial, "held since {}", unsafe { &*h });
                }
            }
        }
        self.holder.store(here as *const _ as usize, Ordering::Relaxed);
        SpinGuard { lock: self, irq }
    }

    pub fn try_lock(&self) -> Option<SpinGuard<'_, T>> {
        let irq = cpu::interrupts_enabled();
        cpu::cli();
        if self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            Some(SpinGuard { lock: self, irq })
        } else {
            if irq {
                cpu::sti();
            }
            None
        }
    }

}

pub struct SpinGuard<'a, T: ?Sized> {
    lock: &'a Spin<T>,
    irq: bool,
}

impl<T: ?Sized> Deref for SpinGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized> DerefMut for SpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T: ?Sized> Drop for SpinGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
        if self.irq {
            cpu::sti();
        }
    }
}

pub struct Mutex<T: ?Sized> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}
unsafe impl<T: ?Sized + Send> Send for Mutex<T> {}

impl<T> Mutex<T> {
    pub const fn new(v: T) -> Self {
        Mutex { locked: AtomicBool::new(false), data: UnsafeCell::new(v) }
    }
}

impl<T: ?Sized> Mutex<T> {
    pub fn lock(&self) -> MutexGuard<'_, T> {
        while self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            crate::proc::sched::yield_now();
        }
        MutexGuard { lock: self }
    }

    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        self.locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| MutexGuard { lock: self })
    }
}

pub struct MutexGuard<'a, T: ?Sized> {
    lock: &'a Mutex<T>,
}

impl<T: ?Sized> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T: ?Sized> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

/// A value initialised exactly once during boot.
pub struct Once<T> {
    init: AtomicBool,
    data: UnsafeCell<Option<T>>,
}

unsafe impl<T: Send + Sync> Sync for Once<T> {}

impl<T> Once<T> {
    pub const fn new() -> Self {
        Once { init: AtomicBool::new(false), data: UnsafeCell::new(None) }
    }

    pub fn set(&self, v: T) {
        assert!(!self.init.load(Ordering::Acquire), "Once::set called twice");
        unsafe { *self.data.get() = Some(v) };
        self.init.store(true, Ordering::Release);
    }

    pub fn get(&self) -> Option<&T> {
        if self.init.load(Ordering::Acquire) {
            unsafe { (*self.data.get()).as_ref() }
        } else {
            None
        }
    }
}
