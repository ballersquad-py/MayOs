//! Interrupt Descriptor Table and the assembly entry stubs.
//!
//! Every vector enters through a 16-byte stub that pushes a uniform
//! `TrapFrame` and calls `crate::interrupts::dispatch`. The dispatcher returns
//! the stack pointer to resume from, which is how the scheduler switches
//! threads: it hands back a different thread's saved frame.
//!
//! `syscall` builds the same frame by hand, so system calls, IRQs, exceptions
//! and voluntary yields all share one save/restore path.

use core::arch::global_asm;
use core::mem::size_of;
use core::ptr::addr_of;

use super::gdt::KERNEL_CS;

pub const VEC_TIMER: u8 = 0x20;
pub const VEC_KEYBOARD: u8 = 0x21;
pub const VEC_MOUSE: u8 = 0x2c;
pub const VEC_SYSCALL: u8 = 0x80;
pub const VEC_YIELD: u8 = 0x81;
pub const VEC_SPURIOUS: u8 = 0xff;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct TrapFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub vector: u64,
    pub error: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

impl TrapFrame {
    pub fn from_user(&self) -> bool {
        self.cs & 3 == 3
    }
}

global_asm!(
    r#"
    .section .text
    .global isr_stubs
    .align 16
isr_stubs:
    .set vec, 0
    .rept 256
        .align 16
        .if vec == 8 || vec == 10 || vec == 11 || vec == 12 || vec == 13 || vec == 14 || vec == 17 || vec == 21 || vec == 29 || vec == 30
        .else
            pushq $0
        .endif
        pushq $vec
        jmp isr_common
        .set vec, vec + 1
    .endr

isr_common:
    pushq %rax
    pushq %rbx
    pushq %rcx
    pushq %rdx
    pushq %rsi
    pushq %rdi
    pushq %rbp
    pushq %r8
    pushq %r9
    pushq %r10
    pushq %r11
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15
    movq %rsp, %rdi
    cld
    call interrupt_dispatch
    movq %rax, %rsp
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %r11
    popq %r10
    popq %r9
    popq %r8
    popq %rbp
    popq %rdi
    popq %rsi
    popq %rdx
    popq %rcx
    popq %rbx
    popq %rax
    addq $16, %rsp
    iretq

    .global syscall_entry
syscall_entry:
    movq %rsp, syscall_user_rsp(%rip)
    movq syscall_kernel_rsp(%rip), %rsp
    pushq $0x1b
    pushq syscall_user_rsp(%rip)
    pushq %r11
    pushq $0x23
    pushq %rcx
    pushq $0
    pushq $0x80
    jmp isr_common

    .section .data
    .align 16
    .global syscall_user_rsp
syscall_user_rsp: .quad 0
    .global syscall_kernel_rsp
syscall_kernel_rsp: .quad 0
    "#,
    options(att_syntax)
);

unsafe extern "C" {
    static isr_stubs: u8;
    fn syscall_entry();
    static mut syscall_kernel_rsp: u64;
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Gate {
    off_lo: u16,
    sel: u16,
    ist: u8,
    attr: u8,
    off_mid: u16,
    off_hi: u32,
    _zero: u32,
}

static mut IDT: [Gate; 256] =
    [Gate { off_lo: 0, sel: 0, ist: 0, attr: 0, off_mid: 0, off_hi: 0, _zero: 0 }; 256];

#[repr(C, packed)]
struct Pointer {
    limit: u16,
    base: u64,
}

pub fn init() {
    unsafe {
        let base = addr_of!(isr_stubs) as u64;
        let idt = &raw mut IDT;
        for v in 0..256usize {
            let h = base + (v as u64) * 16;
            (*idt)[v] = Gate {
                off_lo: h as u16,
                sel: KERNEL_CS,
                ist: if v == 8 { 1 } else { 0 },
                attr: 0x8e,
                off_mid: (h >> 16) as u16,
                off_hi: (h >> 32) as u32,
                _zero: 0,
            };
        }
        let ptr = Pointer { limit: (size_of::<[Gate; 256]>() - 1) as u16, base: idt as u64 };
        core::arch::asm!("lidt [{}]", in(reg) &ptr);
    }
}

/// Configure `syscall`/`sysret` MSRs and enable NX pages.
pub fn init_syscall() {
    use super::cpu::*;
    unsafe {
        let efer = rdmsr(MSR_EFER);
        wrmsr(MSR_EFER, efer | 1 | (1 << 11));
        wrmsr(MSR_STAR, (0x10u64 << 48) | ((KERNEL_CS as u64) << 32));
        wrmsr(MSR_LSTAR, syscall_entry as *const () as u64);
        // Clear IF, TF, DF and AC on entry.
        wrmsr(MSR_SFMASK, 0x200 | 0x100 | 0x400 | 0x40000);
    }
}

pub fn set_syscall_stack(top: u64) {
    unsafe { *(&raw mut syscall_kernel_rsp) = top };
}

pub fn enable_nx() {
    use super::cpu::*;
    unsafe {
        let efer = rdmsr(MSR_EFER);
        wrmsr(MSR_EFER, efer | (1 << 11));
    }
}
