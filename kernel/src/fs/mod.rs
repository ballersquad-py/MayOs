//! Virtual file system front-end.
//!
//! Today there is one mounted volume: the FAT32 data disk at `/`. All paths
//! handed to this module are absolute; `normalize` resolves relative paths
//! against a working directory.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

pub use fat32::{DirEntry, FsError, FsStats, Timestamp};

use crate::drivers::ramdisk::RamDisk;
use crate::drivers::virtio_blk::VirtioBlk;
use crate::sync::Mutex;

/// The block devices a volume can live on.
pub enum Disk {
    Virtio(VirtioBlk),
    Ram(RamDisk),
}

impl fat32::BlockDevice for Disk {
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> core::result::Result<(), ()> {
        match self {
            Disk::Virtio(d) => d.read(lba, buf),
            Disk::Ram(d) => d.read(lba, buf),
        }
    }
    fn write(&mut self, lba: u64, buf: &[u8]) -> core::result::Result<(), ()> {
        match self {
            Disk::Virtio(d) => d.write(lba, buf),
            Disk::Ram(d) => d.write(lba, buf),
        }
    }
    fn flush(&mut self) -> core::result::Result<(), ()> {
        match self {
            Disk::Virtio(d) => d.flush(),
            Disk::Ram(d) => d.flush(),
        }
    }
}

type Volume = fat32::FatFs<Disk>;

static VOLUME: Mutex<Option<Volume>> = Mutex::new(None);
/// Bumped on every modification so views (e.g. the explorer) can refresh.
static GENERATION: AtomicU64 = AtomicU64::new(0);

pub type Result<T> = core::result::Result<T, FsError>;

fn clock() -> Timestamp {
    let t = crate::arch::rtc::now();
    Timestamp { year: t.year, month: t.month, day: t.day, hour: t.hour, minute: t.minute, second: t.second }
}

/// Human-readable description of where `/` lives.
pub fn backend() -> &'static str {
    let mut g = VOLUME.lock();
    match g.as_mut().map(|v| v.device()) {
        Some(Disk::Virtio(_)) => "virtio-blk (persistent)",
        Some(Disk::Ram(_)) => "RAM disk (changes are lost at power off)",
        None => "none",
    }
}

pub fn mount(dev: Disk) -> Result<()> {
    let mut v = fat32::FatFs::mount(dev)?;
    v.set_clock(clock);
    *VOLUME.lock() = Some(v);
    Ok(())
}

pub fn is_mounted() -> bool {
    VOLUME.lock().is_some()
}

pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

fn with<R>(f: impl FnOnce(&mut Volume) -> Result<R>) -> Result<R> {
    let mut g = VOLUME.lock();
    match g.as_mut() {
        Some(v) => f(v),
        None => Err(FsError::Io),
    }
}

fn modify<R>(f: impl FnOnce(&mut Volume) -> Result<R>) -> Result<R> {
    let r = with(f);
    GENERATION.fetch_add(1, Ordering::Relaxed);
    r
}

pub fn stat(path: &str) -> Result<DirEntry> {
    with(|v| v.stat(path))
}

pub fn exists(path: &str) -> bool {
    stat(path).is_ok()
}

pub fn is_dir(path: &str) -> bool {
    stat(path).map(|e| e.is_dir).unwrap_or(false)
}

/// Directory listing sorted with folders first, then by name.
pub fn read_dir(path: &str) -> Result<Vec<DirEntry>> {
    let mut entries = with(|v| v.read_dir(path))?;
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| cmp_names(&a.name, &b.name)));
    Ok(entries)
}

fn cmp_names(a: &str, b: &str) -> core::cmp::Ordering {
    let la = a.chars().flat_map(|c| c.to_lowercase());
    let lb = b.chars().flat_map(|c| c.to_lowercase());
    la.cmp(lb)
}

pub fn read_file(path: &str) -> Result<Vec<u8>> {
    with(|v| v.read_file(path))
}

pub fn write_file(path: &str, data: &[u8]) -> Result<()> {
    modify(|v| v.write_file(path, data))
}

pub fn create_file(path: &str) -> Result<()> {
    modify(|v| v.create_file(path))
}

pub fn create_dir(path: &str) -> Result<()> {
    modify(|v| v.create_dir(path))
}

pub fn remove(path: &str) -> Result<()> {
    modify(|v| v.remove(path))
}

pub fn remove_all(path: &str) -> Result<()> {
    modify(|v| v.remove_all(path))
}

pub fn rename(from: &str, to: &str) -> Result<()> {
    modify(|v| v.rename(from, to))
}

/// Copy a file or a whole directory tree.
pub fn copy(from: &str, to: &str) -> Result<()> {
    let src = stat(from)?;
    if exists(to) {
        return Err(FsError::AlreadyExists);
    }
    if src.is_dir {
        let f = normalize("/", from);
        let t = normalize("/", to);
        if t == f || t.starts_with(&format!("{}/", f.trim_end_matches('/'))) {
            return Err(FsError::InvalidPath);
        }
        create_dir(to)?;
        for child in read_dir(from)? {
            copy(&join(from, &child.name), &join(to, &child.name))?;
        }
        Ok(())
    } else {
        modify(|v| v.copy_file(from, to))
    }
}

pub fn stats() -> Result<FsStats> {
    with(|v| Ok(v.stats()))
}

pub fn volume_label() -> String {
    with(|v| Ok(v.volume_label().to_string())).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Resolve `path` against `cwd`, collapsing `.` and `..`.
pub fn normalize(cwd: &str, path: &str) -> String {
    let full = if path.starts_with('/') { path.to_string() } else { format!("{}/{}", cwd, path) };
    let mut parts: Vec<&str> = Vec::new();
    for p in full.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    let mut out = String::from("/");
    out.push_str(&parts.join("/"));
    out
}

pub fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') { format!("{}{}", dir, name) } else { format!("{}/{}", dir, name) }
}

pub fn parent(path: &str) -> String {
    let p = normalize("/", path);
    match p.rfind('/') {
        Some(0) | None => String::from("/"),
        Some(i) => p[..i].to_string(),
    }
}

pub fn file_name(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or("")
}

pub fn extension(name: &str) -> Option<String> {
    let i = name.rfind('.')?;
    if i == 0 {
        return None;
    }
    Some(name[i + 1..].to_ascii_lowercase())
}

/// Pick "name", "name 2", "name 3", ... that does not exist in `dir`.
pub fn unique_name(dir: &str, base: &str) -> String {
    if !exists(&join(dir, base)) {
        return base.to_string();
    }
    let (stem, ext) = match base.rfind('.') {
        Some(i) if i > 0 => (&base[..i], &base[i..]),
        _ => (base, ""),
    };
    for n in 2..10_000 {
        let candidate = format!("{} {}{}", stem, n, ext);
        if !exists(&join(dir, &candidate)) {
            return candidate;
        }
    }
    base.to_string()
}

pub fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{}.{} KB", bytes / 1024, (bytes % 1024) * 10 / 1024)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{}.{} MB", bytes / (1024 * 1024), (bytes % (1024 * 1024)) * 10 / (1024 * 1024))
    } else {
        format!("{}.{} GB", bytes >> 30, ((bytes & ((1 << 30) - 1)) * 10) >> 30)
    }
}

pub fn format_time(t: &Timestamp) -> String {
    format!("{:04}-{:02}-{:02} {:02}:{:02}", t.year, t.month, t.day, t.hour, t.minute)
}
