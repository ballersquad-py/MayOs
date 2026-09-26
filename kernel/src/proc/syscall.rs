//! System call dispatch. ABI: number in rax, arguments in rdi, rsi, rdx,
//! r10, r8, r9; result in rax (negative values are errors).

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;

use super::process::{self, OpenFile, Process};
use super::{sched, usermem};
use crate::arch::idt::TrapFrame;
use crate::fs::{self, FsError};

pub const SYS_EXIT: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_READ: u64 = 2;
pub const SYS_OPEN: u64 = 3;
pub const SYS_CLOSE: u64 = 4;
pub const SYS_SBRK: u64 = 5;
pub const SYS_SLEEP: u64 = 6;
pub const SYS_UPTIME: u64 = 7;
pub const SYS_GETPID: u64 = 8;
pub const SYS_READDIR: u64 = 9;
pub const SYS_MKDIR: u64 = 10;
pub const SYS_UNLINK: u64 = 11;
pub const SYS_RENAME: u64 = 12;
pub const SYS_YIELD: u64 = 13;
pub const SYS_TIME: u64 = 14;
pub const SYS_SEEK: u64 = 15;
pub const SYS_SPAWN: u64 = 16;
pub const SYS_WAIT: u64 = 17;

const EPERM: i64 = -1;
const ENOENT: i64 = -2;
const EIO: i64 = -5;
const EBADF: i64 = -9;
const ENOMEM: i64 = -12;
const EFAULT: i64 = -14;
const EEXIST: i64 = -17;
const ENOTDIR: i64 = -20;
const EISDIR: i64 = -21;
const EINVAL: i64 = -22;
const ENOSPC: i64 = -28;
const ENOSYS: i64 = -38;
const ENOTEMPTY: i64 = -39;

const O_READ: u64 = 1;
const O_WRITE: u64 = 2;
const O_CREATE: u64 = 4;
const O_TRUNC: u64 = 8;
const O_APPEND: u64 = 16;

fn errno(e: FsError) -> i64 {
    match e {
        FsError::NotFound => ENOENT,
        FsError::NotADirectory => ENOTDIR,
        FsError::IsADirectory => EISDIR,
        FsError::AlreadyExists => EEXIST,
        FsError::DirectoryNotEmpty => ENOTEMPTY,
        FsError::InvalidName | FsError::InvalidPath => EINVAL,
        FsError::NoSpace => ENOSPC,
        FsError::Io | FsError::Corrupt | FsError::Unsupported => EIO,
    }
}

/// Handle the system call in `f`. Returns true if the caller must not be
/// resumed (it exited).
pub fn handle(f: &mut TrapFrame) -> bool {
    let Some(p) = sched::current_process() else {
        f.rax = ENOSYS as u64;
        return false;
    };
    if p.linux.is_some() {
        return super::linux::syscall(&p, f);
    }
    let (a0, a1, a2, a3) = (f.rdi, f.rsi, f.rdx, f.r10);
    let ret: i64 = match f.rax {
        SYS_EXIT => {
            process::exit_current_process(a0 as i64, None);
            return true;
        }
        SYS_WRITE => sys_write(&p, a0, a1, a2),
        SYS_READ => sys_read(&p, a0, a1, a2),
        SYS_OPEN => sys_open(&p, a0, a1, a2),
        SYS_CLOSE => sys_close(&p, a0),
        SYS_SBRK => p.sbrk(a0 as i64).map(|v| v as i64).unwrap_or(ENOMEM),
        SYS_SLEEP => {
            sched::sleep_ms(a0.min(3_600_000));
            0
        }
        SYS_UPTIME => crate::time::uptime_ms() as i64,
        SYS_GETPID => p.pid as i64,
        SYS_READDIR => sys_readdir(&p, a0, a1, a2, a3),
        SYS_MKDIR => with_path(&p, a0, a1, |path| fs::create_dir(path).map(|_| 0).unwrap_or_else(errno)),
        SYS_UNLINK => with_path(&p, a0, a1, |path| fs::remove(path).map(|_| 0).unwrap_or_else(errno)),
        SYS_RENAME => match (path_arg(&p, a0, a1), path_arg(&p, a2, a3)) {
            (Ok(from), Ok(to)) => fs::rename(&from, &to).map(|_| 0).unwrap_or_else(errno),
            _ => EFAULT,
        },
        SYS_YIELD => {
            sched::yield_now();
            0
        }
        SYS_TIME => {
            let t = crate::arch::rtc::now();
            ((t.year as i64) << 40)
                | ((t.month as i64) << 32)
                | ((t.day as i64) << 24)
                | ((t.hour as i64) << 16)
                | ((t.minute as i64) << 8)
                | t.second as i64
        }
        SYS_SEEK => sys_seek(&p, a0, a1 as i64, a2),
        SYS_SPAWN => sys_spawn(&p, a0, a1, a2, a3),
        SYS_WAIT => sys_wait(a0),
        _ => ENOSYS,
    };
    f.rax = ret as u64;
    false
}

fn path_arg(p: &Process, ptr: u64, len: u64) -> Result<String, i64> {
    let s = usermem::read_str(p.pml4(), ptr, len).ok_or(EFAULT)?;
    Ok(fs::normalize(&p.cwd, &s))
}

fn with_path(p: &Process, ptr: u64, len: u64, f: impl FnOnce(&str) -> i64) -> i64 {
    match path_arg(p, ptr, len) {
        Ok(path) => f(&path),
        Err(e) => e,
    }
}

fn sys_write(p: &Process, fd: u64, ptr: u64, len: u64) -> i64 {
    let Some(data) = usermem::read_bytes(p.pml4(), ptr, len) else { return EFAULT };
    if fd == 1 || fd == 2 {
        p.console.write(&data);
        return len as i64;
    }
    let mut files = p.files.lock();
    let Some(Some(file)) = files.get_mut(fd.wrapping_sub(3) as usize) else { return EBADF };
    if !file.writable {
        return EBADF;
    }
    let end = file.pos + data.len();
    if end > file.data.len() {
        file.data.resize(end, 0);
    }
    file.data[file.pos..end].copy_from_slice(&data);
    file.pos = end;
    file.dirty = true;
    len as i64
}

fn sys_read(p: &Process, fd: u64, ptr: u64, len: u64) -> i64 {
    if fd == 0 {
        // Block until input arrives (or the terminal closes our input).
        loop {
            match p.console.try_read(len as usize) {
                None => return 0,
                Some(v) if !v.is_empty() => {
                    return if usermem::write_bytes(p.pml4(), ptr, &v) { v.len() as i64 } else { EFAULT };
                }
                Some(_) => sched::sleep_ms(10),
            }
        }
    }
    let mut files = p.files.lock();
    let Some(Some(file)) = files.get_mut(fd.wrapping_sub(3) as usize) else { return EBADF };
    let n = (len as usize).min(file.data.len().saturating_sub(file.pos));
    let chunk = &file.data[file.pos..file.pos + n];
    if !usermem::write_bytes(p.pml4(), ptr, chunk) {
        return EFAULT;
    }
    file.pos += n;
    n as i64
}

fn sys_open(p: &Process, ptr: u64, len: u64, flags: u64) -> i64 {
    let path = match path_arg(p, ptr, len) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let writable = flags & (O_WRITE | O_APPEND) != 0;
    let data = match fs::read_file(&path) {
        Ok(d) => {
            if flags & O_TRUNC != 0 && writable {
                alloc::vec::Vec::new()
            } else {
                d
            }
        }
        Err(FsError::NotFound) if flags & O_CREATE != 0 && writable => {
            if let Err(e) = fs::create_file(&path) {
                return errno(e);
            }
            alloc::vec::Vec::new()
        }
        Err(e) => return errno(e),
    };
    if flags & (O_READ | O_WRITE | O_APPEND) == 0 {
        return EINVAL;
    }
    let pos = if flags & O_APPEND != 0 { data.len() } else { 0 };
    let dirty = flags & O_TRUNC != 0 && writable;
    let file = OpenFile { path, data, pos, writable, dirty };
    let mut files = p.files.lock();
    let slot = match files.iter().position(|f| f.is_none()) {
        Some(i) => {
            files[i] = Some(file);
            i
        }
        None => {
            if files.len() >= 64 {
                return EPERM;
            }
            files.push(Some(file));
            files.len() - 1
        }
    };
    slot as i64 + 3
}

fn flush(file: &OpenFile) -> i64 {
    if file.dirty {
        return fs::write_file(&file.path, &file.data).map(|_| 0).unwrap_or_else(errno);
    }
    0
}

fn sys_close(p: &Process, fd: u64) -> i64 {
    let mut files = p.files.lock();
    let Some(slot) = files.get_mut(fd.wrapping_sub(3) as usize) else { return EBADF };
    match slot.take() {
        Some(file) => flush(&file),
        None => EBADF,
    }
}

/// Flush files a process left open when it exited.
pub fn close_all(p: &Process) {
    if let Some(mut files) = p.files.try_lock() {
        for f in files.iter_mut().filter_map(|f| f.take()) {
            flush(&f);
        }
    }
}

fn sys_seek(p: &Process, fd: u64, off: i64, whence: u64) -> i64 {
    let mut files = p.files.lock();
    let Some(Some(file)) = files.get_mut(fd.wrapping_sub(3) as usize) else { return EBADF };
    let base = match whence {
        0 => 0,
        1 => file.pos as i64,
        2 => file.data.len() as i64,
        _ => return EINVAL,
    };
    let new = base + off;
    if new < 0 {
        return EINVAL;
    }
    file.pos = new as usize;
    new
}

fn sys_readdir(p: &Process, pptr: u64, plen: u64, buf: u64, buflen: u64) -> i64 {
    let path = match path_arg(p, pptr, plen) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let entries = match fs::read_dir(&path) {
        Ok(e) => e,
        Err(e) => return errno(e),
    };
    let mut out = String::new();
    for e in entries {
        let line = format!("{}\t{}\t{}\n", e.name, if e.is_dir { 'D' } else { 'F' }, e.size);
        if out.len() + line.len() > buflen as usize {
            break;
        }
        out.push_str(&line);
    }
    if usermem::write_bytes(p.pml4(), buf, out.as_bytes()) { out.len() as i64 } else { EFAULT }
}

fn sys_spawn(p: &Arc<Process>, pptr: u64, plen: u64, aptr: u64, alen: u64) -> i64 {
    let path = match path_arg(p, pptr, plen) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let Some(args) = usermem::read_str(p.pml4(), aptr, alen) else { return EFAULT };
    match process::spawn(&path, &args, &p.cwd, p.console.clone()) {
        Ok(child) => child.pid as i64,
        Err(msg) => {
            p.console.write(format!("{}\n", msg).as_bytes());
            ENOENT
        }
    }
}

fn sys_wait(pid: u64) -> i64 {
    let Some(child) = process::find(pid) else { return ENOENT };
    loop {
        if let Some(code) = child.has_exited() {
            process::reap(pid);
            return code;
        }
        sched::sleep_ms(10);
    }
}
