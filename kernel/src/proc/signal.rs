//! Linux signals: handlers (`rt_sigaction`), per-thread masks and pending
//! sets, delivery on the way back to user mode (after a system call or a
//! fault) through a Linux-compatible `rt_sigframe`, and `rt_sigreturn`.
//!
//! Signals reach a thread when it next leaves the kernel. Threads blocked
//! in `poll`, `epoll_wait`, `futex`, `nanosleep` and similar waits notice
//! a pending signal and return `EINTR`.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;

use super::process::Process;
use super::{sched, usermem};
use crate::arch::cpu::{self, FpuState};
use crate::arch::idt::TrapFrame;

pub const SIGKILL: u64 = 9;
pub const SIGSEGV: u64 = 11;
pub const SIGSTOP: u64 = 19;

const SA_SIGINFO: u64 = 4;
const SA_ONSTACK: u64 = 0x0800_0000;
const SA_RESTORER: u64 = 0x0400_0000;
const SA_NODEFER: u64 = 0x4000_0000;
const SA_RESETHAND: u64 = 0x8000_0000;

const SIG_DFL: u64 = 0;
const SIG_IGN: u64 = 1;

/// si_code values
pub const SI_USER: i32 = 0;
pub const SI_TKILL: i32 = -6;

#[derive(Clone, Copy, Default)]
pub struct Action {
    pub handler: u64,
    pub flags: u64,
    pub restorer: u64,
    pub mask: u64,
}

#[derive(Clone, Copy, Default)]
struct Info {
    code: i32,
    /// Fault address, or the sending pid.
    value: u64,
}

#[derive(Clone, Default)]
pub struct ThreadSig {
    pub mask: u64,
    pending: u64,
    /// sigaltstack: (sp, size, flags)
    alt: (u64, u64, u32),
    /// Mask to put back after a handler that interrupted rt_sigsuspend.
    suspend_restore: Option<u64>,
}

#[derive(Clone)]
pub struct Signals {
    pub actions: [Action; 64],
    pending: u64,
    info: [Info; 64],
    threads: BTreeMap<u64, ThreadSig>,
    /// Mask of threads seen for the first time (the forking thread's).
    inherit_mask: u64,
}

impl Default for Signals {
    fn default() -> Self {
        Signals { actions: [Action::default(); 64], pending: 0, info: [Info::default(); 64], threads: BTreeMap::new(), inherit_mask: 0 }
    }
}

fn bit(sig: u64) -> u64 {
    1u64 << (sig - 1)
}

/// Signals whose default action is to do nothing (SIGCHLD, SIGCONT,
/// SIGURG, SIGWINCH, and the stop signals: there is no job control).
fn default_ignored(sig: u64) -> bool {
    matches!(sig, 17 | 18 | 23 | 28 | 19 | 20 | 21 | 22)
}

impl Signals {
    fn thread(&mut self, tid: u64) -> &mut ThreadSig {
        let mask = self.inherit_mask;
        self.threads.entry(tid).or_insert_with(|| ThreadSig { mask, alt: (0, 0, 2), ..Default::default() })
    }

    /// After fork: only the calling thread survives.
    pub fn fork_copy(&self, parent_tid: u64) -> Signals {
        let mut s = self.clone();
        s.inherit_mask = self.threads.get(&parent_tid).map(|t| t.mask).unwrap_or(0);
        s.threads.clear();
        s.pending = 0;
        s
    }

    /// execve: handlers go back to the default, ignored signals stay
    /// ignored, the mask is kept.
    pub fn exec_reset(&mut self) {
        for a in self.actions.iter_mut() {
            if a.handler != SIG_IGN {
                *a = Action::default();
            }
        }
        for t in self.threads.values_mut() {
            t.alt = (0, 0, 2);
        }
    }

    /// A new thread (clone) starts with its creator's mask.
    pub fn new_thread(&mut self, parent: u64, child: u64) {
        let mask = self.threads.get(&parent).map(|t| t.mask).unwrap_or(0);
        self.threads.insert(child, ThreadSig { mask, alt: (0, 0, 2), ..Default::default() });
    }

    pub fn thread_exit(&mut self, tid: u64) {
        self.threads.remove(&tid);
    }
}

fn state(p: &Process) -> Option<&crate::sync::Spin<Signals>> {
    p.linux.as_deref().map(|l| &l.sig)
}

/// What happens when `sig` arrives with no handler.
enum Disposition {
    Ignore,
    Kill,
    Handle,
}

fn disposition(s: &Signals, sig: u64) -> Disposition {
    if sig == SIGKILL || sig == SIGSTOP {
        return if sig == SIGKILL { Disposition::Kill } else { Disposition::Ignore };
    }
    match s.actions[sig as usize - 1].handler {
        SIG_IGN => Disposition::Ignore,
        SIG_DFL if default_ignored(sig) => Disposition::Ignore,
        SIG_DFL => Disposition::Kill,
        _ => Disposition::Handle,
    }
}

fn kill_message(sig: u64) -> Option<&'static str> {
    match sig {
        6 => Some("Aborted\n"),
        11 => Some("Segmentation fault\n"),
        7 => Some("Bus error\n"),
        4 => Some("Illegal instruction\n"),
        8 => Some("Floating point exception\n"),
        9 | 15 => Some("Killed\n"),
        _ => None,
    }
}

/// Send `sig` to process `t` (to thread `tid` if given). Returns true if
/// the *calling* thread was ended by it.
pub fn send(t: &Arc<Process>, tid: Option<u64>, sig: u64, code: i32, from_pid: u64) -> bool {
    if sig == 0 || sig > 64 {
        return false;
    }
    let Some(st) = state(t) else {
        super::process::kill_signal(t, sig as i64);
        return false;
    };
    let mut s = st.lock();
    match disposition(&s, sig) {
        Disposition::Ignore => false,
        Disposition::Kill => {
            drop(s);
            let own = sched::current_process().is_some_and(|c| c.pid == t.pid);
            if own {
                super::process::exit_current_process(-(sig as i64), kill_message(sig));
                true
            } else {
                super::process::kill_signal(t, sig as i64);
                false
            }
        }
        Disposition::Handle => {
            s.info[sig as usize - 1] = Info { code, value: from_pid };
            match tid {
                Some(id) => s.thread(id).pending |= bit(sig),
                None => s.pending |= bit(sig),
            }
            false
        }
    }
}

/// Queue `sig` for the current thread (delivered on the way out of the
/// system call, e.g. SIGPIPE after writing to a closed pipe).
pub fn raise_current(p: &Process, sig: u64) {
    let Some(st) = state(p) else { return };
    let tid = sched::current_id();
    let mut s = st.lock();
    if matches!(disposition(&s, sig), Disposition::Ignore) {
        return;
    }
    s.info[sig as usize - 1] = Info { code: SI_USER, value: p.pid };
    s.thread(tid).pending |= bit(sig);
}

/// True if the current thread has a signal it should be interrupted for.
pub fn interrupted(p: &Process) -> bool {
    let Some(st) = state(p) else { return false };
    let tid = sched::current_id();
    let mut s = st.lock();
    let pending = s.pending;
    let t = s.thread(tid);
    (t.pending | pending) & !t.mask != 0
}

/// rt_sigaction(sig, act, oldact, size)
pub fn sigaction(p: &Process, sig: u64, act: u64, old: u64) -> i64 {
    let Some(st) = state(p) else { return -38 };
    if sig == 0 || sig > 64 {
        return -22;
    }
    let pml4 = p.pml4();
    let mut s = st.lock();
    let cur = s.actions[sig as usize - 1];
    if old != 0 {
        let mut b = [0u8; 32];
        b[0..8].copy_from_slice(&cur.handler.to_le_bytes());
        b[8..16].copy_from_slice(&cur.flags.to_le_bytes());
        b[16..24].copy_from_slice(&cur.restorer.to_le_bytes());
        b[24..32].copy_from_slice(&cur.mask.to_le_bytes());
        if !usermem::write_bytes(pml4, old, &b) {
            return -14;
        }
    }
    if act != 0 {
        if sig == SIGKILL || sig == SIGSTOP {
            return -22;
        }
        let Some(b) = usermem::read_bytes(pml4, act, 32) else { return -14 };
        let g = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let a = Action { handler: g(0), flags: g(8), restorer: g(16), mask: g(24) & !(bit(SIGKILL) | bit(SIGSTOP)) };
        s.actions[sig as usize - 1] = a;
        if a.handler == SIG_IGN || a.handler == SIG_DFL && default_ignored(sig) {
            // Pending instances are thrown away.
            s.pending &= !bit(sig);
            for t in s.threads.values_mut() {
                t.pending &= !bit(sig);
            }
        }
    }
    0
}

/// rt_sigprocmask(how, set, oldset)
pub fn sigprocmask(p: &Process, how: u64, set: u64, old: u64) -> i64 {
    let Some(st) = state(p) else { return -38 };
    let pml4 = p.pml4();
    let tid = sched::current_id();
    let new = if set != 0 {
        match usermem::read_u64(pml4, set) {
            Some(v) => Some(v),
            None => return -14,
        }
    } else {
        None
    };
    let mut s = st.lock();
    let t = s.thread(tid);
    if old != 0 && !usermem::write_u64(pml4, old, t.mask) {
        return -14;
    }
    if let Some(v) = new {
        t.mask = match how {
            0 => t.mask | v,
            1 => t.mask & !v,
            2 => v,
            _ => return -22,
        } & !(bit(SIGKILL) | bit(SIGSTOP));
    }
    0
}

/// sigaltstack(ss, old_ss)
pub fn sigaltstack(p: &Process, ss: u64, old: u64, user_sp: u64) -> i64 {
    let Some(st) = state(p) else { return -38 };
    let pml4 = p.pml4();
    let tid = sched::current_id();
    let mut s = st.lock();
    let t = s.thread(tid);
    let on = t.alt.1 != 0 && user_sp > t.alt.0 && user_sp <= t.alt.0 + t.alt.1;
    if old != 0 {
        let mut b = [0u8; 24];
        b[0..8].copy_from_slice(&t.alt.0.to_le_bytes());
        let flags: u32 = if t.alt.1 == 0 { 2 } else if on { 1 } else { 0 }; // SS_DISABLE / SS_ONSTACK
        b[8..12].copy_from_slice(&flags.to_le_bytes());
        b[16..24].copy_from_slice(&t.alt.1.to_le_bytes());
        if !usermem::write_bytes(pml4, old, &b) {
            return -14;
        }
    }
    if ss != 0 {
        if on {
            return -1; // EPERM
        }
        let Some(b) = usermem::read_bytes(pml4, ss, 24) else { return -14 };
        let sp = u64::from_le_bytes(b[0..8].try_into().unwrap());
        let flags = u32::from_le_bytes(b[8..12].try_into().unwrap());
        let size = u64::from_le_bytes(b[16..24].try_into().unwrap());
        t.alt = if flags & 2 != 0 { (0, 0, 2) } else { (sp, size, 0) };
    }
    0
}

/// rt_sigpending(set)
pub fn sigpending(p: &Process, set: u64) -> i64 {
    let Some(st) = state(p) else { return -38 };
    let tid = sched::current_id();
    let v = {
        let mut s = st.lock();
        let pend = s.pending;
        let t = s.thread(tid);
        (t.pending | pend) & t.mask
    };
    if usermem::write_u64(p.pml4(), set, v) { 0 } else { -14 }
}

/// rt_sigsuspend(mask): wait with `mask` until a handler runs.
pub fn sigsuspend(p: &Process, maskp: u64) -> i64 {
    let Some(st) = state(p) else { return -38 };
    let Some(mask) = usermem::read_u64(p.pml4(), maskp) else { return -14 };
    let tid = sched::current_id();
    {
        let mut s = st.lock();
        let t = s.thread(tid);
        t.suspend_restore = Some(t.mask);
        t.mask = mask & !(bit(SIGKILL) | bit(SIGSTOP));
    }
    while !interrupted(p) {
        if p.has_exited().is_some() {
            break;
        }
        sched::sleep_ms(5);
    }
    -4 // EINTR
}

/// rt_sigtimedwait(set, info, timeout): take a pending signal from `set`.
pub fn sigtimedwait(p: &Process, setp: u64, infop: u64, timeout_ms: Option<u64>) -> i64 {
    let Some(st) = state(p) else { return -38 };
    let Some(set) = usermem::read_u64(p.pml4(), setp) else { return -14 };
    let tid = sched::current_id();
    let deadline = timeout_ms.map(|ms| crate::time::uptime_ms() + ms).unwrap_or(u64::MAX);
    loop {
        let got = {
            let mut s = st.lock();
            let pend = s.pending;
            let t = s.thread(tid);
            let ready = (t.pending | pend) & set;
            if ready != 0 {
                let sig = ready.trailing_zeros() as u64 + 1;
                t.pending &= !bit(sig);
                s.pending &= !bit(sig);
                Some((sig, s.info[sig as usize - 1]))
            } else {
                None
            }
        };
        if let Some((sig, info)) = got {
            if infop != 0 {
                usermem::write_bytes(p.pml4(), infop, &siginfo(sig, info));
            }
            return sig as i64;
        }
        if interrupted(p) {
            return -4;
        }
        if crate::time::uptime_ms() >= deadline {
            return -11; // EAGAIN
        }
        sched::sleep_ms(2);
    }
}

fn siginfo(sig: u64, info: Info) -> [u8; 128] {
    let mut b = [0u8; 128];
    b[0..4].copy_from_slice(&(sig as u32).to_le_bytes());
    b[8..12].copy_from_slice(&info.code.to_le_bytes());
    if info.code > 0 && matches!(sig, 4 | 7 | 8 | 11) {
        b[16..24].copy_from_slice(&info.value.to_le_bytes()); // si_addr
    } else {
        b[16..20].copy_from_slice(&(info.value as u32).to_le_bytes()); // si_pid
        b[20..24].copy_from_slice(&1000u32.to_le_bytes()); // si_uid
    }
    b
}

// Layout of the Linux x86_64 signal frame (arch/x86/include/asm/sigframe.h):
//   rsp -> pretcode (return address = sa_restorer)
//          struct ucontext  (uc_flags, uc_link, uc_stack, sigcontext, sigmask)
//          struct siginfo
const UC_SIZE: u64 = 304;
const UC_MCONTEXT: u64 = 40;
const UC_SIGMASK: u64 = 296;
const FRAME_SIZE: u64 = 8 + UC_SIZE + 128;

/// Deliver one pending signal to the current thread, if any: rewrites `f`
/// so the thread resumes in the handler. Returns true if the process was
/// ended instead.
pub fn deliver(p: &Arc<Process>, f: &mut TrapFrame) -> bool {
    let Some(st) = state(p) else { return false };
    let tid = sched::current_id();
    let (sig, act, info, old_mask, alt) = {
        let mut s = st.lock();
        let pend = s.pending;
        let t = s.thread(tid);
        let ready = (t.pending | pend) & !t.mask;
        if ready == 0 {
            return false;
        }
        let sig = ready.trailing_zeros() as u64 + 1;
        t.pending &= !bit(sig);
        let alt = t.alt;
        let old_mask = t.suspend_restore.take().unwrap_or(t.mask);
        s.pending &= !bit(sig);
        let act = s.actions[sig as usize - 1];
        (sig, act, s.info[sig as usize - 1], old_mask, alt)
    };
    match disposition(&st.lock(), sig) {
        Disposition::Ignore => return false,
        Disposition::Kill => {
            super::process::exit_current_process(-(sig as i64), kill_message(sig));
            return true;
        }
        Disposition::Handle => {}
    }
    if setup_frame(p, f, sig, act, info, old_mask, alt) {
        let mut s = st.lock();
        if act.flags & SA_RESETHAND != 0 {
            s.actions[sig as usize - 1] = Action::default();
        }
        let t = s.thread(tid);
        t.mask |= act.mask;
        if act.flags & SA_NODEFER == 0 {
            t.mask |= bit(sig);
        }
        t.mask &= !(bit(SIGKILL) | bit(SIGSTOP));
        false
    } else {
        // No room for the frame: the process dies of SIGSEGV.
        super::process::exit_current_process(-(SIGSEGV as i64), Some("Segmentation fault (bad signal stack)\n"));
        true
    }
}

fn setup_frame(p: &Process, f: &mut TrapFrame, sig: u64, act: Action, info: Info, old_mask: u64, alt: (u64, u64, u32)) -> bool {
    if act.flags & SA_RESTORER == 0 || act.restorer == 0 {
        return false;
    }
    let pml4 = p.pml4();
    let on_alt = alt.1 != 0 && f.rsp > alt.0 && f.rsp <= alt.0 + alt.1;
    let mut sp = if act.flags & SA_ONSTACK != 0 && alt.1 != 0 && !on_alt { alt.0 + alt.1 } else { f.rsp - 128 };
    // Floating-point state (fxsave format, 64-byte aligned).
    sp -= 512;
    sp &= !63;
    let fx = sp;
    let mut fpu = FpuState([0; 512]);
    cpu::fxsave(&mut fpu);
    if !usermem::write_bytes(pml4, fx, &fpu.0) {
        return false;
    }
    sp -= FRAME_SIZE;
    sp = (sp & !15) - 8;
    let uc = sp + 8;
    let mut b = alloc::vec![0u8; FRAME_SIZE as usize];
    let mut put = |o: u64, v: u64| b[o as usize..o as usize + 8].copy_from_slice(&v.to_le_bytes());
    put(0, act.restorer);
    // uc_flags: UC_FP_XSTATE | UC_SIGCONTEXT_SS | UC_STRICT_RESTORE_SS
    put(8, 0x6);
    put(8 + 16, alt.0);
    put(8 + 24, if alt.1 == 0 { 2 } else if on_alt { 1 } else { 0 });
    put(8 + 32, alt.1);
    let g = [
        f.r8, f.r9, f.r10, f.r11, f.r12, f.r13, f.r14, f.r15, f.rdi, f.rsi, f.rbp, f.rbx, f.rdx, f.rax, f.rcx, f.rsp, f.rip, f.rflags,
        0x2b_0000_0000_0033, // cs 0x33, ss 0x2b
        f.error,
        f.vector,
        old_mask,
        if info.code > 0 { info.value } else { 0 },
        fx,
    ];
    for (i, v) in g.iter().enumerate() {
        put(8 + UC_MCONTEXT + i as u64 * 8, *v);
    }
    put(8 + UC_SIGMASK, old_mask);
    let si = siginfo(sig, info);
    b[(8 + UC_SIZE) as usize..].copy_from_slice(&si);
    if !usermem::write_bytes(pml4, sp, &b) {
        return false;
    }
    f.rdi = sig;
    f.rsi = uc + UC_SIZE;
    f.rdx = uc;
    f.rax = 0;
    f.rip = act.handler;
    f.rsp = sp;
    f.rflags &= !(0x100 | 0x400 | 0x40000); // TF, DF, AC
    // The handler starts with clean floating-point state.
    cpu::fxrstor(&cpu::fpu_initial());
    let _ = act.flags & SA_SIGINFO;
    true
}

/// rt_sigreturn: restore the state saved by `setup_frame`.
pub fn sigreturn(p: &Process, f: &mut TrapFrame) -> bool {
    let pml4 = p.pml4();
    let uc = f.rsp;
    let Some(b) = usermem::read_bytes(pml4, uc, UC_SIZE) else { return false };
    let g = |i: usize| u64::from_le_bytes(b[(UC_MCONTEXT as usize + i * 8)..(UC_MCONTEXT as usize + i * 8 + 8)].try_into().unwrap());
    let (cs, ss) = (f.cs, f.ss);
    f.r8 = g(0);
    f.r9 = g(1);
    f.r10 = g(2);
    f.r11 = g(3);
    f.r12 = g(4);
    f.r13 = g(5);
    f.r14 = g(6);
    f.r15 = g(7);
    f.rdi = g(8);
    f.rsi = g(9);
    f.rbp = g(10);
    f.rbx = g(11);
    f.rdx = g(12);
    f.rax = g(13);
    f.rcx = g(14);
    f.rsp = g(15);
    f.rip = g(16);
    // Only the arithmetic flags, direction, trap and AC come back.
    f.rflags = (g(17) & 0x4_0dd5) | 0x202;
    f.cs = cs;
    f.ss = ss;
    let fx = g(23);
    if fx != 0
        && let Some(state) = usermem::read_bytes(pml4, fx, 512)
    {
        let mut s = FpuState([0; 512]);
        s.0.copy_from_slice(&state);
        // Reserved MXCSR bits would fault in fxrstor.
        let mxcsr = u32::from_le_bytes(s.0[24..28].try_into().unwrap()) & 0xffff;
        s.0[24..28].copy_from_slice(&mxcsr.to_le_bytes());
        cpu::fxrstor(&s);
    }
    let mask = u64::from_le_bytes(b[UC_SIGMASK as usize..UC_SIGMASK as usize + 8].try_into().unwrap());
    if let Some(st) = state(p) {
        let tid = sched::current_id();
        st.lock().thread(tid).mask = mask & !(bit(SIGKILL) | bit(SIGSTOP));
    }
    true
}

/// A fault in a Linux program: run its handler for `sig` if it has one
/// (and the signal is not blocked). Returns true if the thread resumes.
pub fn fault(p: &Arc<Process>, f: &mut TrapFrame, sig: u64, code: i32, addr: u64) -> bool {
    let Some(st) = state(p) else { return false };
    let tid = sched::current_id();
    {
        let mut s = st.lock();
        let handled = matches!(disposition(&s, sig), Disposition::Handle);
        let blocked = s.thread(tid).mask & bit(sig) != 0;
        if !handled || blocked {
            return false;
        }
        s.info[sig as usize - 1] = Info { code, value: addr };
        s.thread(tid).pending |= bit(sig);
    }
    // Deliver it now, ahead of anything else pending.
    let (act, old_mask, alt) = {
        let mut s = st.lock();
        let act = s.actions[sig as usize - 1];
        let t = s.thread(tid);
        t.pending &= !bit(sig);
        (act, t.suspend_restore.take().unwrap_or(t.mask), t.alt)
    };
    if !setup_frame(p, f, sig, act, Info { code, value: addr }, old_mask, alt) {
        return false;
    }
    let mut s = st.lock();
    if act.flags & SA_RESETHAND != 0 {
        s.actions[sig as usize - 1] = Action::default();
    }
    let t = s.thread(tid);
    t.mask |= act.mask;
    if act.flags & SA_NODEFER == 0 {
        t.mask |= bit(sig);
    }
    true
}
