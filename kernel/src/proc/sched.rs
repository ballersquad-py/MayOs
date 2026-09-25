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
    Blocked,
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
    let (kstack, top) = new_stack();
    let frame = frame_for(top);
    let rsp = push_frame(top, frame);
    let pml4 = process.as_ref().map(|p| p.pml4).unwrap_or_else(crate::mem::paging::kernel_pml4);
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
    }));
    id
}

pub fn spawn_kernel(name: &str, entry: extern "C" fn(usize), arg: usize) -> u64 {
    add_thread(
        name,
        |top| idt::TrapFrame {
            rip: kernel_thread_trampoline as usize as u64,
            rdi: entry as usize as u64,
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

pub fn current_id() -> u64 {
    let s = SCHED.lock();
    s.threads[s.current].id
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
