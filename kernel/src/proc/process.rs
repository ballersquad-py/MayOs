//! User processes: an address space, open files and a console.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use super::{elf, sched};
use crate::mem::paging::{self, NO_EXECUTE, USER, WRITABLE};
use crate::mem::{pmm, PAGE_SIZE};
use crate::sync::{Mutex, Spin};

pub const STACK_TOP: u64 = 0x0000_7fff_ffff_0000;
pub const STACK_PAGES: u64 = 32; // 128 KiB
pub const HEAP_LIMIT: u64 = 0x0000_4000_0000_0000;

/// A byte pipe between a process and whatever displays it (a terminal).
pub struct Console {
    inner: Spin<ConsoleInner>,
}

struct ConsoleInner {
    output: VecDeque<u8>,
    input: VecDeque<u8>,
    input_closed: bool,
}

impl Console {
    pub fn new() -> Arc<Console> {
        Arc::new(Console {
            inner: Spin::new(ConsoleInner { output: VecDeque::new(), input: VecDeque::new(), input_closed: false }),
        })
    }

    pub fn write(&self, data: &[u8]) {
        let mut c = self.inner.lock();
        c.output.extend(data.iter().copied());
        // Keep memory bounded if nobody is reading.
        while c.output.len() > 1024 * 1024 {
            c.output.pop_front();
        }
    }

    pub fn take_output(&self) -> Vec<u8> {
        let mut c = self.inner.lock();
        c.output.drain(..).collect()
    }

    pub fn send_input(&self, data: &[u8]) {
        self.inner.lock().input.extend(data.iter().copied());
    }

    pub fn close_input(&self) {
        self.inner.lock().input_closed = true;
    }

    /// Non-blocking read. `None` means end of input.
    pub fn try_read(&self, max: usize) -> Option<Vec<u8>> {
        let mut c = self.inner.lock();
        if c.input.is_empty() {
            return if c.input_closed { None } else { Some(Vec::new()) };
        }
        let n = max.min(c.input.len());
        Some(c.input.drain(..n).collect())
    }
}

pub struct OpenFile {
    pub path: String,
    pub data: Vec<u8>,
    pub pos: usize,
    pub writable: bool,
    pub dirty: bool,
}

pub struct Process {
    pub pid: u64,
    pub name: String,
    pub pml4: u64,
    pub cwd: String,
    #[allow(dead_code)]
    pub args: String,
    pub console: Arc<Console>,
    pub brk: Spin<(u64, u64)>,
    pub files: Mutex<Vec<Option<OpenFile>>>,
    pub exit_code: Spin<Option<i64>>,
}

impl Drop for Process {
    fn drop(&mut self) {
        paging::destroy_address_space(self.pml4);
    }
}

static NEXT_PID: AtomicU64 = AtomicU64::new(1);
static TABLE: Spin<Vec<Arc<Process>>> = Spin::new(Vec::new());

/// Load an executable from the file system and start it.
pub fn spawn(path: &str, args: &str, cwd: &str, console: Arc<Console>) -> Result<Arc<Process>, String> {
    let data = crate::fs::read_file(path).map_err(|e| alloc::format!("{}: {}", path, e))?;
    let pml4 = paging::new_address_space().ok_or("out of memory")?;
    let image = match elf::load(pml4, &data) {
        Ok(i) => i,
        Err(e) => {
            paging::destroy_address_space(pml4);
            return Err(alloc::format!("{}: {}", path, e));
        }
    };
    // Stack, with the argument string copied to its top.
    let stack_bottom = STACK_TOP - STACK_PAGES * PAGE_SIZE;
    let mut frames = Vec::new();
    for i in 0..STACK_PAGES {
        let Some(f) = pmm::alloc_frame_zeroed() else {
            paging::destroy_address_space(pml4);
            return Err("out of memory".into());
        };
        paging::map(pml4, stack_bottom + i * PAGE_SIZE, f, USER | WRITABLE | NO_EXECUTE).map_err(|_| "map failed")?;
        frames.push(f);
    }
    let args_bytes = args.as_bytes();
    let args_len = args_bytes.len().min(2048);
    let args_addr = STACK_TOP - 4096;
    let top_frame = *frames.last().unwrap();
    unsafe {
        let dst = crate::mem::phys_to_virt(top_frame) as *mut u8;
        core::ptr::copy_nonoverlapping(args_bytes.as_ptr(), dst, args_len);
    }
    let user_rsp = args_addr - 8; // entry sees rsp = 16n + 8, like after a call

    let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);
    let name = String::from(crate::fs::file_name(path));
    let proc = Arc::new(Process {
        pid,
        name,
        pml4,
        cwd: String::from(cwd),
        args: String::from(args),
        console,
        brk: Spin::new((image.brk, image.brk)),
        files: Mutex::new(Vec::new()),
        exit_code: Spin::new(None),
    });
    TABLE.lock().push(proc.clone());
    sched::spawn_user(proc.clone(), image.entry, user_rsp, args_addr, args_len as u64);
    Ok(proc)
}

/// Terminate the calling process (from a syscall or a fault).
pub fn exit_current_process(code: i64, message: Option<&str>) {
    if let Some(p) = sched::current_process() {
        if let Some(m) = message {
            p.console.write(m.as_bytes());
        }
        super::syscall::close_all(&p);
        finish(&p, code);
    }
}

fn finish(p: &Arc<Process>, code: i64) {
    {
        let mut e = p.exit_code.lock();
        if e.is_none() {
            *e = Some(code);
        }
    }
    sched::kill_process_threads(p.pid);
}

/// Forget an exited process once its parent has collected the exit code.
pub fn reap(pid: u64) {
    TABLE.lock().retain(|x| x.pid != pid);
}

/// Kill a process from outside (e.g. Ctrl+C in the terminal).
pub fn kill(p: &Arc<Process>) {
    p.console.write(b"^C\n");
    finish(p, -130);
}

pub fn find(pid: u64) -> Option<Arc<Process>> {
    TABLE.lock().iter().find(|p| p.pid == pid).cloned()
}

pub fn list() -> Vec<Arc<Process>> {
    TABLE.lock().clone()
}

impl Process {
    pub fn has_exited(&self) -> Option<i64> {
        *self.exit_code.lock()
    }

    /// Grow or shrink the heap. Returns the previous break.
    pub fn sbrk(&self, incr: i64) -> Option<u64> {
        let mut b = self.brk.lock();
        let old = b.1;
        let new = (old as i64).checked_add(incr)? as u64;
        if new < b.0 || new > HEAP_LIMIT {
            return None;
        }
        let old_top = old.div_ceil(PAGE_SIZE) * PAGE_SIZE;
        let new_top = new.div_ceil(PAGE_SIZE) * PAGE_SIZE;
        let mut page = old_top;
        while page < new_top {
            let f = pmm::alloc_frame_zeroed()?;
            if paging::map(self.pml4, page, f, USER | WRITABLE | NO_EXECUTE).is_err() {
                pmm::free_frame(f);
                return None;
            }
            page += PAGE_SIZE;
        }
        let mut page = new_top;
        while page < old_top {
            if let Some(f) = paging::unmap(self.pml4, page) {
                pmm::free_frame(f);
            }
            page += PAGE_SIZE;
        }
        b.1 = new;
        Some(old)
    }
}
