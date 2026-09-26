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

/// Saved x87/SSE/AVX register state (`xsave` format when the CPU has
/// AVX, `fxsave` otherwise; the first 512 bytes are the same in both).
#[repr(C, align(64))]
#[derive(Clone)]
pub struct FpuState(pub [u8; FPU_BYTES]);

/// Room for x87 + SSE + AVX (832 bytes of xsave area).
pub const FPU_BYTES: usize = 1024;

impl FpuState {
    pub const fn zeroed() -> FpuState {
        FpuState([0; FPU_BYTES])
    }
}

static mut FPU_INIT: FpuState = FpuState::zeroed();

/// XCR0 in use (x87|SSE|AVX = 7), or 0 when only fxsave is used.
static XCR0: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Linux-style CPU flag names for /proc/cpuinfo.
pub fn cpu_flags() -> alloc::string::String {
    let (_, _, c1, d1) = cpuid(1, 0);
    let (_, b7, _, _) = cpuid(7, 0);
    let avx = xsave_mask() & 4 != 0;
    let mut f = alloc::string::String::from("fpu tsc cx8 cmov mmx fxsr sse sse2");
    let list: [(bool, &str); 14] = [
        (c1 & 1 != 0, "pni"), (c1 & (1 << 1) != 0, "pclmulqdq"), (c1 & (1 << 9) != 0, "ssse3"),
        (c1 & (1 << 12) != 0 && avx, "fma"), (c1 & (1 << 13) != 0, "cx16"), (c1 & (1 << 19) != 0, "sse4_1"),
        (c1 & (1 << 20) != 0, "sse4_2"), (c1 & (1 << 23) != 0, "popcnt"), (c1 & (1 << 25) != 0, "aes"),
        (avx, "xsave avx"), (c1 & (1 << 29) != 0 && avx, "f16c"), (b7 & (1 << 3) != 0, "bmi1"),
        (b7 & (1 << 5) != 0 && avx, "avx2"), (b7 & (1 << 8) != 0, "bmi2"),
    ];
    let _ = d1;
    for (on, name) in list {
        if on {
            f.push(' ');
            f.push_str(name);
        }
    }
    f
}

pub fn xsave_mask() -> u64 {
    XCR0.load(core::sync::atomic::Ordering::Relaxed)
}

fn cpuid(leaf: u32, sub: u32) -> (u32, u32, u32, u32) {
    let r = unsafe { core::arch::x86_64::__cpuid_count(leaf, sub) };
    (r.eax, r.ebx, r.ecx, r.edx)
}

/// Make an xsave image loadable: clear the compaction and reserved header
/// fields and keep only enabled components (signal frames come from user
/// memory).
pub fn sanitize(s: &mut FpuState) {
    let mask = xsave_mask();
    let bv = u64::from_le_bytes(s.0[512..520].try_into().unwrap()) & if mask != 0 { mask } else { 3 };
    s.0[512..520].copy_from_slice(&bv.to_le_bytes());
    for b in &mut s.0[520..576] {
        *b = 0;
    }
    let mxcsr = u32::from_le_bytes(s.0[24..28].try_into().unwrap()) & 0xffff;
    s.0[24..28].copy_from_slice(&mxcsr.to_le_bytes());
}

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
        // AVX needs XSAVE (CR4.OSXSAVE) and XCR0 with the YMM bit, so user
        // code (llvmpipe, JITs) can use 256-bit vectors.
        let (_, _, ecx, _) = cpuid(1, 0);
        let has_xsave = ecx & (1 << 26) != 0;
        let has_avx = ecx & (1 << 28) != 0;
        let supported = if has_xsave { cpuid(0xd, 0).0 as u64 } else { 0 };
        let xsave_size = if has_xsave { cpuid(0xd, 0).1 as usize } else { 0 };
        if has_xsave && has_avx && supported & 7 == 7 {
            cr4 |= 1 << 18; // OSXSAVE
        }
        asm!("mov cr4, {}", in(reg) cr4, options(nomem, nostack));
        if cr4 & (1 << 18) != 0 {
            asm!("xsetbv", in("ecx") 0u32, in("eax") 7u32, in("edx") 0u32, options(nomem, nostack));
            if cpuid(0xd, 0).1 as usize <= FPU_BYTES {
                XCR0.store(7, core::sync::atomic::Ordering::Relaxed);
            }
            let _ = xsave_size;
        }
        let mxcsr: u32 = 0x1f80;
        asm!("fninit", "ldmxcsr [{}]", in(reg) &mxcsr, options(nostack));
        (*(&raw mut FPU_INIT)).0 = [0; FPU_BYTES];
        fxsave(&mut *(&raw mut FPU_INIT));
    }
}

pub fn fpu_initial() -> FpuState {
    unsafe { (*(&raw const FPU_INIT)).clone() }
}

#[inline]
pub fn fxsave(s: &mut FpuState) {
    let m = xsave_mask();
    unsafe {
        if m != 0 {
            asm!("xsave64 [{}]", in(reg) s as *mut FpuState, in("eax") m as u32, in("edx") (m >> 32) as u32, options(nostack))
        } else {
            asm!("fxsave64 [{}]", in(reg) s as *mut FpuState, options(nostack))
        }
    }
}

#[inline]
pub fn fxrstor(s: &FpuState) {
    let m = xsave_mask();
    unsafe {
        if m != 0 {
            asm!("xrstor64 [{}]", in(reg) s as *const FpuState, in("eax") m as u32, in("edx") (m >> 32) as u32, options(nostack))
        } else {
            asm!("fxrstor64 [{}]", in(reg) s as *const FpuState, options(nostack))
        }
    }
}
