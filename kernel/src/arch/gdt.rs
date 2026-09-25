//! Global Descriptor Table and Task State Segment.
//!
//! The segment order matters for `syscall`/`sysret`:
//! 0x08 kernel code, 0x10 kernel data, 0x18 user data, 0x20 user code.

use core::arch::asm;
use core::mem::size_of;
use core::ptr::addr_of;

pub const KERNEL_CS: u16 = 0x08;
pub const KERNEL_DS: u16 = 0x10;
pub const USER_DS: u16 = 0x18 | 3;
pub const USER_CS: u16 = 0x20 | 3;
const TSS_SEL: u16 = 0x28;

#[repr(C, packed)]
struct Tss {
    _r0: u32,
    rsp: [u64; 3],
    _r1: u64,
    ist: [u64; 7],
    _r2: u64,
    _r3: u16,
    iomap_base: u16,
}

static mut TSS: Tss = Tss {
    _r0: 0,
    rsp: [0; 3],
    _r1: 0,
    ist: [0; 7],
    _r2: 0,
    _r3: 0,
    iomap_base: size_of::<Tss>() as u16,
};

static mut GDT: [u64; 7] = [
    0,
    0x00af_9a00_0000_ffff, // kernel code
    0x00cf_9200_0000_ffff, // kernel data
    0x00cf_f200_0000_ffff, // user data
    0x00af_fa00_0000_ffff, // user code
    0,                     // TSS low
    0,                     // TSS high
];

#[repr(C, align(16))]
struct Stack([u8; 16384]);
static mut DOUBLE_FAULT_STACK: Stack = Stack([0; 16384]);

#[repr(C, packed)]
struct Pointer {
    limit: u16,
    base: u64,
}

pub fn init() {
    unsafe {
        let df_top = addr_of!(DOUBLE_FAULT_STACK) as u64 + 16384;
        TSS.ist[0] = df_top;

        let base = addr_of!(TSS) as u64;
        let limit = (size_of::<Tss>() - 1) as u64;
        GDT[5] = (limit & 0xffff)
            | ((base & 0xff_ffff) << 16)
            | (0x89 << 40)
            | (((limit >> 16) & 0xf) << 48)
            | (((base >> 24) & 0xff) << 56);
        GDT[6] = base >> 32;

        let ptr = Pointer { limit: (size_of::<[u64; 7]>() - 1) as u16, base: addr_of!(GDT) as u64 };
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
    }
}

/// Stack used when an interrupt arrives while running in ring 3.
pub fn set_kernel_stack(top: u64) {
    unsafe {
        let p = &raw mut TSS;
        core::ptr::write_unaligned(&raw mut (*p).rsp[0], top);
    }
}
