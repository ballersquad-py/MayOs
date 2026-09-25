//! Virtual file system front-end.
//!
//! Volumes are FAT32 file systems on block devices. The main volume is
//! mounted at `/`; extra disks appear as top-level folders (`/disk1`, ...).
//! All paths handed to this module are absolute; `normalize` resolves
//! relative paths against a working directory.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

pub use fat32::{DirEntry, FsError, FsStats, Timestamp};

use crate::drivers::ahci::AhciDisk;
use crate::drivers::ide::IdeDisk;
use crate::drivers::ramdisk::RamDisk;
use crate::drivers::virtio_blk::VirtioBlk;
use crate::sync::Mutex;

/// The block devices a volume can live on.
pub enum Disk {
    Virtio(VirtioBlk),
    Ram(RamDisk),
    Ahci(AhciDisk),
    Ide(IdeDisk),
}

impl Disk {
    pub fn persistent(&self) -> bool {
        !matches!(self, Disk::Ram(_))
    }

    pub fn describe(&self) -> &'static str {
        match self {
            Disk::Virtio(_) => "virtio disk",
            Disk::Ram(_) => "RAM disk (changes are lost at power off)",
            Disk::Ahci(_) => "SATA disk",
            Disk::Ide(_) => "IDE disk",
        }
    }

    pub fn sectors(&self) -> u64 {
        match self {
            Disk::Virtio(d) => d.capacity,
            Disk::Ram(d) => d.sectors,
            Disk::Ahci(d) => d.sectors,
            Disk::Ide(d) => d.sectors,
        }
    }
}

impl fat32::BlockDevice for Disk {
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> core::result::Result<(), ()> {
        match self {
            Disk::Virtio(d) => d.read(lba, buf),
            Disk::Ram(d) => d.read(lba, buf),
            Disk::Ahci(d) => d.read(lba, buf),
            Disk::Ide(d) => d.read(lba, buf),
        }
    }
    fn write(&mut self, lba: u64, buf: &[u8]) -> core::result::Result<(), ()> {
        match self {
            Disk::Virtio(d) => d.write(lba, buf),
            Disk::Ram(d) => d.write(lba, buf),
            Disk::Ahci(d) => d.write(lba, buf),
            Disk::Ide(d) => d.write(lba, buf),
        }
    }
    fn flush(&mut self) -> core::result::Result<(), ()> {
        match self {
            Disk::Virtio(d) => d.flush(),
            Disk::Ram(d) => d.flush(),
            Disk::Ahci(d) => d.flush(),
            Disk::Ide(d) => d.flush(),
        }
    }
}

pub type Volume = fat32::FatFs<Disk>;

struct Mount {
    /// "/" for the main volume, otherwise "/name".
    point: String,
    vol: Volume,
}

static MOUNTS: Mutex<Vec<Mount>> = Mutex::new(Vec::new());
/// Bumped on every modification so views (e.g. the explorer) can refresh.
static GENERATION: AtomicU64 = AtomicU64::new(0);

pub type Result<T> = core::result::Result<T, FsError>;

fn clock() -> Timestamp {
    let t = crate::arch::rtc::now();
    Timestamp { year: t.year, month: t.month, day: t.day, hour: t.hour, minute: t.minute, second: t.second }
}

pub fn open_volume(dev: Disk) -> core::result::Result<Volume, (FsError, Disk)> {
    match fat32::FatFs::try_mount(dev) {
        Ok(mut v) => {
            v.set_clock(clock);
            Ok(v)
        }
        Err(e) => Err(e),
    }
}

/// Mount `vol` at `point` ("/" replaces the main volume).
pub fn mount_volume(point: &str, vol: Volume) {
    let mut m = MOUNTS.lock();
    m.retain(|x| x.point != point);
    m.push(Mount { point: String::from(point), vol });
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

pub fn mount(dev: Disk) -> Result<()> {
    match open_volume(dev) {
        Ok(v) => {
            mount_volume("/", v);
            Ok(())
        }
        Err((e, _)) => Err(e),
    }
}

/// Remove a mount and give its volume back (used when promoting a disk
/// to be the main volume).
pub fn take_mount(point: &str) -> Option<Volume> {
    let mut m = MOUNTS.lock();
    let i = m.iter().position(|x| x.point == point)?;
    GENERATION.fetch_add(1, Ordering::Relaxed);
    Some(m.remove(i).vol)
}

pub struct MountInfo {
    pub point: String,
    pub label: String,
    pub backend: &'static str,
    pub persistent: bool,
    pub stats: FsStats,
}

pub fn mounts() -> Vec<MountInfo> {
    let mut m = MOUNTS.lock();
    let mut out: Vec<MountInfo> = m
        .iter_mut()
        .map(|x| {
            let stats = x.vol.stats();
            let label = x.vol.volume_label().to_string();
            let dev = x.vol.device();
            MountInfo { point: x.point.clone(), label, backend: dev.describe(), persistent: dev.persistent(), stats }
        })
        .collect();
    out.sort_by(|a, b| a.point.cmp(&b.point));
    out
}

/// Human-readable description of where `/` lives.
pub fn backend() -> &'static str {
    mounts().into_iter().find(|m| m.point == "/").map(|m| m.backend).unwrap_or("none")
}

pub fn root_is_persistent() -> bool {
    mounts().into_iter().find(|m| m.point == "/").map(|m| m.persistent).unwrap_or(false)
}

pub fn is_mounted() -> bool {
    MOUNTS.lock().iter().any(|m| m.point == "/")
}

pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

/// Find the volume for `path` and the path inside it.
fn with<R>(path: &str, f: impl FnOnce(&mut Volume, &str) -> Result<R>) -> Result<R> {
    let path = normalize("/", path);
    let mut g = MOUNTS.lock();
    let mut best: Option<(usize, usize)> = None;
    for (i, m) in g.iter().enumerate() {
        let p = m.point.as_str();
        let matches = p == "/" || path == p || path.starts_with(&format!("{}/", p));
        if matches && best.map(|(_, len)| p.len() > len).unwrap_or(true) {
            best = Some((i, p.len()));
        }
    }
    let Some((i, len)) = best else { return Err(FsError::Io) };
    let inner = if len <= 1 { path.clone() } else { String::from(&path[len..]) };
    let inner = if inner.is_empty() { String::from("/") } else { inner };
    f(&mut g[i].vol, &inner)
}

fn modify<R>(path: &str, f: impl FnOnce(&mut Volume, &str) -> Result<R>) -> Result<R> {
    let r = with(path, f);
    GENERATION.fetch_add(1, Ordering::Relaxed);
    r
}

/// Mount points directly under `dir` (only "/" has any).
fn mount_points_in(dir: &str) -> Vec<String> {
    if normalize("/", dir) != "/" {
        return Vec::new();
    }
    MOUNTS.lock().iter().filter(|m| m.point != "/").map(|m| String::from(&m.point[1..])).collect()
}

pub fn is_mount_point(path: &str) -> bool {
    let p = normalize("/", path);
    p != "/" && MOUNTS.lock().iter().any(|m| m.point == p)
}

pub fn stat(path: &str) -> Result<DirEntry> {
    let p = normalize("/", path);
    if is_mount_point(&p) {
        return Ok(DirEntry::virtual_dir(file_name(&p)));
    }
    with(&p, |v, inner| v.stat(inner))
}

pub fn exists(path: &str) -> bool {
    stat(path).is_ok()
}

pub fn is_dir(path: &str) -> bool {
    stat(path).map(|e| e.is_dir).unwrap_or(false)
}

/// Directory listing sorted with folders first, then by name.
pub fn read_dir(path: &str) -> Result<Vec<DirEntry>> {
    let mut entries = with(path, |v, inner| v.read_dir(inner))?;
    for m in mount_points_in(path) {
        entries.retain(|e| !e.name.eq_ignore_ascii_case(&m));
        entries.push(DirEntry::virtual_dir(&m));
    }
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| cmp_names(&a.name, &b.name)));
    Ok(entries)
}

fn cmp_names(a: &str, b: &str) -> core::cmp::Ordering {
    let la = a.chars().flat_map(|c| c.to_lowercase());
    let lb = b.chars().flat_map(|c| c.to_lowercase());
    la.cmp(lb)
}

pub fn read_file(path: &str) -> Result<Vec<u8>> {
    with(path, |v, inner| v.read_file(inner))
}

pub fn write_file(path: &str, data: &[u8]) -> Result<()> {
    modify(path, |v, inner| v.write_file(inner, data))
}

pub fn create_file(path: &str) -> Result<()> {
    modify(path, |v, inner| v.create_file(inner))
}

pub fn create_dir(path: &str) -> Result<()> {
    if is_mount_point(path) {
        return Err(FsError::AlreadyExists);
    }
    modify(path, |v, inner| v.create_dir(inner))
}

pub fn remove(path: &str) -> Result<()> {
    if is_mount_point(path) {
        return Err(FsError::InvalidPath);
    }
    modify(path, |v, inner| v.remove(inner))
}

pub fn remove_all(path: &str) -> Result<()> {
    if is_mount_point(path) || normalize("/", path) == "/" {
        return Err(FsError::InvalidPath);
    }
    modify(path, |v, inner| v.remove_all(inner))
}

/// Which mount a path belongs to.
fn mount_of(path: &str) -> String {
    let p = normalize("/", path);
    let m = MOUNTS.lock();
    m.iter()
        .filter(|x| x.point == "/" || p == x.point || p.starts_with(&format!("{}/", x.point)))
        .max_by_key(|x| x.point.len())
        .map(|x| x.point.clone())
        .unwrap_or_else(|| String::from("/"))
}

pub fn rename(from: &str, to: &str) -> Result<()> {
    if is_mount_point(from) {
        return Err(FsError::InvalidPath);
    }
    if mount_of(from) != mount_of(to) {
        // Different disks: copy, then delete the original.
        copy(from, to)?;
        return if is_dir(from) { remove_all(from) } else { remove(from) };
    }
    let to_inner = {
        let point = mount_of(to);
        let p = normalize("/", to);
        if point == "/" { p } else { String::from(&p[point.len()..]) }
    };
    modify(from, |v, inner| v.rename(inner, &to_inner))
}

/// Copy a file or a whole directory tree (also between disks).
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
            if is_mount_point(&join(from, &child.name)) {
                continue;
            }
            copy(&join(from, &child.name), &join(to, &child.name))?;
        }
        Ok(())
    } else {
        let data = read_file(from)?;
        write_file(to, &data)
    }
}

pub fn stats() -> Result<FsStats> {
    with("/", |v, _| Ok(v.stats()))
}

pub fn volume_label() -> String {
    with("/", |v, _| Ok(v.volume_label().to_string())).unwrap_or_default()
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
