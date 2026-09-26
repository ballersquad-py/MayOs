//! Global Descriptor Table and Task State Segment, one of each per CPU.
//!
//! The segment order matters for `syscall`/`sysret`:
//! 0x08 kernel code, 0x10 kernel data, 0x18 user data, 0x20 user code.

use alloc::vec;
use core::arch::asm;
use core::mem::size_of;

use super::percpu::{self, PerCpu, Tss};

pub const KERNEL_CS: u16 = 0x08;
pub const KERNEL_DS: u16 = 0x10;
pub const USER_DS: u16 = 0x18 | 3;
pub const USER_CS: u16 = 0x20 | 3;
const TSS_SEL: u16 = 0x28;

#[repr(C, packed)]
struct Pointer {
    limit: u16,
    base: u64,
}

/// Load this CPU's GDT and TSS (in its per-CPU area) and point GS at the
/// area again (loading segments clears the GS base).
pub fn init_cpu(pc: &'static mut PerCpu) {
    // Double-fault stack.
    let df = vec![0u8; 16384].leak();
    pc.tss.ist[0] = df.as_ptr() as u64 + df.len() as u64;
    pc.gdt = [
        0,
        0x00af_9a00_0000_ffff, // kernel code
        0x00cf_9200_0000_ffff, // kernel data
        0x00cf_f200_0000_ffff, // user data
        0x00af_fa00_0000_ffff, // user code
        0,                     // TSS low
        0,                     // TSS high
    ];
    let base = &raw const pc.tss as u64;
    let limit = (size_of::<Tss>() - 1) as u64;
    pc.gdt[5] = (limit & 0xffff) | ((base & 0xff_ffff) << 16) | (0x89 << 40) | (((limit >> 16) & 0xf) << 48) | (((base >> 24) & 0xff) << 56);
    pc.gdt[6] = base >> 32;
    let ptr = Pointer { limit: (size_of::<[u64; 7]>() - 1) as u16, base: pc.gdt.as_ptr() as u64 };
    let self_ptr = pc.self_ptr;
    unsafe {
        asm!(
            "lgdt [{p}]",
            "push {cs}",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            "mov ds, {ds:x}",
            "mov es, {ds:x}",
            "mov ss, {ds:x}",
            "xor {tmp:e}, {tmp:e}",
            "mov fs, {tmp:x}",
            "mov gs, {tmp:x}",
            "ltr {tss:x}",
            p = in(reg) &ptr,
            cs = in(reg) KERNEL_CS as u64,
            ds = in(reg) KERNEL_DS as u64,
            tss = in(reg) TSS_SEL as u64,
            tmp = out(reg) _,
        );
        super::cpu::wrmsr(super::cpu::MSR_GS_BASE, self_ptr);
    }
}

/// Stack used when an interrupt arrives while running in ring 3.
pub fn set_kernel_stack(top: u64) {
    let pc = percpu::this();
    let p = &raw mut pc.tss;
    unsafe { core::ptr::write_unaligned(&raw mut (*p).rsp[0], top) };
}
