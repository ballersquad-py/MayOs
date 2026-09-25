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
use super::{sched, usermem};
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
const EISDIR: i64 = 21;
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
}

pub type DescRef = Arc<Mutex<Desc>>;

pub struct LinuxState {
    pub fds: Spin<Vec<Option<DescRef>>>,
    pub regions: Spin<Vec<Region>>,
    pub mmap_next: Spin<u64>,
    /// (heap start, current break)
    pub brk: Spin<(u64, u64)>,
    pub cwd: Spin<String>,
    pub exe: String,
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
pub fn setup(pml4: u64, image: &super::elf::LoadedImage, path: &str, args: &str, cwd: &str) -> Result<(LinuxState, u64), String> {
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
    let mut argv: Vec<String> = alloc::vec![String::from(path)];
    argv.extend(split_args(args));
    let env = [
        String::from("PATH=/bin"),
        String::from("HOME=/home"),
        String::from("USER=user"),
        String::from("LANG=C.UTF-8"),
        String::from("TERM=xterm"),
        alloc::format!("PWD={}", cwd),
    ];
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
        (7, 0),            // AT_BASE (no interpreter)
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
    let state = LinuxState {
        fds: Spin::new(alloc::vec![
            Some(Arc::new(Mutex::new(Desc::Console))),
            Some(Arc::new(Mutex::new(Desc::Console))),
            Some(Arc::new(Mutex::new(Desc::Console))),
        ]),
        regions: Spin::new(alloc::vec![Region { start: bottom, end: top - eager * PAGE_SIZE, writable: true, exec: false }]),
        mmap_next: Spin::new(MMAP_BASE),
        brk: Spin::new((image.brk, image.brk)),
        cwd: Spin::new(String::from(cwd)),
        exe: String::from(path),
    };
    Ok((state, sp))
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
    if write && !r.writable {
        return false;
    }
    if paging::translate(p.pml4, page).is_some() {
        return true;
    }
    let Some(f) = pmm::alloc_frame_zeroed() else { return false };
    let mut flags = USER;
    if r.writable {
        flags |= WRITABLE;
    }
    if !r.exec {
        flags |= NO_EXECUTE;
    }
    if paging::map(p.pml4, page, f, flags).is_err() {
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
        if let Some(f) = paging::unmap(p.pml4, a) {
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
            out.push(Region { start: end, ..*r });
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
    let writable = prot & 2 != 0 || flags & MAP_ANONYMOUS == 0;
    l.regions.lock().push(Region { start, end: start + len, writable: writable || prot == 0, exec: prot & 4 != 0 });
    if flags & MAP_ANONYMOUS == 0 {
        // File mapping: copy the contents in (private mappings only).
        let Some(d) = get_fd(p, fd) else { return -EBADF };
        let mut buf = vec![0u8; len as usize];
        let n = {
            let mut g = d.lock();
            read_at(&mut g, off, &mut buf)
        };
        if n < 0 {
            return n;
        }
        if !usermem::write_bytes(p.pml4, start, &buf[..n as usize]) {
            return -ENOMEM;
        }
    }
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
        l.regions.lock().push(Region { start: old_top, end: new_top, writable: true, exec: false });
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
    let s = usermem::read_cstr(p.pml4, ptr, 4096).ok_or(-EFAULT)?;
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
        "/proc/self/maps" | "/proc/self/cmdline" | "/proc/self/stat" | "/proc/self/status" => String::new(),
        _ => return None,
    };
    Some(Desc::Virtual { data: text.into_bytes(), pos: 0 })
}

const O_ACCMODE: u64 = 3;
const O_CREAT: u64 = 0x40;
const O_EXCL: u64 = 0x80;
const O_TRUNC: u64 = 0x200;
const O_APPEND: u64 = 0x400;
const O_NONBLOCK: u64 = 0x800;
const O_DIRECTORY: u64 = 0x10000;

fn sys_openat(p: &Process, dirfd: i64, ptr: u64, flags: u64) -> i64 {
    let path = match path_at(p, dirfd, ptr) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if let Some(d) = virtual_file(&path) {
        return add_fd(p, d);
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
        match fs::open(&path) {
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
        Desc::File { .. } | Desc::Dir { .. } | Desc::Virtual { .. } => -EBADF,
        Desc::Null | Desc::Zero | Desc::Random => data.len() as i64,
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
    if n > 0 && !usermem::write_bytes(p.pml4, ptr, &buf[..n as usize]) {
        return -EFAULT;
    }
    n
}

fn sys_write(p: &Process, fd: i64, ptr: u64, len: u64) -> i64 {
    let Some(d) = get_fd(p, fd) else { return -EBADF };
    let Some(data) = usermem::read_bytes(p.pml4, ptr, len.min(16 * 1024 * 1024)) else { return -EFAULT };
    write_desc(p, &d, &data)
}

fn iovecs(p: &Process, iov: u64, cnt: u64) -> Option<Vec<(u64, u64)>> {
    let mut v = Vec::new();
    for i in 0..cnt.min(1024) {
        let base = usermem::read_u64(p.pml4, iov + i * 16)?;
        let len = usermem::read_u64(p.pml4, iov + i * 16 + 8)?;
        v.push((base, len));
    }
    Some(v)
}

fn sys_writev(p: &Process, fd: i64, iov: u64, cnt: u64) -> i64 {
    let Some(vecs) = iovecs(p, iov, cnt) else { return -EFAULT };
    let mut all = Vec::new();
    for (b, l) in vecs {
        match usermem::read_bytes(p.pml4, b, l) {
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
        if !usermem::write_bytes(p.pml4, b, &buf[off..off + k]) {
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
    if n > 0 && !usermem::write_bytes(p.pml4, ptr, &buf[..n as usize]) {
        return -EFAULT;
    }
    n
}

fn sys_lseek(p: &Process, fd: i64, off: i64, whence: u64) -> i64 {
    let Some(d) = get_fd(p, fd) else { return -EBADF };
    let mut g = d.lock();
    let (pos, size): (&mut u64, u64) = match &mut *g {
        Desc::File { pos, size, .. } => (pos, *size),
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
    let mut fds = l.fds.lock();
    match fds.get_mut(fd as usize) {
        Some(slot @ Some(_)) => {
            *slot = None;
            0
        }
        _ => -EBADF,
    }
}

fn sys_dup(p: &Process, fd: i64, to: Option<i64>, min: usize) -> i64 {
    let Some(d) = get_fd(p, fd) else { return -EBADF };
    match to {
        Some(t) if t >= 0 => {
            if t == fd {
                return t;
            }
            let Some(l) = linux(p) else { return -EBADF };
            let mut fds = l.fds.lock();
            while fds.len() <= t as usize {
                fds.push(None);
            }
            fds[t as usize] = Some(d);
            t
        }
        _ => add_fd_ref(p, d, min),
    }
}

fn sys_pipe(p: &Process, ptr: u64) -> i64 {
    let pipe = Arc::new(Pipe { buf: Spin::new(VecDeque::new()), writers: 1.into(), readers: 1.into() });
    let r = add_fd(p, Desc::PipeRead(pipe.clone()));
    let w = add_fd(p, Desc::PipeWrite(pipe));
    if !usermem::write_u32(p.pml4, ptr, r as u32) || !usermem::write_u32(p.pml4, ptr + 4, w as u32) {
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
    if let Some(d) = virtual_file(path) {
        let (mode, size) = match &d {
            Desc::Virtual { data, .. } => (0o100444, data.len() as u64),
            _ => (0o20666, 0),
        };
        return Ok(stat_buf(mode, size, 0, 1));
    }
    if path == "/" {
        return Ok(stat_buf(0o40755, 4096, 0, 2));
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
    if !usermem::write_bytes(p.pml4, ptr, &out) {
        return -EFAULT;
    }
    out.len() as i64
}

// --- sockets ----------------------------------------------------------

fn read_sockaddr(p: &Process, ptr: u64, len: u64) -> Result<(net::Ipv4, u16), i64> {
    let b = usermem::read_bytes(p.pml4, ptr, len.min(128)).ok_or(-EFAULT)?;
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
    usermem::write_bytes(p.pml4, ptr, &b) && (lenptr == 0 || usermem::write_u32(p.pml4, lenptr, 16))
}

fn sys_socket(p: &Process, domain: u64, ty: u64) -> i64 {
    if domain != 2 {
        return -EAFNOSUPPORT;
    }
    let nonblock = ty & 0x800 != 0;
    match ty & 0xf {
        1 => add_fd(p, Desc::Tcp { stream: None, listener: None, bound: 0, nonblock }),
        2 => add_fd(p, Desc::Udp { port: crate::network::udp_bind(), remote: None, nonblock }),
        _ => -EINVAL,
    }
}

fn sys_connect(p: &Process, fd: i64, ptr: u64, len: u64) -> i64 {
    let Some(d) = get_fd(p, fd) else { return -EBADF };
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
    let Some(data) = usermem::read_bytes(p.pml4, buf, len) else { return -EFAULT };
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
                if !usermem::write_bytes(p.pml4, buf, &data[..n]) {
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
    let name = usermem::read_u64(p.pml4, ptr)?;
    let namelen = usermem::read_u64(p.pml4, ptr + 8)? & 0xffff_ffff;
    let iov = usermem::read_u64(p.pml4, ptr + 16)?;
    let iovlen = usermem::read_u64(p.pml4, ptr + 24)?;
    Some((name, namelen, iovecs(p, iov, iovlen)?))
}

fn sys_sendmsg(p: &Process, fd: i64, ptr: u64) -> i64 {
    let Some((name, namelen, vecs)) = msghdr(p, ptr) else { return -EFAULT };
    let mut data = Vec::new();
    for (b, l) in vecs {
        match usermem::read_bytes(p.pml4, b, l) {
            Some(d) => data.extend_from_slice(&d),
            None => return -EFAULT,
        }
    }
    let Some(d) = get_fd(p, fd) else { return -EBADF };
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

fn sys_recvmsg(p: &Process, fd: i64, ptr: u64) -> i64 {
    let Some((name, _, vecs)) = msghdr(p, ptr) else { return -EFAULT };
    let Some(d) = get_fd(p, fd) else { return -EBADF };
    let total: u64 = vecs.iter().map(|v| v.1).sum();
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
        if !usermem::write_bytes(p.pml4, b, &data[off..off + k]) {
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
    usermem::write_u64(p.pml4, ptr + 40, 0);
    usermem::write_u32(p.pml4, ptr + 48, 0);
    off as i64
}

fn sys_sockname(p: &Process, fd: i64, ptr: u64, lenp: u64, peer: bool) -> i64 {
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
    const POLLIN: u16 = 1;
    const POLLOUT: u16 = 4;
    const POLLHUP: u16 = 0x10;
    const POLLNVAL: u16 = 0x20;
    let Some(d) = get_fd(p, fd) else { return POLLNVAL };
    let Some(g) = d.try_lock() else { return 0 };
    let (r, w) = match &*g {
        Desc::Console => (p.console.has_input(), true),
        Desc::Tcp { stream: Some(s), .. } => (s.readable(), s.writable()),
        Desc::Tcp { listener: Some(l), .. } => (l.pending(), false),
        Desc::Tcp { .. } => (false, false),
        Desc::Udp { port, .. } => (crate::network::udp_pending(*port), true),
        Desc::PipeRead(pp) => (!pp.buf.lock().is_empty() || pp.writers.load(Ordering::Relaxed) == 0, false),
        Desc::PipeWrite(_) => (false, true),
        _ => (true, true),
    };
    let mut rev = 0;
    if r && events & POLLIN != 0 {
        rev |= POLLIN;
    }
    if w && events & POLLOUT != 0 {
        rev |= POLLOUT;
    }
    if let Desc::PipeRead(pp) = &*g
        && pp.writers.load(Ordering::Relaxed) == 0
    {
        rev |= POLLHUP;
    }
    rev
}

fn sys_poll(p: &Process, ptr: u64, n: u64, timeout_ms: i64) -> i64 {
    let deadline = if timeout_ms < 0 { u64::MAX } else { crate::time::uptime_ms() + timeout_ms as u64 };
    loop {
        let Some(raw) = usermem::read_bytes(p.pml4, ptr, n * 8) else { return -EFAULT };
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
            if !usermem::write_bytes(p.pml4, ptr, &out) {
                return -EFAULT;
            }
            return count;
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
    let s = usermem::read_u64(p.pml4, ptr)? as i64;
    let ns = usermem::read_u64(p.pml4, ptr + 8)? as i64;
    Some((s.max(0) as u64) * 1000 + (ns.max(0) as u64) / 1_000_000)
}

fn sys_futex(p: &Process, addr: u64, op: u64, val: u64, tptr: u64) -> i64 {
    let cmd = op & 0x7f;
    match cmd {
        0 | 9 => {
            // FUTEX_WAIT(_BITSET)
            let Some(b) = usermem::read_bytes(p.pml4, addr, 4) else { return -EFAULT };
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
            FUTEX.lock().push(Waiter { pml4: p.pml4, addr, woken: woken.clone() });
            loop {
                if woken.load(Ordering::Acquire) {
                    return 0;
                }
                if crate::time::uptime_ms() >= deadline {
                    FUTEX.lock().retain(|w| !Arc::ptr_eq(&w.woken, &woken));
                    return -ETIMEDOUT;
                }
                sched::sleep_ms(1);
            }
        }
        1 | 10 => futex_wake(p.pml4, addr, val),
        3 | 4 => futex_wake(p.pml4, addr, u64::MAX),
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
    if flags & CLONE_VM == 0 {
        // No fork(): MayOS processes can't be copied (yet).
        return -ENOSYS;
    }
    let mut frame = f.clone();
    frame.rax = 0;
    if newsp != 0 {
        frame.rsp = newsp;
    }
    let fs = if flags & CLONE_SETTLS != 0 { tls } else { 0 };
    let tid = sched::spawn_user_frame(p.clone(), frame, fs);
    if flags & CLONE_PARENT_SETTID != 0 {
        usermem::write_u32(p.pml4, ptid, tid as u32);
    }
    if flags & CLONE_CHILD_SETTID != 0 {
        usermem::write_u32(p.pml4, ctid, tid as u32);
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
        usermem::write_u32(p.pml4, ctid, 0);
        futex_wake(p.pml4, ctid, 1);
    }
    if sched::process_thread_count(p.pid) <= 1 {
        super::process::exit_current_process(code, None);
    }
    sched::exit_current()
}

// --- time & misc --------------------------------------------------------

fn unix_ms() -> u64 {
    let t = crate::arch::rtc::now();
    let ts = fs::Timestamp { year: t.year, month: t.month, day: t.day, hour: t.hour, minute: t.minute, second: t.second };
    unix_time(&ts).max(0) as u64 * 1000 + crate::time::uptime_ms() % 1000
}

fn write_timespec(p: &Process, ptr: u64, us: u64) -> bool {
    usermem::write_u64(p.pml4, ptr, us / 1_000_000) && usermem::write_u64(p.pml4, ptr + 8, (us % 1_000_000) * 1000)
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
    if usermem::write_bytes(p.pml4, ptr, &b) { 0 } else { -EFAULT }
}

// -------------------------------------------------------------------------
// Dispatch
// -------------------------------------------------------------------------

/// Handle a Linux system call. Returns true if the thread must not resume.
pub fn syscall(p: &Arc<Process>, f: &mut TrapFrame) -> bool {
    let (a0, a1, a2, a3, a4, a5) = (f.rdi, f.rsi, f.rdx, f.r10, f.r8, f.r9);
    let pml4 = p.pml4;
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
        10 => 0, // mprotect
        11 => sys_munmap(p, a0, a1),
        12 => sys_brk(p, a0),
        13 | 14 | 131 => {
            // rt_sigaction / rt_sigprocmask / sigaltstack: signals are not
            // delivered, so report empty old state.
            let old = match f.rax {
                13 => a2,
                14 => a2,
                _ => a1,
            };
            if old != 0 {
                usermem::write_bytes(pml4, old, &[0u8; 24]);
            }
            0
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
            0x5421 => {
                // FIONBIO
                let on = usermem::read_bytes(pml4, a2, 4).map(|b| b != [0, 0, 0, 0]).unwrap_or(false);
                if let Some(d) = get_fd(p, a0 as i64) {
                    match &mut *d.lock() {
                        Desc::Tcp { nonblock, .. } | Desc::Udp { nonblock, .. } => *nonblock = on,
                        _ => {}
                    }
                }
                0
            }
            _ => -ENOTTY,
        },
        17 => sys_pread(p, a0 as i64, a1, a2, a3),
        18 => -ENOSYS, // pwrite64
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
        25 => -ENOMEM, // mremap: callers fall back to copying
        28 => 0,       // madvise
        32 => sys_dup(p, a0 as i64, None, 0),
        33 => sys_dup(p, a0 as i64, Some(a1 as i64), 0),
        35 => {
            if let Some(ms) = timespec_ms(p, a0) {
                sched::sleep_ms(ms.max(1));
            }
            0
        }
        39 => p.pid as i64,
        41 => sys_socket(p, a0, a1),
        42 => sys_connect(p, a0 as i64, a1, a2),
        43 => sys_accept(p, a0 as i64, a1, a2, 0),
        44 => sys_sendto(p, a0 as i64, a1, a2, a4, a5),
        45 => sys_recvfrom(p, a0 as i64, a1, a2, a4, a5),
        46 => sys_sendmsg(p, a0 as i64, a1),
        47 => sys_recvmsg(p, a0 as i64, a1),
        48 => 0, // shutdown
        49 => sys_bind(p, a0 as i64, a1, a2),
        50 => sys_listen(p, a0 as i64),
        51 => sys_sockname(p, a0 as i64, a1, a2, false),
        52 => sys_sockname(p, a0 as i64, a1, a2, true),
        54 => 0, // setsockopt
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
        57 | 58 => -ENOSYS, // fork, vfork
        59 => -ENOSYS,      // execve
        60 => thread_exit(p, a0 as i64),
        61 => -ECHILD, // wait4
        62 | 200 | 234 => {
            // kill / tkill / tgkill: fatal signals end the process.
            let sig = if f.rax == 234 { a2 } else { a1 };
            if matches!(sig, 6 | 9 | 11 | 15) {
                let msg = if sig == 6 { "Aborted\n" } else { "Killed\n" };
                super::process::exit_current_process(128 + sig as i64, Some(msg));
                return true;
            }
            0
        }
        63 => sys_uname(p, a0),
        72 => {
            // fcntl
            match a1 {
                0 | 1030 => sys_dup(p, a0 as i64, None, a2 as usize),
                1 | 2 => 0,
                3 => match get_fd(p, a0 as i64) {
                    Some(d) => match &*d.lock() {
                        Desc::Tcp { nonblock: true, .. } | Desc::Udp { nonblock: true, .. } => 2 | O_NONBLOCK as i64,
                        Desc::File { writable: true, .. } => 2,
                        _ => 2,
                    },
                    None => -EBADF,
                },
                4 => {
                    if let Some(d) = get_fd(p, a0 as i64) {
                        match &mut *d.lock() {
                            Desc::Tcp { nonblock, .. } | Desc::Udp { nonblock, .. } => *nonblock = a2 & O_NONBLOCK != 0,
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
                Ok(path) => fs::remove(&path).map(|_| 0).unwrap_or_else(fs_err),
                Err(e) => e,
            }
        }
        89 | 267 => {
            // readlink(at): only /proc/self/exe
            let (pp, buf, len) = if f.rax == 89 { (a0, a1, a2) } else { (a1, a2, a3) };
            let path = usermem::read_cstr(pml4, pp, 4096).unwrap_or_default();
            if path == "/proc/self/exe" {
                let exe = linux(p).map(|l| l.exe.clone()).unwrap_or_default();
                let n = exe.len().min(len as usize);
                if usermem::write_bytes(pml4, buf, &exe.as_bytes()[..n]) { n as i64 } else { -EFAULT }
            } else {
                -EINVAL
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
        110 => 1,
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
                sched::sleep_ms(ms.max(1));
            }
            0
        }
        231 => {
            super::process::exit_current_process(a0 as i64, None);
            return true;
        }
        257 => sys_openat(p, a0 as i32 as i64, a1, a2),
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
        292 => sys_dup(p, a0 as i64, Some(a1 as i64), 0),
        293 => sys_pipe(p, a0),
        318 => {
            // getrandom
            let mut b = vec![0u8; (a1 as usize).min(1 << 20)];
            fill_random(&mut b);
            if usermem::write_bytes(pml4, a0, &b) { b.len() as i64 } else { -EFAULT }
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
    linux(p).map(|l| l.exe.to_string()).unwrap_or_default()
}
