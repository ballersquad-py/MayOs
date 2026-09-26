//! Central interrupt dispatcher, called from the assembly stubs in
//! `arch::idt` with a pointer to the saved `TrapFrame`.

use crate::arch::idt::{self, TrapFrame};
use crate::arch::{apic, cpu};
use crate::proc::{sched, syscall};

const EXCEPTION_NAMES: [&str; 32] = [
    "Divide error",
    "Debug",
    "NMI",
    "Breakpoint",
    "Overflow",
    "Bound range exceeded",
    "Invalid opcode",
    "Device not available",
    "Double fault",
    "Coprocessor segment overrun",
    "Invalid TSS",
    "Segment not present",
    "Stack-segment fault",
    "General protection fault",
    "Page fault",
    "Reserved",
    "x87 floating-point error",
    "Alignment check",
    "Machine check",
    "SIMD floating-point error",
    "Virtualization exception",
    "Control protection exception",
    "Reserved",
    "Reserved",
    "Reserved",
    "Reserved",
    "Reserved",
    "Reserved",
    "Hypervisor injection",
    "VMM communication",
    "Security exception",
    "Reserved",
];

#[unsafe(no_mangle)]
extern "C" fn interrupt_dispatch(frame: &mut TrapFrame) -> u64 {
    let vector = frame.vector as u8;
    let rsp = frame as *mut TrapFrame as u64;
    match vector {
        0..=31 => exception(frame),
        idt::VEC_TIMER => {
            apic::eoi();
            sched::schedule(rsp)
        }
        idt::VEC_KEYBOARD => {
            crate::drivers::ps2::on_keyboard_irq();
            apic::eoi();
            rsp
        }
        idt::VEC_MOUSE => {
            crate::drivers::ps2::on_mouse_irq();
            apic::eoi();
            rsp
        }
        idt::VEC_SYSCALL => {
            // Run the system call with interrupts enabled so the rest of the
            // system keeps moving; it may block and switch threads.
            cpu::sti();
            let exit = syscall::handle(frame);
            cpu::cli();
            if exit { sched::schedule(rsp) } else { rsp }
        }
        idt::VEC_YIELD => sched::schedule(rsp),
        idt::VEC_SPURIOUS => rsp,
        _ => {
            apic::eoi();
            rsp
        }
    }
}

fn exception(frame: &mut TrapFrame) -> u64 {
    let v = frame.vector as usize;
    let name = EXCEPTION_NAMES[v];
    if frame.from_user() && v == 14 && crate::proc::linux::page_fault(cpu::read_cr2(), frame.error) {
        // Memory handed out on first use (Linux programs).
        return frame as *mut TrapFrame as u64;
    }
    if frame.from_user() {
        let msg = alloc::format!(
            "\n[process crashed] {} at rip={:#x} addr={:#x} err={:#x}\n",
            name,
            frame.rip,
            if v == 14 { cpu::read_cr2() } else { 0 },
            frame.error
        );
        crate::kprint!("{}", msg);
        // Details for debugging on the serial port.
        if let Some(p) = sched::current_process() {
            let code = crate::proc::usermem::read_bytes(p.pml4(), frame.rip, 16).unwrap_or_default();
            crate::kprintln!(
                "  code {:02x?}\n  rax={:#x} rbx={:#x} rcx={:#x} rdx={:#x} rsi={:#x} rdi={:#x}\n  rsp={:#x} rbp={:#x} r8={:#x} r12={:#x} r13={:#x} r14={:#x} r15={:#x}",
                code, frame.rax, frame.rbx, frame.rcx, frame.rdx, frame.rsi, frame.rdi, frame.rsp, frame.rbp, frame.r8, frame.r12, frame.r13, frame.r14, frame.r15
            );
        }
        crate::proc::process::exit_current_process(-1, Some(&msg));
        return sched::schedule(frame as *mut TrapFrame as u64);
    }
    panic!(
        "{} (vector {}) in kernel\n rip={:#018x} err={:#x} cr2={:#018x}\n rsp={:#018x} rbp={:#018x} rax={:#018x}\n rbx={:#018x} rcx={:#018x} rdx={:#018x}",
        name,
        v,
        frame.rip,
        frame.error,
        cpu::read_cr2(),
        frame.rsp,
        frame.rbp,
        frame.rax,
        frame.rbx,
        frame.rcx,
        frame.rdx
    );
}
