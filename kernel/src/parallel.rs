//! Run a piece of work on every CPU at once: `run(parts, f)` calls
//! `f(0) .. f(parts - 1)` spread over worker threads (one per extra CPU)
//! and the caller, and returns when all are done. Used for pixel work
//! that splits into bands (video scaling and colour conversion).

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::proc::sched;
use crate::sync::{Mutex, Spin};

struct Job {
    f: usize,
    vtable: usize,
    parts: usize,
}

static JOB: Spin<Option<Job>> = Spin::new(None);
static NEXT: AtomicUsize = AtomicUsize::new(0);
static DONE: AtomicUsize = AtomicUsize::new(0);
/// Workers inside `work_on` (a job may not be replaced under them).
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
static BUSY: Mutex<()> = Mutex::new(());
static WORKERS: AtomicUsize = AtomicUsize::new(usize::MAX);
static GO: [AtomicBool; 64] = [const { AtomicBool::new(false) }; 64];

type Work = dyn Fn(usize) + Sync;

fn work_on() -> bool {
    let (f, vt, parts) = match JOB.lock().as_ref() {
        Some(j) => (j.f, j.vtable, j.parts),
        None => return false,
    };
    // SAFETY: `run` keeps the closure alive until DONE reaches `parts`.
    let f: &Work = unsafe { core::mem::transmute::<(usize, usize), &Work>((f, vt)) };
    loop {
        let i = NEXT.fetch_add(1, Ordering::AcqRel);
        if i >= parts {
            return true;
        }
        f(i);
        DONE.fetch_add(1, Ordering::AcqRel);
    }
}

extern "C" fn worker(me: usize) {
    loop {
        sched::wait_flag(&GO[me], 1000);
        if GO[me].swap(false, Ordering::AcqRel) {
            ACTIVE.fetch_add(1, Ordering::AcqRel);
            work_on();
            ACTIVE.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

fn workers() -> usize {
    let n = WORKERS.load(Ordering::Acquire);
    if n != usize::MAX {
        return n;
    }
    let n = crate::smp::online().saturating_sub(1).min(GO.len());
    for i in 0..n {
        sched::spawn_kernel("parallel", worker, i);
    }
    WORKERS.store(n, Ordering::Release);
    n
}

/// Number of pieces worth splitting work into (one per CPU).
pub fn width() -> usize {
    let _g = BUSY.lock();
    workers() + 1
}

pub fn run(parts: usize, f: &(dyn Fn(usize) + Sync + '_)) {
    if parts == 0 {
        return;
    }
    let _g = BUSY.lock();
    let n = workers();
    if n == 0 || parts == 1 {
        for i in 0..parts {
            f(i);
        }
        return;
    }
    let (fp, vt): (usize, usize) = unsafe { core::mem::transmute::<&(dyn Fn(usize) + Sync + '_), (usize, usize)>(f) };
    NEXT.store(0, Ordering::Release);
    DONE.store(0, Ordering::Release);
    *JOB.lock() = Some(Job { f: fp, vtable: vt, parts });
    for g in GO.iter().take(n.min(parts - 1)) {
        g.store(true, Ordering::Release);
    }
    sched::wake(0);
    work_on();
    // Parts a worker took but has not finished: give the CPU away while
    // waiting (spinning here starves busy programs, and the worker itself
    // when it shares this CPU).
    let mut spins = 0u32;
    while DONE.load(Ordering::Acquire) < parts {
        spins += 1;
        if spins > 200 {
            sched::yield_now();
        } else {
            core::hint::spin_loop();
        }
    }
    *JOB.lock() = None;
    while ACTIVE.load(Ordering::Acquire) != 0 {
        sched::yield_now();
    }
}
