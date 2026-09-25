//! Files and directories.

use alloc::string::String;
use alloc::vec::Vec;

use crate::sys;

pub const READ: u64 = 1;
pub const WRITE: u64 = 2;
pub const CREATE: u64 = 4;
pub const TRUNCATE: u64 = 8;
pub const APPEND: u64 = 16;

pub type Result<T> = core::result::Result<T, i64>;

fn check(r: i64) -> Result<i64> {
    if r < 0 { Err(r) } else { Ok(r) }
}

pub struct File {
    fd: u64,
}

impl File {
    pub fn open(path: &str, flags: u64) -> Result<File> {
        let fd = check(sys::syscall(sys::OPEN, path.as_ptr() as u64, path.len() as u64, flags))?;
        Ok(File { fd: fd as u64 })
    }

    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        check(sys::syscall(sys::READ, self.fd, buf.as_mut_ptr() as u64, buf.len() as u64)).map(|n| n as usize)
    }

    pub fn write(&mut self, data: &[u8]) -> Result<usize> {
        check(sys::syscall(sys::WRITE, self.fd, data.as_ptr() as u64, data.len() as u64)).map(|n| n as usize)
    }

    pub fn seek(&mut self, offset: i64, whence: u64) -> Result<u64> {
        check(sys::syscall(sys::SEEK, self.fd, offset as u64, whence)).map(|n| n as u64)
    }

    /// Close and flush; returns any error from writing the file back.
    pub fn close(mut self) -> Result<()> {
        let fd = core::mem::replace(&mut self.fd, u64::MAX);
        check(sys::syscall(sys::CLOSE, fd, 0, 0)).map(|_| ())
    }
}

impl Drop for File {
    fn drop(&mut self) {
        if self.fd != u64::MAX {
            sys::syscall(sys::CLOSE, self.fd, 0, 0);
        }
    }
}

pub fn read(path: &str) -> Result<Vec<u8>> {
    let mut f = File::open(path, READ)?;
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

pub fn read_to_string(path: &str) -> Result<String> {
    read(path).map(|d| String::from_utf8_lossy(&d).into_owned())
}

pub fn write(path: &str, data: &[u8]) -> Result<()> {
    let mut f = File::open(path, WRITE | CREATE | TRUNCATE)?;
    f.write(data)?;
    f.close()
}

pub fn append(path: &str, data: &[u8]) -> Result<()> {
    let mut f = File::open(path, WRITE | CREATE | APPEND)?;
    f.write(data)?;
    f.close()
}

pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

pub fn read_dir(path: &str) -> Result<Vec<DirEntry>> {
    let mut buf = alloc::vec![0u8; 64 * 1024];
    let n = check(unsafe {
        sys::syscall4(sys::READDIR, path.as_ptr() as u64, path.len() as u64, buf.as_mut_ptr() as u64, buf.len() as u64)
    })? as usize;
    let text = String::from_utf8_lossy(&buf[..n]).into_owned();
    Ok(text
        .lines()
        .filter_map(|l| {
            let mut parts = l.split('\t');
            let name = parts.next()?;
            let kind = parts.next()?;
            let size = parts.next()?.parse().unwrap_or(0);
            Some(DirEntry { name: String::from(name), is_dir: kind == "D", size })
        })
        .collect())
}

pub fn create_dir(path: &str) -> Result<()> {
    check(sys::syscall(sys::MKDIR, path.as_ptr() as u64, path.len() as u64, 0)).map(|_| ())
}

pub fn remove(path: &str) -> Result<()> {
    check(sys::syscall(sys::UNLINK, path.as_ptr() as u64, path.len() as u64, 0)).map(|_| ())
}

pub fn rename(from: &str, to: &str) -> Result<()> {
    check(unsafe {
        sys::syscall4(sys::RENAME, from.as_ptr() as u64, from.len() as u64, to.as_ptr() as u64, to.len() as u64)
    })
    .map(|_| ())
}
