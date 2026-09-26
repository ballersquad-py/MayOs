//! Multiple CPUs: start the application processors through Limine's MP
//! feature, and keep their TLBs in step when user mappings change.

use alloc::boxed::Box;
use alloc::vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::arch::{apic, cpu, gdt, idt, percpu};
use crate::proc::sched;

/// CPUs running (the boot CPU included).
static ONLINE: AtomicUsize = AtomicUsize::new(1);

struct ApBoot {
    stack_top: u64,
    index: usize,
}

core::arch::global_asm!(
    r#"
    .global ap_trampoline
ap_trampoline:
    movq 24(%rdi), %rax
    movq (%rax), %rsp
    call ap_main
1:  hlt
    jmp 1b
    "#,
    options(att_syntax)
);

unsafe extern "C" {
    fn ap_trampoline();
}

/// Start every other CPU (unless `nosmp` is on the command line).
pub fn start() {
    let Some(r) = crate::boot::MP.response() else { return };
    if crate::boot::cmdline().contains("nosmp") {
        return;
    }
    let infos = unsafe { core::slice::from_raw_parts(r.cpus, r.cpu_count as usize) };
    let mut next = 1usize;
    for &info in infos {
        let info = unsafe { &*info };
        if info.lapic_id == r.bsp_lapic_id || next >= percpu::MAX_CPUS {
            continue;
        }
        let stack = vec![0u8; 64 * 1024].leak();
        let boot = Box::leak(Box::new(ApBoot { stack_top: (stack.as_ptr() as u64 + stack.len() as u64) & !15, index: next }));
        let p = info as *const crate::boot::MpInfo as *mut crate::boot::MpInfo;
        unsafe { (*p).extra_argument = boot as *const ApBoot as u64 };
        info.goto_address.store(ap_trampoline as *const () as u64, Ordering::SeqCst);
        next += 1;
    }
    // Wait for them (a second at most).
    let until = crate::time::uptime_ms() + 1000;
    while ONLINE.load(Ordering::Acquire) < next && crate::time::uptime_ms() < until {
        core::hint::spin_loop();
    }
    crate::kprintln!("smp: {} cpu(s) online", ONLINE.load(Ordering::Acquire));
}

#[unsafe(no_mangle)]
extern "C" fn ap_main(info: *const crate::boot::MpInfo) -> ! {
    let info = unsafe { &*info };
    let boot = unsafe { &*(info.extra_argument as *const ApBoot) };
    unsafe { cpu::write_cr3(crate::mem::paging::kernel_pml4()) };
    let pc = percpu::install(boot.index, info.lapic_id);
    gdt::init_cpu(pc);
    idt::load();
    idt::init_syscall();
    cpu::enable_sse();
    apic::init_ap();
    sched::init_ap(boot.index);
    let n = ONLINE.fetch_add(1, Ordering::AcqRel) + 1;
    percpu::set_count(n);
    loop {
        cpu::sti_hlt();
    }
}

pub fn online() -> usize {
    ONLINE.load(Ordering::Acquire)
}

static SHOOT_LOCK: AtomicBool = AtomicBool::new(false);
static TARGET: AtomicU64 = AtomicU64::new(0);
static PENDING: AtomicUsize = AtomicUsize::new(0);

/// User mappings of `pml4` changed (unmapped or made stricter): make every
/// CPU drop stale translations.
pub fn tlb_shootdown(pml4: u64) {
    // This CPU: reload CR3 if it is the changed address space.
    if cpu::read_cr3() & 0x000f_ffff_ffff_f000 == pml4 {
        unsafe { cpu::write_cr3(pml4) };
    }
    let n = online();
    if n <= 1 {
        return;
    }
    while SHOOT_LOCK.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        core::hint::spin_loop();
    }
    TARGET.store(pml4, Ordering::Release);
    PENDING.store(n - 1, Ordering::Release);
    apic::ipi_others(idt::VEC_TLB);
    let until = crate::time::uptime_ms() + 100;
    while PENDING.load(Ordering::Acquire) > 0 && crate::time::uptime_ms() < until {
        core::hint::spin_loop();
    }
    SHOOT_LOCK.store(false, Ordering::Release);
}

pub fn on_tlb_ipi() {
    let t = TARGET.load(Ordering::Acquire);
    let cr3 = cpu::read_cr3() & 0x000f_ffff_ffff_f000;
    if cr3 == t {
        unsafe { cpu::write_cr3(cr3) };
    }
    PENDING.fetch_sub(1, Ordering::AcqRel);
}
