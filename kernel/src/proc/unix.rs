//! Local IPC objects for Linux programs: Unix-domain stream sockets (with
//! descriptor passing), shared memory (`memfd_create`, `/dev/shm`),
//! `eventfd` and `epoll` interest lists.
//!
//! A socket may be connected to a kernel service instead of another
//! process: the Wayland compositor is one. Bytes written to such a socket
//! are handed to the service, which answers by queueing bytes (and
//! descriptors) for the program to read.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use super::linux::DescRef;
use crate::mem::{phys_to_virt, pmm, PAGE_SIZE};
use crate::sync::Spin;

// -------------------------------------------------------------------------
// Byte queues with attached descriptors
// -------------------------------------------------------------------------

#[derive(Default)]
pub struct Queue {
    data: VecDeque<u8>,
    /// Descriptors sent with the byte at stream position `.0`.
    fds: VecDeque<(u64, Vec<DescRef>)>,
    /// Stream position of `data[0]`.
    head: u64,
    /// The writing side has gone.
    pub closed: bool,
}

/// Most a queue holds before writers must wait (like a socket buffer).
const QUEUE_LIMIT: usize = 4 << 20;

impl Queue {
    pub fn push(&mut self, bytes: &[u8], fds: Vec<DescRef>) {
        if !fds.is_empty() {
            let at = self.head + self.data.len() as u64;
            self.fds.push_back((at, fds));
        }
        self.data.extend(bytes.iter().copied());
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty() && self.fds.is_empty()
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Take up to `max` bytes and the descriptors that came with them.
    pub fn pop(&mut self, max: usize) -> (Vec<u8>, Vec<DescRef>) {
        let n = max.min(self.data.len());
        let bytes: Vec<u8> = self.data.drain(..n).collect();
        self.head += n as u64;
        let mut fds = Vec::new();
        while let Some((at, _)) = self.fds.front() {
            // Deliver with the first byte of their message (or when that
            // message had no bytes left to read).
            if *at < self.head || self.data.is_empty() {
                fds.extend(self.fds.pop_front().unwrap().1);
            } else {
                break;
            }
        }
        (bytes, fds)
    }
}

pub type QueueRef = Arc<Spin<Queue>>;

/// The kernel side of a connection (e.g. a Wayland client session).
pub trait Service: Send + Sync {
    /// The program wrote `data` (with `fds` attached).
    fn receive(&self, data: &[u8], fds: Vec<DescRef>);
    /// The program closed its end.
    fn closed(&self);
}

/// Something that accepts connections on a path.
pub trait ServiceFactory: Send + Sync {
    /// A program connected; `to_client` is what it will read.
    fn connect(&self, to_client: QueueRef, pid: u64) -> Arc<dyn Service>;
}

pub enum Peer {
    Queue(QueueRef),
    Service(Arc<dyn Service>),
}

/// One end of a connected stream socket.
pub struct Endpoint {
    pub rx: QueueRef,
    pub tx: Peer,
    pub path: Option<String>,
}

impl Endpoint {
    /// A connected pair (socketpair, or connect/accept).
    pub fn pair(path: Option<String>) -> (Endpoint, Endpoint) {
        let a: QueueRef = Arc::new(Spin::new(Queue::default()));
        let b: QueueRef = Arc::new(Spin::new(Queue::default()));
        (
            Endpoint { rx: a.clone(), tx: Peer::Queue(b.clone()), path: path.clone() },
            Endpoint { rx: b, tx: Peer::Queue(a), path },
        )
    }

    /// Write; `None` if the other side is gone (EPIPE), `Some(0)` if the
    /// buffer is full.
    pub fn send(&self, data: &[u8], fds: Vec<DescRef>) -> Option<usize> {
        match &self.tx {
            Peer::Queue(q) => {
                let mut q = q.lock();
                if q.closed {
                    return None;
                }
                if q.len() >= QUEUE_LIMIT && !data.is_empty() {
                    return Some(0);
                }
                q.push(data, fds);
                Some(data.len())
            }
            Peer::Service(s) => {
                s.receive(data, fds);
                Some(data.len())
            }
        }
    }

    pub fn writable(&self) -> bool {
        match &self.tx {
            Peer::Queue(q) => {
                let q = q.lock();
                q.closed || q.len() < QUEUE_LIMIT
            }
            Peer::Service(_) => true,
        }
    }

    pub fn readable(&self) -> bool {
        let q = self.rx.lock();
        !q.is_empty() || q.closed
    }

    pub fn hung_up(&self) -> bool {
        self.rx.lock().closed
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        match &self.tx {
            Peer::Queue(q) => q.lock().closed = true,
            Peer::Service(s) => s.closed(),
        }
        // Nobody reads our queue any more: drop descriptors in flight.
        let mut q = self.rx.lock();
        q.fds.clear();
        q.closed = true;
    }
}

/// A listening socket made by a program.
pub struct Listener {
    pub path: String,
    pub pending: Spin<VecDeque<Endpoint>>,
}

enum Bound {
    Program(Weak<Listener>),
    Kernel(Arc<dyn ServiceFactory>),
}

static BOUND: Spin<BTreeMap<String, Bound>> = Spin::new(BTreeMap::new());

/// Offer a kernel service at `path`.
pub fn register_service(path: &str, f: Arc<dyn ServiceFactory>) {
    BOUND.lock().insert(String::from(path), Bound::Kernel(f));
}

/// bind + listen by a program. `None` if the address is taken.
pub fn listen(path: &str) -> Option<Arc<Listener>> {
    let mut b = BOUND.lock();
    if let Some(Bound::Program(w)) = b.get(path)
        && w.strong_count() > 0
    {
        return None;
    }
    if matches!(b.get(path), Some(Bound::Kernel(_))) {
        return None;
    }
    let l = Arc::new(Listener { path: String::from(path), pending: Spin::new(VecDeque::new()) });
    b.insert(String::from(path), Bound::Program(Arc::downgrade(&l)));
    Some(l)
}

pub fn is_bound(path: &str) -> bool {
    match BOUND.lock().get(path) {
        Some(Bound::Program(w)) => w.strong_count() > 0,
        Some(Bound::Kernel(_)) => true,
        None => false,
    }
}

/// connect: a new endpoint for the caller, or `None` (ECONNREFUSED).
pub fn connect(path: &str, pid: u64) -> Option<Endpoint> {
    let target = match BOUND.lock().get(path) {
        Some(Bound::Program(w)) => w.upgrade().map(Ok),
        Some(Bound::Kernel(f)) => Some(Err(f.clone())),
        None => None,
    }?;
    match target {
        Ok(listener) => {
            let (mine, theirs) = Endpoint::pair(Some(String::from(path)));
            listener.pending.lock().push_back(theirs);
            Some(mine)
        }
        Err(factory) => {
            let rx: QueueRef = Arc::new(Spin::new(Queue::default()));
            let service = factory.connect(rx.clone(), pid);
            Some(Endpoint { rx, tx: Peer::Service(service), path: Some(String::from(path)) })
        }
    }
}

// -------------------------------------------------------------------------
// Shared memory
// -------------------------------------------------------------------------

/// Memory shared between programs (and the desktop): the pages stay put,
/// so every mapping sees the same bytes.
pub struct Shm {
    pages: Spin<Vec<u64>>,
    size: AtomicU64,
}

const SHM_LIMIT: u64 = 512 << 20;

impl Shm {
    pub fn new() -> Arc<Shm> {
        Arc::new(Shm { pages: Spin::new(Vec::new()), size: AtomicU64::new(0) })
    }

    pub fn size(&self) -> u64 {
        self.size.load(Ordering::Relaxed)
    }

    /// ftruncate. Growing adds zeroed pages; shrinking frees pages.
    pub fn resize(&self, size: u64) -> bool {
        if size > SHM_LIMIT {
            return false;
        }
        let want = size.div_ceil(PAGE_SIZE) as usize;
        let mut pages = self.pages.lock();
        while pages.len() < want {
            match pmm::alloc_frame_zeroed() {
                Some(f) => pages.push(f),
                None => return false,
            }
        }
        // Keep pages that may still be mapped somewhere; only the size
        // shrinks (their contents are unreachable through read/write).
        self.size.store(size, Ordering::Relaxed);
        true
    }

    /// Physical page `i`, allocating up to it if needed.
    pub fn page(&self, i: usize) -> Option<u64> {
        let mut pages = self.pages.lock();
        while pages.len() <= i {
            pages.push(pmm::alloc_frame_zeroed()?);
        }
        Some(pages[i])
    }

    pub fn read_at(&self, off: u64, buf: &mut [u8]) -> usize {
        let size = self.size();
        if off >= size {
            return 0;
        }
        let n = (buf.len() as u64).min(size - off) as usize;
        let mut done = 0;
        while done < n {
            let pos = off + done as u64;
            let Some(pg) = self.page((pos / PAGE_SIZE) as usize) else { break };
            let in_page = (PAGE_SIZE - pos % PAGE_SIZE) as usize;
            let k = in_page.min(n - done);
            unsafe {
                core::ptr::copy_nonoverlapping((phys_to_virt(pg) + pos % PAGE_SIZE) as *const u8, buf[done..].as_mut_ptr(), k);
            }
            done += k;
        }
        done
    }

    pub fn write_at(&self, off: u64, data: &[u8]) -> usize {
        let end = off + data.len() as u64;
        if end > self.size() && !self.resize(end) {
            return 0;
        }
        let mut done = 0;
        while done < data.len() {
            let pos = off + done as u64;
            let Some(pg) = self.page((pos / PAGE_SIZE) as usize) else { break };
            let k = ((PAGE_SIZE - pos % PAGE_SIZE) as usize).min(data.len() - done);
            unsafe {
                core::ptr::copy_nonoverlapping(data[done..].as_ptr(), (phys_to_virt(pg) + pos % PAGE_SIZE) as *mut u8, k);
            }
            done += k;
        }
        done
    }

    /// Copy `len` bytes from `off` into `out` (for the compositor).
    pub fn copy_out(&self, off: u64, out: &mut [u8]) -> bool {
        self.read_at(off, out) == out.len()
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        for &f in self.pages.lock().iter() {
            pmm::free_frame(f);
        }
    }
}

/// POSIX shared memory objects by name (/dev/shm/NAME).
static SHM_NAMES: Spin<BTreeMap<String, Arc<Shm>>> = Spin::new(BTreeMap::new());

pub fn shm_open(name: &str, create: bool, excl: bool, trunc: bool) -> Result<Arc<Shm>, i64> {
    let mut m = SHM_NAMES.lock();
    if let Some(s) = m.get(name) {
        if create && excl {
            return Err(-17); // EEXIST
        }
        if trunc {
            s.resize(0);
        }
        return Ok(s.clone());
    }
    if !create {
        return Err(-2); // ENOENT
    }
    let s = Shm::new();
    m.insert(String::from(name), s.clone());
    Ok(s)
}

pub fn shm_unlink(name: &str) -> bool {
    SHM_NAMES.lock().remove(name).is_some()
}

pub fn shm_names() -> Vec<String> {
    SHM_NAMES.lock().keys().cloned().collect()
}

// -------------------------------------------------------------------------
// eventfd
// -------------------------------------------------------------------------

pub struct EventFd {
    pub count: Spin<u64>,
    pub semaphore: bool,
}

impl EventFd {
    pub fn new(init: u64, semaphore: bool) -> Arc<EventFd> {
        Arc::new(EventFd { count: Spin::new(init), semaphore })
    }

    /// Non-blocking read: the value (or 1 in semaphore mode).
    pub fn take(&self) -> Option<u64> {
        let mut c = self.count.lock();
        if *c == 0 {
            return None;
        }
        if self.semaphore {
            *c -= 1;
            Some(1)
        } else {
            Some(core::mem::take(&mut *c))
        }
    }

    pub fn add(&self, v: u64) {
        let mut c = self.count.lock();
        *c = c.saturating_add(v).min(u64::MAX - 1);
    }
}

// -------------------------------------------------------------------------
// epoll
// -------------------------------------------------------------------------

pub struct Interest {
    pub fd: i32,
    pub events: u32,
    pub data: u64,
    pub desc: DescRef,
    /// EPOLLONESHOT already fired.
    pub disabled: bool,
}

pub struct Epoll {
    pub list: Spin<Vec<Interest>>,
}

impl Epoll {
    pub fn new() -> Arc<Epoll> {
        Arc::new(Epoll { list: Spin::new(Vec::new()) })
    }
}

