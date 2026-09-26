//! Linux compatibility: MayOS runs statically linked Linux x86_64 programs
//! (musl or glibc static, Rust's `x86_64-unknown-linux-musl`) by speaking
//! the Linux system-call ABI. Memory is handed out on first touch (so
//! programs can reserve large stacks and heaps), threads are `clone`d
//! kernel threads sharing the address space, and files, sockets and pipes
//! live in a per-process descriptor table.

use alloc::collections::VecDeque;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use super::process::Process;
use super::screen::{self, InputKind, Screen};
use super::unix::{self, Endpoint, EventFd, Epoll, Listener, Shm};
use super::{sched, signal, usermem};
use crate::arch::idt::TrapFrame;
use crate::fs;
use crate::mem::paging::{self, NO_EXECUTE, USER, WRITABLE};
use crate::mem::{pmm, PAGE_SIZE};
use crate::network::tcp::{TcpListener, TcpStream};
use crate::sync::{Mutex, Spin};

// errno values (returned negated)
const EPERM: i64 = 1;
const ENOENT: i64 = 2;
const EINTR: i64 = 4;
const EIO: i64 = 5;
const EBADF: i64 = 9;
const ECHILD: i64 = 10;
const EAGAIN: i64 = 11;
const ENOMEM: i64 = 12;
const EFAULT: i64 = 14;
const EEXIST: i64 = 17;
const ENOTDIR: i64 = 20;
const ENODEV: i64 = 19;
const EISDIR: i64 = 21;
const ENOEXEC: i64 = 8;
const ESRCH: i64 = 3;
const E2BIG: i64 = 7;
const EINVAL: i64 = 22;
const ENOTTY: i64 = 25;
const ESPIPE: i64 = 29;
const EPIPE: i64 = 32;
const ERANGE: i64 = 34;
const ENOSYS: i64 = 38;
const ENOTEMPTY: i64 = 39;
const ENOTSOCK: i64 = 88;
const EAFNOSUPPORT: i64 = 97;
const EADDRINUSE: i64 = 98;
const ENOTCONN: i64 = 107;
const ETIMEDOUT: i64 = 110;
const ECONNREFUSED: i64 = 111;
const ECONNRESET: i64 = 104;
const EHOSTUNREACH: i64 = 113;

const MMAP_BASE: u64 = 0x0000_7000_0000_0000;
const STACK_SIZE: u64 = 8 * 1024 * 1024;

/// A range of address space whose pages are allocated when first touched.
#[derive(Clone, Copy)]
pub struct Region {
    pub start: u64,
    pub end: u64,
    pub writable: bool,
    pub exec: bool,
    /// File whose contents fill the pages (index + 1 into
    /// `LinuxState::mapped`, 0 for zero-filled memory), and the file offset
    /// of `start`.
    pub file: u32,
    pub foff: u64,
    /// PROT_NONE: reserved, every access faults.
    pub none: bool,
}

impl Region {
    fn anon(start: u64, end: u64, writable: bool, exec: bool) -> Region {
        Region { start, end, writable, exec, file: 0, foff: 0, none: false }
    }

    /// The part of this region from `start` on.
    fn from(&self, start: u64) -> Region {
        Region { start, foff: if self.file != 0 { self.foff + (start - self.start) } else { 0 }, ..*self }
    }
}

pub struct Pipe {
    buf: Spin<VecDeque<u8>>,
    writers: core::sync::atomic::AtomicUsize,
    readers: core::sync::atomic::AtomicUsize,
}

pub enum Desc {
    Console,
    File {
        path: String,
        /// Whole contents for files opened for writing.
        data: Option<Vec<u8>>,
        /// Streaming reader for read-only files.
        file: Option<fs::File>,
        size: u64,
        pos: u64,
        writable: bool,
        append: bool,
        dirty: bool,
    },
    /// In-memory contents (/etc/resolv.conf, /proc/...).
    Virtual { data: Vec<u8>, pos: usize },
    Dir { path: String, entries: Vec<(String, bool)>, pos: usize },
    Null,
    Zero,
    Random,
    Tcp { stream: Option<TcpStream>, listener: Option<TcpListener>, bound: u16, nonblock: bool },
    Udp { port: u16, remote: Option<(net::Ipv4, u16)>, nonblock: bool },
    PipeRead(Arc<Pipe>),
    PipeWrite(Arc<Pipe>),
    /// /dev/fb0
    Fb { screen: Arc<Screen>, pos: u64 },
    /// /dev/input/event0, event1, mice
    Input { screen: Arc<Screen>, kind: InputKind, nonblock: bool },
    /// Unix-domain stream socket: connected, listening, or neither yet.
    Unix { ep: Option<Endpoint>, listener: Option<Arc<Listener>>, bound: Option<String>, nonblock: bool },
    /// memfd_create / shm_open memory.
    Memfd { shm: Arc<Shm>, pos: u64 },
    EventFd { ev: Arc<EventFd>, nonblock: bool },
    Epoll(Arc<Epoll>),
    TimerFd { t: Arc<TimerFd>, nonblock: bool },
}

/// timerfd: expirations counted against the uptime clock (microseconds).
pub struct TimerFd {
    /// (next expiry, interval); next = 0 when disarmed.
    state: Spin<(u64, u64)>,
    realtime: bool,
}

impl TimerFd {
    /// Expirations since the last read (and re-arm the timer).
    fn take(&self) -> u64 {
        let now = crate::time::uptime_us();
        let mut s = self.state.lock();
        let (next, interval) = *s;
        if next == 0 || now < next {
            return 0;
        }
        if interval == 0 {
            *s = (0, 0);
            1
        } else {
            let n = 1 + (now - next) / interval;
            s.0 = next + n * interval;
            n
        }
    }

    fn ready(&self) -> bool {
        let (next, _) = *self.state.lock();
        next != 0 && crate::time::uptime_us() >= next
    }
}

pub type DescRef = Arc<Mutex<Desc>>;

pub struct LinuxState {
    pub fds: Spin<Vec<Option<DescRef>>>,
    pub regions: Spin<Vec<Region>>,
    pub mmap_next: Spin<u64>,
    /// (heap start, current break)
    pub brk: Spin<(u64, u64)>,
    pub cwd: Spin<String>,
    pub exe: Spin<String>,
    /// Descriptors closed by execve (O_CLOEXEC / FD_CLOEXEC).
    pub cloexec: Spin<alloc::collections::BTreeSet<usize>>,
    /// Window for /dev/fb0 and /dev/input, made on first use.
    pub screen: Spin<Option<Arc<Screen>>>,
    /// Shared memory mapped into this process (kept while it lives).
    pub shared: Spin<Vec<Arc<Shm>>>,
    /// Signal handlers, masks and pending signals.
    pub sig: Spin<super::signal::Signals>,
    /// Files with mappings (see `Region::file`).
    pub mapped: Spin<Vec<DescRef>>,
}

impl Drop for Desc {
    fn drop(&mut self) {
        match self {
            Desc::File { path, data: Some(d), dirty: true, .. } => {
                let _ = fs::write_file(path, d);
            }
            Desc::Udp { port, .. } => crate::network::udp_unbind(*port),
            Desc::PipeRead(p) => {
                p.readers.fetch_sub(1, Ordering::Relaxed);
            }
            Desc::PipeWrite(p) => {
                p.writers.fetch_sub(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

// -------------------------------------------------------------------------
// Process start
// -------------------------------------------------------------------------

/// Build the Linux initial stack (argc, argv, envp, auxv) for a program
/// whose image is loaded; returns (state, initial rsp).
/// Where dynamic linkers are loaded.
const INTERP_BASE: u64 = 0x0000_7f00_0000_0000;

fn default_env(cwd: &str) -> Vec<String> {
    alloc::vec![
        String::from("PATH=/bin:/usr/bin:/sbin:/usr/sbin"),
        String::from("HOME=/home"),
        String::from("USER=user"),
        String::from("LANG=C.UTF-8"),
        String::from("TERM=xterm"),
        String::from("XDG_RUNTIME_DIR=/run"),
        String::from("WAYLAND_DISPLAY=wayland-0"),
        String::from("GDK_BACKEND=wayland"),
        String::from("NO_AT_BRIDGE=1"),
        String::from("XDG_DATA_DIRS=/usr/share"),
        // Firefox: Wayland, software drawing, no sandboxes (they need
        // seccomp and namespaces), no crash reporter.
        String::from("MOZ_ENABLE_WAYLAND=1"),
        String::from("MOZ_DISABLE_CONTENT_SANDBOX=1"),
        String::from("MOZ_DISABLE_GMP_SANDBOX=1"),
        String::from("MOZ_DISABLE_RDD_SANDBOX=1"),
        String::from("MOZ_DISABLE_SOCKET_PROCESS_SANDBOX=1"),
        String::from("MOZ_DISABLE_UTILITY_SANDBOX=1"),
        String::from("MOZ_CRASHREPORTER_DISABLE=1"),
        String::from("MOZ_FORCE_DISABLE_E10S=1"),
        String::from("LIBGL_ALWAYS_SOFTWARE=1"),
        alloc::format!("PWD={}", cwd),
    ]
}

/// Load the program's dynamic linker, if it names one: (entry, AT_BASE).
fn load_interp(pml4: u64, image: &super::elf::LoadedImage) -> Result<(u64, u64), String> {
    let Some(path) = &image.interp else { return Ok((image.entry, 0)) };
    let data = fs::read_file(&fs::resolve_link(path)).map_err(|e| alloc::format!("dynamic linker {}: {} (copy it from the Linux distribution the program comes from)", path, e))?;
    let li = super::elf::load_at(pml4, &data, INTERP_BASE)?;
    Ok((li.entry, INTERP_BASE))
}

pub fn setup(pml4: u64, image: &super::elf::LoadedImage, path: &str, args: &str, cwd: &str) -> Result<(LinuxState, u64, u64), String> {
    let mut argv: Vec<String> = alloc::vec![String::from(path)];
    argv.extend(split_args(args));
    let (entry, at_base) = load_interp(pml4, image)?;
    let (rsp, stack) = build_stack(pml4, image, at_base, &argv, &default_env(cwd), path)?;
    let state = LinuxState {
        fds: Spin::new(alloc::vec![
            Some(Arc::new(Mutex::new(Desc::Console))),
            Some(Arc::new(Mutex::new(Desc::Console))),
            Some(Arc::new(Mutex::new(Desc::Console))),
        ]),
        regions: Spin::new(alloc::vec![stack]),
        mmap_next: Spin::new(MMAP_BASE),
        brk: Spin::new((image.brk, image.brk)),
        cwd: Spin::new(String::from(cwd)),
        exe: Spin::new(fs::resolve_link(path)),
        cloexec: Spin::new(alloc::collections::BTreeSet::new()),
        screen: Spin::new(None),
        shared: Spin::new(Vec::new()),
        sig: Spin::new(super::signal::Signals::default()),
        mapped: Spin::new(Vec::new()),
    };
    Ok((state, rsp, entry))
}

/// Build the initial stack (argc, argv, envp, auxv) in `pml4`; returns the
/// stack pointer and the stack's demand-paged region.
fn build_stack(pml4: u64, image: &super::elf::LoadedImage, at_base: u64, argv: &[String], env: &[String], path: &str) -> Result<(u64, Region), String> {
    let top = super::process::STACK_TOP;
    let bottom = top - STACK_SIZE;
    // The top of the stack is needed right away.
    let eager = 16u64;
    let mut frames = Vec::new();
    for i in 0..eager {
        let f = pmm::alloc_frame_zeroed().ok_or("out of memory")?;
        paging::map(pml4, top - (i + 1) * PAGE_SIZE, f, USER | WRITABLE | NO_EXECUTE).map_err(|_| "map failed")?;
        frames.push((top - (i + 1) * PAGE_SIZE, f));
    }
    // Write into the new address space through the kernel's physical map.
    let poke = |addr: u64, bytes: &[u8]| {
        for (k, b) in bytes.iter().enumerate() {
            let a = addr + k as u64;
            let page = a & !(PAGE_SIZE - 1);
            if let Some(&(_, f)) = frames.iter().find(|(p, _)| *p == page) {
                unsafe { *((crate::mem::phys_to_virt(f) + (a - page)) as *mut u8) = *b };
            }
        }
    };
    // Strings at the very top.
    let mut sp = top - 16;
    let mut put_str = |s: &str| -> u64 {
        sp -= s.len() as u64 + 1;
        poke(sp, s.as_bytes());
        poke(sp + s.len() as u64, &[0]);
        sp
    };
    let argv_ptrs: Vec<u64> = argv.iter().map(|a| put_str(a)).collect();
    let env_ptrs: Vec<u64> = env.iter().map(|e| put_str(e)).collect();
    let execfn = put_str(path);
    let platform = put_str("x86_64");
    // 16 random bytes for AT_RANDOM.
    sp -= 16;
    let random = sp;
    let mut rnd = [0u8; 16];
    fill_random(&mut rnd);
    poke(random, &rnd);
    sp &= !15;
    let auxv: [(u64, u64); 17] = [
        (3, image.phdr),   // AT_PHDR
        (4, image.phent),  // AT_PHENT
        (5, image.phnum),  // AT_PHNUM
        (6, PAGE_SIZE),    // AT_PAGESZ
        (7, at_base),      // AT_BASE (dynamic linker)
        (8, 0),            // AT_FLAGS
        (9, image.entry),  // AT_ENTRY
        (11, 1000),        // AT_UID
        (12, 1000),        // AT_EUID
        (13, 1000),        // AT_GID
        (14, 1000),        // AT_EGID
        (16, 0x0078_bfbf_f), // AT_HWCAP (fpu..sse2)
        (17, 100),         // AT_CLKTCK
        (23, 0),           // AT_SECURE
        (25, random),      // AT_RANDOM
        (31, execfn),      // AT_EXECFN
        (15, platform),    // AT_PLATFORM
    ];
    let words = 1 + argv_ptrs.len() + 1 + env_ptrs.len() + 1 + (auxv.len() + 1) * 2;
    sp -= words as u64 * 8;
    sp &= !15;
    let mut w = sp;
    let mut put = |v: u64| {
        poke(w, &v.to_le_bytes());
        w += 8;
    };
    put(argv_ptrs.len() as u64);
    for p in &argv_ptrs {
        put(*p);
    }
    put(0);
    for p in &env_ptrs {
        put(*p);
    }
    put(0);
    for (k, v) in auxv {
        put(k);
        put(v);
    }
    put(0);
    put(0);
    Ok((sp, Region::anon(bottom, top - eager * PAGE_SIZE, true, false)))
}

fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote = None;
    let mut any = false;
    for c in s.chars() {
        match (c, quote) {
            ('"' | '\'', None) => {
                quote = Some(c);
                any = true;
            }
            (q, Some(o)) if q == o => quote = None,
            (' ' | '\t', None) => {
                if any || !cur.is_empty() {
                    out.push(core::mem::take(&mut cur));
                    any = false;
                }
            }
            (c, _) => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

// -------------------------------------------------------------------------
// Memory
// -------------------------------------------------------------------------

fn linux(p: &Process) -> Option<&LinuxState> {
    p.linux.as_deref()
}

/// Map a page of a demand region if `addr` is in one. Used by the page
/// fault handler and by system calls touching user memory.
pub fn fault_in(addr: u64, write: bool) -> bool {
    let Some(p) = sched::current_process() else { return false };
    let Some(l) = linux(&p) else { return false };
    let page = addr & !(PAGE_SIZE - 1);
    let region = l.regions.lock().iter().find(|r| page >= r.start && page < r.end).copied();
    let Some(r) = region else { return false };
    if r.none || write && !r.writable {
        return false;
    }
    if paging::translate(p.pml4(), page).is_some() {
        return true;
    }
    let Some(f) = pmm::alloc_frame_zeroed() else { return false };
    if r.file != 0 {
        let d = l.mapped.lock().get(r.file as usize - 1).cloned();
        if let Some(d) = d {
            let buf = unsafe { core::slice::from_raw_parts_mut(crate::mem::phys_to_virt(f) as *mut u8, PAGE_SIZE as usize) };
            let mut g = d.lock();
            let mut n = 0usize;
            while n < buf.len() {
                let got = read_at(&mut g, r.foff + (page - r.start) + n as u64, &mut buf[n..]);
                if got <= 0 {
                    break;
                }
                n += got as usize;
            }
        }
        // Another thread may have brought the page in meanwhile.
        if paging::translate(p.pml4(), page).is_some() {
            pmm::free_frame(f);
            return true;
        }
    }
    let mut flags = USER;
    if r.writable {
        flags |= WRITABLE;
    }
    if !r.exec {
        flags |= NO_EXECUTE;
    }
    if paging::map(p.pml4(), page, f, flags).is_err() {
        pmm::free_frame(f);
        return false;
    }
    true
}

/// Page fault from a Linux program: true if it was handled.
pub fn page_fault(addr: u64, error: u64) -> bool {
    // Only faults on missing pages (bit 0 clear) can be demand paging.
    error & 1 == 0 && fault_in(addr, error & 2 != 0)
}

fn unmap_range(p: &Process, start: u64, end: u64) {
    let mut a = start;
    while a < end {
        let borrowed = paging::translate(p.pml4(), a).is_some_and(|(_, e)| e & paging::BORROWED != 0);
        if let Some(f) = paging::unmap(p.pml4(), a)
            && !borrowed
        {
            pmm::free_frame(f);
        }
        a += PAGE_SIZE;
    }
}

fn remove_regions(l: &LinuxState, start: u64, end: u64) {
    let mut regs = l.regions.lock();
    let mut out = Vec::new();
    for r in regs.iter() {
        if r.end <= start || r.start >= end {
            out.push(*r);
            continue;
        }
        if r.start < start {
            out.push(Region { end: start, ..*r });
        }
        if r.end > end {
            out.push(r.from(end));
        }
    }
    *regs = out;
}

fn sys_mmap(p: &Process, addr: u64, len: u64, prot: u64, flags: u64, fd: i64, off: u64) -> i64 {
    let Some(l) = linux(p) else { return -ENOSYS };
    if len == 0 {
        return -EINVAL;
    }
    let len = len.div_ceil(PAGE_SIZE) * PAGE_SIZE;
    const MAP_FIXED: u64 = 0x10;
    const MAP_ANONYMOUS: u64 = 0x20;
    const MAP_FIXED_NOREPLACE: u64 = 0x100000;
    let start = if flags & (MAP_FIXED | MAP_FIXED_NOREPLACE) != 0 {
        if addr & (PAGE_SIZE - 1) != 0 {
            return -EINVAL;
        }
        remove_regions(l, addr, addr + len);
        unmap_range(p, addr, addr + len);
        addr
    } else {
        let mut next = l.mmap_next.lock();
        let s = *next;
        *next += len + PAGE_SIZE; // a gap between mappings
        s
    };
    if flags & MAP_ANONYMOUS == 0
        && let Some(d) = get_fd(p, fd)
        && let Desc::Fb { screen, .. } = &*d.lock()
    {
        // Shared mapping of the screen memory itself.
        let (phys, pages) = screen.map();
        let first = off / PAGE_SIZE;
        let n = (len / PAGE_SIZE).min((pages as u64).saturating_sub(first));
        if n == 0 {
            return -EINVAL;
        }
        for i in 0..n {
            let f = phys + (first + i) * PAGE_SIZE;
            if paging::map(p.pml4(), start + i * PAGE_SIZE, f, USER | WRITABLE | NO_EXECUTE | paging::BORROWED).is_err() {
                return -ENOMEM;
            }
        }
        return start as i64;
    }
    const MAP_SHARED: u64 = 1;
    if flags & MAP_ANONYMOUS == 0 && flags & 3 == MAP_SHARED
        && let Some(d) = get_fd(p, fd)
        && let Desc::Memfd { shm, .. } = &*d.lock()
    {
        // The same pages as every other mapping of this memory.
        let first = off / PAGE_SIZE;
        let mut rights = USER | paging::BORROWED;
        if prot & 2 != 0 {
            rights |= WRITABLE;
        }
        if prot & 4 == 0 {
            rights |= NO_EXECUTE;
        }
        for i in 0..len / PAGE_SIZE {
            let Some(pg) = shm.page((first + i) as usize) else { return -ENOMEM };
            if paging::map(p.pml4(), start + i * PAGE_SIZE, pg, rights).is_err() {
                return -ENOMEM;
            }
        }
        l.shared.lock().push(shm.clone());
        return start as i64;
    }
    let writable = prot & 2 != 0 || flags & MAP_ANONYMOUS == 0;
    let mut region = Region::anon(start, start + len, writable || prot == 0, prot & 4 != 0);
    region.none = prot == 0;
    if flags & MAP_ANONYMOUS == 0 {
        // File mapping (private): pages are read from the file when first
        // touched, so large libraries cost only what is used.
        let Some(d) = get_fd(p, fd) else { return -EBADF };
        if !matches!(&*d.lock(), Desc::File { .. } | Desc::Memfd { .. } | Desc::Virtual { .. }) {
            return -ENODEV;
        }
        let mut m = l.mapped.lock();
        let idx = match m.iter().position(|x| Arc::ptr_eq(x, &d)) {
            Some(i) => i,
            None => {
                m.push(d);
                m.len() - 1
            }
        };
        region.file = idx as u32 + 1;
        region.foff = off;
    }
    l.regions.lock().push(region);
    start as i64
}

fn sys_munmap(p: &Process, addr: u64, len: u64) -> i64 {
    let Some(l) = linux(p) else { return -ENOSYS };
    let end = addr + len.div_ceil(PAGE_SIZE) * PAGE_SIZE;
    remove_regions(l, addr, end);
    unmap_range(p, addr, end);
    0
}

fn sys_brk(p: &Process, addr: u64) -> i64 {
    let Some(l) = linux(p) else { return -ENOSYS };
    let mut b = l.brk.lock();
    if addr == 0 || addr < b.0 || addr > b.0 + (1 << 36) {
        return b.1 as i64;
    }
    let old_top = b.1.div_ceil(PAGE_SIZE) * PAGE_SIZE;
    let new_top = addr.div_ceil(PAGE_SIZE) * PAGE_SIZE;
    if new_top > old_top {
        l.regions.lock().push(Region::anon(old_top, new_top, true, false));
    } else if new_top < old_top {
        drop(b);
        remove_regions(l, new_top, old_top);
        unmap_range(p, new_top, old_top);
        b = l.brk.lock();
    }
    b.1 = addr;
    addr as i64
}

// -------------------------------------------------------------------------
// Descriptors
// -------------------------------------------------------------------------

fn get_fd(p: &Process, fd: i64) -> Option<DescRef> {
    let l = linux(p)?;
    if fd < 0 {
        return None;
    }
    l.fds.lock().get(fd as usize).cloned().flatten()
}

fn add_fd(p: &Process, d: Desc) -> i64 {
    add_fd_ref(p, Arc::new(Mutex::new(d)), 0)
}

fn add_fd_ref(p: &Process, d: DescRef, min: usize) -> i64 {
    let Some(l) = linux(p) else { return -ENOSYS };
    let mut fds = l.fds.lock();
    for i in min..fds.len() {
        if fds[i].is_none() {
            fds[i] = Some(d);
            return i as i64;
        }
    }
    if fds.len() >= 1024 {
        return -24; // EMFILE
    }
    while fds.len() < min {
        fds.push(None);
    }
    fds.push(Some(d));
    fds.len() as i64 - 1
}

fn fs_err(e: fs::FsError) -> i64 {
    -match e {
        fs::FsError::NotFound => ENOENT,
        fs::FsError::NotADirectory => ENOTDIR,
        fs::FsError::IsADirectory => EISDIR,
        fs::FsError::AlreadyExists => EEXIST,
        fs::FsError::DirectoryNotEmpty => ENOTEMPTY,
        fs::FsError::InvalidName | fs::FsError::InvalidPath => EINVAL,
        fs::FsError::NoSpace => 28,
        _ => EIO,
    }
}

/// Resolve a path argument (relative to `dirfd` or the working directory).
fn path_at(p: &Process, dirfd: i64, ptr: u64) -> Result<String, i64> {
    let s = usermem::read_cstr(p.pml4(), ptr, 4096).ok_or(-EFAULT)?;
    let base = if s.starts_with('/') || dirfd == -100 {
        linux(p).map(|l| l.cwd.lock().clone()).unwrap_or_else(|| String::from("/"))
    } else {
        match get_fd(p, dirfd).map(|d| match &*d.lock() {
            Desc::Dir { path, .. } => Some(path.clone()),
            _ => None,
        }) {
            Some(Some(path)) => path,
            _ => return Err(-EBADF),
        }
    };
    Ok(fs::normalize(&base, &s))
}

fn virtual_file(path: &str) -> Option<Desc> {
    let text = match path {
        "/dev/null" => return Some(Desc::Null),
        "/dev/zero" => return Some(Desc::Zero),
        "/dev/urandom" | "/dev/random" => return Some(Desc::Random),
        "/dev/tty" | "/dev/stdout" | "/dev/stderr" | "/dev/stdin" => return Some(Desc::Console),
        "/etc/resolv.conf" => {
            let dns = crate::network::status().map(|s| s.dns).unwrap_or_default();
            let mut t = String::new();
            for d in dns.iter().take(3) {
                t.push_str(&alloc::format!("nameserver {}\n", d));
            }
            if t.is_empty() {
                t.push_str("nameserver 10.0.2.3\n");
            }
            t
        }
        "/etc/hosts" => String::from("127.0.0.1 localhost\n::1 localhost\n"),
        "/etc/passwd" => String::from("root:x:0:0:root:/root:/bin/sh\nuser:x:1000:1000:user:/home:/bin/sh\n"),
        "/etc/group" => String::from("root:x:0:\nuser:x:1000:\n"),
        "/etc/hostname" => String::from("mayos\n"),
        "/etc/os-release" => String::from("NAME=MayOS\nID=mayos\nPRETTY_NAME=\"MayOS\"\n"),
        "/proc/cpuinfo" => String::from("processor\t: 0\nvendor_id\t: GenuineIntel\nmodel name\t: MayOS virtual CPU\nflags\t\t: fpu sse sse2\n\n"),
        "/proc/meminfo" => {
            let (free, total) = pmm::stats();
            alloc::format!("MemTotal: {} kB\nMemFree: {} kB\nMemAvailable: {} kB\n", total * 4, free * 4, free * 4)
        }
                _ => return None,
    };
    Some(Desc::Virtual { data: text.into_bytes(), pos: 0 })
}

/// The per-process screen, created (and its window opened) on first use.
fn process_screen(p: &Process) -> Option<Arc<Screen>> {
    let l = linux(p)?;
    let mut s = l.screen.lock();
    if s.is_none() {
        let exe = l.exe.lock().clone();
        let name = exe.rsplit('/').next().unwrap_or("Linux program");
        *s = Screen::new(p.pid, String::from(name));
    }
    s.clone()
}

fn graphics_file(p: &Process, path: &str, flags: u64) -> Option<i64> {
    let kind = match path {
        "/dev/fb0" | "/dev/fb/0" => None,
        "/dev/input/event0" => Some(InputKind::Keyboard),
        "/dev/input/event1" => Some(InputKind::Pointer),
        "/dev/input/mice" | "/dev/input/mouse0" => Some(InputKind::Mice),
        _ => return None,
    };
    let Some(screen) = process_screen(p) else { return Some(-ENOMEM) };
    let d = match kind {
        None => Desc::Fb { screen, pos: 0 },
        Some(kind) => Desc::Input { screen, kind, nonblock: flags & O_NONBLOCK != 0 },
    };
    Some(add_fd(p, d))
}

const O_ACCMODE: u64 = 3;
const O_CREAT: u64 = 0x40;
const O_EXCL: u64 = 0x80;
const O_TRUNC: u64 = 0x200;
const O_APPEND: u64 = 0x400;
const O_NONBLOCK: u64 = 0x800;
const O_DIRECTORY: u64 = 0x10000;
const O_CLOEXEC: u64 = 0x80000;

fn sys_openat(p: &Process, dirfd: i64, ptr: u64, flags: u64) -> i64 {
    let path = match path_at(p, dirfd, ptr) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let fd = openat_inner(p, path, flags);
    if fd >= 0 && flags & O_CLOEXEC != 0
        && let Some(l) = linux(p)
    {
        l.cloexec.lock().insert(fd as usize);
    }
    fd
}

fn openat_inner(p: &Process, path: String, flags: u64) -> i64 {
    if let Some(d) = proc_file(p, &path) {
        return add_fd(p, d);
    }
    if let Some(d) = virtual_file(&path) {
        return add_fd(p, d);
    }
    if let Some(d) = graphics_file(p, &path, flags) {
        return d;
    }
    if let Some(name) = path.strip_prefix("/dev/shm/") {
        return match unix::shm_open(name, flags & O_CREAT != 0, flags & O_EXCL != 0, flags & O_TRUNC != 0) {
            Ok(shm) => add_fd(p, Desc::Memfd { shm, pos: 0 }),
            Err(e) => e,
        };
    }
    if matches!(proc_self(p, &path).as_str(), "/proc/self/fd" | "/proc/self/task") {
        let entries: Vec<(String, bool)> = if proc_self(p, &path).ends_with("fd") {
            let fds: Vec<String> = linux(p).map(|l| l.fds.lock().iter().enumerate().filter(|(_, d)| d.is_some()).map(|(i, _)| alloc::format!("{}", i)).collect()).unwrap_or_default();
            fds.into_iter().map(|n: String| (n, false)).collect()
        } else {
            sched::process_thread_ids(p.pid).into_iter().map(|t| (alloc::format!("{}", t), true)).collect()
        };
        return add_fd(p, Desc::Dir { path: proc_self(p, &path), entries, pos: 0 });
    }
    if path == "/dev/input" {
        let entries = ["event0", "event1", "mice"].iter().map(|n| (String::from(*n), false)).collect();
        return add_fd(p, Desc::Dir { path, entries, pos: 0 });
    }
    let writable = flags & O_ACCMODE != 0;
    if fs::is_dir(&path) {
        if writable {
            return -EISDIR;
        }
        let entries = match fs::read_dir(&path) {
            Ok(e) => e.into_iter().map(|e| (e.name, e.is_dir)).collect(),
            Err(e) => return fs_err(e),
        };
        return add_fd(p, Desc::Dir { path, entries, pos: 0 });
    }
    if flags & O_DIRECTORY != 0 {
        return if fs::exists(&path) { -ENOTDIR } else { -ENOENT };
    }
    let exists = fs::exists(&path);
    if exists && flags & O_CREAT != 0 && flags & O_EXCL != 0 {
        return -EEXIST;
    }
    if !exists {
        if flags & O_CREAT == 0 {
            return -ENOENT;
        }
        if let Err(e) = fs::create_file(&path) {
            return fs_err(e);
        }
    }
    let desc = if writable {
        let data = if flags & O_TRUNC != 0 { Vec::new() } else { fs::read_file(&path).unwrap_or_default() };
        let size = data.len() as u64;
        Desc::File {
            path,
            data: Some(data),
            file: None,
            size,
            pos: 0,
            writable: true,
            append: flags & O_APPEND != 0,
            dirty: flags & O_TRUNC != 0 || !exists,
        }
    } else {
        match fs::open(&fs::resolve_link(&path)) {
            Ok(f) => Desc::File { size: f.size, path, data: None, file: Some(f), pos: 0, writable: false, append: false, dirty: false },
            Err(e) => return fs_err(e),
        }
    };
    add_fd(p, desc)
}

/// Read from a file-like descriptor at `off` (without moving its position).
fn read_at(d: &mut Desc, off: u64, buf: &mut [u8]) -> i64 {
    match d {
        Desc::File { data: Some(data), .. } => {
            let s = (off as usize).min(data.len());
            let n = buf.len().min(data.len() - s);
            buf[..n].copy_from_slice(&data[s..s + n]);
            n as i64
        }
        Desc::File { file: Some(f), .. } => f.read_at(off, buf).map(|n| n as i64).unwrap_or(-EIO),
        Desc::Virtual { data, .. } => {
            let s = (off as usize).min(data.len());
            let n = buf.len().min(data.len() - s);
            buf[..n].copy_from_slice(&data[s..s + n]);
            n as i64
        }
        Desc::Memfd { shm, .. } => shm.read_at(off, buf) as i64,
        _ => -ESPIPE,
    }
}

fn read_desc(p: &Process, d: &DescRef, buf: &mut [u8]) -> i64 {
    let mut g = d.lock();
    match &mut *g {
        Desc::Console => {
            drop(g);
            loop {
                match p.console.try_read(buf.len()) {
                    None => return 0,
                    Some(v) if !v.is_empty() => {
                        buf[..v.len()].copy_from_slice(&v);
                        return v.len() as i64;
                    }
                    Some(_) => sched::sleep_ms(10),
                }
            }
        }
        Desc::File { pos, .. } => {
            let off = *pos;
            let n = read_at(&mut g, off, buf);
            if n > 0
                && let Desc::File { pos, .. } = &mut *g
            {
                *pos += n as u64;
            }
            n
        }
        Desc::Virtual { data, pos } => {
            let n = buf.len().min(data.len().saturating_sub(*pos));
            buf[..n].copy_from_slice(&data[*pos..*pos + n]);
            *pos += n;
            n as i64
        }
        Desc::Null => 0,
        Desc::Zero => {
            buf.fill(0);
            buf.len() as i64
        }
        Desc::Random => {
            fill_random(buf);
            buf.len() as i64
        }
        Desc::Dir { .. } => -EISDIR,
        Desc::Tcp { stream: Some(s), nonblock, .. } => {
            let timeout = if *nonblock { 0 } else { 3_600_000 };
            match s.read(buf, timeout) {
                Ok(n) => n as i64,
                Err(crate::network::NetError::Timeout) if *nonblock => -EAGAIN,
                Err(crate::network::NetError::Timeout) => -ETIMEDOUT,
                Err(_) => -ECONNRESET,
            }
        }
        Desc::Tcp { .. } => -ENOTCONN,
        Desc::Udp { port, nonblock, .. } => {
            let (port, nb) = (*port, *nonblock);
            drop(g);
            match crate::network::udp_recv(port, if nb { 0 } else { 3_600_000 }) {
                Some((_, _, data)) => {
                    let n = data.len().min(buf.len());
                    buf[..n].copy_from_slice(&data[..n]);
                    n as i64
                }
                None => -EAGAIN,
            }
        }
        Desc::PipeRead(pipe) => {
            let pipe = pipe.clone();
            drop(g);
            loop {
                {
                    let mut q = pipe.buf.lock();
                    if !q.is_empty() {
                        let n = buf.len().min(q.len());
                        for (d, s) in buf.iter_mut().zip(q.drain(..n)) {
                            *d = s;
                        }
                        return n as i64;
                    }
                }
                if pipe.writers.load(Ordering::Relaxed) == 0 {
                    return 0;
                }
                sched::sleep_ms(2);
            }
        }
        Desc::PipeWrite(_) => -EBADF,
        Desc::Fb { screen, pos } => {
            let b = screen.buf.lock().clone();
            let px = b.pixels();
            let bytes = unsafe { core::slice::from_raw_parts(px.as_ptr() as *const u8, px.len() * 4) };
            let s = (*pos as usize).min(bytes.len());
            let n = buf.len().min(bytes.len() - s);
            buf[..n].copy_from_slice(&bytes[s..s + n]);
            *pos += n as u64;
            n as i64
        }
        Desc::Unix { ep: Some(ep), nonblock, .. } => {
            let (rx, nb) = (ep.rx.clone(), *nonblock);
            drop(g);
            match unix_wait(&rx, buf.len(), nb) {
                Ok((bytes, _fds)) => {
                    buf[..bytes.len()].copy_from_slice(&bytes);
                    bytes.len() as i64
                }
                Err(e) => e,
            }
        }
        Desc::Unix { .. } => -ENOTCONN,
        Desc::Memfd { shm, pos } => {
            let n = shm.read_at(*pos, buf);
            *pos += n as u64;
            n as i64
        }
        Desc::EventFd { ev, nonblock } => {
            let (ev, nb) = (ev.clone(), *nonblock);
            drop(g);
            if buf.len() < 8 {
                return -EINVAL;
            }
            loop {
                if let Some(v) = ev.take() {
                    buf[..8].copy_from_slice(&v.to_le_bytes());
                    return 8;
                }
                if nb {
                    return -EAGAIN;
                }
                sched::sleep_ms(1);
            }
        }
        Desc::Epoll(_) => -EINVAL,
        Desc::TimerFd { t, nonblock } => {
            let (t, nb) = (t.clone(), *nonblock);
            drop(g);
            if buf.len() < 8 {
                return -EINVAL;
            }
            loop {
                let n = t.take();
                if n > 0 {
                    buf[..8].copy_from_slice(&n.to_le_bytes());
                    return 8;
                }
                if nb {
                    return -EAGAIN;
                }
                if signal::interrupted(p) {
                    return -EINTR;
                }
                sched::sleep_ms(1);
            }
        }
        Desc::Input { screen, kind, nonblock } => {
            let (screen, kind, nb) = (screen.clone(), *kind, *nonblock);
            drop(g);
            if buf.len() < 24 && kind != InputKind::Mice {
                return -EINVAL;
            }
            loop {
                if let Some(n) = screen::read_input(&screen, kind, buf) {
                    return n as i64;
                }
                if screen.closed.load(Ordering::Relaxed) {
                    return -ENODEV;
                }
                if nb {
                    return -EAGAIN;
                }
                sched::sleep_ms(4);
            }
        }
    }
}

fn write_desc(p: &Process, d: &DescRef, data: &[u8]) -> i64 {
    let mut g = d.lock();
    match &mut *g {
        Desc::Console => {
            p.console.write(data);
            data.len() as i64
        }
        Desc::File { data: Some(buf), pos, writable: true, append, dirty, size, .. } => {
            if *append {
                *pos = buf.len() as u64;
            }
            let end = *pos as usize + data.len();
            if end > buf.len() {
                buf.resize(end, 0);
            }
            buf[*pos as usize..end].copy_from_slice(data);
            *pos = end as u64;
            *size = buf.len() as u64;
            *dirty = true;
            data.len() as i64
        }
        Desc::File { .. } | Desc::Dir { .. } => -EBADF,
        // /proc settings (oom_score_adj, ...): accepted and ignored.
        Desc::Virtual { .. } => data.len() as i64,
        Desc::Null | Desc::Zero | Desc::Random => data.len() as i64,
        Desc::Fb { screen, pos } => {
            let b = screen.buf.lock().clone();
            let len = (b.w * b.h * 4) as usize;
            let s = (*pos as usize).min(len);
            let n = data.len().min(len - s);
            unsafe {
                let dst = (crate::mem::phys_to_virt(b.phys) as *mut u8).add(s);
                core::ptr::copy_nonoverlapping(data.as_ptr(), dst, n);
            }
            *pos += n as u64;
            if n == 0 && !data.is_empty() { -28 } else { n as i64 } // ENOSPC
        }
        Desc::Input { .. } => data.len() as i64,
        Desc::Unix { ep: Some(ep), nonblock, .. } => unix_send(ep, *nonblock, data, Vec::new()),
        Desc::Unix { .. } => -ENOTCONN,
        Desc::Memfd { shm, pos } => {
            let n = shm.write_at(*pos, data);
            *pos += n as u64;
            n as i64
        }
        Desc::EventFd { ev, .. } => {
            if data.len() < 8 {
                return -EINVAL;
            }
            ev.add(u64::from_le_bytes(data[..8].try_into().unwrap()));
            8
        }
        Desc::Epoll(_) | Desc::TimerFd { .. } => -EINVAL,
        Desc::Tcp { stream: Some(s), .. } => match s.write_all(data, 60_000) {
            Ok(()) => data.len() as i64,
            Err(_) => -EPIPE,
        },
        Desc::Tcp { .. } => -ENOTCONN,
        Desc::Udp { port, remote: Some((ip, rport)), .. } => {
            crate::network::udp_send(*port, *ip, *rport, data);
            data.len() as i64
        }
        Desc::Udp { .. } => -107,
        Desc::PipeWrite(pipe) => {
            if pipe.readers.load(Ordering::Relaxed) == 0 {
                return -EPIPE;
            }
            pipe.buf.lock().extend(data.iter().copied());
            data.len() as i64
        }
        Desc::PipeRead(_) => -EBADF,
    }
}

fn sys_read(p: &Process, fd: i64, ptr: u64, len: u64) -> i64 {
    let Some(d) = get_fd(p, fd) else { return -EBADF };
    let mut buf = vec![0u8; (len as usize).min(4 * 1024 * 1024)];
    let n = read_desc(p, &d, &mut buf);
    if n > 0 && !usermem::write_bytes(p.pml4(), ptr, &buf[..n as usize]) {
        return -EFAULT;
    }
    n
}

fn sys_write(p: &Process, fd: i64, ptr: u64, len: u64) -> i64 {
    let Some(d) = get_fd(p, fd) else { return -EBADF };
    let Some(data) = usermem::read_bytes(p.pml4(), ptr, len.min(16 * 1024 * 1024)) else { return -EFAULT };
    write_desc(p, &d, &data)
}

fn iovecs(p: &Process, iov: u64, cnt: u64) -> Option<Vec<(u64, u64)>> {
    let mut v = Vec::new();
    for i in 0..cnt.min(1024) {
        let base = usermem::read_u64(p.pml4(), iov + i * 16)?;
        let len = usermem::read_u64(p.pml4(), iov + i * 16 + 8)?;
        v.push((base, len));
    }
    Some(v)
}

fn sys_writev(p: &Process, fd: i64, iov: u64, cnt: u64) -> i64 {
    let Some(vecs) = iovecs(p, iov, cnt) else { return -EFAULT };
    let mut all = Vec::new();
    for (b, l) in vecs {
        match usermem::read_bytes(p.pml4(), b, l) {
            Some(d) => all.extend_from_slice(&d),
            None => return -EFAULT,
        }
    }
    let Some(d) = get_fd(p, fd) else { return -EBADF };
    write_desc(p, &d, &all)
}

fn sys_readv(p: &Process, fd: i64, iov: u64, cnt: u64) -> i64 {
    let Some(vecs) = iovecs(p, iov, cnt) else { return -EFAULT };
    let total: u64 = vecs.iter().map(|v| v.1).sum();
    let Some(d) = get_fd(p, fd) else { return -EBADF };
    let mut buf = vec![0u8; total.min(4 * 1024 * 1024) as usize];
    let n = read_desc(p, &d, &mut buf);
    if n <= 0 {
        return n;
    }
    let mut off = 0usize;
    for (b, l) in vecs {
        if off >= n as usize {
            break;
        }
        let k = (l as usize).min(n as usize - off);
        if !usermem::write_bytes(p.pml4(), b, &buf[off..off + k]) {
            return -EFAULT;
        }
        off += k;
    }
    n
}

fn sys_pread(p: &Process, fd: i64, ptr: u64, len: u64, off: u64) -> i64 {
    let Some(d) = get_fd(p, fd) else { return -EBADF };
    let mut buf = vec![0u8; (len as usize).min(4 * 1024 * 1024)];
    let n = read_at(&mut d.lock(), off, &mut buf);
    if n > 0 && !usermem::write_bytes(p.pml4(), ptr, &buf[..n as usize]) {
        return -EFAULT;
    }
    n
}

fn sys_lseek(p: &Process, fd: i64, off: i64, whence: u64) -> i64 {
    let Some(d) = get_fd(p, fd) else { return -EBADF };
    let mut g = d.lock();
    let (pos, size): (&mut u64, u64) = match &mut *g {
        Desc::File { pos, size, .. } => (pos, *size),
        Desc::Memfd { shm, pos } => {
            let size = shm.size();
            (pos, size)
        }
        Desc::Virtual { data, pos } => {
            let size = data.len() as i64;
            let new = match whence {
                0 => off,
                1 => *pos as i64 + off,
                2 => size + off,
                _ => return -EINVAL,
            };
            if new < 0 {
                return -EINVAL;
            }
            *pos = new as usize;
            return new;
        }
        Desc::Dir { pos, .. } => {
            if whence == 0 && off == 0 {
                *pos = 0;
                return 0;
            }
            return -EINVAL;
        }
        _ => return -ESPIPE,
    };
    let new = match whence {
        0 => off,
        1 => *pos as i64 + off,
        2 => size as i64 + off,
        _ => return -EINVAL,
    };
    if new < 0 {
        return -EINVAL;
    }
    *pos = new as u64;
    new
}

fn sys_close(p: &Process, fd: i64) -> i64 {
    let Some(l) = linux(p) else { return -EBADF };
    l.cloexec.lock().remove(&(fd as usize));
    let mut fds = l.fds.lock();
    let old = match fds.get_mut(fd as usize) {
        Some(slot @ Some(_)) => slot.take(),
        _ => return -EBADF,
    };
    drop(fds);
    drop(old);
    0
}

fn sys_dup(p: &Process, fd: i64, to: Option<i64>, min: usize) -> i64 {
    let Some(d) = get_fd(p, fd) else { return -EBADF };
    match to {
        Some(t) if t >= 0 => {
            if t == fd {
                return t;
            }
            let Some(l) = linux(p) else { return -EBADF };
            l.cloexec.lock().remove(&(t as usize));
            let mut fds = l.fds.lock();
            while fds.len() <= t as usize {
                fds.push(None);
            }
            let old = fds[t as usize].replace(d);
            drop(fds);
            drop(old);
            t
        }
        _ => add_fd_ref(p, d, min),
    }
}

fn sys_pipe(p: &Process, ptr: u64) -> i64 {
    let pipe = Arc::new(Pipe { buf: Spin::new(VecDeque::new()), writers: 1.into(), readers: 1.into() });
    let r = add_fd(p, Desc::PipeRead(pipe.clone()));
    let w = add_fd(p, Desc::PipeWrite(pipe));
    if !usermem::write_u32(p.pml4(), ptr, r as u32) || !usermem::write_u32(p.pml4(), ptr + 4, w as u32) {
        return -EFAULT;
    }
    0
}

// --- stat -------------------------------------------------------------

fn unix_time(t: &fs::Timestamp) -> i64 {
    let (y, m, d) = (t.year as i64, t.month as i64, t.day as i64);
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    days * 86400 + t.hour as i64 * 3600 + t.minute as i64 * 60 + t.second as i64
}

fn stat_buf(mode: u32, size: u64, mtime: i64, ino: u64) -> [u8; 144] {
    let mut b = [0u8; 144];
    let mut put = |o: usize, v: &[u8]| b[o..o + v.len()].copy_from_slice(v);
    put(0, &1u64.to_le_bytes()); // st_dev
    put(8, &ino.to_le_bytes()); // st_ino
    put(16, &1u64.to_le_bytes()); // st_nlink
    put(24, &mode.to_le_bytes()); // st_mode
    put(28, &1000u32.to_le_bytes()); // st_uid
    put(32, &1000u32.to_le_bytes()); // st_gid
    put(48, &(size as i64).to_le_bytes()); // st_size
    put(56, &4096i64.to_le_bytes()); // st_blksize
    put(64, &(size.div_ceil(512) as i64).to_le_bytes()); // st_blocks
    for o in [72, 88, 104] {
        put(o, &mtime.to_le_bytes()); // atime, mtime, ctime
    }
    b
}

fn stat_path(path: &str) -> Result<[u8; 144], i64> {
    let resolved = fs::resolve_link(path);
    let path = resolved.as_str();
    if let Some(d) = virtual_file(path) {
        let (mode, size) = match &d {
            Desc::Virtual { data, .. } => (0o100444, data.len() as u64),
            _ => (0o20666, 0),
        };
        return Ok(stat_buf(mode, size, 0, 1));
    }
    if path == "/" || path == "/dev/input" {
        return Ok(stat_buf(0o40755, 4096, 0, 2));
    }
    if matches!(path, "/dev/fb0" | "/dev/input/event0" | "/dev/input/event1" | "/dev/input/mice") {
        return Ok(stat_buf(0o20660, 0, 0, 9));
    }
    let e = fs::stat(path).map_err(fs_err)?;
    let ino = path.bytes().fold(1469598103934665603u64, |h, b| (h ^ b as u64).wrapping_mul(1099511628211));
    let mode = if e.is_dir { 0o40755 } else { 0o100644 | if fs::extension(&e.name).is_none() && path.starts_with("/bin") { 0o111 } else { 0 } };
    Ok(stat_buf(mode, if e.is_dir { 4096 } else { e.size as u64 }, unix_time(&e.modified), ino))
}

fn stat_fd(p: &Process, fd: i64) -> Result<[u8; 144], i64> {
    let d = get_fd(p, fd).ok_or(-EBADF)?;
    let g = d.lock();
    Ok(match &*g {
        Desc::File { path, size, .. } => {
            let mut b = stat_path(path).unwrap_or_else(|_| stat_buf(0o100644, *size, 0, 3));
            b[48..56].copy_from_slice(&(*size as i64).to_le_bytes());
            b
        }
        Desc::Dir { path, .. } => stat_path(path)?,
        Desc::Virtual { data, .. } => stat_buf(0o100444, data.len() as u64, 0, 4),
        Desc::Console => stat_buf(0o20620, 0, 0, 5),
        Desc::Null | Desc::Zero | Desc::Random => stat_buf(0o20666, 0, 0, 6),
        Desc::Fb { .. } => stat_buf(0o20660, 0, 0, 9),
        Desc::Input { .. } => stat_buf(0o20660, 0, 0, 10),
        Desc::Unix { .. } => stat_buf(0o140777, 0, 0, 11),
        Desc::Memfd { shm, .. } => stat_buf(0o100600, shm.size(), 0, Arc::as_ptr(shm) as u64),
        Desc::EventFd { .. } | Desc::Epoll(_) | Desc::TimerFd { .. } => stat_buf(0o600, 0, 0, 12),
        Desc::Tcp { .. } | Desc::Udp { .. } => stat_buf(0o140777, 0, 0, 7),
        Desc::PipeRead(_) | Desc::PipeWrite(_) => stat_buf(0o10600, 0, 0, 8),
    })
}

fn sys_getdents64(p: &Process, fd: i64, ptr: u64, len: u64) -> i64 {
    let Some(d) = get_fd(p, fd) else { return -EBADF };
    let mut g = d.lock();
    let Desc::Dir { entries, pos, .. } = &mut *g else { return -ENOTDIR };
    let mut out = Vec::new();
    // "." and ".." come first.
    while *pos < entries.len() + 2 {
        let (name, is_dir) = match *pos {
            0 => (".", true),
            1 => ("..", true),
            i => (entries[i - 2].0.as_str(), entries[i - 2].1),
        };
        let reclen = (19 + name.len() + 1).div_ceil(8) * 8;
        if out.len() + reclen > len as usize {
            break;
        }
        let mut rec = vec![0u8; reclen];
        rec[0..8].copy_from_slice(&(*pos as u64 + 10).to_le_bytes());
        rec[8..16].copy_from_slice(&(*pos as i64 + 1).to_le_bytes());
        rec[16..18].copy_from_slice(&(reclen as u16).to_le_bytes());
        rec[18] = if is_dir { 4 } else { 8 };
        rec[19..19 + name.len()].copy_from_slice(name.as_bytes());
        out.extend_from_slice(&rec);
        *pos += 1;
    }
    if out.is_empty() && *pos < entries.len() + 2 {
        return -EINVAL;
    }
    if !usermem::write_bytes(p.pml4(), ptr, &out) {
        return -EFAULT;
    }
    out.len() as i64
}

// --- sockets ----------------------------------------------------------

fn read_sockaddr(p: &Process, ptr: u64, len: u64) -> Result<(net::Ipv4, u16), i64> {
    let b = usermem::read_bytes(p.pml4(), ptr, len.min(128)).ok_or(-EFAULT)?;
    if b.len() < 8 {
        return Err(-EINVAL);
    }
    let family = u16::from_le_bytes([b[0], b[1]]);
    if family != 2 {
        return Err(-EAFNOSUPPORT);
    }
    let port = u16::from_be_bytes([b[2], b[3]]);
    Ok((net::Ipv4([b[4], b[5], b[6], b[7]]), port))
}

fn write_sockaddr(p: &Process, ptr: u64, lenptr: u64, addr: (net::Ipv4, u16)) -> bool {
    if ptr == 0 {
        return true;
    }
    let mut b = [0u8; 16];
    b[0..2].copy_from_slice(&2u16.to_le_bytes());
    b[2..4].copy_from_slice(&addr.1.to_be_bytes());
    b[4..8].copy_from_slice(&addr.0 .0);
    usermem::write_bytes(p.pml4(), ptr, &b) && (lenptr == 0 || usermem::write_u32(p.pml4(), lenptr, 16))
}

fn sys_socket(p: &Process, domain: u64, ty: u64) -> i64 {
    let nonblock = ty & 0x800 != 0;
    let fd = match (domain, ty & 0xf) {
        (1, 1 | 5) => add_fd(p, Desc::Unix { ep: None, listener: None, bound: None, nonblock }),
        (1, _) => -EINVAL,
        (2, 1) => add_fd(p, Desc::Tcp { stream: None, listener: None, bound: 0, nonblock }),
        (2, 2) => add_fd(p, Desc::Udp { port: crate::network::udp_bind(), remote: None, nonblock }),
        (2, _) => -EINVAL,
        _ => -EAFNOSUPPORT,
    };
    set_cloexec(p, fd, ty & O_CLOEXEC != 0);
    fd
}

fn set_cloexec(p: &Process, fd: i64, on: bool) {
    if fd >= 0 && on
        && let Some(l) = linux(p)
    {
        l.cloexec.lock().insert(fd as usize);
    }
}

/// Path of a `sockaddr_un` (abstract names start with '@').
fn read_sockaddr_un(p: &Process, ptr: u64, len: u64) -> Result<String, i64> {
    let b = usermem::read_bytes(p.pml4(), ptr, len.min(110)).ok_or(-EFAULT)?;
    if b.len() < 3 || u16::from_le_bytes([b[0], b[1]]) != 1 {
        return Err(-EINVAL);
    }
    let raw = &b[2..];
    if raw[0] == 0 {
        return Ok(alloc::format!("@{}", String::from_utf8_lossy(&raw[1..])));
    }
    let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    let path = String::from_utf8_lossy(&raw[..end]).to_string();
    let base = linux(p).map(|l| l.cwd.lock().clone()).unwrap_or_else(|| String::from("/"));
    Ok(fs::normalize(&base, &path))
}

fn write_sockaddr_un(p: &Process, ptr: u64, lenp: u64, path: &str) -> bool {
    if ptr == 0 {
        return true;
    }
    let mut b = alloc::vec![1u8, 0];
    if let Some(abs) = path.strip_prefix('@') {
        b.push(0);
        b.extend_from_slice(abs.as_bytes());
    } else {
        b.extend_from_slice(path.as_bytes());
        b.push(0);
    }
    let max = usermem::read_bytes(p.pml4(), lenp, 4).map(|v| u32::from_le_bytes(v.try_into().unwrap()) as usize).unwrap_or(0);
    let n = b.len().min(max);
    usermem::write_bytes(p.pml4(), ptr, &b[..n]) && usermem::write_u32(p.pml4(), lenp, b.len() as u32)
}

/// Wait for bytes on a Unix socket's queue.
fn unix_wait(rx: &unix::QueueRef, max: usize, nonblock: bool) -> Result<(Vec<u8>, Vec<DescRef>), i64> {
    loop {
        {
            let mut q = rx.lock();
            if !q.is_empty() {
                return Ok(q.pop(max));
            }
            if q.closed {
                return Ok((Vec::new(), Vec::new()));
            }
        }
        if nonblock {
            return Err(-EAGAIN);
        }
        sched::sleep_ms(1);
    }
}

fn unix_send(ep: &Endpoint, nonblock: bool, data: &[u8], mut fds: Vec<DescRef>) -> i64 {
    loop {
        match ep.send(data, core::mem::take(&mut fds)) {
            None => return -EPIPE,
            Some(0) if !data.is_empty() => {
                if nonblock {
                    return -EAGAIN;
                }
                sched::sleep_ms(1);
            }
            Some(n) => return n as i64,
        }
    }
}

fn sys_socketpair(p: &Process, domain: u64, ty: u64, out: u64) -> i64 {
    if domain != 1 {
        return -EAFNOSUPPORT;
    }
    let nonblock = ty & 0x800 != 0;
    let (a, b) = Endpoint::pair(None);
    let fa = add_fd(p, Desc::Unix { ep: Some(a), listener: None, bound: None, nonblock });
    let fb = add_fd(p, Desc::Unix { ep: Some(b), listener: None, bound: None, nonblock });
    set_cloexec(p, fa, ty & O_CLOEXEC != 0);
    set_cloexec(p, fb, ty & O_CLOEXEC != 0);
    if !usermem::write_u32(p.pml4(), out, fa as u32) || !usermem::write_u32(p.pml4(), out + 4, fb as u32) {
        return -EFAULT;
    }
    0
}

fn sys_connect(p: &Process, fd: i64, ptr: u64, len: u64) -> i64 {
    let Some(d) = get_fd(p, fd) else { return -EBADF };
    if matches!(&*d.lock(), Desc::Unix { .. }) {
        let path = match read_sockaddr_un(p, ptr, len) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let Some(ep) = unix::connect(&path, p.pid) else {
            return if fs::exists(&path) || unix::is_bound(&path) { -ECONNREFUSED } else { -ENOENT };
        };
        if let Desc::Unix { ep: slot, .. } = &mut *d.lock() {
            *slot = Some(ep);
        }
        return 0;
    }
    let addr = match read_sockaddr(p, ptr, len) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let mut g = d.lock();
    match &mut *g {
        Desc::Tcp { stream, .. } => match TcpStream::connect(addr.0, addr.1, 15_000) {
            Ok(s) => {
                *stream = Some(s);
                0
            }
            Err(crate::network::NetError::Refused) | Err(crate::network::NetError::Reset) => -ECONNREFUSED,
            Err(crate::network::NetError::Timeout) => -ETIMEDOUT,
            Err(_) => -EHOSTUNREACH,
        },
        Desc::Udp { remote, .. } => {
            *remote = Some(addr);
            0
        }
        _ => -ENOTSOCK,
    }
}

fn sys_bind(p: &Process, fd: i64, ptr: u64, len: u64) -> i64 {
    let Some(d) = get_fd(p, fd) else { return -EBADF };
    if let Desc::Unix { bound, .. } = &mut *d.lock() {
        return match read_sockaddr_un(p, ptr, len) {
            Ok(path) if unix::is_bound(&path) => -EADDRINUSE,
            Ok(path) => {
                *bound = Some(path);
                0
            }
            Err(e) => e,
        };
    }
    let addr = match read_sockaddr(p, ptr, len) {
        Ok(a) => a,
        Err(e) => return e,
    };
    match &mut *d.lock() {
        Desc::Tcp { bound, .. } => {
            *bound = addr.1;
            0
        }
        Desc::Udp { .. } => 0,
        _ => -ENOTSOCK,
    }
}

fn sys_listen(p: &Process, fd: i64) -> i64 {
    let Some(d) = get_fd(p, fd) else { return -EBADF };
    match &mut *d.lock() {
        Desc::Unix { listener: Some(_), .. } => 0,
        Desc::Unix { listener, bound: Some(path), .. } => match unix::listen(path) {
            Some(l) => {
                *listener = Some(l);
                0
            }
            None => -EADDRINUSE,
        },
        Desc::Unix { .. } => -EINVAL,
        Desc::Tcp { listener, bound, .. } => match TcpListener::bind(*bound) {
            Ok(l) => {
                *listener = Some(l);
                0
            }
            Err(_) => -EADDRINUSE,
        },
        _ => -ENOTSOCK,
    }
}

fn sys_accept(p: &Process, fd: i64, ptr: u64, lenptr: u64, flags: u64) -> i64 {
    let Some(d) = get_fd(p, fd) else { return -EBADF };
    let unix_l = match &*d.lock() {
        Desc::Unix { listener: Some(l), nonblock, .. } => Some((l.clone(), *nonblock)),
        _ => None,
    };
    if let Some((l, nb)) = unix_l {
        let ep = loop {
            if let Some(ep) = l.pending.lock().pop_front() {
                break ep;
            }
            if nb {
                return -EAGAIN;
            }
            sched::sleep_ms(1);
        };
        let path = l.path.clone();
        let r = add_fd(p, Desc::Unix { ep: Some(ep), listener: None, bound: None, nonblock: flags & 0x800 != 0 });
        set_cloexec(p, r, flags & O_CLOEXEC != 0);
        write_sockaddr_un(p, ptr, lenptr, &path);
        return r;
    }
    let stream = {
        let g = d.lock();
        let Desc::Tcp { listener: Some(l), nonblock, .. } = &*g else { return -EINVAL };
        match l.accept(if *nonblock { 0 } else { u32::MAX as u64 }) {
            Some(s) => s,
            None => return -EAGAIN,
        }
    };
    let peer = stream.peer();
    let r = add_fd(p, Desc::Tcp { stream: Some(stream), listener: None, bound: 0, nonblock: flags & 0x800 != 0 });
    if let Some(a) = peer {
        write_sockaddr(p, ptr, lenptr, a);
    }
    r
}

fn sys_sendto(p: &Process, fd: i64, buf: u64, len: u64, addr: u64, alen: u64) -> i64 {
    let Some(d) = get_fd(p, fd) else { return -EBADF };
    let Some(data) = usermem::read_bytes(p.pml4(), buf, len) else { return -EFAULT };
    if addr != 0 {
        let dest = match read_sockaddr(p, addr, alen) {
            Ok(a) => a,
            Err(e) => return e,
        };
        if let Desc::Udp { port, .. } = &*d.lock() {
            crate::network::udp_send(*port, dest.0, dest.1, &data);
            return len as i64;
        }
    }
    write_desc(p, &d, &data)
}

fn sys_recvfrom(p: &Process, fd: i64, buf: u64, len: u64, addr: u64, alenp: u64) -> i64 {
    let Some(d) = get_fd(p, fd) else { return -EBADF };
    let udp = match &*d.lock() {
        Desc::Udp { port, nonblock, .. } => Some((*port, *nonblock)),
        _ => None,
    };
    if let Some((port, nb)) = udp {
        return match crate::network::udp_recv(port, if nb { 0 } else { 3_600_000 }) {
            Some((ip, sport, data)) => {
                let n = data.len().min(len as usize);
                if !usermem::write_bytes(p.pml4(), buf, &data[..n]) {
                    return -EFAULT;
                }
                write_sockaddr(p, addr, alenp, (ip, sport));
                n as i64
            }
            None => -EAGAIN,
        };
    }
    sys_read(p, fd, buf, len)
}

/// Read a `struct msghdr`: (name ptr, name len, iovecs, control len ptr offset).
fn msghdr(p: &Process, ptr: u64) -> Option<(u64, u64, Vec<(u64, u64)>)> {
    let name = usermem::read_u64(p.pml4(), ptr)?;
    let namelen = usermem::read_u64(p.pml4(), ptr + 8)? & 0xffff_ffff;
    let iov = usermem::read_u64(p.pml4(), ptr + 16)?;
    let iovlen = usermem::read_u64(p.pml4(), ptr + 24)?;
    Some((name, namelen, iovecs(p, iov, iovlen)?))
}

fn sys_sendmsg(p: &Process, fd: i64, ptr: u64) -> i64 {
    let Some((name, namelen, vecs)) = msghdr(p, ptr) else { return -EFAULT };
    let mut data = Vec::new();
    for (b, l) in vecs {
        match usermem::read_bytes(p.pml4(), b, l) {
            Some(d) => data.extend_from_slice(&d),
            None => return -EFAULT,
        }
    }
    let Some(d) = get_fd(p, fd) else { return -EBADF };
    if matches!(&*d.lock(), Desc::Unix { .. }) {
        // Descriptors travelling with the bytes (SCM_RIGHTS).
        let mut fds = Vec::new();
        let ctl = usermem::read_u64(p.pml4(), ptr + 32).unwrap_or(0);
        let ctllen = usermem::read_u64(p.pml4(), ptr + 40).unwrap_or(0).min(4096);
        if ctl != 0 && ctllen >= 16 {
            let Some(c) = usermem::read_bytes(p.pml4(), ctl, ctllen) else { return -EFAULT };
            let mut o = 0usize;
            while o + 16 <= c.len() {
                let len = u64::from_le_bytes(c[o..o + 8].try_into().unwrap()) as usize;
                let level = i32::from_le_bytes(c[o + 8..o + 12].try_into().unwrap());
                let ty = i32::from_le_bytes(c[o + 12..o + 16].try_into().unwrap());
                if len < 16 || o + len > c.len() {
                    break;
                }
                if level == 1 && ty == 1 {
                    for k in (o + 16..o + len).step_by(4) {
                        if k + 4 > o + len {
                            break;
                        }
                        let n = i32::from_le_bytes(c[k..k + 4].try_into().unwrap());
                        match get_fd(p, n as i64) {
                            Some(x) => fds.push(x),
                            None => return -EBADF,
                        }
                    }
                }
                o += (len + 7) & !7;
            }
        }
        let g = d.lock();
        return match &*g {
            Desc::Unix { ep: Some(ep), nonblock, .. } => unix_send(ep, *nonblock, &data, fds),
            _ => -ENOTCONN,
        };
    }
    if name != 0 {
        let dest = match read_sockaddr(p, name, namelen) {
            Ok(a) => a,
            Err(e) => return e,
        };
        if let Desc::Udp { port, .. } = &*d.lock() {
            crate::network::udp_send(*port, dest.0, dest.1, &data);
            return data.len() as i64;
        }
    }
    write_desc(p, &d, &data)
}

fn sys_recvmsg(p: &Process, fd: i64, ptr: u64, flags: u64) -> i64 {
    let Some((name, _, vecs)) = msghdr(p, ptr) else { return -EFAULT };
    let Some(d) = get_fd(p, fd) else { return -EBADF };
    let total: u64 = vecs.iter().map(|v| v.1).sum();
    let unix_rx = match &*d.lock() {
        Desc::Unix { ep: Some(ep), nonblock, .. } => Some((ep.rx.clone(), *nonblock)),
        Desc::Unix { .. } => return -ENOTCONN,
        _ => None,
    };
    if let Some((rx, nb)) = unix_rx {
        const MSG_DONTWAIT: u64 = 0x40;
        const MSG_CMSG_CLOEXEC: u64 = 0x4000_0000;
        let (data, fds) = match unix_wait(&rx, total.min(4 << 20) as usize, nb || flags & MSG_DONTWAIT != 0) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let mut off = 0usize;
        for (b, l) in vecs {
            if off >= data.len() {
                break;
            }
            let k = (l as usize).min(data.len() - off);
            if !usermem::write_bytes(p.pml4(), b, &data[off..off + k]) {
                return -EFAULT;
            }
            off += k;
        }
        // Install received descriptors and describe them in msg_control.
        let ctl = usermem::read_u64(p.pml4(), ptr + 32).unwrap_or(0);
        let ctllen = usermem::read_u64(p.pml4(), ptr + 40).unwrap_or(0);
        let mut msg_flags = 0u32;
        let mut used = 0u64;
        if !fds.is_empty() {
            let room = if ctl == 0 || ctllen < 16 { 0 } else { ((ctllen - 16) / 4) as usize };
            let mut nums = Vec::new();
            for (i, x) in fds.into_iter().enumerate() {
                if i >= room {
                    msg_flags |= 8; // MSG_CTRUNC: the rest are closed
                    continue;
                }
                let n = add_fd_ref(p, x, 0);
                set_cloexec(p, n, flags & MSG_CMSG_CLOEXEC != 0);
                nums.push(n as i32);
            }
            if !nums.is_empty() {
                let len = 16 + nums.len() * 4;
                let mut c = Vec::with_capacity(len);
                c.extend_from_slice(&(len as u64).to_le_bytes());
                c.extend_from_slice(&1i32.to_le_bytes());
                c.extend_from_slice(&1i32.to_le_bytes());
                for n in &nums {
                    c.extend_from_slice(&n.to_le_bytes());
                }
                if !usermem::write_bytes(p.pml4(), ctl, &c) {
                    return -EFAULT;
                }
                used = ((len + 7) & !7) as u64;
            }
        }
        usermem::write_u64(p.pml4(), ptr + 40, used);
        usermem::write_u32(p.pml4(), ptr + 48, msg_flags);
        return off as i64;
    }
    let udp = match &*d.lock() {
        Desc::Udp { port, nonblock, .. } => Some((*port, *nonblock)),
        _ => None,
    };
    let (data, from) = match udp {
        Some((port, nb)) => match crate::network::udp_recv(port, if nb { 0 } else { 3_600_000 }) {
            Some((ip, sport, data)) => (data, Some((ip, sport))),
            None => return -EAGAIN,
        },
        None => {
            let mut buf = vec![0u8; total.min(4 << 20) as usize];
            let n = read_desc(p, &d, &mut buf);
            if n < 0 {
                return n;
            }
            buf.truncate(n as usize);
            (buf, None)
        }
    };
    let mut off = 0usize;
    for (b, l) in vecs {
        if off >= data.len() {
            break;
        }
        let k = (l as usize).min(data.len() - off);
        if !usermem::write_bytes(p.pml4(), b, &data[off..off + k]) {
            return -EFAULT;
        }
        off += k;
    }
    if let Some(a) = from
        && name != 0
    {
        write_sockaddr(p, name, ptr + 8, a);
    }
    // No control data, no flags.
    usermem::write_u64(p.pml4(), ptr + 40, 0);
    usermem::write_u32(p.pml4(), ptr + 48, 0);
    off as i64
}

fn sys_sockname(p: &Process, fd: i64, ptr: u64, lenp: u64, peer: bool) -> i64 {
    if let Some(d) = get_fd(p, fd)
        && let Desc::Unix { ep, bound, listener, .. } = &*d.lock()
    {
        let path = if peer {
            ep.as_ref().and_then(|e| e.path.clone())
        } else {
            bound.clone().or_else(|| listener.as_ref().map(|l| l.path.clone()))
        };
        write_sockaddr_un(p, ptr, lenp, path.as_deref().unwrap_or(""));
        return 0;
    }
    let Some(d) = get_fd(p, fd) else { return -EBADF };
    let me = crate::network::status().map(|s| s.ip).unwrap_or(net::Ipv4::UNSPECIFIED);
    let a = match &*d.lock() {
        Desc::Tcp { stream: Some(s), .. } => if peer { s.peer() } else { s.local() },
        Desc::Tcp { listener: Some(l), .. } if !peer => Some((me, l.port())),
        Desc::Udp { port, remote, .. } => if peer { *remote } else { Some((me, *port)) },
        Desc::Tcp { .. } => if peer { None } else { Some((me, 0)) },
        _ => return -ENOTSOCK,
    };
    match a {
        Some(a) => {
            write_sockaddr(p, ptr, lenp, a);
            0
        }
        None => -ENOTCONN,
    }
}

// --- poll ---------------------------------------------------------------

fn ready(p: &Process, fd: i64, events: u16) -> u16 {
    const POLLNVAL: u16 = 0x20;
    match get_fd(p, fd) {
        Some(d) => desc_ready(p, &d, events),
        None => POLLNVAL,
    }
}

/// Poll bits (POLLIN 1, POLLOUT 4, POLLERR 8, POLLHUP 0x10) ready on `d`.
fn desc_ready(p: &Process, d: &DescRef, events: u16) -> u16 {
    const POLLIN: u16 = 1;
    const POLLOUT: u16 = 4;
    const POLLHUP: u16 = 0x10;
    let Some(g) = d.try_lock() else { return 0 };
    let mut hup = false;
    let (r, w) = match &*g {
        Desc::Console => (p.console.has_input(), true),
        Desc::Tcp { stream: Some(s), .. } => (s.readable(), s.writable()),
        Desc::Tcp { listener: Some(l), .. } => (l.pending(), false),
        Desc::Tcp { .. } => (false, false),
        Desc::Udp { port, .. } => (crate::network::udp_pending(*port), true),
        Desc::PipeRead(pp) => {
            hup = pp.writers.load(Ordering::Relaxed) == 0;
            (!pp.buf.lock().is_empty() || hup, false)
        }
        Desc::PipeWrite(_) => (false, true),
        Desc::Input { screen, kind, .. } => (screen::has_input(screen, *kind), true),
        Desc::Unix { ep: Some(ep), .. } => {
            hup = ep.hung_up();
            (ep.readable(), ep.writable())
        }
        Desc::Unix { listener: Some(l), .. } => (!l.pending.lock().is_empty(), false),
        Desc::Unix { .. } => (false, false),
        Desc::EventFd { ev, .. } => (*ev.count.lock() > 0, true),
        Desc::TimerFd { t, .. } => (t.ready(), false),
        Desc::Epoll(e) => {
            let list: Vec<(DescRef, u32)> = e.list.lock().iter().filter(|i| !i.disabled).map(|i| (i.desc.clone(), i.events)).collect();
            drop(g);
            let any = list.iter().any(|(d, ev)| desc_ready(p, d, *ev as u16) != 0);
            return if any && events & POLLIN != 0 { POLLIN } else { 0 };
        }
        _ => (true, true),
    };
    let mut rev = 0;
    if r && events & POLLIN != 0 {
        rev |= POLLIN;
    }
    if w && events & POLLOUT != 0 {
        rev |= POLLOUT;
    }
    if hup {
        rev |= POLLHUP;
    }
    rev
}

fn sys_epoll_ctl(p: &Process, epfd: i64, op: u64, fd: i32, evp: u64) -> i64 {
    let Some(e) = get_fd(p, epfd) else { return -EBADF };
    let ep = match &*e.lock() {
        Desc::Epoll(x) => x.clone(),
        _ => return -EINVAL,
    };
    let (events, data) = if evp != 0 {
        let Some(b) = usermem::read_bytes(p.pml4(), evp, 12) else { return -EFAULT };
        (u32::from_le_bytes(b[0..4].try_into().unwrap()), u64::from_le_bytes(b[4..12].try_into().unwrap()))
    } else {
        (0, 0)
    };
    let mut list = ep.list.lock();
    let pos = list.iter().position(|i| i.fd == fd);
    match op {
        1 => {
            // EPOLL_CTL_ADD
            if pos.is_some() {
                return -EEXIST;
            }
            let Some(desc) = get_fd(p, fd as i64) else { return -EBADF };
            list.push(unix::Interest { fd, events, data, desc, disabled: false });
            0
        }
        2 => match pos {
            Some(i) => {
                list.remove(i);
                0
            }
            None => -ENOENT,
        },
        3 => match pos {
            Some(i) => {
                list[i].events = events;
                list[i].data = data;
                list[i].disabled = false;
                0
            }
            None => -ENOENT,
        },
        _ => -EINVAL,
    }
}

/// epoll_wait: level-triggered (edge-triggered interests are reported
/// like level-triggered ones).
fn sys_epoll_wait(p: &Process, epfd: i64, out: u64, max: i32, timeout_ms: i64) -> i64 {
    let Some(e) = get_fd(p, epfd) else { return -EBADF };
    let ep = match &*e.lock() {
        Desc::Epoll(x) => x.clone(),
        _ => return -EINVAL,
    };
    if max <= 0 {
        return -EINVAL;
    }
    let deadline = if timeout_ms < 0 { u64::MAX } else { crate::time::uptime_ms() + timeout_ms as u64 };
    loop {
        let items: Vec<(usize, DescRef, u32, u64)> =
            ep.list.lock().iter().enumerate().filter(|(_, i)| !i.disabled).map(|(k, i)| (k, i.desc.clone(), i.events, i.data)).collect();
        let mut buf = Vec::new();
        let mut fired = Vec::new();
        for (k, d, events, data) in items {
            if buf.len() / 12 >= max as usize {
                break;
            }
            // EPOLLIN 1, EPOLLOUT 4, EPOLLERR 8, EPOLLHUP 0x10, EPOLLRDHUP 0x2000
            let rev = desc_ready(p, &d, (events & 0xffff) as u16 | 0x18) as u32;
            let rev = rev & (events | 0x18);
            if rev != 0 {
                buf.extend_from_slice(&rev.to_le_bytes());
                buf.extend_from_slice(&data.to_le_bytes());
                if events & (1 << 30) != 0 {
                    fired.push(k); // EPOLLONESHOT
                }
            }
        }
        if !buf.is_empty() || crate::time::uptime_ms() >= deadline {
            let mut list = ep.list.lock();
            for k in fired {
                if let Some(i) = list.get_mut(k) {
                    i.disabled = true;
                }
            }
            drop(list);
            if !buf.is_empty() && !usermem::write_bytes(p.pml4(), out, &buf) {
                return -EFAULT;
            }
            return (buf.len() / 12) as i64;
        }
        if signal::interrupted(p) {
            return -EINTR;
        }
        sched::sleep_ms(1);
    }
}

fn sys_poll(p: &Process, ptr: u64, n: u64, timeout_ms: i64) -> i64 {
    let deadline = if timeout_ms < 0 { u64::MAX } else { crate::time::uptime_ms() + timeout_ms as u64 };
    loop {
        let Some(raw) = usermem::read_bytes(p.pml4(), ptr, n * 8) else { return -EFAULT };
        let mut out = raw.clone();
        let mut count = 0;
        for i in 0..n as usize {
            let fd = i32::from_le_bytes(raw[i * 8..i * 8 + 4].try_into().unwrap()) as i64;
            let ev = u16::from_le_bytes([raw[i * 8 + 4], raw[i * 8 + 5]]);
            let rev = if fd < 0 { 0 } else { ready(p, fd, ev) };
            out[i * 8 + 6..i * 8 + 8].copy_from_slice(&rev.to_le_bytes());
            if rev != 0 {
                count += 1;
            }
        }
        if count > 0 || crate::time::uptime_ms() >= deadline {
            if !usermem::write_bytes(p.pml4(), ptr, &out) {
                return -EFAULT;
            }
            return count;
        }
        if signal::interrupted(p) {
            return -EINTR;
        }
        sched::sleep_ms(2);
    }
}

// --- threads and futexes ---------------------------------------------

struct Waiter {
    pml4: u64,
    addr: u64,
    woken: Arc<AtomicBool>,
}

static FUTEX: Spin<Vec<Waiter>> = Spin::new(Vec::new());

fn futex_wake(pml4: u64, addr: u64, n: u64) -> i64 {
    let mut q = FUTEX.lock();
    let mut woken = 0;
    q.retain(|w| {
        if (woken as u64) < n && w.pml4 == pml4 && w.addr == addr {
            w.woken.store(true, Ordering::Release);
            woken += 1;
            false
        } else {
            true
        }
    });
    woken
}

fn timespec_ms(p: &Process, ptr: u64) -> Option<u64> {
    if ptr == 0 {
        return None;
    }
    let s = usermem::read_u64(p.pml4(), ptr)? as i64;
    let ns = usermem::read_u64(p.pml4(), ptr + 8)? as i64;
    Some((s.max(0) as u64) * 1000 + (ns.max(0) as u64) / 1_000_000)
}

fn sys_futex(p: &Process, addr: u64, op: u64, val: u64, tptr: u64) -> i64 {
    let cmd = op & 0x7f;
    match cmd {
        0 | 9 => {
            // FUTEX_WAIT(_BITSET)
            let Some(b) = usermem::read_bytes(p.pml4(), addr, 4) else { return -EFAULT };
            if u32::from_le_bytes(b.try_into().unwrap()) != val as u32 {
                return -EAGAIN;
            }
            let deadline = match timespec_ms(p, tptr) {
                Some(ms) if cmd == 9 => {
                    // Absolute (monotonic or realtime) time.
                    let now = if op & 256 != 0 { unix_ms() } else { crate::time::uptime_ms() };
                    crate::time::uptime_ms() + ms.saturating_sub(now)
                }
                Some(ms) => crate::time::uptime_ms() + ms,
                None => u64::MAX,
            };
            let woken = Arc::new(AtomicBool::new(false));
            FUTEX.lock().push(Waiter { pml4: p.pml4(), addr, woken: woken.clone() });
            loop {
                if woken.load(Ordering::Acquire) {
                    return 0;
                }
                if crate::time::uptime_ms() >= deadline {
                    FUTEX.lock().retain(|w| !Arc::ptr_eq(&w.woken, &woken));
                    return -ETIMEDOUT;
                }
                if signal::interrupted(p) {
                    FUTEX.lock().retain(|w| !Arc::ptr_eq(&w.woken, &woken));
                    return -EINTR;
                }
                sched::sleep_ms(1);
            }
        }
        1 | 10 => futex_wake(p.pml4(), addr, val),
        3 | 4 => futex_wake(p.pml4(), addr, u64::MAX),
        _ => -ENOSYS,
    }
}

fn sys_clone(p: &Arc<Process>, f: &TrapFrame) -> i64 {
    const CLONE_VM: u64 = 0x100;
    const CLONE_SETTLS: u64 = 0x80000;
    const CLONE_PARENT_SETTID: u64 = 0x100000;
    const CLONE_CHILD_CLEARTID: u64 = 0x200000;
    const CLONE_CHILD_SETTID: u64 = 0x1000000;
    let (flags, newsp, ptid, ctid, tls) = (f.rdi, f.rsi, f.rdx, f.r10, f.r8);
    const CLONE_VFORK: u64 = 0x4000;
    if flags & CLONE_VM == 0 || flags & CLONE_VFORK != 0 {
        // A new process (fork, vfork, posix_spawn).
        return sys_fork(p, f, flags, newsp, ptid, ctid, tls);
    }
    let mut frame = f.clone();
    frame.rax = 0;
    if newsp != 0 {
        frame.rsp = newsp;
    }
    let fs = if flags & CLONE_SETTLS != 0 { tls } else { 0 };
    let tid = sched::spawn_user_frame(p.clone(), frame, fs);
    if let Some(l) = linux(p) {
        l.sig.lock().new_thread(sched::current_id(), tid);
    }
    if flags & CLONE_PARENT_SETTID != 0 {
        usermem::write_u32(p.pml4(), ptid, tid as u32);
    }
    if flags & CLONE_CHILD_SETTID != 0 {
        usermem::write_u32(p.pml4(), ctid, tid as u32);
    }
    if flags & CLONE_CHILD_CLEARTID != 0 {
        sched::set_clear_child_tid_of(tid, ctid);
    }
    tid as i64
}

/// A thread of a Linux program ends.
fn thread_exit(p: &Arc<Process>, code: i64) -> ! {
    let ctid = sched::clear_child_tid();
    if ctid != 0 {
        usermem::write_u32(p.pml4(), ctid, 0);
        futex_wake(p.pml4(), ctid, 1);
    }
    if let Some(l) = linux(p) {
        l.sig.lock().thread_exit(sched::current_id());
    }
    if sched::process_thread_count(p.pid) <= 1 {
        super::process::exit_current_process(code, None);
    }
    sched::exit_current()
}

// --- time & misc --------------------------------------------------------

/// Sleep, waking early for a signal. Returns true if a signal came.
fn sleep_interruptible(p: &Process, ms: u64) -> bool {
    let until = crate::time::uptime_ms() + ms;
    loop {
        if signal::interrupted(p) {
            return true;
        }
        let now = crate::time::uptime_ms();
        if now >= until {
            return false;
        }
        sched::sleep_ms((until - now).min(10));
    }
}

pub fn unix_ms() -> u64 {
    let t = crate::arch::rtc::now();
    let ts = fs::Timestamp { year: t.year, month: t.month, day: t.day, hour: t.hour, minute: t.minute, second: t.second };
    unix_time(&ts).max(0) as u64 * 1000 + crate::time::uptime_ms() % 1000
}

fn write_timespec(p: &Process, ptr: u64, us: u64) -> bool {
    usermem::write_u64(p.pml4(), ptr, us / 1_000_000) && usermem::write_u64(p.pml4(), ptr + 8, (us % 1_000_000) * 1000)
}

/// Random bytes (ChaCha20, seeded from RDRAND/TSC at first use).
pub fn fill_random(buf: &mut [u8]) {
    use rand_core::{RngCore, SeedableRng};
    static RNG: Spin<Option<rand_chacha::ChaCha20Rng>> = Spin::new(None);
    let mut g = RNG.lock();
    let rng = g.get_or_insert_with(|| {
        let mut seed = [0u8; 32];
        for (i, c) in seed.chunks_mut(8).enumerate() {
            let mut v = crate::arch::cpu::rdtsc().rotate_left(i as u32 * 17) ^ crate::time::uptime_us();
            if core::arch::x86_64::__cpuid(1).ecx & (1 << 30) != 0 {
                let mut r = 0u64;
                unsafe { core::arch::asm!("rdrand {}", out(reg) r) };
                v ^= r;
            }
            c.copy_from_slice(&v.to_le_bytes());
        }
        rand_chacha::ChaCha20Rng::from_seed(seed)
    });
    rng.fill_bytes(buf);
}

fn sys_uname(p: &Process, ptr: u64) -> i64 {
    let mut b = [0u8; 65 * 6];
    for (i, s) in ["Linux", "mayos", "6.1.0-mayos", "#1 MayOS", "x86_64", "(none)"].iter().enumerate() {
        b[i * 65..i * 65 + s.len()].copy_from_slice(s.as_bytes());
    }
    if usermem::write_bytes(p.pml4(), ptr, &b) { 0 } else { -EFAULT }
}

// -------------------------------------------------------------------------
// Dispatch
// -------------------------------------------------------------------------

/// Handle a Linux system call. Returns true if the thread must not resume.
/// Log every Linux system call to the serial port (`linuxtrace on`).
pub static TRACE: AtomicBool = AtomicBool::new(false);

pub fn syscall(p: &Arc<Process>, f: &mut TrapFrame) -> bool {
    let (nr, args) = (f.rax, [f.rdi, f.rsi, f.rdx, f.r10]);
    let mut exited = syscall_inner(p, f);
    if !exited && nr != 15 {
        exited = signal::deliver(p, f);
    }
    if TRACE.load(Ordering::Relaxed) {
        crate::kprintln!("[{}] sys {} ({:#x}, {:#x}, {:#x}, {:#x}) = {}", p.pid, nr, args[0], args[1], args[2], args[3], f.rax as i64);
    }
    exited
}

fn syscall_inner(p: &Arc<Process>, f: &mut TrapFrame) -> bool {
    let (a0, a1, a2, a3, a4, a5) = (f.rdi, f.rsi, f.rdx, f.r10, f.r8, f.r9);
    let pml4 = p.pml4();
    let ret: i64 = match f.rax {
        0 => sys_read(p, a0 as i64, a1, a2),
        1 => sys_write(p, a0 as i64, a1, a2),
        2 => sys_openat(p, -100, a0, a1),
        3 => sys_close(p, a0 as i64),
        4 | 6 => match path_at(p, -100, a0).and_then(|path| stat_path(&path)) {
            Ok(b) => if usermem::write_bytes(pml4, a1, &b) { 0 } else { -EFAULT },
            Err(e) => e,
        },
        5 => match stat_fd(p, a0 as i64) {
            Ok(b) => if usermem::write_bytes(pml4, a1, &b) { 0 } else { -EFAULT },
            Err(e) => e,
        },
        7 => sys_poll(p, a0, a1, a2 as i32 as i64),
        8 => sys_lseek(p, a0 as i64, a1 as i64, a2),
        9 => sys_mmap(p, a0, a1, a2, a3, a4 as i64, a5),
        10 => sys_mprotect(p, a0, a1, a2),
        11 => sys_munmap(p, a0, a1),
        12 => sys_brk(p, a0),
        13 => signal::sigaction(p, a0, a1, a2),
        14 => signal::sigprocmask(p, a0, a1, a2),
        131 => signal::sigaltstack(p, a0, a1, f.rsp),
        15 => {
            // rt_sigreturn: the frame holds every register, rax included.
            if !signal::sigreturn(p, f) {
                super::process::exit_current_process(-11, Some("Segmentation fault (bad signal frame)\n"));
                return true;
            }
            return signal::deliver(p, f);
        }
        127 => signal::sigpending(p, a0),
        128 => signal::sigtimedwait(p, a0, a1, timespec_ms(p, a2)),
        130 => signal::sigsuspend(p, a0),
        34 => {
            // pause
            while !signal::interrupted(p) {
                sched::sleep_ms(10);
            }
            -EINTR
        }
        16 => match a1 {
            0x5413 => {
                // TIOCGWINSZ: 30 rows, 100 columns
                let ws: [u16; 4] = [30, 100, 0, 0];
                let b: Vec<u8> = ws.iter().flat_map(|v| v.to_le_bytes()).collect();
                if usermem::write_bytes(pml4, a2, &b) { 0 } else { -EFAULT }
            }
            0x5401 => match get_fd(p, a0 as i64).map(|d| matches!(&*d.lock(), Desc::Console)) {
                // TCGETS: only the console is a terminal.
                Some(true) => {
                    usermem::write_bytes(pml4, a2, &[0u8; 60]);
                    0
                }
                _ => -ENOTTY,
            },
            0x541b => {
                // FIONREAD: bytes waiting
                let n = match get_fd(p, a0 as i64) {
                    Some(d) => match &*d.lock() {
                        Desc::Unix { ep: Some(ep), .. } => ep.rx.lock().len(),
                        Desc::PipeRead(pp) => pp.buf.lock().len(),
                        Desc::Console => p.console.has_input() as usize,
                        _ => 0,
                    },
                    None => usize::MAX,
                };
                if n == usize::MAX { -EBADF } else if usermem::write_u32(pml4, a2, n as u32) { 0 } else { -EFAULT }
            }
            0x5421 => {
                // FIONBIO
                let on = usermem::read_bytes(pml4, a2, 4).map(|b| b != [0, 0, 0, 0]).unwrap_or(false);
                if let Some(d) = get_fd(p, a0 as i64) {
                    match &mut *d.lock() {
                        Desc::Tcp { nonblock, .. } | Desc::Udp { nonblock, .. } | Desc::Input { nonblock, .. } => *nonblock = on,
                        _ => {}
                    }
                }
                0
            }
            _ => device_ioctl(p, a0 as i64, a1, a2),
        },
        17 => sys_pread(p, a0 as i64, a1, a2, a3),
        19 => sys_readv(p, a0 as i64, a1, a2),
        20 => sys_writev(p, a0 as i64, a1, a2),
        21 => match path_at(p, -100, a0) {
            Ok(path) => if fs::exists(&path) || virtual_file(&path).is_some() { 0 } else { -ENOENT },
            Err(e) => e,
        },
        22 => sys_pipe(p, a0),
        24 => {
            sched::yield_now();
            0
        }
        25 => sys_mremap(p, a0, a1, a2, a3),
        28 => 0,       // madvise
        32 => sys_dup(p, a0 as i64, None, 0),
        33 => sys_dup(p, a0 as i64, Some(a1 as i64), 0),
        35 => match timespec_ms(p, a0) {
            Some(ms) => if sleep_interruptible(p, ms.max(1)) { -EINTR } else { 0 },
            None => -EFAULT,
        },
        39 => p.pid as i64,
        41 => sys_socket(p, a0, a1),
        42 => sys_connect(p, a0 as i64, a1, a2),
        43 => sys_accept(p, a0 as i64, a1, a2, 0),
        44 => sys_sendto(p, a0 as i64, a1, a2, a4, a5),
        45 => sys_recvfrom(p, a0 as i64, a1, a2, a4, a5),
        46 => sys_sendmsg(p, a0 as i64, a1),
        47 => sys_recvmsg(p, a0 as i64, a1, a2),
        48 => {
            // shutdown: the peer sees end of file once the writing side goes.
            if let Some(d) = get_fd(p, a0 as i64)
                && let Desc::Unix { ep: Some(ep), .. } = &*d.lock()
                && a1 >= 1
                && let unix::Peer::Queue(q) = &ep.tx
            {
                q.lock().closed = true;
            }
            0
        }
        53 => sys_socketpair(p, a0, a1, a3),
        49 => sys_bind(p, a0 as i64, a1, a2),
        50 => sys_listen(p, a0 as i64),
        51 => sys_sockname(p, a0 as i64, a1, a2, false),
        52 => sys_sockname(p, a0 as i64, a1, a2, true),
        54 => 0, // setsockopt
        55 if a1 == 1 && a2 == 17 => {
            // SO_PEERCRED: the peer is one of our processes, same user.
            let cred: Vec<u8> = [p.pid as u32, 1000, 1000].iter().flat_map(|v| v.to_le_bytes()).collect();
            if usermem::write_bytes(pml4, a3, &cred) && usermem::write_u32(pml4, a4, 12) { 0 } else { -EFAULT }
        }
        55 => {
            // getsockopt: report "no error" / zero.
            if a3 != 0 {
                usermem::write_u32(pml4, a3, 0);
            }
            if a4 != 0 {
                usermem::write_u32(pml4, a4, 4);
            }
            0
        }
        56 => sys_clone(p, f),
        57 => sys_fork(p, f, 0, 0, 0, 0, 0),
        58 => sys_fork(p, f, 0x4000, 0, 0, 0, 0), // vfork
        59 | 322 => {
            let (path, argv, envp) = if f.rax == 59 { (a0, a1, a2) } else { (a1, a2, a3) };
            let dirfd = if f.rax == 59 { -100 } else { a0 as i32 as i64 };
            match sys_execve(p, f, dirfd, path, argv, envp) {
                Ok(()) => return false,
                Err(e) => e,
            }
        }
        60 => thread_exit(p, a0 as i64),
        319 => {
            // memfd_create(name, flags)
            let fd = add_fd(p, Desc::Memfd { shm: Shm::new(), pos: 0 });
            set_cloexec(p, fd, a1 & 1 != 0);
            fd
        }
        285 => match get_fd(p, a0 as i64) {
            // fallocate: grow shared memory (mode 0 only)
            Some(d) => match &*d.lock() {
                Desc::Memfd { shm, .. } => {
                    let end = a2 + a3;
                    if end <= shm.size() || shm.resize(end) { 0 } else { -ENOMEM }
                }
                _ => 0,
            },
            None => -EBADF,
        },
        284 | 290 => {
            // eventfd / eventfd2(initval, flags)
            let flags = if f.rax == 290 { a1 } else { 0 };
            let fd = add_fd(p, Desc::EventFd { ev: EventFd::new(a0 & 0xffff_ffff, flags & 1 != 0), nonblock: flags & 0x800 != 0 });
            set_cloexec(p, fd, flags & O_CLOEXEC != 0);
            fd
        }
        213 | 291 => {
            let fd = add_fd(p, Desc::Epoll(Epoll::new()));
            set_cloexec(p, fd, f.rax == 291 && a0 & O_CLOEXEC != 0);
            fd
        }
        233 => sys_epoll_ctl(p, a0 as i64, a1, a2 as i32, a3),
        232 | 281 => sys_epoll_wait(p, a0 as i64, a1, a2 as i32, a3 as i32 as i64),
        441 => {
            let ms = if a3 == 0 { -1 } else { timespec_ms(p, a3).map(|v| v as i64).unwrap_or(-1) };
            sys_epoll_wait(p, a0 as i64, a1, a2 as i32, ms)
        }
        105 | 106 | 113 | 114 | 117 | 119 => 0, // set*id: one user
        61 => sys_wait4(p, a0 as i32 as i64, a1, a2),
        62 => sys_kill(p, a0 as i32 as i64, a1),
        200 | 234 => {
            // tkill(tid, sig) / tgkill(tgid, tid, sig)
            let (tgid, tid, sig) = if f.rax == 234 { (a0, a1, a2) } else { (p.pid, a0, a1) };
            if sig > 64 {
                -EINVAL
            } else {
                let target = if tgid == p.pid { Some(p.clone()) } else { super::process::find(tgid) };
                match target {
                    Some(t) => {
                        if signal::send(&t, Some(tid), sig, signal::SI_TKILL, p.pid) {
                            return true;
                        }
                        0
                    }
                    None => -ESRCH,
                }
            }
        }
        63 => sys_uname(p, a0),
        72 => {
            // fcntl
            match a1 {
                0 | 1030 => {
                    let r = sys_dup(p, a0 as i64, None, a2 as usize);
                    if r >= 0 && a1 == 1030
                        && let Some(l) = linux(p)
                    {
                        l.cloexec.lock().insert(r as usize);
                    }
                    r
                }
                1 => linux(p).map(|l| l.cloexec.lock().contains(&(a0 as usize)) as i64).unwrap_or(0),
                2 => {
                    if let Some(l) = linux(p) {
                        let mut c = l.cloexec.lock();
                        if a2 & 1 != 0 { c.insert(a0 as usize); } else { c.remove(&(a0 as usize)); }
                    }
                    0
                }
                3 => match get_fd(p, a0 as i64) {
                    Some(d) => match &*d.lock() {
                        Desc::Tcp { nonblock: true, .. } | Desc::Udp { nonblock: true, .. } | Desc::Input { nonblock: true, .. } => 2 | O_NONBLOCK as i64,
                        Desc::File { writable: true, .. } => 2,
                        _ => 2,
                    },
                    None => -EBADF,
                },
                5 | 36 => {
                    // F_GETLK / F_OFD_GETLK: nobody else holds a lock.
                    if usermem::write_bytes(pml4, a2, &2u16.to_le_bytes()) { 0 } else { -EFAULT }
                }
                4 => {
                    if let Some(d) = get_fd(p, a0 as i64) {
                        match &mut *d.lock() {
                            Desc::Tcp { nonblock, .. } | Desc::Udp { nonblock, .. } | Desc::Input { nonblock, .. } => *nonblock = a2 & O_NONBLOCK != 0,
                            _ => {}
                        }
                    }
                    0
                }
                _ => 0,
            }
        }
        74 | 75 => {
            // fsync / fdatasync: write the file back now.
            if let Some(d) = get_fd(p, a0 as i64)
                && let Desc::File { path, data: Some(data), dirty, .. } = &mut *d.lock()
                && *dirty
            {
                let _ = fs::write_file(path, data);
                *dirty = false;
            }
            0
        }
        77 => {
            // ftruncate
            match get_fd(p, a0 as i64) {
                Some(d) => match &mut *d.lock() {
                    Desc::File { data: Some(data), size, dirty, .. } => {
                        data.resize(a1 as usize, 0);
                        *size = a1;
                        *dirty = true;
                        0
                    }
                    Desc::Memfd { shm, .. } => if shm.resize(a1) { 0 } else { -ENOMEM },
                    _ => -EINVAL,
                },
                None => -EBADF,
            }
        }
        79 => {
            let cwd = linux(p).map(|l| l.cwd.lock().clone()).unwrap_or_default();
            if cwd.len() + 1 > a1 as usize {
                -ERANGE
            } else {
                let mut b = cwd.into_bytes();
                b.push(0);
                if usermem::write_bytes(pml4, a0, &b) { b.len() as i64 } else { -EFAULT }
            }
        }
        80 => match path_at(p, -100, a0) {
            Ok(path) if fs::is_dir(&path) => {
                if let Some(l) = linux(p) {
                    *l.cwd.lock() = path;
                }
                0
            }
            Ok(_) => -ENOENT,
            Err(e) => e,
        },
        82 | 264 | 316 => {
            // rename / renameat / renameat2
            let (from, to) = if f.rax == 82 { (path_at(p, -100, a0), path_at(p, -100, a1)) } else { (path_at(p, a0 as i64, a1), path_at(p, a2 as i64, a3)) };
            match (from, to) {
                (Ok(a), Ok(b)) => fs::rename(&a, &b).map(|_| 0).unwrap_or_else(fs_err),
                (Err(e), _) | (_, Err(e)) => e,
            }
        }
        83 | 258 => {
            let path = if f.rax == 83 { path_at(p, -100, a0) } else { path_at(p, a0 as i64, a1) };
            match path {
                Ok(path) => fs::create_dir(&path).map(|_| 0).unwrap_or_else(fs_err),
                Err(e) => e,
            }
        }
        84 | 87 | 263 => {
            let path = if f.rax == 263 { path_at(p, a0 as i64, a1) } else { path_at(p, -100, a0) };
            match path {
                Ok(path) if path.starts_with("/dev/shm/") => if unix::shm_unlink(&path[9..]) { 0 } else { -ENOENT },
                Ok(path) => fs::remove(&path).map(|_| 0).unwrap_or_else(fs_err),
                Err(e) => e,
            }
        }
        89 | 267 => {
            // readlink(at)
            let (pp, buf, len) = if f.rax == 89 { (a0, a1, a2) } else { (a1, a2, a3) };
            let dirfd = if f.rax == 89 { -100 } else { a0 as i32 as i64 };
            match readlink(p, dirfd, pp) {
                Ok(target) => {
                    let n = target.len().min(len as usize);
                    if usermem::write_bytes(pml4, buf, &target.as_bytes()[..n]) { n as i64 } else { -EFAULT }
                }
                Err(e) => e,
            }
        }
        96 => {
            let ms = unix_ms();
            if a0 != 0 {
                usermem::write_u64(pml4, a0, ms / 1000);
                usermem::write_u64(pml4, a0 + 8, (ms % 1000) * 1000);
            }
            0
        }
        97 | 302 => {
            // getrlimit / prlimit64
            let (res, old) = if f.rax == 97 { (a0, a1) } else { (a1, a3) };
            let lim = match res {
                3 => STACK_SIZE,
                7 => 1024,
                _ => u64::MAX,
            };
            if old != 0 {
                usermem::write_u64(pml4, old, lim);
                usermem::write_u64(pml4, old + 8, lim);
            }
            0
        }
        99 => {
            let (free, total) = pmm::stats();
            let mut b = [0u8; 112];
            b[0..8].copy_from_slice(&(crate::time::uptime_ms() / 1000).to_le_bytes());
            b[32..40].copy_from_slice(&(total as u64 * 4096).to_le_bytes());
            b[40..48].copy_from_slice(&(free as u64 * 4096).to_le_bytes());
            b[104..108].copy_from_slice(&1u32.to_le_bytes());
            if usermem::write_bytes(pml4, a0, &b) { 0 } else { -EFAULT }
        }
        102 | 104 | 107 | 108 => 1000,
        110 => p.parent.max(1) as i64,
        137 | 138 => {
            let mut b = [0u8; 120];
            b[0..8].copy_from_slice(&0x4d44u64.to_le_bytes()); // MSDOS_SUPER_MAGIC
            b[8..16].copy_from_slice(&4096u64.to_le_bytes());
            if let Ok(s) = fs::stats() {
                b[16..24].copy_from_slice(&(s.total_bytes() / 4096).to_le_bytes());
                b[24..32].copy_from_slice(&(s.free_bytes() / 4096).to_le_bytes());
                b[32..40].copy_from_slice(&(s.free_bytes() / 4096).to_le_bytes());
            }
            b[64..72].copy_from_slice(&255u64.to_le_bytes());
            if usermem::write_bytes(pml4, if f.rax == 137 { a1 } else { a1 }, &b) { 0 } else { -EFAULT }
        }
        158 => {
            // arch_prctl
            match a0 {
                0x1002 => {
                    sched::set_fs_base(a1);
                    0
                }
                0x1003 => {
                    let v = unsafe { crate::arch::cpu::rdmsr(crate::arch::cpu::MSR_FS_BASE) };
                    if usermem::write_u64(pml4, a1, v) { 0 } else { -EFAULT }
                }
                _ => -EINVAL,
            }
        }
        186 => sched::current_id() as i64,
        201 => {
            let t = (unix_ms() / 1000) as i64;
            if a0 != 0 {
                usermem::write_u64(pml4, a0, t as u64);
            }
            t
        }
        202 => sys_futex(p, a0, a1, a2, a3),
        204 => {
            // sched_getaffinity: one CPU
            let mut m = vec![0u8; (a1 as usize).min(128)];
            if !m.is_empty() {
                m[0] = 1;
            }
            if usermem::write_bytes(pml4, a2, &m) { m.len() as i64 } else { -EFAULT }
        }
        217 => sys_getdents64(p, a0 as i64, a1, a2),
        218 => {
            sched::set_clear_child_tid(a0);
            sched::current_id() as i64
        }
        228 => {
            // clock_gettime
            let us = if a0 == 0 || a0 == 5 || a0 == 8 { unix_ms() * 1000 + crate::time::uptime_us() % 1000 } else { crate::time::uptime_us() };
            if write_timespec(p, a1, us) { 0 } else { -EFAULT }
        }
        229 => {
            if a1 != 0 {
                write_timespec(p, a1, 1);
            }
            0
        }
        230 => {
            // clock_nanosleep
            if let Some(ms) = timespec_ms(p, a2) {
                let ms = if a1 & 1 != 0 {
                    let now = if a0 == 0 { unix_ms() } else { crate::time::uptime_ms() };
                    ms.saturating_sub(now)
                } else {
                    ms
                };
                if sleep_interruptible(p, ms.max(1)) { -EINTR } else { 0 }
            } else {
                -EFAULT
            }
        }
        231 => {
            super::process::exit_current_process(a0 as i64, None);
            return true;
        }
        257 => sys_openat(p, a0 as i32 as i64, a1, a2),
        332 => {
            // statx(dirfd, path, flags, mask, buf), built from the stat data
            const AT_EMPTY_PATH: u64 = 0x1000;
            let r = if a2 & AT_EMPTY_PATH != 0 && usermem::read_cstr(pml4, a1, 2).map(|s| s.is_empty()).unwrap_or(false) {
                stat_fd(p, a0 as i32 as i64)
            } else {
                path_at(p, a0 as i32 as i64, a1).and_then(|path| stat_path(&path))
            };
            match r {
                Ok(st) => {
                    let g8 = |o: usize| u64::from_le_bytes(st[o..o + 8].try_into().unwrap());
                    let g4 = |o: usize| u32::from_le_bytes(st[o..o + 4].try_into().unwrap());
                    let mut x = [0u8; 256];
                    let mut put = |o: usize, v: &[u8]| x[o..o + v.len()].copy_from_slice(v);
                    put(0, &0x7ffu32.to_le_bytes()); // STATX_BASIC_STATS
                    put(4, &4096u32.to_le_bytes());
                    put(16, &(g8(16) as u32).to_le_bytes());
                    put(20, &g4(28).to_le_bytes());
                    put(24, &g4(32).to_le_bytes());
                    put(28, &(g4(24) as u16).to_le_bytes());
                    put(32, &g8(8).to_le_bytes());
                    put(40, &g8(48).to_le_bytes());
                    put(48, &g8(64).to_le_bytes());
                    for o in [64usize, 80, 96, 112] {
                        put(o, &g8(88).to_le_bytes());
                    }
                    put(140, &1u32.to_le_bytes()); // dev minor
                    if usermem::write_bytes(pml4, a4, &x) { 0 } else { -EFAULT }
                }
                Err(e) => e,
            }
        }
        118 | 120 => {
            // getresuid / getresgid: one user
            for ptr in [a0, a1, a2] {
                usermem::write_u32(pml4, ptr, 1000);
            }
            0
        }
        157 | 221 | 324 => 0, // prctl, fadvise64, membarrier
        262 => {
            // newfstatat
            const AT_EMPTY_PATH: u64 = 0x1000;
            let r = if a3 & AT_EMPTY_PATH != 0 && usermem::read_cstr(pml4, a1, 2).map(|s| s.is_empty()).unwrap_or(false) {
                stat_fd(p, a0 as i32 as i64)
            } else {
                path_at(p, a0 as i32 as i64, a1).and_then(|path| stat_path(&path))
            };
            match r {
                Ok(b) => if usermem::write_bytes(pml4, a2, &b) { 0 } else { -EFAULT },
                Err(e) => e,
            }
        }
        269 | 439 => match path_at(p, a0 as i32 as i64, a1) {
            Ok(path) => if fs::exists(&path) || virtual_file(&path).is_some() { 0 } else { -ENOENT },
            Err(e) => e,
        },
        271 => {
            // ppoll (timeout is a timespec)
            let t = timespec_ms(p, a2).map(|m| m as i64).unwrap_or(-1);
            sys_poll(p, a0, a1, t)
        }
        273 => 0, // set_robust_list
        288 => sys_accept(p, a0 as i64, a1, a2, a3),
        86 | 265 => -EPERM, // link: FAT32 has no hard links
        292 => {
            let r = sys_dup(p, a0 as i64, Some(a1 as i64), 0);
            if r >= 0 && a2 & O_CLOEXEC != 0
                && let Some(l) = linux(p)
            {
                l.cloexec.lock().insert(r as usize);
            }
            r
        }
        293 => {
            let r = sys_pipe(p, a0);
            if r == 0 && a1 & O_CLOEXEC != 0
                && let (Some(l), Some(b)) = (linux(p), usermem::read_bytes(pml4, a0, 8))
            {
                let mut c = l.cloexec.lock();
                c.insert(u32::from_le_bytes(b[0..4].try_into().unwrap()) as usize);
                c.insert(u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize);
            }
            r
        }
        318 => {
            // getrandom
            let mut b = vec![0u8; (a1 as usize).min(1 << 20)];
            fill_random(&mut b);
            if usermem::write_bytes(pml4, a0, &b) { b.len() as i64 } else { -EFAULT }
        }
        18 => sys_pwrite(p, a0 as i64, a1, a2, a3),
        295 | 296 => sys_preadv(p, f.rax == 296, a0 as i64, a1, a2, a3),
        23 | 270 => {
            // select(n, r, w, e, timeval) / pselect6(n, r, w, e, timespec, sigmask)
            let ms = if a4 == 0 {
                -1
            } else if f.rax == 23 {
                let sec = usermem::read_u64(pml4, a4).unwrap_or(0);
                let usec = usermem::read_u64(pml4, a4 + 8).unwrap_or(0);
                (sec * 1000 + usec / 1000) as i64
            } else {
                timespec_ms(p, a4).map(|v| v as i64).unwrap_or(-1)
            };
            sys_select(p, a0 as usize, a1, a2, a3, ms)
        }
        26 | 149 | 150 | 151 | 152 | 325 => 0, // msync, mlock*, munlock*
        27 => {
            // mincore: everything counts as resident
            let n = a1.div_ceil(PAGE_SIZE) as usize;
            if usermem::write_bytes(pml4, a2, &vec![1u8; n]) { 0 } else { -EFAULT }
        }
        36 | 37 | 38 => {
            // getitimer / alarm / setitimer: timers are not armed.
            if f.rax == 36 && a1 != 0 {
                usermem::write_bytes(pml4, a1, &[0u8; 32]);
            }
            if f.rax == 38 && a2 != 0 {
                usermem::write_bytes(pml4, a2, &[0u8; 32]);
            }
            0
        }
        40 => sys_sendfile(p, a0 as i64, a1 as i64, a2, a3),
        73 => if get_fd(p, a0 as i64).is_some() { 0 } else { -EBADF }, // flock: one user
        76 => match path_at(p, -100, a0) {
            // truncate(path, length)
            Ok(path) => match fs::read_file(&path) {
                Ok(mut d) => {
                    d.resize(a1 as usize, 0);
                    fs::write_file(&path, &d).map(|_| 0).unwrap_or_else(fs_err)
                }
                Err(e) => fs_err(e),
            },
            Err(e) => e,
        },
        81 => match get_fd(p, a0 as i64) {
            // fchdir
            Some(d) => match &*d.lock() {
                Desc::Dir { path, .. } => {
                    if let Some(l) = linux(p) {
                        *l.cwd.lock() = path.clone();
                    }
                    0
                }
                _ => -ENOTDIR,
            },
            None => -EBADF,
        },
        85 => sys_openat(p, -100, a0, 0x241), // creat: O_CREAT | O_WRONLY | O_TRUNC
        88 | 266 => {
            // symlink(target, linkpath) / symlinkat(target, dirfd, linkpath):
            // stored as a MayOS link file (FAT32 has no symbolic links).
            let target = usermem::read_cstr(pml4, a0, 4096).unwrap_or_default();
            let link = if f.rax == 88 { path_at(p, -100, a1) } else { path_at(p, a1 as i32 as i64, a2) };
            match link {
                Ok(link) if fs::exists(&link) => -EEXIST,
                Ok(link) => {
                    let mut d = fs::LINK_MAGIC.to_vec();
                    d.extend_from_slice(target.as_bytes());
                    fs::write_file(&link, &d).map(|_| 0).unwrap_or_else(fs_err)
                }
                Err(e) => e,
            }
        }
        90 | 91 | 92 | 93 | 94 | 260 | 268 | 132 | 235 | 280 | 261 => 0, // chmod, chown, utime families
        95 => 0o022, // umask: the old mask
        98 => {
            // getrusage: CPU time of the process's threads
            let ms = sched::process_cpu_ms(p.pid);
            let mut b = [0u8; 144];
            b[0..8].copy_from_slice(&(ms / 1000).to_le_bytes());
            b[8..16].copy_from_slice(&((ms % 1000) * 1000).to_le_bytes());
            if usermem::write_bytes(pml4, a1, &b) { 0 } else { -EFAULT }
        }
        100 => {
            // times: ticks of 10 ms
            let t = sched::process_cpu_ms(p.pid) / 10;
            if a0 != 0 {
                let b: Vec<u8> = [t, 0, 0, 0].iter().flat_map(|v| v.to_le_bytes()).collect();
                usermem::write_bytes(pml4, a0, &b);
            }
            (crate::time::uptime_ms() / 10) as i64
        }
        103 | 116 | 126 | 162 | 203 | 277 | 306 => 0, // syslog, setgroups, capset, sync, sched_setaffinity, sync_file_range, syncfs
        109 => 0, // setpgid
        111 | 112 | 121 | 124 => p.pid as i64, // getpgrp, setsid, getpgid, getsid
        115 => 0, // getgroups: none extra
        125 => {
            // capget: no capabilities
            if a1 != 0 {
                usermem::write_bytes(pml4, a1, &[0u8; 24]);
            }
            0
        }
        140 => 20, // getpriority: nice 0
        141 => 0,
        142 | 143 => {
            if f.rax == 143 {
                usermem::write_u32(pml4, a1, 0);
            }
            0
        }
        144 | 145 | 146 | 147 => 0, // sched_setscheduler, getscheduler (SCHED_OTHER), priority range
        148 => if write_timespec(p, a1, 10_000) { 0 } else { -EFAULT },
        160 => 0, // setrlimit
        161 | 165 | 166 | 272 => -EPERM, // chroot, mount, umount2, unshare
        274 => -EPERM, // get_robust_list of another thread
        317 => -EINVAL, // seccomp: not available (sandboxes must be off)
        309 => {
            // getcpu
            if a0 != 0 {
                usermem::write_u32(pml4, a0, 0);
            }
            if a1 != 0 {
                usermem::write_u32(pml4, a1, 0);
            }
            0
        }
        253 | 294 => {
            // inotify_init / inotify_init1: a descriptor that never reports
            // changes (files are only changed by this machine's programs).
            let flags = if f.rax == 294 { a0 } else { 0 };
            let fd = add_fd(p, Desc::EventFd { ev: EventFd::new(0, false), nonblock: flags & O_NONBLOCK != 0 });
            set_cloexec(p, fd, flags & O_CLOEXEC != 0);
            fd
        }
        254 => {
            // inotify_add_watch: a new watch number
            static WD: core::sync::atomic::AtomicI64 = core::sync::atomic::AtomicI64::new(1);
            WD.fetch_add(1, Ordering::Relaxed)
        }
        255 => 0, // inotify_rm_watch
        283 => {
            // timerfd_create(clock, flags)
            let t = Arc::new(TimerFd { state: Spin::new((0, 0)), realtime: a0 == 0 });
            let fd = add_fd(p, Desc::TimerFd { t, nonblock: a1 & O_NONBLOCK != 0 });
            set_cloexec(p, fd, a1 & O_CLOEXEC != 0);
            fd
        }
        286 | 287 => sys_timerfd(p, f.rax == 286, a0 as i64, a1, a2, a3),
        436 => {
            // close_range(first, last, flags)
            const CLOSE_RANGE_CLOEXEC: u64 = 4;
            let n = linux(p).map(|l| l.fds.lock().len()).unwrap_or(0) as u64;
            for fd in a0..=a1.min(n.saturating_sub(1)) {
                if a2 & CLOSE_RANGE_CLOEXEC != 0 {
                    if get_fd(p, fd as i64).is_some() {
                        set_cloexec(p, fd as i64, true);
                    }
                } else {
                    sys_close(p, fd as i64);
                }
            }
            0
        }
        307 => {
            // sendmmsg(fd, msgvec, n, flags): struct mmsghdr is 64 bytes
            let mut sent = 0;
            for i in 0..a2.min(1024) {
                let r = sys_sendmsg(p, a0 as i64, a1 + i * 64);
                if r < 0 {
                    if sent == 0 {
                        sent = r;
                    }
                    break;
                }
                usermem::write_u32(pml4, a1 + i * 64 + 56, r as u32);
                sent += 1;
            }
            sent
        }
        299 => {
            // recvmmsg: one message at a time
            if a2 == 0 {
                0
            } else {
                let r = sys_recvmsg(p, a0 as i64, a1, a3);
                if r >= 0 {
                    usermem::write_u32(pml4, a1 + 56, r as u32);
                    1
                } else {
                    r
                }
            }
        }
        _ => {
            // Report each missing call once.
            static SEEN: Spin<[u64; 8]> = Spin::new([0; 8]);
            let n = f.rax as usize;
            if n < 512 {
                let mut seen = SEEN.lock();
                if seen[n / 64] & (1 << (n % 64)) == 0 {
                    seen[n / 64] |= 1 << (n % 64);
                    crate::kprintln!("linux: {} called unsupported system call {}", p.name, f.rax);
                }
            }
            -ENOSYS
        }
    };
    let _ = (EPERM, EINTR, EIO);
    f.rax = ret as u64;
    false
}

pub fn is_linux(p: &Process) -> bool {
    p.linux.is_some()
}

pub fn exe_name(p: &Process) -> String {
    linux(p).map(|l| l.exe.lock().clone()).unwrap_or_default()
}

/// ioctls of the framebuffer and input devices.
fn device_ioctl(p: &Process, fd: i64, cmd: u64, arg: u64) -> i64 {
    let Some(d) = get_fd(p, fd) else { return -EBADF };
    let (screen, kind) = match &*d.lock() {
        Desc::Fb { screen, .. } => (screen.clone(), None),
        Desc::Input { screen, kind, .. } => (screen.clone(), Some(*kind)),
        _ => return -ENOTTY,
    };
    let put = |b: &[u8]| if usermem::write_bytes(p.pml4(), arg, b) { 0 } else { -EFAULT };
    let Some(kind) = kind else {
        return match cmd {
            0x4600 => put(&screen::var_info(&screen)), // FBIOGET_VSCREENINFO
            0x4601 => {
                // FBIOPUT_VSCREENINFO: only the resolution can change.
                let Some(v) = usermem::read_bytes(p.pml4(), arg, 160) else { return -EFAULT };
                let w = u32::from_le_bytes(v[0..4].try_into().unwrap());
                let h = u32::from_le_bytes(v[4..8].try_into().unwrap());
                let bpp = u32::from_le_bytes(v[24..28].try_into().unwrap());
                if bpp != 32 && bpp != 0 || !screen.set_size(w, h) {
                    put(&screen::var_info(&screen));
                    return -EINVAL;
                }
                put(&screen::var_info(&screen))
            }
            0x4602 => put(&screen::fix_info(&screen)), // FBIOGET_FSCREENINFO
            0x4606 | 0x4611 => {
                // FBIOPAN_DISPLAY / FBIOBLANK
                screen.presents.fetch_add(1, Ordering::Relaxed);
                0
            }
            0x4604 | 0x4605 => 0, // colour maps: true colour only
            0x40044620 => {
                // FBIO_WAITFORVSYNC: the desktop draws at about 60 Hz.
                screen.presents.fetch_add(1, Ordering::Relaxed);
                sched::sleep_ms(16 - crate::time::uptime_ms() % 16);
                0
            }
            _ => -ENOTTY,
        };
    };
    if kind == InputKind::Mice {
        return -ENOTTY;
    }
    // evdev: _IOC(dir, 'E', nr, size)
    let (ty, nr, size) = ((cmd >> 8) & 0xff, cmd & 0xff, ((cmd >> 16) & 0x3fff) as usize);
    if ty != 0x45 {
        return -ENOTTY;
    }
    let bits = |set: &[u16]| {
        let mut b = alloc::vec![0u8; size];
        for &i in set {
            if let Some(x) = b.get_mut(i as usize / 8) {
                *x |= 1 << (i % 8);
            }
        }
        b
    };
    let keyboard = kind == InputKind::Keyboard;
    match nr {
        0x01 => put(&0x10001i32.to_le_bytes()), // EVIOCGVERSION
        0x02 => put(&[0x06, 0, 0x5e, 0x4d, if keyboard { 1 } else { 2 }, 0, 1, 0]), // EVIOCGID: virtual bus
        0x06 | 0x07 | 0x08 => {
            // EVIOCGNAME / EVIOCGPHYS / EVIOCGUNIQ
            let name = if nr == 0x06 { kind.name() } else { "mayos" };
            let mut b = alloc::vec![0u8; size.min(name.len() + 1)];
            let n = b.len().saturating_sub(1);
            b[..n].copy_from_slice(&name.as_bytes()[..n]);
            if put(&b) == 0 { b.len() as i64 } else { -EFAULT }
        }
        0x09 => put(&bits(if keyboard { &[] } else { &[0] })), // EVIOCGPROP: pointer
        0x18 | 0x19 | 0x1b => put(&alloc::vec![0u8; size]), // key / led / switch state
        0x20 => put(&bits(if keyboard { &[0, 1] } else { &[0, 1, 2, 3] })),
        0x21 => {
            // EV_KEY bits
            let keys: Vec<u16> = if keyboard { (1..=111).chain([125]).collect() } else { alloc::vec![0x110, 0x111, 0x112] };
            put(&bits(&keys))
        }
        0x22 => put(&bits(if keyboard { &[] } else { &[0, 1, 8] })),
        0x23 => put(&bits(if keyboard { &[] } else { &[0, 1] })),
        0x24..=0x3f => put(&alloc::vec![0u8; size]),
        0x40 | 0x41 if !keyboard => {
            // EVIOCGABS(ABS_X / ABS_Y): value, min, max, fuzz, flat, resolution
            let (w, h) = screen.size();
            let pos = *screen.last_pos.lock();
            let (v, max) = if nr == 0x40 { (pos.0, w as i32 - 1) } else { (pos.1, h as i32 - 1) };
            let b: Vec<u8> = [v, 0, max, 0, 0, 0].iter().flat_map(|x| x.to_le_bytes()).collect();
            put(&b)
        }
        0x40..=0x7f => -EINVAL,
        0x90 | 0x91 => 0, // EVIOCGRAB / EVIOCREVOKE
        0x03 | 0xa0 => 0, // EVIOCSREP / EVIOCSCLOCKID
        _ => -EINVAL,
    }
}

// --- processes ----------------------------------------------------------

/// Drop every descriptor (process exit).
pub fn close_all(l: &LinuxState) {
    let old = core::mem::take(&mut *l.fds.lock());
    drop(old);
}

/// fork / vfork / clone without CLONE_VM: a copy of the process whose
/// only thread continues from the same place with rax = 0.
fn sys_fork(p: &Arc<Process>, f: &TrapFrame, flags: u64, newsp: u64, ptid: u64, ctid: u64, tls: u64) -> i64 {
    const CLONE_VFORK: u64 = 0x4000;
    const CLONE_SETTLS: u64 = 0x80000;
    const CLONE_PARENT_SETTID: u64 = 0x100000;
    const CLONE_CHILD_CLEARTID: u64 = 0x200000;
    const CLONE_CHILD_SETTID: u64 = 0x1000000;
    let Some(l) = linux(p) else { return -ENOSYS };
    let Some(pml4) = paging::clone_address_space(p.pml4()) else { return -ENOMEM };
    let state = LinuxState {
        fds: Spin::new(l.fds.lock().clone()),
        regions: Spin::new(l.regions.lock().clone()),
        mmap_next: Spin::new(*l.mmap_next.lock()),
        brk: Spin::new(*l.brk.lock()),
        cwd: Spin::new(l.cwd.lock().clone()),
        exe: Spin::new(l.exe.lock().clone()),
        cloexec: Spin::new(l.cloexec.lock().clone()),
        screen: Spin::new(None),
        shared: Spin::new(l.shared.lock().clone()),
        sig: Spin::new(l.sig.lock().fork_copy(sched::current_id())),
        mapped: Spin::new(l.mapped.lock().clone()),
    };
    let child = super::process::fork_process(p, pml4, state);
    let mut frame = f.clone();
    frame.rax = 0;
    if newsp != 0 {
        frame.rsp = newsp;
    }
    let fs = if flags & CLONE_SETTLS != 0 { tls } else { sched::fs_base() };
    if flags & CLONE_CHILD_SETTID != 0 {
        usermem::write_u32(pml4, ctid, child.pid as u32);
    }
    if flags & CLONE_PARENT_SETTID != 0 {
        usermem::write_u32(p.pml4(), ptid, child.pid as u32);
    }
    let tid = sched::spawn_user_frame(child.clone(), frame, fs);
    if flags & CLONE_CHILD_CLEARTID != 0 {
        sched::set_clear_child_tid_of(tid, ctid);
    }
    if flags & CLONE_VFORK != 0 {
        // The parent sleeps until the child has run execve or exited.
        while child.has_exited().is_none() && child.pml4() == pml4 {
            sched::sleep_ms(1);
        }
    }
    child.pid as i64
}

fn read_strv(pml4: u64, mut ptr: u64) -> Result<Vec<String>, i64> {
    let mut v = Vec::new();
    if ptr == 0 {
        return Ok(v);
    }
    let mut total = 0;
    loop {
        let a = usermem::read_u64(pml4, ptr).ok_or(-EFAULT)?;
        if a == 0 {
            return Ok(v);
        }
        let s = usermem::read_cstr(pml4, a, 128 * 1024).ok_or(-EFAULT)?;
        total += s.len() + 1 + 8;
        if total > 48 * 1024 || v.len() > 4096 {
            return Err(-E2BIG);
        }
        v.push(s);
        ptr += 8;
    }
}

/// Replace the program. On success the trap frame starts the new one.
fn sys_execve(p: &Arc<Process>, f: &mut TrapFrame, dirfd: i64, pathp: u64, argvp: u64, envp: u64) -> Result<(), i64> {
    let l = linux(p).ok_or(-ENOSYS)?;
    let mut path = path_at(p, dirfd, pathp)?;
    let mut argv = read_strv(p.pml4(), argvp)?;
    let env = read_strv(p.pml4(), envp)?;
    let mut data = fs::read_file(&fs::resolve_link(&path)).map_err(fs_err)?;
    // Scripts: "#!interpreter [one argument]".
    for _ in 0..4 {
        if !data.starts_with(b"#!") {
            break;
        }
        let line_end = data.iter().position(|&b| b == b'\n').unwrap_or(data.len()).min(256);
        let line = String::from_utf8_lossy(&data[2..line_end]).trim().to_string();
        let mut parts = line.splitn(2, [' ', '\t']);
        let interp = parts.next().unwrap_or("").to_string();
        if interp.is_empty() {
            return Err(-ENOEXEC);
        }
        let mut new_argv = alloc::vec![interp.clone()];
        if let Some(arg) = parts.next().map(str::trim).filter(|a| !a.is_empty()) {
            new_argv.push(arg.to_string());
        }
        new_argv.push(path.clone());
        new_argv.extend(argv.into_iter().skip(1));
        argv = new_argv;
        path = interp;
        data = fs::read_file(&fs::resolve_link(&path)).map_err(fs_err)?;
    }
    if !super::elf::is_elf(&data) {
        return Err(-ENOEXEC);
    }
    if argv.is_empty() {
        argv.push(path.clone());
    }
    let pml4 = paging::new_address_space().ok_or(-ENOMEM)?;
    let prepared = (|| -> Result<(u64, u64, u64, Region), String> {
        let image = super::elf::load(pml4, &data)?;
        let (entry, at_base) = load_interp(pml4, &image)?;
        let (rsp, stack) = build_stack(pml4, &image, at_base, &argv, &env, &path)?;
        Ok((entry, rsp, image.brk, stack))
    })();
    let (entry, rsp, brk, stack) = match prepared {
        Ok(v) => v,
        Err(e) => {
            paging::destroy_address_space(pml4);
            p.console.write(alloc::format!("{}: {}\n", path, e).as_bytes());
            return Err(-ENOEXEC);
        }
    };
    // Point of no return: the old program goes away.
    sched::kill_other_threads(p.pid);
    let old = p.replace_pml4(pml4);
    sched::switch_address_space(pml4);
    paging::destroy_address_space(old);
    *l.regions.lock() = alloc::vec![stack];
    l.mapped.lock().clear();
    *l.mmap_next.lock() = MMAP_BASE;
    *l.brk.lock() = (brk, brk);
    // The real file: programs find their files next to it (Firefox).
    *l.exe.lock() = fs::resolve_link(&path);
    let closing: Vec<usize> = core::mem::take(&mut *l.cloexec.lock()).into_iter().collect();
    let dropped: Vec<Option<DescRef>> = {
        let mut fds = l.fds.lock();
        closing.iter().filter_map(|&fd| fds.get_mut(fd).map(|s| s.take())).collect()
    };
    drop(dropped);
    sched::set_clear_child_tid(0);
    l.sig.lock().exec_reset();
    let (cs, ss) = (f.cs, f.ss);
    *f = TrapFrame { rip: entry, rsp, cs, ss, rflags: 0x202, ..Default::default() };
    // Fresh floating-point state.
    crate::arch::cpu::fxrstor(&crate::arch::cpu::fpu_initial());
    Ok(())
}

/// wait4: collect an exited child. Status: exit code << 8, or the signal.
fn sys_wait4(p: &Arc<Process>, pid: i64, status: u64, options: u64) -> i64 {
    const WNOHANG: u64 = 1;
    loop {
        let kids: Vec<Arc<Process>> = super::process::children(p.pid)
            .into_iter()
            .filter(|c| pid == -1 || pid == 0 || pid < -1 || c.pid as i64 == pid)
            .collect();
        if kids.is_empty() {
            return -ECHILD;
        }
        if let Some((c, code)) = kids.iter().find_map(|c| c.has_exited().map(|code| (c, code))) {
            let st: u32 = if code == -130 { 2 } else if code < 0 && code >= -64 { (-code) as u32 } else { ((code as u32) & 0xff) << 8 };
            if status != 0 && !usermem::write_u32(p.pml4(), status, st) {
                return -EFAULT;
            }
            let id = c.pid;
            super::process::reap(id);
            return id as i64;
        }
        if options & WNOHANG != 0 {
            return 0;
        }
        if signal::interrupted(p) {
            return -EINTR;
        }
        sched::sleep_ms(2);
    }
}

/// kill: queue the signal, or end the target if it has no handler.
fn sys_kill(p: &Arc<Process>, pid: i64, sig: u64) -> i64 {
    let target = if pid == 0 || pid == -1 { p.pid } else { pid.unsigned_abs() };
    let Some(t) = super::process::find(target) else { return -ESRCH };
    if sig > 64 {
        return -EINVAL;
    }
    if t.has_exited().is_some() {
        return if sig == 0 { -ESRCH } else { 0 };
    }
    if signal::send(&t, None, sig, signal::SI_USER, p.pid) {
        sched::exit_current();
    }
    0
}

fn sys_mprotect(p: &Process, addr: u64, len: u64, prot: u64) -> i64 {
    let Some(l) = linux(p) else { return -ENOSYS };
    if addr & (PAGE_SIZE - 1) != 0 {
        return -EINVAL;
    }
    let end = addr + len.div_ceil(PAGE_SIZE) * PAGE_SIZE;
    let mut flags = USER;
    if prot & 2 != 0 {
        flags |= WRITABLE;
    }
    if prot & 4 == 0 {
        flags |= NO_EXECUTE;
    }
    // Pages still to be faulted in get the new rights from their region.
    {
        let mut regs = l.regions.lock();
        let mut out = Vec::new();
        for r in regs.iter() {
            if r.end <= addr || r.start >= end {
                out.push(*r);
                continue;
            }
            if r.start < addr {
                out.push(Region { end: addr, ..*r });
            }
            out.push(Region { end: r.end.min(end), writable: prot & 2 != 0 || prot == 0, exec: prot & 4 != 0, none: prot == 0, ..r.from(r.start.max(addr)) });
            if r.end > end {
                out.push(r.from(end));
            }
        }
        *regs = out;
    }
    let mut a = addr;
    while a < end {
        // PROT_NONE keeps the page (and its contents) but only for the
        // kernel: user accesses fault until the rights come back.
        paging::set_flags(p.pml4(), a, if prot == 0 { flags & !USER } else { flags });
        a += PAGE_SIZE;
    }
    0
}

/// preadv / pwritev(fd, iov, cnt, offset)
fn sys_preadv(p: &Process, write: bool, fd: i64, iov: u64, cnt: u64, mut off: u64) -> i64 {
    let Some(vecs) = iovecs(p, iov, cnt) else { return -EFAULT };
    let mut total = 0i64;
    for (b, l) in vecs {
        let r = if write { sys_pwrite(p, fd, b, l, off) } else { sys_pread(p, fd, b, l, off) };
        if r < 0 {
            return if total == 0 { r } else { total };
        }
        total += r;
        off += r as u64;
        if (r as u64) < l {
            break;
        }
    }
    total
}

/// sendfile(out, in, offset*, count)
fn sys_sendfile(p: &Process, out: i64, inp: i64, offp: u64, count: u64) -> i64 {
    let Some(src) = get_fd(p, inp) else { return -EBADF };
    let Some(dst) = get_fd(p, out) else { return -EBADF };
    let mut buf = vec![0u8; (count as usize).min(1 << 20)];
    let n = if offp != 0 {
        let off = usermem::read_u64(p.pml4(), offp).unwrap_or(0);
        let n = read_at(&mut src.lock(), off, &mut buf);
        if n > 0 {
            usermem::write_u64(p.pml4(), offp, off + n as u64);
        }
        n
    } else {
        read_desc(p, &src, &mut buf)
    };
    if n <= 0 { n } else { write_desc(p, &dst, &buf[..n as usize]) }
}

/// pwrite64: write at an offset without moving the file position.
fn sys_pwrite(p: &Process, fd: i64, ptr: u64, len: u64, off: u64) -> i64 {
    let Some(d) = get_fd(p, fd) else { return -EBADF };
    let Some(data) = usermem::read_bytes(p.pml4(), ptr, len.min(16 * 1024 * 1024)) else { return -EFAULT };
    let mut g = d.lock();
    match &mut *g {
        Desc::File { data: Some(buf), writable: true, dirty, size, .. } => {
            let end = off as usize + data.len();
            if end > buf.len() {
                buf.resize(end, 0);
            }
            buf[off as usize..end].copy_from_slice(&data);
            *size = buf.len() as u64;
            *dirty = true;
            data.len() as i64
        }
        Desc::Memfd { pos, .. } => {
            let saved = *pos;
            *pos = off;
            drop(g);
            let r = write_desc(p, &d, &data);
            if let Desc::Memfd { pos, .. } = &mut *d.lock() {
                *pos = saved;
            }
            r
        }
        Desc::File { .. } => -EBADF,
        _ => -ESPIPE,
    }
}

/// select / pselect6 on top of the poll readiness checks.
fn sys_select(p: &Process, n: usize, rp: u64, wp: u64, ep: u64, timeout_ms: i64) -> i64 {
    let n = n.min(1024);
    let words = n.div_ceil(64);
    let read_set = |ptr: u64| -> Option<Vec<u64>> {
        if ptr == 0 {
            return Some(vec![0; words]);
        }
        (0..words).map(|i| usermem::read_u64(p.pml4(), ptr + i as u64 * 8)).collect()
    };
    let (Some(r), Some(w), Some(_e)) = (read_set(rp), read_set(wp), read_set(ep)) else { return -EFAULT };
    let deadline = if timeout_ms < 0 { u64::MAX } else { crate::time::uptime_ms() + timeout_ms as u64 };
    loop {
        let mut ro = vec![0u64; words];
        let mut wo = vec![0u64; words];
        let mut count = 0;
        for fd in 0..n {
            let (wr, ww) = (r[fd / 64] >> (fd % 64) & 1 != 0, w[fd / 64] >> (fd % 64) & 1 != 0);
            if !wr && !ww {
                continue;
            }
            let ev = ready(p, fd as i64, if wr { 1 } else { 0 } | if ww { 4 } else { 0 });
            if ev & 0x20 != 0 {
                return -EBADF;
            }
            if wr && ev & (1 | 0x10 | 8) != 0 {
                ro[fd / 64] |= 1 << (fd % 64);
                count += 1;
            }
            if ww && ev & (4 | 8) != 0 {
                wo[fd / 64] |= 1 << (fd % 64);
                count += 1;
            }
        }
        if count > 0 || crate::time::uptime_ms() >= deadline {
            let write_set = |ptr: u64, v: &[u64]| ptr == 0 || v.iter().enumerate().all(|(i, x)| usermem::write_u64(p.pml4(), ptr + i as u64 * 8, *x));
            if !write_set(rp, &ro) || !write_set(wp, &wo) || !write_set(ep, &vec![0; words]) {
                return -EFAULT;
            }
            return count;
        }
        if signal::interrupted(p) {
            return -EINTR;
        }
        sched::sleep_ms(2);
    }
}

/// timerfd_settime(fd, flags, new, old) / timerfd_gettime(fd, cur)
fn sys_timerfd(p: &Process, set: bool, fd: i64, a1: u64, a2: u64, a3: u64) -> i64 {
    let Some(d) = get_fd(p, fd) else { return -EBADF };
    let t = match &*d.lock() {
        Desc::TimerFd { t, .. } => t.clone(),
        _ => return -EINVAL,
    };
    let pml4 = p.pml4();
    let ts_us = |ptr: u64| -> Option<u64> {
        let s = usermem::read_u64(pml4, ptr)?;
        let ns = usermem::read_u64(pml4, ptr + 8)?;
        Some(s.saturating_mul(1_000_000) + ns / 1000)
    };
    let put_us = |ptr: u64, us: u64| usermem::write_u64(pml4, ptr, us / 1_000_000) && usermem::write_u64(pml4, ptr + 8, (us % 1_000_000) * 1000);
    let now = crate::time::uptime_us();
    let cur = *t.state.lock();
    let out = if set { a3 } else { a1 };
    if out != 0 {
        let left = if cur.0 == 0 { 0 } else { cur.0.saturating_sub(now).max(1) };
        if !(put_us(out, cur.1) && put_us(out + 16, left)) {
            return -EFAULT;
        }
    }
    if set {
        let (Some(interval), Some(value)) = (ts_us(a2), ts_us(a2 + 16)) else { return -EFAULT };
        const TFD_TIMER_ABSTIME: u64 = 1;
        let next = if value == 0 {
            0
        } else if a1 & TFD_TIMER_ABSTIME != 0 {
            let clock_now = if t.realtime { unix_ms() * 1000 } else { now };
            now + value.saturating_sub(clock_now).max(1)
        } else {
            now + value
        };
        *t.state.lock() = (next, interval);
    }
    0
}

/// mremap: grow or shrink an anonymous mapping, moving it if allowed.
fn sys_mremap(p: &Process, old: u64, old_len: u64, new_len: u64, flags: u64) -> i64 {
    const MREMAP_MAYMOVE: u64 = 1;
    let Some(l) = linux(p) else { return -ENOSYS };
    if old & (PAGE_SIZE - 1) != 0 || new_len == 0 {
        return -EINVAL;
    }
    let old_len = old_len.div_ceil(PAGE_SIZE) * PAGE_SIZE;
    let new_len = new_len.div_ceil(PAGE_SIZE) * PAGE_SIZE;
    // The old range must be one mapping (musl probes the stack this way
    // and expects EFAULT past its end).
    let region = l.regions.lock().iter().find(|r| old >= r.start && old < r.end).copied();
    let Some(r) = region else { return -EFAULT };
    if old + old_len > r.end {
        return -EFAULT;
    }
    if new_len <= old_len {
        remove_regions(l, old + new_len, old + old_len);
        unmap_range(p, old + new_len, old + old_len);
        return old as i64;
    }
    if flags & MREMAP_MAYMOVE == 0 {
        return -ENOMEM;
    }
    // Move: a fresh range, with the old pages re-pointed there.
    let start = {
        let mut next = l.mmap_next.lock();
        let s = *next;
        *next += new_len + PAGE_SIZE;
        s
    };
    l.regions.lock().push(Region { start, end: start + new_len, ..r });
    let mut a = 0;
    while a < old_len {
        if let Some((phys, entry)) = paging::translate(p.pml4(), old + a)
            && entry & paging::BORROWED == 0
        {
            let frame = phys & !(PAGE_SIZE - 1);
            paging::unmap(p.pml4(), old + a);
            let mut flags = USER;
            if r.writable {
                flags |= WRITABLE;
            }
            if !r.exec {
                flags |= NO_EXECUTE;
            }
            if paging::map(p.pml4(), start + a, frame, flags).is_err() {
                pmm::free_frame(frame);
            }
        }
        a += PAGE_SIZE;
    }
    remove_regions(l, old, old + old_len);
    unmap_range(p, old, old + old_len);
    start as i64
}

/// readlink: MayOS link files and the /proc/self links.
fn readlink(p: &Process, dirfd: i64, ptr: u64) -> Result<String, i64> {
    let raw = usermem::read_cstr(p.pml4(), ptr, 4096).ok_or(-EFAULT)?;
    let raw = proc_self(p, &raw);
    if raw == "/proc/self/exe" {
        return Ok(linux(p).map(|l| l.exe.lock().clone()).unwrap_or_default());
    }
    if raw == "/proc/self/cwd" {
        return Ok(linux(p).map(|l| l.cwd.lock().clone()).unwrap_or_default());
    }
    if let Some(n) = raw.strip_prefix("/proc/self/fd/") {
        let fd: i64 = n.parse().map_err(|_| -ENOENT)?;
        let d = get_fd(p, fd).ok_or(-ENOENT)?;
        let g = d.lock();
        return Ok(match &*g {
            Desc::File { path, .. } | Desc::Dir { path, .. } => path.clone(),
            Desc::Console => String::from("/dev/tty"),
            Desc::Null => String::from("/dev/null"),
            Desc::PipeRead(_) | Desc::PipeWrite(_) => alloc::format!("pipe:[{}]", fd + 1000),
            Desc::Tcp { .. } | Desc::Udp { .. } | Desc::Unix { .. } => alloc::format!("socket:[{}]", fd + 1000),
            Desc::Memfd { .. } => String::from("/memfd: (deleted)"),
            _ => alloc::format!("anon_inode:[{}]", fd),
        });
    }
    let path = path_at(p, dirfd, ptr)?;
    let data = fs::read_file(&path).map_err(fs_err)?;
    if data.len() < 1024 && data.starts_with(fs::LINK_MAGIC) {
        Ok(String::from_utf8_lossy(&data[fs::LINK_MAGIC.len()..]).trim().to_string())
    } else {
        Err(-EINVAL)
    }
}

/// "/proc/<own pid>/..." and "/proc/thread-self/..." name "/proc/self/...".
fn proc_self(p: &Process, path: &str) -> String {
    let own = alloc::format!("/proc/{}/", p.pid);
    if let Some(rest) = path.strip_prefix(own.as_str()) {
        return alloc::format!("/proc/self/{}", rest);
    }
    if let Some(rest) = path.strip_prefix("/proc/thread-self/") {
        return alloc::format!("/proc/self/{}", rest);
    }
    String::from(path)
}

/// Per-process /proc files and a few /sys files programs look at.
fn proc_file(p: &Process, path: &str) -> Option<Desc> {
    let path = proc_self(p, path);
    let l = linux(p)?;
    let name = l.exe.lock().rsplit('/').next().unwrap_or("").chars().take(15).collect::<String>();
    let threads = sched::process_thread_count(p.pid);
    let text = match path.as_str() {
        "/proc/self/maps" | "/proc/self/smaps" => {
            let mut regs = l.regions.lock().clone();
            regs.sort_by_key(|r| r.start);
            let mut t = String::new();
            for r in regs {
                t.push_str(&alloc::format!(
                    "{:x}-{:x} r{}{}p 00000000 00:00 0\n",
                    r.start,
                    r.end,
                    if r.writable { 'w' } else { '-' },
                    if r.exec { 'x' } else { '-' }
                ));
            }
            t
        }
        "/proc/self/cmdline" => {
            let mut t = l.exe.lock().clone();
            t.push('\0');
            t
        }
        "/proc/self/comm" => alloc::format!("{}\n", name),
        "/proc/self/stat" => {
            let cpu = sched::process_cpu_ms(p.pid) / 10;
            alloc::format!(
                "{} ({}) R {} {} {} 0 -1 0 0 0 0 0 {} 0 0 0 20 0 {} 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n",
                p.pid, name, p.parent.max(1), p.pid, p.pid, cpu, threads
            )
        }
        "/proc/self/statm" => String::from("0 0 0 0 0 0 0\n"),
        "/proc/self/status" => alloc::format!(
            "Name:\t{}\nState:\tR (running)\nTgid:\t{}\nPid:\t{}\nPPid:\t{}\nTracerPid:\t0\nUid:\t1000\t1000\t1000\t1000\nGid:\t1000\t1000\t1000\t1000\nVmRSS:\t0 kB\nThreads:\t{}\nSigQ:\t0/0\n",
            name, p.pid, p.pid, p.parent.max(1), threads
        ),
        "/proc/self/oom_score_adj" | "/proc/self/oom_adj" | "/proc/self/loginuid" => String::from("0\n"),
        "/proc/self/mountinfo" | "/proc/mounts" | "/proc/self/mounts" => String::from("1 0 0:1 / / rw - vfat /dev/root rw\n"),
        "/proc/self/limits" => String::from("Limit Soft Limit Hard Limit Units\nMax stack size 8388608 unlimited bytes\nMax open files 1024 1024 files\n"),
        "/proc/sys/kernel/osrelease" => String::from("6.1.0-mayos\n"),
        "/proc/sys/kernel/random/uuid" | "/proc/sys/kernel/random/boot_id" => {
            let mut b = [0u8; 16];
            fill_random(&mut b);
            let h: String = b.iter().map(|x| alloc::format!("{:02x}", x)).collect();
            alloc::format!("{}-{}-{}-{}-{}\n", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
        }
        "/proc/sys/kernel/yama/ptrace_scope" => String::from("1\n"),
        "/proc/sys/kernel/pid_max" => String::from("4194304\n"),
        "/proc/sys/vm/overcommit_memory" => String::from("0\n"),
        "/proc/uptime" => {
            let ms = crate::time::uptime_ms();
            alloc::format!("{}.{:02} {}.{:02}\n", ms / 1000, ms % 1000 / 10, ms / 1000, ms % 1000 / 10)
        }
        "/proc/loadavg" => String::from("0.00 0.00 0.00 1/1 1\n"),
        "/proc/stat" => String::from("cpu  0 0 0 0 0 0 0 0 0 0\ncpu0 0 0 0 0 0 0 0 0 0 0\nbtime 0\n"),
        "/proc/version" => String::from("Linux version 6.1.0-mayos (MayOS) #1\n"),
        "/proc/filesystems" => String::from("\tvfat\nnodev\ttmpfs\n"),
        "/sys/devices/system/cpu/online" | "/sys/devices/system/cpu/present" | "/sys/devices/system/cpu/possible" => String::from("0\n"),
        "/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq" => String::from("2000000\n"),
        "/etc/machine-id" | "/var/lib/dbus/machine-id" => String::from("6d61796f736d61796f736d61796f7331\n"),
        "/etc/localtime" => return None,
        _ => return None,
    };
    Some(Desc::Virtual { data: text.into_bytes(), pos: 0 })
}
