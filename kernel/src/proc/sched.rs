//! Preemptive round-robin scheduler (single CPU).
//!
//! A thread's saved context is simply a pointer to the `TrapFrame` sitting on
//! top of its kernel stack. Switching threads means returning a different
//! frame pointer from the interrupt dispatcher.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::sync::atomic::{AtomicBool, Ordering};

use super::process::Process;
use crate::arch::{cpu, gdt, idt};
use crate::sync::Spin;

const KSTACK_SIZE: usize = 128 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Ready,
    Running,
    Sleeping(u64),
    Dead,
}

pub struct Thread {
    pub id: u64,
    pub name: String,
    rsp: u64,
    kstack: *mut u8,
    kstack_top: u64,
    pub state: State,
    pub process: Option<Arc<Process>>,
    pml4: u64,
    pub cpu_ms: u64,
    /// Thread-local storage pointer (FS base) of Linux threads.
    fs_base: u64,
    fpu: Box<cpu::FpuState>,
    /// Linux `set_tid_address`/CLONE_CHILD_CLEARTID: zeroed and woken at exit.
    pub clear_child_tid: u64,
}

unsafe impl Send for Thread {}

impl Drop for Thread {
    fn drop(&mut self) {
        if !self.kstack.is_null() {
            unsafe { alloc::alloc::dealloc(self.kstack, Layout::from_size_align(KSTACK_SIZE, 16).unwrap()) };
        }
    }
}

struct Scheduler {
    threads: Vec<Box<Thread>>,
    current: usize,
    next_id: u64,
    slice_start: u64,
}

static SCHED: Spin<Scheduler> =
    Spin::new(Scheduler { threads: Vec::new(), current: 0, next_id: 1, slice_start: 0 });
static STARTED: AtomicBool = AtomicBool::new(false);

/// Register the boot context as thread 0, the idle thread.
pub fn init() {
    let mut s = SCHED.lock();
    s.threads.push(Box::new(Thread {
        id: 0,
        name: String::from("idle"),
        rsp: 0,
        kstack: core::ptr::null_mut(),
        kstack_top: 0,
        state: State::Running,
        process: None,
        pml4: crate::mem::paging::kernel_pml4(),
        cpu_ms: 0,
        fs_base: 0,
        fpu: Box::new(cpu::fpu_initial()),
        clear_child_tid: 0,
    }));
    s.current = 0;
}

pub fn start() {
    STARTED.store(true, Ordering::Release);
}

extern "C" fn kernel_thread_trampoline(entry: extern "C" fn(usize), arg: usize) -> ! {
    cpu::sti();
    entry(arg);
    exit_current();
}

fn new_stack() -> (*mut u8, u64) {
    let stack = unsafe { alloc::alloc::alloc(Layout::from_size_align(KSTACK_SIZE, 16).unwrap()) };
    assert!(!stack.is_null(), "out of memory for kernel stack");
    (stack, stack as u64 + KSTACK_SIZE as u64)
}

fn push_frame(top: u64, frame: idt::TrapFrame) -> u64 {
    let rsp = top - core::mem::size_of::<idt::TrapFrame>() as u64;
    unsafe { core::ptr::write(rsp as *mut idt::TrapFrame, frame) };
    rsp
}

fn add_thread(name: &str, frame_for: impl FnOnce(u64) -> idt::TrapFrame, process: Option<Arc<Process>>) -> u64 {
    add_thread_with(name, frame_for, process, 0, cpu::fpu_initial())
}

fn add_thread_with(name: &str, frame_for: impl FnOnce(u64) -> idt::TrapFrame, process: Option<Arc<Process>>, fs_base: u64, fpu: cpu::FpuState) -> u64 {
    let (kstack, top) = new_stack();
    let frame = frame_for(top);
    let rsp = push_frame(top, frame);
    let pml4 = process.as_ref().map(|p| p.pml4()).unwrap_or_else(crate::mem::paging::kernel_pml4);
    let mut s = SCHED.lock();
    let id = s.next_id;
    s.next_id += 1;
    s.threads.push(Box::new(Thread {
        id,
        name: String::from(name),
        rsp,
        kstack,
        kstack_top: top,
        state: State::Ready,
        process,
        pml4,
        cpu_ms: 0,
        fs_base,
        fpu: Box::new(fpu),
        clear_child_tid: 0,
    }));
    id
}

/// Start another thread of a user process from a full register frame
/// (Linux `clone`): the new thread shares the address space.
pub fn spawn_user_frame(process: Arc<Process>, frame: idt::TrapFrame, fs_base: u64) -> u64 {
    let name = process.name.clone();
    // The child starts with the parent's floating-point state.
    let mut fpu = cpu::fpu_initial();
    cpu::fxsave(&mut fpu);
    add_thread_with(&name, |_| frame, Some(process), fs_base, fpu)
}

/// Set the calling thread's FS base (thread-local storage).
pub fn set_fs_base(v: u64) {
    let mut s = SCHED.lock();
    let c = s.current;
    s.threads[c].fs_base = v;
    unsafe { cpu::wrmsr(cpu::MSR_FS_BASE, v) };
}

pub fn fs_base() -> u64 {
    let s = SCHED.lock();
    s.threads[s.current].fs_base
}

pub fn current_id() -> u64 {
    let s = SCHED.lock();
    s.threads[s.current].id
}

pub fn set_clear_child_tid(addr: u64) {
    let mut s = SCHED.lock();
    let c = s.current;
    s.threads[c].clear_child_tid = addr;
}

/// Set the clear-child-tid address of another thread (after `clone`).
pub fn set_clear_child_tid_of(id: u64, addr: u64) {
    let mut s = SCHED.lock();
    if let Some(t) = s.threads.iter_mut().find(|t| t.id == id) {
        t.clear_child_tid = addr;
    }
}

pub fn clear_child_tid() -> u64 {
    let s = SCHED.lock();
    s.threads[s.current].clear_child_tid
}

/// Threads (ids) of a process that are still alive.
pub fn process_thread_count(pid: u64) -> usize {
    let s = SCHED.lock();
    s.threads.iter().filter(|t| t.state != State::Dead && t.process.as_ref().map(|p| p.pid == pid).unwrap_or(false)).count()
}

pub fn spawn_kernel(name: &str, entry: extern "C" fn(usize), arg: usize) -> u64 {
    add_thread(
        name,
        |top| idt::TrapFrame {
            rip: kernel_thread_trampoline as *const () as u64,
            rdi: entry as *const () as u64,
            rsi: arg as u64,
            cs: gdt::KERNEL_CS as u64,
            ss: gdt::KERNEL_DS as u64,
            rflags: 0x202,
            rsp: top - 8,
            ..Default::default()
        },
        None,
    )
}

pub fn spawn_user(process: Arc<Process>, entry: u64, user_rsp: u64, arg0: u64, arg1: u64) -> u64 {
    let name = process.name.clone();
    add_thread(
        &name,
        |_| idt::TrapFrame {
            rip: entry,
            rdi: arg0,
            rsi: arg1,
            cs: gdt::USER_CS as u64,
            ss: gdt::USER_DS as u64,
            rflags: 0x202,
            rsp: user_rsp,
            ..Default::default()
        },
        Some(process),
    )
}

/// Called from the interrupt dispatcher with interrupts disabled. Saves the
/// current frame and returns the frame of the thread to run next.
pub fn schedule(frame_rsp: u64) -> u64 {
    if !STARTED.load(Ordering::Acquire) {
        return frame_rsp;
    }
    let mut s = SCHED.lock();
    let now = crate::time::uptime_ms();
    let cur = s.current;
    let elapsed = now.saturating_sub(s.slice_start);
    {
        let t = &mut s.threads[cur];
        t.rsp = frame_rsp;
        t.cpu_ms += elapsed;
        if t.state == State::Running {
            t.state = State::Ready;
        }
    }

    // Reap dead threads other than the one whose stack we are standing on.
    let mut i = 1;
    while i < s.threads.len() {
        if s.threads[i].state == State::Dead && i != s.current {
            let t = s.threads.remove(i);
            if i < s.current {
                s.current -= 1;
            }
            drop(t);
        } else {
            i += 1;
        }
    }
    let cur = s.current;

    for t in s.threads.iter_mut() {
        if let State::Sleeping(until) = t.state
            && now >= until
        {
            t.state = State::Ready;
        }
    }

    let n = s.threads.len();
    let mut next = 0;
    for k in 1..=n {
        let i = (cur + k) % n;
        if i != 0 && s.threads[i].state == State::Ready {
            next = i;
            break;
        }
    }
    if next != cur {
        // Floating-point registers and thread-local storage go with the thread.
        cpu::fxsave(&mut s.threads[cur].fpu);
        cpu::fxrstor(&s.threads[next].fpu);
        if s.threads[next].fs_base != s.threads[cur].fs_base {
            unsafe { cpu::wrmsr(cpu::MSR_FS_BASE, s.threads[next].fs_base) };
        }
    }
    s.current = next;
    s.slice_start = now;
    let t = &mut s.threads[next];
    t.state = State::Running;
    if t.kstack_top != 0 {
        gdt::set_kernel_stack(t.kstack_top);
        idt::set_syscall_stack(t.kstack_top);
    }
    if cpu::read_cr3() & 0x000f_ffff_ffff_f000 != t.pml4 {
        unsafe { cpu::write_cr3(t.pml4) };
    }
    t.rsp
}

pub fn yield_now() {
    if STARTED.load(Ordering::Acquire) {
        unsafe { core::arch::asm!("int 0x81") };
    }
}

fn set_current_state(state: State) {
    let mut s = SCHED.lock();
    let c = s.current;
    s.threads[c].state = state;
}

pub fn sleep_ms(ms: u64) {
    if !STARTED.load(Ordering::Acquire) {
        let until = crate::time::uptime_ms() + ms;
        while crate::time::uptime_ms() < until {
            core::hint::spin_loop();
        }
        return;
    }
    set_current_state(State::Sleeping(crate::time::uptime_ms() + ms));
    yield_now();
}

pub fn exit_current() -> ! {
    set_current_state(State::Dead);
    yield_now();
    unreachable!("dead thread was scheduled");
}

pub fn current_process() -> Option<Arc<Process>> {
    let s = SCHED.lock();
    s.threads[s.current].process.clone()
}

/// Mark every thread of a process dead (used when it exits or faults).
pub fn kill_process_threads(pid: u64) {
    let mut s = SCHED.lock();
    for t in s.threads.iter_mut() {
        if t.process.as_ref().map(|p| p.pid) == Some(pid) {
            t.state = State::Dead;
        }
    }
}

/// End every thread of `pid` except the calling one (execve).
pub fn kill_other_threads(pid: u64) {
    let mut s = SCHED.lock();
    let cur = s.current;
    for (i, t) in s.threads.iter_mut().enumerate() {
        if i != cur && t.process.as_ref().map(|p| p.pid) == Some(pid) {
            t.state = State::Dead;
        }
    }
}

/// Run the calling thread in another address space from now on.
pub fn switch_address_space(pml4: u64) {
    let mut s = SCHED.lock();
    let c = s.current;
    s.threads[c].pml4 = pml4;
    s.threads[c].fs_base = 0;
    unsafe {
        cpu::write_cr3(pml4);
        cpu::wrmsr(cpu::MSR_FS_BASE, 0);
    }
}

pub struct ThreadInfo {
    pub id: u64,
    pub name: String,
    pub state: State,
    pub pid: Option<u64>,
    pub cpu_ms: u64,
}

pub fn list() -> Vec<ThreadInfo> {
    let s = SCHED.lock();
    s.threads
        .iter()
        .map(|t| ThreadInfo {
            id: t.id,
            name: t.name.clone(),
            state: t.state,
            pid: t.process.as_ref().map(|p| p.pid),
            cpu_ms: t.cpu_ms,
        })
        .collect()
}
