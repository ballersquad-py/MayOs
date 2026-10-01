//! Preemptive round-robin scheduler: one run queue shared by all CPUs.
//!
//! A thread's saved context is simply a pointer to the `TrapFrame` sitting on
//! top of its kernel stack. Switching threads means returning a different
//! frame pointer from the interrupt dispatcher.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::process::Process;
use crate::arch::percpu::{self, MAX_CPUS, MSR_KERNEL_GS_BASE};
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
    /// GS base of Linux threads (arch_prctl ARCH_SET_GS).
    gs_base: u64,
    /// Sleeping in `wait_event`: the event count it saw (wakes when the
    /// count moves), or NO_EVENT.
    wait_seen: u64,
    /// Sleeping in `wait_flag`: the flag (`*const AtomicBool`) that ends it.
    wait_flag: usize,
    /// Linux system call in progress (number, first two arguments), for
    /// the `threads` command; NO_EVENT when none.
    pub syscall: [u64; 3],
    /// Executing on some CPU (set until that CPU is off its stack).
    on_cpu: AtomicBool,
    /// A CPU's idle thread (runs only there, when nothing else can).
    idle: bool,
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

const NO_EVENT: u64 = u64::MAX;

/// Threads taken off the run queue, waiting to be freed by `reaper`.
static GRAVE: Spin<Vec<Box<Thread>>> = Spin::new(Vec::new());

/// Free dead threads (and, with the last one, their process) outside the
/// scheduler.
extern "C" fn reaper(_: usize) {
    loop {
        let dead: Vec<Box<Thread>> = core::mem::take(&mut *GRAVE.lock());
        drop(dead);
        sleep_ms(20);
    }
}

/// Start the reaper thread (once the scheduler exists).
pub fn start_reaper() {
    spawn_kernel("reaper", reaper, 0);
}

/// Bumped whenever a thread becomes ready to run: idle CPUs only look for
/// work when it moved.
static READY_GEN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

struct Scheduler {
    /// Earliest wake-up time of a sleeping thread (skip the scan before).
    next_deadline: u64,
    /// Event count at the last scan of sleepers.
    events_seen: u64,
    /// A thread died since the last reap.
    dead: bool,
    /// READY_GEN when each CPU last found nothing to run.
    idle_seen: [u64; MAX_CPUS],
    threads: Vec<Box<Thread>>,
    /// Index of the thread each CPU runs.
    current: [usize; MAX_CPUS],
    /// Index of each CPU's idle thread.
    idle: [usize; MAX_CPUS],
    next_id: u64,
    slice_start: [u64; MAX_CPUS],
}

impl Scheduler {
    fn cur(&self) -> usize {
        self.current[percpu::index()]
    }
}

static SCHED: Spin<Scheduler> =
    Spin::new(Scheduler { next_deadline: 0, events_seen: 0, dead: false, idle_seen: [u64::MAX; MAX_CPUS], threads: Vec::new(), current: [0; MAX_CPUS], idle: [0; MAX_CPUS], next_id: 1, slice_start: [0; MAX_CPUS] });
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
        gs_base: 0,
        wait_seen: NO_EVENT,
        wait_flag: 0,
        syscall: [NO_EVENT, 0, 0],
        on_cpu: AtomicBool::new(true),
        idle: true,
        fpu: Box::new(cpu::fpu_initial()),
        clear_child_tid: 0,
    }));
    s.current[0] = 0;
    s.idle[0] = 0;
    percpu::this().current_thread = &*s.threads[0] as *const Thread as u64;
}

/// Register the running context of another CPU as its idle thread.
pub fn init_ap(cpu: usize) {
    let mut s = SCHED.lock();
    let id = s.next_id;
    s.next_id += 1;
    s.threads.push(Box::new(Thread {
        id,
        name: alloc::format!("idle{}", cpu),
        rsp: 0,
        kstack: core::ptr::null_mut(),
        kstack_top: 0,
        state: State::Running,
        process: None,
        pml4: crate::mem::paging::kernel_pml4(),
        cpu_ms: 0,
        fs_base: 0,
        gs_base: 0,
        wait_seen: NO_EVENT,
        wait_flag: 0,
        syscall: [NO_EVENT, 0, 0],
        on_cpu: AtomicBool::new(true),
        idle: true,
        fpu: Box::new(cpu::fpu_initial()),
        clear_child_tid: 0,
    }));
    let i = s.threads.len() - 1;
    s.current[cpu] = i;
    s.idle[cpu] = i;
    percpu::this().current_thread = &*s.threads[i] as *const Thread as u64;
    s.slice_start[cpu] = crate::time::uptime_ms();
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
        gs_base: 0,
        wait_seen: NO_EVENT,
        wait_flag: 0,
        syscall: [NO_EVENT, 0, 0],
        on_cpu: AtomicBool::new(false),
        idle: false,
        fpu: Box::new(fpu),
        clear_child_tid: 0,
    }));
    READY_GEN.fetch_add(1, Ordering::AcqRel);
    id
}

/// Start another thread of a user process from a full register frame
/// (Linux `clone`): the new thread shares the address space.
pub fn spawn_user_frame(process: Arc<Process>, frame: idt::TrapFrame, fs_base: u64) -> u64 {
    let name = process.name.clone();
    // The child starts with the parent's floating-point state.
    let mut fpu = cpu::fpu_initial();
    cpu::fxsave(&mut fpu);
    let gs = gs_base();
    let id = add_thread_with(&name, |_| frame, Some(process), fs_base, fpu);
    // The new thread inherits the GS base.
    let mut s = SCHED.lock();
    if let Some(t) = s.threads.iter_mut().find(|t| t.id == id) {
        t.gs_base = gs;
    }
    id
}

/// The thread running on this CPU (it cannot be freed while it runs).
fn me() -> Option<&'static mut Thread> {
    // One gs-relative load: preemption cannot split it (the thread may
    // move to another CPU at any time while interrupts are on).
    let p: u64;
    unsafe { core::arch::asm!("mov {}, gs:[40]", out(reg) p, options(nostack, readonly, preserves_flags)) };
    if p == 0 { None } else { Some(unsafe { &mut *(p as *mut Thread) }) }
}

/// Set the calling thread's GS base.
pub fn set_gs_base(v: u64) {
    if let Some(t) = me() {
        t.gs_base = v;
    }
    // In the kernel the program's GS base waits in KERNEL_GS_BASE.
    unsafe { cpu::wrmsr(MSR_KERNEL_GS_BASE, v) };
}

pub fn gs_base() -> u64 {
    me().map(|t| t.gs_base).unwrap_or(0)
}

/// Set the calling thread's FS base (thread-local storage).
pub fn set_fs_base(v: u64) {
    if let Some(t) = me() {
        t.fs_base = v;
    }
    unsafe { cpu::wrmsr(cpu::MSR_FS_BASE, v) };
}

pub fn fs_base() -> u64 {
    me().map(|t| t.fs_base).unwrap_or(0)
}

/// Record the system call this thread is in (or leaves: `nr` NO_SYSCALL).
pub fn set_syscall(nr: u64, a0: u64, a1: u64) {
    if let Some(t) = me() {
        t.syscall = [nr, a0, a1];
    }
}

pub const NO_SYSCALL: u64 = NO_EVENT;

pub fn current_id() -> u64 {
    me().map(|t| t.id).unwrap_or(0)
}

pub fn set_clear_child_tid(addr: u64) {
    if let Some(t) = me() {
        t.clear_child_tid = addr;
    }
}

/// Set the clear-child-tid address of another thread (after `clone`).
pub fn set_clear_child_tid_of(id: u64, addr: u64) {
    let mut s = SCHED.lock();
    if let Some(t) = s.threads.iter_mut().find(|t| t.id == id) {
        t.clear_child_tid = addr;
    }
}

pub fn clear_child_tid() -> u64 {
    me().map(|t| t.clear_child_tid).unwrap_or(0)
}

/// Threads (ids) of a process that are still alive.
pub fn process_thread_count(pid: u64) -> usize {
    let s = SCHED.lock();
    s.threads.iter().filter(|t| t.state != State::Dead && t.process.as_ref().map(|p| p.pid == pid).unwrap_or(false)).count()
}

/// CPU time used by all threads of a process.
pub fn process_cpu_ms(pid: u64) -> u64 {
    let s = SCHED.lock();
    s.threads.iter().filter(|t| t.process.as_ref().is_some_and(|p| p.pid == pid)).map(|t| t.cpu_ms).sum()
}

/// CPU time used by one thread.
pub fn thread_cpu_ms(id: u64) -> Option<u64> {
    let s = SCHED.lock();
    s.threads.iter().find(|t| t.id == id).map(|t| t.cpu_ms)
}

/// CPU time used by the running thread.
pub fn current_cpu_ms() -> u64 {
    thread_cpu_ms(current_id()).unwrap_or(0)
}

/// Thread ids of a process.
pub fn process_thread_ids(pid: u64) -> Vec<u64> {
    let s = SCHED.lock();
    s.threads.iter().filter(|t| t.state != State::Dead && t.process.as_ref().is_some_and(|p| p.pid == pid)).map(|t| t.id).collect()
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
    let c = percpu::index();
    let mut s = SCHED.lock();
    let now = crate::time::uptime_ms();
    let cur = s.current[c];
    let elapsed = now.saturating_sub(s.slice_start[c]);
    {
        let t = &mut s.threads[cur];
        t.rsp = frame_rsp;
        t.cpu_ms += elapsed;
        if t.state == State::Running {
            t.state = State::Ready;
            if !t.idle {
                READY_GEN.fetch_add(1, Ordering::AcqRel);
            }
        }
    }

    // Reap dead threads no CPU is standing on (only after something died).
    let mut i = if s.dead { 0 } else { usize::MAX };
    let mut left = false;
    while i < s.threads.len() {
        let t = &s.threads[i];
        let in_use = t.idle || t.on_cpu.load(Ordering::Acquire) || s.current.iter().take(percpu::count()).any(|&k| k == i);
        if t.state == State::Dead && in_use {
            left = true;
        }
        if t.state == State::Dead && !in_use {
            let t = s.threads.remove(i);
            for k in s.current.iter_mut() {
                if *k > i {
                    *k -= 1;
                }
            }
            for k in s.idle.iter_mut() {
                if *k > i {
                    *k -= 1;
                }
            }
            // Freed by the reaper thread: dropping a thread may drop its
            // process (closing files, freeing the address space), which
            // must not happen here, under the scheduler lock.
            GRAVE.lock().push(t);
        } else {
            i += 1;
        }
    }
    if s.dead {
        s.dead = left;
    }
    let cur = s.current[c];

    // Wake sleepers whose time came, or whose event or flag arrived; only
    // when a deadline passed or events happened since the last look.
    let ev = EVENTS.load(Ordering::Acquire);
    if now >= s.next_deadline || ev != s.events_seen {
        s.events_seen = ev;
        let mut next = u64::MAX;
        for t in s.threads.iter_mut() {
            if let State::Sleeping(until) = t.state {
                let flagged = t.wait_flag != 0 && unsafe { (*(t.wait_flag as *const AtomicBool)).load(Ordering::Acquire) };
                if now >= until || (t.wait_seen != NO_EVENT && t.wait_seen != ev) || flagged {
                    t.state = State::Ready;
                    READY_GEN.fetch_add(1, Ordering::AcqRel);
                } else {
                    next = next.min(until);
                }
            }
        }
        s.next_deadline = next;
    }

    // Next ready thread after the current one that no CPU is running.
    let n = s.threads.len();
    let mut next = s.idle[c];
    let ready_gen = READY_GEN.load(Ordering::Acquire);
    // An idle CPU with nothing new to run stays idle without a scan.
    let skip = cur == s.idle[c] && s.idle_seen[c] == ready_gen;
    for k in if skip { 1..=0 } else { 1..=n } {
        let i = (cur + k) % n;
        let t = &s.threads[i];
        if !t.idle && t.state == State::Ready && (i == cur || !t.on_cpu.load(Ordering::Acquire)) {
            next = i;
            break;
        }
    }
    if next == s.idle[c] && s.threads[cur].state == State::Ready && !s.threads[cur].idle {
        next = cur;
    }
    if next == s.idle[c] && !skip {
        s.idle_seen[c] = ready_gen;
    }
    if next != cur {
        // Floating-point registers and thread-local storage go with the thread.
        cpu::fxsave(&mut s.threads[cur].fpu);
        cpu::fxrstor(&s.threads[next].fpu);
        if s.threads[next].fs_base != s.threads[cur].fs_base {
            unsafe { cpu::wrmsr(cpu::MSR_FS_BASE, s.threads[next].fs_base) };
        }
        unsafe { cpu::wrmsr(MSR_KERNEL_GS_BASE, s.threads[next].gs_base) };
        s.threads[next].on_cpu.store(true, Ordering::Release);
        // The old thread is released by the entry code once this CPU has
        // left its stack.
        percpu::this().prev_on_cpu = &s.threads[cur].on_cpu as *const AtomicBool as u64;
    }
    if next == s.idle[c] {
        IDLE_CPUS.fetch_or(1u64 << c, Ordering::AcqRel);
    } else {
        IDLE_CPUS.fetch_and(!(1u64 << c), Ordering::AcqRel);
    }
    s.current[c] = next;
    s.slice_start[c] = now;
    percpu::this().current_thread = &*s.threads[next] as *const Thread as u64;
    let t = &mut s.threads[next];
    t.state = State::Running;
    if t.kstack_top != 0 {
        gdt::set_kernel_stack(t.kstack_top);
        idt::set_syscall_stack(t.kstack_top);
    }
    if cpu::read_cr3() & 0x000f_ffff_ffff_f000 != t.pml4 {
        percpu::this().cr3.store(t.pml4, Ordering::Release);
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
    let c = s.cur();
    s.threads[c].state = state;
    match state {
        State::Sleeping(until) => s.next_deadline = s.next_deadline.min(until),
        State::Dead => s.dead = true,
        _ => {}
    }
}

/// Bumped by `notify` whenever something waiting threads may care about
/// happens (data queued, a descriptor ready, a process ended, a signal).
static EVENTS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The current event count: read it before checking for readiness, then
/// pass it to `wait_event`.
pub fn events() -> u64 {
    EVENTS.load(Ordering::Acquire)
}

/// Sleep up to `ms`, or until `notify` is called, unless events happened
/// since `seen` (then return at once: nothing is missed).
pub fn wait_event(seen: u64, ms: u64) {
    if !STARTED.load(Ordering::Acquire) {
        return sleep_ms(ms);
    }
    {
        let mut s = SCHED.lock();
        if EVENTS.load(Ordering::Acquire) != seen {
            return;
        }
        let until = crate::time::uptime_ms() + ms.max(1);
        let c = s.cur();
        s.threads[c].state = State::Sleeping(until);
        s.threads[c].wait_seen = seen;
        s.next_deadline = s.next_deadline.min(until);
    }
    yield_now();
    if let Some(t) = me() {
        t.wait_seen = NO_EVENT;
    }
}

/// Something happened: threads in `wait_event` wake at the next
/// scheduling point of any CPU. Lock-free (called very often).
pub fn notify() {
    EVENTS.fetch_add(1, Ordering::AcqRel);
    kick_idle();
}

/// Sleep up to `ms` unless `flag` is set; setting it and calling `wake`
/// ends the sleep early.
pub fn wait_flag(flag: &core::sync::atomic::AtomicBool, ms: u64) {
    {
        let mut s = SCHED.lock();
        if flag.load(Ordering::Acquire) {
            return;
        }
        let until = crate::time::uptime_ms() + ms.max(1);
        let c = s.cur();
        s.threads[c].state = State::Sleeping(until);
        s.threads[c].wait_flag = flag as *const _ as usize;
        s.next_deadline = s.next_deadline.min(until);
    }
    yield_now();
    if let Some(t) = me() {
        t.wait_flag = 0;
    }
}

/// A `wait_flag` flag was set: get sleepers looked at.
pub fn wake(_id: u64) {
    notify();
}

/// CPUs sitting in their idle thread (bit per CPU index).
static IDLE_CPUS: AtomicU64 = AtomicU64::new(0);

/// Kick one idle CPU so a woken thread runs now rather than at the next
/// timer tick.
fn kick_idle() {
    let mask = IDLE_CPUS.load(Ordering::Acquire);
    if mask == 0 {
        return;
    }
    let me = percpu::index();
    let others = mask & !(1u64 << me);
    if others == 0 {
        return;
    }
    let c = others.trailing_zeros() as usize;
    if IDLE_CPUS.fetch_and(!(1u64 << c), Ordering::AcqRel) & (1u64 << c) != 0 {
        if let Some(pc) = percpu::get(c) {
            let on = cpu::interrupts_enabled();
            cpu::cli();
            crate::arch::apic::ipi_to(pc.lapic_id, crate::arch::idt::VEC_WAKE);
            if on {
                cpu::sti();
            }
        }
    }
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
    me().and_then(|t| t.process.clone())
}

/// Mark every thread of a process dead (used when it exits or faults).
pub fn kill_process_threads(pid: u64) {
    let mut s = SCHED.lock();
    let mut any = false;
    for t in s.threads.iter_mut() {
        if t.process.as_ref().map(|p| p.pid) == Some(pid) {
            t.state = State::Dead;
            any = true;
        }
    }
    s.dead |= any;
}

/// End every thread of `pid` except the calling one (execve).
pub fn kill_other_threads(pid: u64) {
    let mut s = SCHED.lock();
    let cur = s.cur();
    for (i, t) in s.threads.iter_mut().enumerate() {
        if i != cur && t.process.as_ref().map(|p| p.pid) == Some(pid) {
            t.state = State::Dead;
        }
    }
    s.dead = true;
}

/// Wait until no other thread of `pid` is still executing on some CPU
/// (after `kill_other_threads`, before freeing their address space).
pub fn wait_others_off_cpu(pid: u64) {
    loop {
        let busy = {
            let s = SCHED.lock();
            let cur = s.cur();
            s.threads.iter().enumerate().any(|(i, t)| i != cur && t.process.as_ref().map(|p| p.pid) == Some(pid) && t.on_cpu.load(Ordering::Acquire))
        };
        if !busy {
            return;
        }
        yield_now();
    }
}

/// Run the calling thread in another address space from now on.
pub fn switch_address_space(pml4: u64) {
    let mut s = SCHED.lock();
    let c = s.cur();
    s.threads[c].pml4 = pml4;
    s.threads[c].fs_base = 0;
    s.threads[c].gs_base = 0;
    percpu::this().cr3.store(pml4, Ordering::Release);
    unsafe {
        cpu::wrmsr(MSR_KERNEL_GS_BASE, 0);
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
    pub syscall: [u64; 3],
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
            syscall: t.syscall,
        })
        .collect()
}
