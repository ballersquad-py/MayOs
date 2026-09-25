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
    data: UnsafeCell<T>,
}

unsafe impl<T: ?Sized + Send> Sync for Spin<T> {}
unsafe impl<T: ?Sized + Send> Send for Spin<T> {}

impl<T> Spin<T> {
    pub const fn new(v: T) -> Self {
        Spin { locked: AtomicBool::new(false), data: UnsafeCell::new(v) }
    }
}

impl<T: ?Sized> Spin<T> {
    pub fn lock(&self) -> SpinGuard<'_, T> {
        let irq = cpu::interrupts_enabled();
        cpu::cli();
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        SpinGuard { lock: self, irq }
    }

    /// Used by the panic handler, which must never deadlock.
    pub unsafe fn force_unlock(&self) {
        self.locked.store(false, Ordering::Release);
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
