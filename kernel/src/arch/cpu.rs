//! Thin wrappers around x86_64 instructions.

use core::arch::asm;

#[inline]
pub unsafe fn outb(port: u16, v: u8) {
    unsafe { asm!("out dx, al", in("dx") port, in("al") v, options(nomem, nostack, preserves_flags)) }
}

#[inline]
pub unsafe fn inb(port: u16) -> u8 {
    let v: u8;
    unsafe { asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack, preserves_flags)) }
    v
}

#[inline]
pub unsafe fn outw(port: u16, v: u16) {
    unsafe { asm!("out dx, ax", in("dx") port, in("ax") v, options(nomem, nostack, preserves_flags)) }
}

#[inline]
pub unsafe fn inw(port: u16) -> u16 {
    let v: u16;
    unsafe { asm!("in ax, dx", out("ax") v, in("dx") port, options(nomem, nostack, preserves_flags)) }
    v
}

#[inline]
pub unsafe fn outl(port: u16, v: u32) {
    unsafe { asm!("out dx, eax", in("dx") port, in("eax") v, options(nomem, nostack, preserves_flags)) }
}

#[inline]
pub unsafe fn inl(port: u16) -> u32 {
    let v: u32;
    unsafe { asm!("in eax, dx", out("eax") v, in("dx") port, options(nomem, nostack, preserves_flags)) }
    v
}

#[inline]
pub unsafe fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    unsafe { asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack)) }
    ((hi as u64) << 32) | lo as u64
}

#[inline]
pub unsafe fn wrmsr(msr: u32, v: u64) {
    unsafe {
        asm!("wrmsr", in("ecx") msr, in("eax") v as u32, in("edx") (v >> 32) as u32, options(nomem, nostack))
    }
}

#[inline]
pub fn read_cr2() -> u64 {
    let v: u64;
    unsafe { asm!("mov {}, cr2", out(reg) v, options(nomem, nostack)) }
    v
}

#[inline]
pub fn read_cr3() -> u64 {
    let v: u64;
    unsafe { asm!("mov {}, cr3", out(reg) v, options(nomem, nostack)) }
    v
}

#[inline]
pub unsafe fn write_cr3(v: u64) {
    unsafe { asm!("mov cr3, {}", in(reg) v, options(nostack)) }
}

#[inline]
pub fn invlpg(addr: u64) {
    unsafe { asm!("invlpg [{}]", in(reg) addr, options(nostack)) }
}

#[inline]
pub fn cli() {
    unsafe { asm!("cli", options(nomem, nostack)) }
}

#[inline]
pub fn sti() {
    unsafe { asm!("sti", options(nomem, nostack)) }
}

#[inline]
pub fn hlt() {
    unsafe { asm!("hlt", options(nomem, nostack)) }
}

/// Enable interrupts and halt atomically (no wakeup can be missed).
#[inline]
pub fn sti_hlt() {
    unsafe { asm!("sti; hlt", options(nomem, nostack)) }
}

#[inline]
pub fn interrupts_enabled() -> bool {
    let f: u64;
    unsafe { asm!("pushfq; pop {}", out(reg) f, options(nomem, preserves_flags)) }
    f & 0x200 != 0
}

#[inline]
pub fn rdtsc() -> u64 {
    let (lo, hi): (u32, u32);
    unsafe { asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack)) }
    ((hi as u64) << 32) | lo as u64
}

pub fn halt_forever() -> ! {
    loop {
        cli();
        hlt();
    }
}

pub const MSR_EFER: u32 = 0xc000_0080;
pub const MSR_STAR: u32 = 0xc000_0081;
pub const MSR_LSTAR: u32 = 0xc000_0082;
pub const MSR_SFMASK: u32 = 0xc000_0084;

pub const MSR_FS_BASE: u32 = 0xc000_0100;
pub const MSR_GS_BASE: u32 = 0xc000_0101;

/// Saved x87/SSE register state (`fxsave` format).
#[repr(C, align(16))]
#[derive(Clone)]
pub struct FpuState(pub [u8; 512]);

static mut FPU_INIT: FpuState = FpuState([0; 512]);

/// Turn on SSE for user programs (the kernel itself never uses it) and
/// record the clean register state new threads start with.
pub fn enable_sse() {
    unsafe {
        let mut cr0: u64;
        asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack));
        cr0 &= !(1 << 2); // EM: no emulation
        cr0 |= 1 << 1; // MP
        cr0 &= !(1 << 3); // TS: no lazy switching
        asm!("mov cr0, {}", in(reg) cr0, options(nomem, nostack));
        let mut cr4: u64;
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
        cr4 |= (1 << 9) | (1 << 10); // OSFXSR, OSXMMEXCPT
        asm!("mov cr4, {}", in(reg) cr4, options(nomem, nostack));
        let mxcsr: u32 = 0x1f80;
        asm!("fninit", "ldmxcsr [{}]", in(reg) &mxcsr, options(nostack));
        asm!("fxsave64 [{}]", in(reg) &raw mut FPU_INIT, options(nostack));
    }
}

pub fn fpu_initial() -> FpuState {
    unsafe { (*(&raw const FPU_INIT)).clone() }
}

#[inline]
pub fn fxsave(s: &mut FpuState) {
    unsafe { asm!("fxsave64 [{}]", in(reg) s as *mut FpuState, options(nostack)) }
}

#[inline]
pub fn fxrstor(s: &FpuState) {
    unsafe { asm!("fxrstor64 [{}]", in(reg) s as *const FpuState, options(nostack)) }
}
