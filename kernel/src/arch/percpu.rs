//! Per-CPU data, reached through the GS base while in the kernel.
//!
//! Entering the kernel from user mode runs `swapgs`, so GS points here and
//! the user's GS base waits in IA32_KERNEL_GS_BASE; leaving for user mode
//! swaps back. The first fields are used by the assembly entry code.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicUsize, Ordering};

use super::cpu;

pub const MAX_CPUS: usize = 32;

#[repr(C, packed)]
pub struct Tss {
    pub _r0: u32,
    pub rsp: [u64; 3],
    pub _r1: u64,
    pub ist: [u64; 7],
    pub _r2: u64,
    pub _r3: u16,
    pub iomap_base: u16,
}

#[repr(C, align(64))]
pub struct PerCpu {
    /// This structure's own address (offset 0).
    pub self_ptr: u64,
    /// Kernel stack for `syscall` (offset 8).
    pub kernel_rsp: u64,
    /// User stack pointer saved by `syscall` (offset 16).
    pub user_rsp: u64,
    /// CPU number, 0 = the boot CPU (offset 24).
    pub index: u64,
    /// `on_cpu` flag of the thread switched away from, cleared once we are
    /// off its stack (offset 32).
    pub prev_on_cpu: u64,
    pub lapic_id: u32,
    pub gdt: [u64; 7],
    pub tss: Tss,
}

static CPUS: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
static COUNT: AtomicUsize = AtomicUsize::new(1);

pub const MSR_KERNEL_GS_BASE: u32 = 0xc000_0102;

/// Make a per-CPU area for CPU `index` and point GS at it.
pub fn install(index: usize, lapic_id: u32) -> &'static mut PerCpu {
    let pc: &'static mut PerCpu = Box::leak(Box::new(PerCpu {
        self_ptr: 0,
        kernel_rsp: 0,
        user_rsp: 0,
        index: index as u64,
        prev_on_cpu: 0,
        lapic_id,
        gdt: [0; 7],
        tss: Tss { _r0: 0, rsp: [0; 3], _r1: 0, ist: [0; 7], _r2: 0, _r3: 0, iomap_base: core::mem::size_of::<Tss>() as u16 },
    }));
    pc.self_ptr = pc as *mut PerCpu as u64;
    CPUS[index].store(pc.self_ptr as usize, Ordering::Release);
    unsafe {
        cpu::wrmsr(cpu::MSR_GS_BASE, pc.self_ptr);
        cpu::wrmsr(MSR_KERNEL_GS_BASE, 0);
    }
    pc
}

pub fn set_count(n: usize) {
    COUNT.store(n.clamp(1, MAX_CPUS), Ordering::Release);
}

/// CPUs running (or being started).
pub fn count() -> usize {
    COUNT.load(Ordering::Acquire)
}

/// This CPU's number.
#[inline]
pub fn index() -> usize {
    let v: u64;
    unsafe { core::arch::asm!("mov {}, gs:[24]", out(reg) v, options(nostack, readonly, preserves_flags)) };
    v as usize
}

/// This CPU's area.
pub fn this() -> &'static mut PerCpu {
    let v: u64;
    unsafe { core::arch::asm!("mov {}, gs:[0]", out(reg) v, options(nostack, readonly, preserves_flags)) };
    unsafe { &mut *(v as *mut PerCpu) }
}
