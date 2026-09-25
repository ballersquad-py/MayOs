//! A FAT32 file system driver with read and write support, including long
//! file names (VFAT).
//!
//! The crate is `no_std` + `alloc` so the kernel can use it directly, and it
//! is tested on the host against images made by `mkfs.fat` and checked with
//! `fsck.fat` (see `tests/`).
//!
//! Consistency strategy: every public mutating call finishes by flushing
//! cached FAT sectors (to every FAT copy) and the FSInfo sector, so the disk
//! is consistent whenever a call returns. File data is written before the
//! directory entry that points at it, and old clusters are freed last.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

pub const SECTOR: usize = 512;

const ATTR_READ_ONLY: u8 = 0x01;
const ATTR_HIDDEN: u8 = 0x02;
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_ARCHIVE: u8 = 0x20;
const ATTR_LFN: u8 = 0x0f;

const FAT_EOC: u32 = 0x0fff_ffff;
const FAT_BAD: u32 = 0x0fff_fff7;
const FAT_MASK: u32 = 0x0fff_ffff;

const CACHE_ENTRIES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsError {
    NotFound,
    NotADirectory,
    IsADirectory,
    AlreadyExists,
    DirectoryNotEmpty,
    InvalidName,
    InvalidPath,
    NoSpace,
    Io,
    Corrupt,
    Unsupported,
}

impl FsError {
    pub fn as_str(&self) -> &'static str {
        match self {
            FsError::NotFound => "no such file or directory",
            FsError::NotADirectory => "not a directory",
            FsError::IsADirectory => "is a directory",
            FsError::AlreadyExists => "already exists",
            FsError::DirectoryNotEmpty => "directory not empty",
            FsError::InvalidName => "invalid file name",
            FsError::InvalidPath => "invalid path",
            FsError::NoSpace => "no space left on device",
            FsError::Io => "I/O error",
            FsError::Corrupt => "file system is corrupt",
            FsError::Unsupported => "unsupported file system",
        }
    }
}

impl core::fmt::Display for FsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

pub type Result<T> = core::result::Result<T, FsError>;

/// A disk made of 512-byte sectors. `buf.len()` is always a multiple of 512.
pub trait BlockDevice {
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> core::result::Result<(), ()>;
    fn write(&mut self, lba: u64, buf: &[u8]) -> core::result::Result<(), ()>;
    fn flush(&mut self) -> core::result::Result<(), ()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Timestamp {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl Timestamp {
    fn from_fat(date: u16, time: u16) -> Timestamp {
        Timestamp {
            year: 1980 + (date >> 9),
            month: ((date >> 5) & 0xf) as u8,
            day: (date & 0x1f) as u8,
            hour: (time >> 11) as u8,
            minute: ((time >> 5) & 0x3f) as u8,
            second: ((time & 0x1f) * 2) as u8,
        }
    }

    fn to_fat(self) -> (u16, u16) {
        let year = self.year.clamp(1980, 2107);
        let date = ((year - 1980) << 9) | ((self.month.clamp(1, 12) as u16) << 5) | self.day.clamp(1, 31) as u16;
        let time = ((self.hour as u16) << 11) | ((self.minute as u16) << 5) | (self.second as u16 / 2);
        (date, time)
    }
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u32,
    pub first_cluster: u32,
    pub attr: u8,
    pub created: Timestamp,
    pub modified: Timestamp,
    /// Slot index of the first entry belonging to this file (an LFN entry,
    /// or the short entry itself when there is no long name).
    first_slot: usize,
    /// Slot index of the short (8.3) entry.
    sfn_slot: usize,
}

impl DirEntry {
    /// A directory entry that does not live on a volume (used by the VFS
    /// to show mount points).
    pub fn virtual_dir(name: &str) -> DirEntry {
        DirEntry {
            name: String::from(name),
            is_dir: true,
            size: 0,
            first_cluster: 0,
            attr: ATTR_DIRECTORY,
            created: Timestamp::default(),
            modified: Timestamp::default(),
            first_slot: 0,
            sfn_slot: 0,
        }
    }

    pub fn is_hidden(&self) -> bool {
        self.attr & ATTR_HIDDEN != 0
    }

    pub fn is_read_only(&self) -> bool {
        self.attr & ATTR_READ_ONLY != 0
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FsStats {
    pub cluster_size: u32,
    pub total_clusters: u32,
    pub free_clusters: u32,
}

impl FsStats {
    pub fn total_bytes(&self) -> u64 {
        self.total_clusters as u64 * self.cluster_size as u64
    }
    pub fn free_bytes(&self) -> u64 {
        self.free_clusters as u64 * self.cluster_size as u64
    }
}

struct CachedSector {
    lba: u64,
    data: Box<[u8; SECTOR]>,
    dirty: bool,
    stamp: u64,
}

/// A directory loaded into memory, with the clusters that hold it.
struct DirBuf {
    clusters: Vec<u32>,
    data: Vec<u8>,
}

impl DirBuf {
    fn slots(&self) -> usize {
        self.data.len() / 32
    }
    fn slot(&self, i: usize) -> &[u8] {
        &self.data[i * 32..i * 32 + 32]
    }
    fn slot_mut(&mut self, i: usize) -> &mut [u8] {
        &mut self.data[i * 32..i * 32 + 32]
    }
}

pub struct FatFs<D: BlockDevice> {
    dev: D,
    part_lba: u64,
    sectors_per_cluster: u32,
    reserved_sectors: u32,
    num_fats: u32,
    fat_size: u32,
    root_cluster: u32,
    fsinfo_sector: u32,
    data_start: u32,
    total_clusters: u32,
    free_clusters: u32,
    next_free: u32,
    volume_label: String,
    cache: Vec<CachedSector>,
    stamp: u64,
    clock: Option<fn() -> Timestamp>,
}

fn rd16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn rd32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn wr16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
fn wr32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

fn looks_like_fat32(bs: &[u8]) -> bool {
    rd16(bs, 510) == 0xaa55
        && rd16(bs, 11) == 512
        && bs[13] != 0
        && bs[13].is_power_of_two()
        && rd16(bs, 17) == 0 // root entry count is 0 on FAT32
        && rd16(bs, 22) == 0 // 16-bit FAT size is 0 on FAT32
        && rd32(bs, 36) != 0
}

/// Split a path into components, rejecting empty or relative-dot components.
pub fn split_path(path: &str) -> Result<Vec<&str>> {
    let mut out = Vec::new();
    for c in path.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                if out.pop().is_none() {
                    return Err(FsError::InvalidPath);
                }
            }
            c => out.push(c),
        }
    }
    Ok(out)
}

fn valid_long_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.encode_utf16().count() <= 255
        && !name.ends_with(' ')
        && !name.ends_with('.')
        && !name.chars().any(|c| (c as u32) < 0x20 || "\\/:*?\"<>|".contains(c))
}

fn sfn_char_ok(c: u8) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit() || b"!#$%&'()-@^_`{}~".contains(&c)
}

/// If `name` is already a valid, upper-case 8.3 name, return its 11-byte form.
fn exact_short_name(name: &str) -> Option<[u8; 11]> {
    let (base, ext) = match name.rfind('.') {
        Some(i) => (&name[..i], &name[i + 1..]),
        None => (name, ""),
    };
    if base.is_empty() || base.len() > 8 || ext.len() > 3 || base.contains('.') {
        return None;
    }
    if !base.bytes().chain(ext.bytes()).all(sfn_char_ok) {
        return None;
    }
    let mut out = [b' '; 11];
    out[..base.len()].copy_from_slice(base.as_bytes());
    out[8..8 + ext.len()].copy_from_slice(ext.as_bytes());
    Some(out)
}

fn short_name_to_string(sfn: &[u8; 11], ntres: u8) -> String {
    let mut s = String::new();
    let base_lower = ntres & 0x08 != 0;
    let ext_lower = ntres & 0x10 != 0;
    for (i, &c) in sfn[..8].iter().enumerate() {
        if c == b' ' {
            break;
        }
        let c = if i == 0 && c == 0x05 { 0xe5 } else { c };
        let c = if base_lower { c.to_ascii_lowercase() } else { c };
        s.push(c as char);
    }
    if sfn[8] != b' ' {
        s.push('.');
        for &c in sfn[8..].iter() {
            if c == b' ' {
                break;
            }
            s.push(if ext_lower { c.to_ascii_lowercase() } else { c } as char);
        }
    }
    s
}

fn lfn_checksum(sfn: &[u8; 11]) -> u8 {
    let mut sum: u8 = 0;
    for &b in sfn {
        sum = (if sum & 1 != 0 { 0x80u8 } else { 0 }).wrapping_add(sum >> 1).wrapping_add(b);
    }
    sum
}

fn names_equal(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.chars().zip(b.chars()).all(|(x, y)| x == y || x.to_lowercase().eq(y.to_lowercase()))
}

const LFN_OFFSETS: [usize; 13] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];

/// Find the first FAT32 partition in an MBR or GPT partition table.
fn find_fat32_partition<D: BlockDevice>(dev: &mut D, mbr: &[u8]) -> Result<Option<u64>> {
    if rd16(mbr, 510) != 0xaa55 {
        return Ok(None);
    }
    let mut sector = [0u8; SECTOR];
    for i in 0..4 {
        let e = 446 + i * 16;
        let kind = mbr[e + 4];
        let start = rd32(mbr, e + 8) as u64;
        match kind {
            // FAT32 (CHS / LBA), and types Windows sometimes leaves on FAT32.
            0x0b | 0x0c | 0x06 | 0x0e | 0x07 => {
                dev.read(start, &mut sector).map_err(|_| FsError::Io)?;
                if looks_like_fat32(&sector) {
                    return Ok(Some(start));
                }
            }
            0xee => {
                // GPT: header at LBA 1, then the entry array.
                dev.read(1, &mut sector).map_err(|_| FsError::Io)?;
                if &sector[..8] != b"EFI PART" {
                    return Ok(None);
                }
                let entries_lba = u64::from_le_bytes(sector[72..80].try_into().unwrap());
                let count = rd32(&sector, 80).min(256) as u64;
                let size = rd32(&sector, 84) as u64;
                if size < 128 || size > 512 {
                    return Ok(None);
                }
                let per_sector = SECTOR as u64 / size;
                let mut buf = [0u8; SECTOR];
                for n in 0..count {
                    if n % per_sector == 0 {
                        dev.read(entries_lba + n / per_sector, &mut buf).map_err(|_| FsError::Io)?;
                    }
                    let o = ((n % per_sector) * size) as usize;
                    if buf[o..o + 16].iter().all(|&b| b == 0) {
                        continue;
                    }
                    let start = u64::from_le_bytes(buf[o + 32..o + 40].try_into().unwrap());
                    dev.read(start, &mut sector).map_err(|_| FsError::Io)?;
                    if looks_like_fat32(&sector) {
                        return Ok(Some(start));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(None)
}

impl<D: BlockDevice> FatFs<D> {
    /// Mount a FAT32 volume found either at sector 0 or in the first MBR
    /// partition of a FAT32 type.
    #[allow(clippy::type_complexity)]
    fn read_geometry(dev: &mut D) -> Result<(u64, u32, u32, u32, u32, u32, u32, u32, u32, String)> {
        let mut bs = [0u8; SECTOR];
        dev.read(0, &mut bs).map_err(|_| FsError::Io)?;
        let mut part_lba = 0u64;
        if !looks_like_fat32(&bs) {
            part_lba = find_fat32_partition(dev, &bs)?.ok_or(FsError::Unsupported)?;
            dev.read(part_lba, &mut bs).map_err(|_| FsError::Io)?;
            if !looks_like_fat32(&bs) {
                return Err(FsError::Unsupported);
            }
        }
        let sectors_per_cluster = bs[13] as u32;
        let reserved_sectors = rd16(&bs, 14) as u32;
        let num_fats = bs[16] as u32;
        let total_sectors = if rd16(&bs, 19) != 0 { rd16(&bs, 19) as u32 } else { rd32(&bs, 32) };
        let fat_size = rd32(&bs, 36);
        let root_cluster = rd32(&bs, 44);
        let fsinfo_sector = rd16(&bs, 48) as u32;
        let data_start = reserved_sectors + num_fats * fat_size;
        if num_fats == 0 || data_start >= total_sectors {
            return Err(FsError::Corrupt);
        }
        let total_clusters = (total_sectors - data_start) / sectors_per_cluster;
        if total_clusters < 65525 {
            // Technically FAT12/16 by the spec's definition.
            return Err(FsError::Unsupported);
        }
        let label_bytes = &bs[71..82];
        let volume_label = String::from_utf8_lossy(label_bytes).trim_end().to_string();

        Ok((part_lba, sectors_per_cluster, reserved_sectors, num_fats, fat_size, root_cluster, fsinfo_sector, data_start, total_clusters, volume_label))
    }

    pub fn mount(dev: D) -> Result<FatFs<D>> {
        Self::try_mount(dev).map_err(|(e, _)| e)
    }

    /// Like `mount`, but hands the device back if it holds no usable
    /// FAT32 volume (so the caller can, for example, format it).
    pub fn try_mount(mut dev: D) -> core::result::Result<FatFs<D>, (FsError, D)> {
        let geo = match Self::read_geometry(&mut dev) {
            Ok(g) => g,
            Err(e) => return Err((e, dev)),
        };
        let (part_lba, sectors_per_cluster, reserved_sectors, num_fats, fat_size, root_cluster, fsinfo_sector, data_start, total_clusters, volume_label) = geo;
        let mut fs = FatFs {
            dev,
            part_lba,
            sectors_per_cluster,
            reserved_sectors,
            num_fats,
            fat_size,
            root_cluster,
            fsinfo_sector,
            data_start,
            total_clusters,
            free_clusters: 0,
            next_free: 2,
            volume_label,
            cache: Vec::new(),
            stamp: 0,
            clock: None,
        };
        if let Err(e) = fs.count_free_clusters() {
            return Err((e, fs.dev));
        }
        Ok(fs)
    }

    /// Provide a wall clock used for file timestamps.
    pub fn set_clock(&mut self, clock: fn() -> Timestamp) {
        self.clock = Some(clock);
    }

    pub fn volume_label(&self) -> &str {
        &self.volume_label
    }

    pub fn stats(&self) -> FsStats {
        FsStats {
            cluster_size: self.cluster_size() as u32,
            total_clusters: self.total_clusters,
            free_clusters: self.free_clusters,
        }
    }

    pub fn device(&mut self) -> &mut D {
        &mut self.dev
    }

    fn now(&self) -> Timestamp {
        match self.clock {
            Some(f) => f(),
            None => Timestamp { year: 2026, month: 1, day: 1, hour: 0, minute: 0, second: 0 },
        }
    }

    fn cluster_size(&self) -> usize {
        self.sectors_per_cluster as usize * SECTOR
    }

    fn cluster_lba(&self, cluster: u32) -> u64 {
        self.part_lba + self.data_start as u64 + (cluster as u64 - 2) * self.sectors_per_cluster as u64
    }

    fn valid_cluster(&self, c: u32) -> bool {
        c >= 2 && c < self.total_clusters + 2
    }

    // ---------------------------------------------------------------------
    // Sector cache (used for FAT and directory metadata)
    // ---------------------------------------------------------------------

    fn cache_index(&mut self, lba: u64) -> Result<usize> {
        self.stamp += 1;
        if let Some(i) = self.cache.iter().position(|c| c.lba == lba) {
            self.cache[i].stamp = self.stamp;
            return Ok(i);
        }
        let mut data = Box::new([0u8; SECTOR]);
        self.dev.read(lba, &mut data[..]).map_err(|_| FsError::Io)?;
        if self.cache.len() >= CACHE_ENTRIES {
            let (victim, _) = self.cache.iter().enumerate().min_by_key(|(_, c)| c.stamp).unwrap();
            if self.cache[victim].dirty {
                self.write_back(victim)?;
            }
            self.cache.swap_remove(victim);
        }
        self.cache.push(CachedSector { lba, data, dirty: false, stamp: self.stamp });
        Ok(self.cache.len() - 1)
    }

    /// Write a cached sector to disk. FAT sectors are mirrored to every copy.
    fn write_back(&mut self, i: usize) -> Result<()> {
        let lba = self.cache[i].lba;
        let fat_start = self.part_lba + self.reserved_sectors as u64;
        let fat_end = fat_start + self.fat_size as u64;
        if lba >= fat_start && lba < fat_end {
            for n in 0..self.num_fats as u64 {
                let target = lba + n * self.fat_size as u64;
                self.dev.write(target, &self.cache[i].data[..]).map_err(|_| FsError::Io)?;
            }
        } else {
            self.dev.write(lba, &self.cache[i].data[..]).map_err(|_| FsError::Io)?;
        }
        self.cache[i].dirty = false;
        Ok(())
    }

    /// Write all dirty metadata and the FSInfo sector.
    pub fn flush(&mut self) -> Result<()> {
        for i in 0..self.cache.len() {
            if self.cache[i].dirty {
                self.write_back(i)?;
            }
        }
        if self.fsinfo_sector != 0 && self.fsinfo_sector != 0xffff {
            let lba = self.part_lba + self.fsinfo_sector as u64;
            let mut s = [0u8; SECTOR];
            self.dev.read(lba, &mut s).map_err(|_| FsError::Io)?;
            if rd32(&s, 0) == 0x4161_5252 && rd32(&s, 484) == 0x6141_7272 {
                wr32(&mut s, 488, self.free_clusters);
                wr32(&mut s, 492, self.next_free);
                self.dev.write(lba, &s).map_err(|_| FsError::Io)?;
            }
        }
        self.dev.flush().map_err(|_| FsError::Io)
    }

    // ---------------------------------------------------------------------
    // FAT
    // ---------------------------------------------------------------------

    fn fat_location(&self, cluster: u32) -> (u64, usize) {
        let off = cluster as u64 * 4;
        (self.part_lba + self.reserved_sectors as u64 + off / SECTOR as u64, (off % SECTOR as u64) as usize)
    }

    fn fat_get(&mut self, cluster: u32) -> Result<u32> {
        let (lba, off) = self.fat_location(cluster);
        let i = self.cache_index(lba)?;
        Ok(rd32(&self.cache[i].data[..], off) & FAT_MASK)
    }

    fn fat_set(&mut self, cluster: u32, value: u32) -> Result<()> {
        let (lba, off) = self.fat_location(cluster);
        let i = self.cache_index(lba)?;
        let old = rd32(&self.cache[i].data[..], off);
        wr32(&mut self.cache[i].data[..], off, (old & !FAT_MASK) | (value & FAT_MASK));
        self.cache[i].dirty = true;
        Ok(())
    }

    fn count_free_clusters(&mut self) -> Result<()> {
        // Stream the first FAT in large reads rather than through the cache.
        let entries = self.total_clusters as u64 + 2;
        let fat_bytes = entries * 4;
        let chunk_sectors = 64u64;
        let mut buf = vec![0u8; chunk_sectors as usize * SECTOR];
        let mut free = 0u32;
        let mut first_free = None;
        let start = self.part_lba + self.reserved_sectors as u64;
        let total_sectors = fat_bytes.div_ceil(SECTOR as u64);
        let mut s = 0u64;
        while s < total_sectors {
            let n = chunk_sectors.min(total_sectors - s);
            let bytes = n as usize * SECTOR;
            self.dev.read(start + s, &mut buf[..bytes]).map_err(|_| FsError::Io)?;
            for k in 0..bytes / 4 {
                let cluster = (s * SECTOR as u64 / 4) + k as u64;
                if cluster < 2 || cluster >= entries {
                    continue;
                }
                if rd32(&buf, k * 4) & FAT_MASK == 0 {
                    free += 1;
                    if first_free.is_none() {
                        first_free = Some(cluster as u32);
                    }
                }
            }
            s += n;
        }
        self.free_clusters = free;
        self.next_free = first_free.unwrap_or(2);
        Ok(())
    }

    /// Allocate a free cluster, mark it end-of-chain and link it after
    /// `prev` (if any).
    fn alloc_cluster(&mut self, prev: Option<u32>) -> Result<u32> {
        if self.free_clusters == 0 {
            return Err(FsError::NoSpace);
        }
        let max = self.total_clusters + 2;
        let mut c = if self.valid_cluster(self.next_free) { self.next_free } else { 2 };
        for _ in 0..self.total_clusters {
            if self.fat_get(c)? == 0 {
                self.fat_set(c, FAT_EOC)?;
                if let Some(p) = prev {
                    self.fat_set(p, c)?;
                }
                self.free_clusters -= 1;
                self.next_free = if c + 1 >= max { 2 } else { c + 1 };
                return Ok(c);
            }
            c += 1;
            if c >= max {
                c = 2;
            }
        }
        Err(FsError::NoSpace)
    }

    fn chain(&mut self, first: u32) -> Result<Vec<u32>> {
        let mut out = Vec::new();
        let mut c = first;
        while self.valid_cluster(c) {
            out.push(c);
            if out.len() > self.total_clusters as usize {
                return Err(FsError::Corrupt);
            }
            let next = self.fat_get(c)?;
            if next >= 0x0fff_fff8 {
                break;
            }
            if next == 0 || next == FAT_BAD {
                return Err(FsError::Corrupt);
            }
            c = next;
        }
        Ok(out)
    }

    fn free_chain(&mut self, first: u32) -> Result<()> {
        if !self.valid_cluster(first) {
            return Ok(());
        }
        let chain = self.chain(first)?;
        for &c in &chain {
            self.fat_set(c, 0)?;
            self.free_clusters += 1;
        }
        if let Some(&min) = chain.iter().min()
            && min < self.next_free
        {
            self.next_free = min;
        }
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Cluster data
    // ---------------------------------------------------------------------

    /// Read a chain's data, merging physically consecutive clusters into
    /// single device requests.
    fn read_chain(&mut self, chain: &[u32], out: &mut [u8]) -> Result<()> {
        let cs = self.cluster_size();
        let mut i = 0;
        while i < chain.len() && i * cs < out.len() {
            let mut run = 1;
            while i + run < chain.len() && chain[i + run] == chain[i] + run as u32 && run < 128 {
                run += 1;
            }
            let start = i * cs;
            let end = ((i + run) * cs).min(out.len());
            let whole = (end - start) / SECTOR * SECTOR;
            let lba = self.cluster_lba(chain[i]);
            if whole > 0 {
                self.dev.read(lba, &mut out[start..start + whole]).map_err(|_| FsError::Io)?;
            }
            if whole < end - start {
                let mut tmp = [0u8; SECTOR];
                self.dev.read(lba + (whole / SECTOR) as u64, &mut tmp).map_err(|_| FsError::Io)?;
                let rem = end - start - whole;
                out[start + whole..end].copy_from_slice(&tmp[..rem]);
            }
            i += run;
        }
        Ok(())
    }

    fn write_chain(&mut self, chain: &[u32], data: &[u8]) -> Result<()> {
        let cs = self.cluster_size();
        let mut i = 0;
        while i < chain.len() {
            let mut run = 1;
            while i + run < chain.len() && chain[i + run] == chain[i] + run as u32 && run < 128 {
                run += 1;
            }
            let start = i * cs;
            let end = (i + run) * cs;
            let lba = self.cluster_lba(chain[i]);
            if end <= data.len() {
                self.dev.write(lba, &data[start..end]).map_err(|_| FsError::Io)?;
            } else {
                let mut tmp = vec![0u8; end - start];
                if start < data.len() {
                    tmp[..data.len() - start].copy_from_slice(&data[start..]);
                }
                self.dev.write(lba, &tmp).map_err(|_| FsError::Io)?;
            }
            // Keep the metadata cache coherent if it held any of these sectors.
            let first = lba;
            let last = lba + ((end - start) / SECTOR) as u64;
            self.cache.retain(|c| c.lba < first || c.lba >= last);
            i += run;
        }
        Ok(())
    }

    fn zero_cluster(&mut self, c: u32) -> Result<()> {
        let zero = vec![0u8; self.cluster_size()];
        self.write_chain(&[c], &zero)
    }

    // ---------------------------------------------------------------------
    // Directories
    // ---------------------------------------------------------------------

    fn load_dir(&mut self, first_cluster: u32) -> Result<DirBuf> {
        let clusters = self.chain(first_cluster)?;
        let mut data = vec![0u8; clusters.len() * self.cluster_size()];
        // Directory sectors may be in the metadata cache with newer contents;
        // we write them through, so the disk is always current.
        self.read_chain(&clusters, &mut data)?;
        Ok(DirBuf { clusters, data })
    }

    /// Write slots `[from, to)` of a loaded directory back to disk.
    fn store_slots(&mut self, dir: &DirBuf, from: usize, to: usize) -> Result<()> {
        let cs = self.cluster_size();
        let first_byte = from * 32 / SECTOR * SECTOR;
        let last_byte = (to * 32).div_ceil(SECTOR) * SECTOR;
        let mut off = first_byte;
        while off < last_byte {
            let cluster = dir.clusters[off / cs];
            let lba = self.cluster_lba(cluster) + ((off % cs) / SECTOR) as u64;
            let sector = &dir.data[off..off + SECTOR];
            self.dev.write(lba, sector).map_err(|_| FsError::Io)?;
            if let Some(c) = self.cache.iter_mut().find(|c| c.lba == lba) {
                c.data.copy_from_slice(sector);
                c.dirty = false;
            }
            off += SECTOR;
        }
        Ok(())
    }

    fn parse_dir(&self, dir: &DirBuf) -> Vec<DirEntry> {
        let mut out = Vec::new();
        let mut lfn: Vec<u16> = Vec::new();
        let mut lfn_count = 0usize;
        let mut lfn_seen = 0usize;
        let mut lfn_sum = 0u8;
        let mut lfn_start = 0usize;
        for i in 0..dir.slots() {
            let e = dir.slot(i);
            if e[0] == 0x00 {
                break;
            }
            if e[0] == 0xe5 {
                lfn_count = 0;
                continue;
            }
            let attr = e[11];
            if attr & 0x3f == ATTR_LFN {
                let ord = e[0];
                let seq = (ord & 0x1f) as usize;
                if ord & 0x40 != 0 {
                    lfn_count = seq;
                    lfn_seen = 0;
                    lfn_sum = e[13];
                    lfn_start = i;
                    lfn = vec![0xffff; seq * 13];
                } else if lfn_count == 0 || e[13] != lfn_sum {
                    lfn_count = 0;
                    continue;
                }
                if seq == 0 || seq > lfn_count {
                    lfn_count = 0;
                    continue;
                }
                for (k, &o) in LFN_OFFSETS.iter().enumerate() {
                    lfn[(seq - 1) * 13 + k] = rd16(e, o);
                }
                lfn_seen += 1;
                continue;
            }
            let mut sfn = [0u8; 11];
            sfn.copy_from_slice(&e[..11]);
            if attr & ATTR_VOLUME_ID != 0 || &sfn == b".          " || &sfn == b"..         " {
                lfn_count = 0;
                continue;
            }
            let (name, first_slot) = if lfn_count > 0 && lfn_seen == lfn_count && lfn_checksum(&sfn) == lfn_sum {
                let end = lfn.iter().position(|&c| c == 0 || c == 0xffff).unwrap_or(lfn.len());
                (String::from_utf16_lossy(&lfn[..end]), lfn_start)
            } else {
                (short_name_to_string(&sfn, e[12]), i)
            };
            lfn_count = 0;
            let first_cluster = ((rd16(e, 20) as u32) << 16) | rd16(e, 26) as u32;
            out.push(DirEntry {
                name,
                is_dir: attr & ATTR_DIRECTORY != 0,
                size: rd32(e, 28),
                first_cluster,
                attr,
                created: Timestamp::from_fat(rd16(e, 16), rd16(e, 14)),
                modified: Timestamp::from_fat(rd16(e, 24), rd16(e, 22)),
                first_slot,
                sfn_slot: i,
            });
        }
        out
    }

    fn root_entry(&self) -> DirEntry {
        DirEntry {
            name: String::from("/"),
            is_dir: true,
            size: 0,
            first_cluster: self.root_cluster,
            attr: ATTR_DIRECTORY,
            created: Timestamp::default(),
            modified: Timestamp::default(),
            first_slot: 0,
            sfn_slot: 0,
        }
    }

    fn dir_cluster(&self, e: &DirEntry) -> u32 {
        // ".." entries and some tools store 0 for the root directory.
        if e.first_cluster == 0 { self.root_cluster } else { e.first_cluster }
    }

    fn find_in(&mut self, dir_cluster: u32, name: &str) -> Result<Option<DirEntry>> {
        let dir = self.load_dir(dir_cluster)?;
        Ok(self.parse_dir(&dir).into_iter().find(|e| names_equal(&e.name, name)))
    }

    /// Resolve a path to its entry. Returns the root for "/".
    pub fn stat(&mut self, path: &str) -> Result<DirEntry> {
        let parts = split_path(path)?;
        let mut cur = self.root_entry();
        for p in parts {
            if !cur.is_dir {
                return Err(FsError::NotADirectory);
            }
            let dc = self.dir_cluster(&cur);
            cur = self.find_in(dc, p)?.ok_or(FsError::NotFound)?;
        }
        Ok(cur)
    }

    pub fn exists(&mut self, path: &str) -> bool {
        self.stat(path).is_ok()
    }

    /// Resolve the parent directory of `path` and return (parent cluster, name).
    fn parent_of<'a>(&mut self, path: &'a str) -> Result<(u32, &'a str)> {
        let mut parts = split_path(path)?;
        let name = parts.pop().ok_or(FsError::InvalidPath)?;
        let mut cur = self.root_entry();
        for p in parts {
            let dc = self.dir_cluster(&cur);
            cur = self.find_in(dc, p)?.ok_or(FsError::NotFound)?;
            if !cur.is_dir {
                return Err(FsError::NotADirectory);
            }
        }
        Ok((self.dir_cluster(&cur), name))
    }

    pub fn read_dir(&mut self, path: &str) -> Result<Vec<DirEntry>> {
        let e = self.stat(path)?;
        if !e.is_dir {
            return Err(FsError::NotADirectory);
        }
        let c = self.dir_cluster(&e);
        let dir = self.load_dir(c)?;
        Ok(self.parse_dir(&dir))
    }

    fn unique_short_name(&self, dir: &DirBuf, name: &str) -> [u8; 11] {
        let existing: Vec<[u8; 11]> = (0..dir.slots())
            .map(|i| dir.slot(i))
            .take_while(|e| e[0] != 0)
            .filter(|e| e[0] != 0xe5 && e[11] & 0x3f != ATTR_LFN)
            .map(|e| {
                let mut s = [0u8; 11];
                s.copy_from_slice(&e[..11]);
                s
            })
            .collect();
        let upper: String = name.chars().map(|c| c.to_ascii_uppercase()).collect();
        if let Some(sfn) = exact_short_name(&upper)
            && !existing.contains(&sfn)
        {
            return sfn;
        }
        let (base, ext) = match upper.rfind('.') {
            Some(i) if i > 0 => (&upper[..i], &upper[i + 1..]),
            _ => (&upper[..], ""),
        };
        let clean = |s: &str, max: usize| -> Vec<u8> {
            s.bytes()
                .filter(|&c| c != b' ' && c != b'.')
                .map(|c| if c < 0x80 && sfn_char_ok(c) { c } else { b'_' })
                .take(max)
                .collect()
        };
        let base = clean(base, 8);
        let ext = clean(ext, 3);
        for n in 1..1_000_000u32 {
            let tail = alloc::format!("~{}", n);
            let keep = (8 - tail.len()).min(base.len());
            let mut sfn = [b' '; 11];
            let mut k = 0;
            for &c in &base[..keep] {
                sfn[k] = c;
                k += 1;
            }
            if keep == 0 {
                sfn[0] = b'_';
                k = 1;
            }
            for c in tail.bytes() {
                sfn[k] = c;
                k += 1;
            }
            sfn[8..8 + ext.len()].copy_from_slice(&ext);
            if !existing.contains(&sfn) {
                return sfn;
            }
        }
        [b'_'; 11]
    }

    /// Find `count` consecutive free slots, growing the directory if needed.
    fn free_slots(&mut self, dir: &mut DirBuf, count: usize) -> Result<usize> {
        let mut run = 0;
        for i in 0..dir.slots() {
            let b = dir.slot(i)[0];
            if b == 0x00 {
                // Everything from here to the end is free.
                if dir.slots() - (i - run) >= count {
                    return Ok(i - run);
                }
                break;
            }
            if b == 0xe5 {
                run += 1;
                if run == count {
                    return Ok(i + 1 - run);
                }
            } else {
                run = 0;
            }
        }
        // Grow: the trailing run (if the directory ended in free slots) plus
        // new zeroed clusters.
        let trailing = {
            let mut t = 0;
            for i in (0..dir.slots()).rev() {
                let b = dir.slot(i)[0];
                if b == 0x00 || b == 0xe5 {
                    t += 1;
                } else {
                    break;
                }
            }
            t
        };
        let start = dir.slots() - trailing;
        let per_cluster = self.cluster_size() / 32;
        while dir.slots() - start < count {
            let last = *dir.clusters.last().ok_or(FsError::Corrupt)?;
            let c = self.alloc_cluster(Some(last))?;
            self.zero_cluster(c)?;
            dir.clusters.push(c);
            dir.data.extend(core::iter::repeat_n(0u8, per_cluster * 32));
        }
        Ok(start)
    }

    /// Write a new directory entry (with long-name entries if needed).
    fn add_entry(
        &mut self,
        dir_cluster: u32,
        name: &str,
        attr: u8,
        first_cluster: u32,
        size: u32,
        created: Timestamp,
        modified: Timestamp,
    ) -> Result<()> {
        if !valid_long_name(name) {
            return Err(FsError::InvalidName);
        }
        let mut dir = self.load_dir(dir_cluster)?;
        if self.parse_dir(&dir).iter().any(|e| names_equal(&e.name, name)) {
            return Err(FsError::AlreadyExists);
        }
        let sfn = self.unique_short_name(&dir, name);
        let needs_lfn = short_name_to_string(&sfn, 0) != name;
        let utf16: Vec<u16> = name.encode_utf16().collect();
        let lfn_entries = if needs_lfn { utf16.len().div_ceil(13) } else { 0 };
        let start = self.free_slots(&mut dir, lfn_entries + 1)?;
        let sum = lfn_checksum(&sfn);
        for k in 0..lfn_entries {
            // Stored in reverse: slot `start` holds the last piece.
            let seq = lfn_entries - k;
            let e = dir.slot_mut(start + k);
            e.fill(0);
            e[0] = seq as u8 | if k == 0 { 0x40 } else { 0 };
            e[11] = ATTR_LFN;
            e[13] = sum;
            for (j, &o) in LFN_OFFSETS.iter().enumerate() {
                let idx = (seq - 1) * 13 + j;
                let ch = match idx.cmp(&utf16.len()) {
                    core::cmp::Ordering::Less => utf16[idx],
                    core::cmp::Ordering::Equal => 0x0000,
                    core::cmp::Ordering::Greater => 0xffff,
                };
                wr16(e, o, ch);
            }
        }
        let (cdate, ctime) = created.to_fat();
        let (mdate, mtime) = modified.to_fat();
        let e = dir.slot_mut(start + lfn_entries);
        e.fill(0);
        e[..11].copy_from_slice(&sfn);
        e[11] = attr;
        wr16(e, 14, ctime);
        wr16(e, 16, cdate);
        wr16(e, 18, mdate);
        wr16(e, 20, (first_cluster >> 16) as u16);
        wr16(e, 22, mtime);
        wr16(e, 24, mdate);
        wr16(e, 26, first_cluster as u16);
        wr32(e, 28, size);
        self.store_slots(&dir, start, start + lfn_entries + 1)
    }

    fn delete_entry(&mut self, dir_cluster: u32, entry: &DirEntry) -> Result<()> {
        let mut dir = self.load_dir(dir_cluster)?;
        for i in entry.first_slot..=entry.sfn_slot {
            dir.slot_mut(i)[0] = 0xe5;
        }
        self.store_slots(&dir, entry.first_slot, entry.sfn_slot + 1)
    }

    fn update_entry(&mut self, dir_cluster: u32, entry: &DirEntry, first_cluster: u32, size: u32) -> Result<()> {
        let mut dir = self.load_dir(dir_cluster)?;
        let (mdate, mtime) = self.now().to_fat();
        let e = dir.slot_mut(entry.sfn_slot);
        wr16(e, 20, (first_cluster >> 16) as u16);
        wr16(e, 26, first_cluster as u16);
        wr32(e, 28, size);
        wr16(e, 22, mtime);
        wr16(e, 24, mdate);
        wr16(e, 18, mdate);
        e[11] |= ATTR_ARCHIVE;
        self.store_slots(&dir, entry.sfn_slot, entry.sfn_slot + 1)
    }

    // ---------------------------------------------------------------------
    // Public file operations
    // ---------------------------------------------------------------------

    pub fn read_file(&mut self, path: &str) -> Result<Vec<u8>> {
        let e = self.stat(path)?;
        if e.is_dir {
            return Err(FsError::IsADirectory);
        }
        let mut data = vec![0u8; e.size as usize];
        if e.size > 0 {
            let chain = self.chain(e.first_cluster)?;
            if chain.len() * self.cluster_size() < e.size as usize {
                return Err(FsError::Corrupt);
            }
            self.read_chain(&chain, &mut data)?;
        }
        Ok(data)
    }

    /// Create or replace a file with `data`.
    pub fn write_file(&mut self, path: &str, data: &[u8]) -> Result<()> {
        let (parent, name) = self.parent_of(path)?;
        let existing = self.find_in(parent, name)?;
        if let Some(e) = &existing
            && e.is_dir
        {
            return Err(FsError::IsADirectory);
        }
        if data.len() > u32::MAX as usize {
            return Err(FsError::NoSpace);
        }
        let old_first = existing.as_ref().map(|e| e.first_cluster).unwrap_or(0);
        let cs = self.cluster_size();
        let needed = data.len().div_ceil(cs);
        // Free space available once the old contents are released. We only
        // release them after the new data is safely written, so the new data
        // must fit alongside the old.
        if needed as u32 > self.free_clusters {
            let old_len = if old_first != 0 { self.chain(old_first)?.len() as u32 } else { 0 };
            if needed as u32 <= self.free_clusters + old_len {
                // Tight on space: release the old chain first.
                if let Some(e) = &existing {
                    self.update_entry(parent, e, 0, 0)?;
                }
                self.free_chain(old_first)?;
                return self.finish_write(parent, name, existing, data, 0);
            }
            return Err(FsError::NoSpace);
        }
        self.finish_write(parent, name, existing, data, old_first)
    }

    fn finish_write(
        &mut self,
        parent: u32,
        name: &str,
        existing: Option<DirEntry>,
        data: &[u8],
        old_first: u32,
    ) -> Result<()> {
        let cs = self.cluster_size();
        let needed = data.len().div_ceil(cs);
        let mut chain = Vec::with_capacity(needed);
        let mut prev = None;
        for _ in 0..needed {
            match self.alloc_cluster(prev) {
                Ok(c) => {
                    chain.push(c);
                    prev = Some(c);
                }
                Err(e) => {
                    if let Some(&first) = chain.first() {
                        self.free_chain(first)?;
                    }
                    self.flush()?;
                    return Err(e);
                }
            }
        }
        self.write_chain(&chain, data)?;
        let first = chain.first().copied().unwrap_or(0);
        match existing {
            Some(e) => self.update_entry(parent, &e, first, data.len() as u32)?,
            None => {
                let now = self.now();
                if let Err(err) = self.add_entry(parent, name, ATTR_ARCHIVE, first, data.len() as u32, now, now) {
                    if first != 0 {
                        self.free_chain(first)?;
                    }
                    self.flush()?;
                    return Err(err);
                }
            }
        }
        if old_first != 0 {
            self.free_chain(old_first)?;
        }
        self.flush()
    }

    /// Create an empty file. Fails if it already exists.
    pub fn create_file(&mut self, path: &str) -> Result<()> {
        let (parent, name) = self.parent_of(path)?;
        let now = self.now();
        self.add_entry(parent, name, ATTR_ARCHIVE, 0, 0, now, now)?;
        self.flush()
    }

    pub fn create_dir(&mut self, path: &str) -> Result<()> {
        let (parent, name) = self.parent_of(path)?;
        if !valid_long_name(name) {
            return Err(FsError::InvalidName);
        }
        if self.find_in(parent, name)?.is_some() {
            return Err(FsError::AlreadyExists);
        }
        let c = self.alloc_cluster(None)?;
        let now = self.now();
        let (date, time) = now.to_fat();
        let mut buf = vec![0u8; self.cluster_size()];
        let dot = |b: &mut [u8], name: &[u8; 11], cluster: u32| {
            b[..11].copy_from_slice(name);
            b[11] = ATTR_DIRECTORY;
            wr16(b, 14, time);
            wr16(b, 16, date);
            wr16(b, 18, date);
            wr16(b, 20, (cluster >> 16) as u16);
            wr16(b, 22, time);
            wr16(b, 24, date);
            wr16(b, 26, cluster as u16);
        };
        let parent_ref = if parent == self.root_cluster { 0 } else { parent };
        dot(&mut buf[0..32], b".          ", c);
        dot(&mut buf[32..64], b"..         ", parent_ref);
        self.write_chain(&[c], &buf)?;
        if let Err(e) = self.add_entry(parent, name, ATTR_DIRECTORY, c, 0, now, now) {
            self.free_chain(c)?;
            self.flush()?;
            return Err(e);
        }
        self.flush()
    }

    /// Remove a file or an empty directory.
    pub fn remove(&mut self, path: &str) -> Result<()> {
        let (parent, name) = self.parent_of(path)?;
        let e = self.find_in(parent, name)?.ok_or(FsError::NotFound)?;
        if e.is_dir {
            let c = self.dir_cluster(&e);
            let dir = self.load_dir(c)?;
            if !self.parse_dir(&dir).is_empty() {
                return Err(FsError::DirectoryNotEmpty);
            }
        }
        self.delete_entry(parent, &e)?;
        self.free_chain(e.first_cluster)?;
        self.flush()
    }

    /// Remove a file, or a directory and everything inside it.
    pub fn remove_all(&mut self, path: &str) -> Result<()> {
        let e = self.stat(path)?;
        if split_path(path)?.is_empty() {
            return Err(FsError::InvalidPath);
        }
        if e.is_dir {
            for child in self.read_dir(path)? {
                let p = alloc::format!("{}/{}", path.trim_end_matches('/'), child.name);
                self.remove_all(&p)?;
            }
        }
        self.remove(path)
    }

    /// Rename or move a file or directory.
    pub fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let (src_parent, src_name) = self.parent_of(from)?;
        let entry = self.find_in(src_parent, src_name)?.ok_or(FsError::NotFound)?;
        let (dst_parent, dst_name) = self.parent_of(to)?;
        if !valid_long_name(dst_name) {
            return Err(FsError::InvalidName);
        }
        if let Some(existing) = self.find_in(dst_parent, dst_name)? {
            // Allow changing only the case of a name in place.
            let same = dst_parent == src_parent && existing.sfn_slot == entry.sfn_slot;
            if !same {
                return Err(FsError::AlreadyExists);
            }
        }
        if entry.is_dir {
            // Refuse to move a directory inside itself.
            let moving = self.dir_cluster(&entry);
            let mut c = dst_parent;
            let mut guard = 0;
            while c != self.root_cluster {
                if c == moving {
                    return Err(FsError::InvalidPath);
                }
                let dir = self.load_dir(c)?;
                let dotdot = rd16(dir.slot(1), 26) as u32 | ((rd16(dir.slot(1), 20) as u32) << 16);
                c = if dotdot == 0 { self.root_cluster } else { dotdot };
                guard += 1;
                if guard > 4096 {
                    return Err(FsError::Corrupt);
                }
            }
        }
        // Remove the old entry first so a case-only rename does not collide.
        self.delete_entry(src_parent, &entry)?;
        let attr = entry.attr;
        if let Err(e) =
            self.add_entry(dst_parent, dst_name, attr, entry.first_cluster, entry.size, entry.created, self.now())
        {
            // Put the original back.
            let _ = self.add_entry(src_parent, &entry.name, attr, entry.first_cluster, entry.size, entry.created, entry.modified);
            self.flush()?;
            return Err(e);
        }
        if entry.is_dir && dst_parent != src_parent {
            let c = self.dir_cluster(&entry);
            let mut dir = self.load_dir(c)?;
            let parent_ref = if dst_parent == self.root_cluster { 0 } else { dst_parent };
            let e = dir.slot_mut(1);
            if &e[..2] == b".." {
                wr16(e, 20, (parent_ref >> 16) as u16);
                wr16(e, 26, parent_ref as u16);
                self.store_slots(&dir, 1, 2)?;
            }
        }
        self.flush()
    }

    /// Copy a file (not a directory) to a new path.
    pub fn copy_file(&mut self, from: &str, to: &str) -> Result<()> {
        let data = self.read_file(from)?;
        if self.exists(to) {
            return Err(FsError::AlreadyExists);
        }
        self.write_file(to, &data)
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn short_names() {
        assert_eq!(&exact_short_name("README.TXT").unwrap(), b"README  TXT");
        assert_eq!(&exact_short_name("A").unwrap(), b"A          ");
        assert!(exact_short_name("readme.txt").is_none());
        assert!(exact_short_name("TOOLONGNAME.TXT").is_none());
        assert!(exact_short_name("A.B.C").is_none());
        assert_eq!(short_name_to_string(b"README  TXT", 0), "README.TXT");
        assert_eq!(short_name_to_string(b"README  TXT", 0x18), "readme.txt");
    }

    #[test]
    fn checksum_matches_reference() {
        // Value from an independent implementation of the spec algorithm.
        assert_eq!(lfn_checksum(b"HELLOW~1TXT"), 0x1b);
    }

    #[test]
    fn paths() {
        assert_eq!(split_path("/a/b/../c/./d").unwrap(), vec!["a", "c", "d"]);
        assert!(split_path("/..").is_err());
        assert!(split_path("/").unwrap().is_empty());
    }

    #[test]
    fn name_validation() {
        assert!(valid_long_name("My File (1).txt"));
        assert!(!valid_long_name("a/b"));
        assert!(!valid_long_name("what?"));
        assert!(!valid_long_name("trailing."));
        assert!(!valid_long_name(""));
    }

    #[test]
    fn timestamps_roundtrip() {
        let t = Timestamp { year: 2026, month: 9, day: 25, hour: 17, minute: 3, second: 42 };
        let (d, tm) = t.to_fat();
        assert_eq!(Timestamp::from_fat(d, tm), t);
    }
}

/// Create an empty FAT32 file system spanning `total_sectors` sectors of
/// `dev` (no partition table, like `mkfs.fat` on a whole device).
///
/// Cluster sizes follow Microsoft's defaults. Volumes smaller than about
/// 33 MiB cannot hold enough clusters for FAT32 and are rejected.
pub fn format<D: BlockDevice>(dev: &mut D, total_sectors: u64, label: &str, volume_id: u32) -> Result<()> {
    let n = total_sectors.min(0xffff_ffff) as u32;
    let mb = n / 2048;
    let spc: u32 = match mb {
        0..=260 => 1,
        261..=8192 => 8,
        8193..=16384 => 16,
        16385..=32768 => 32,
        _ => 64,
    };
    let reserved = 32u32;
    let num_fats = 2u32;
    // Microsoft's FAT size formula for FAT32.
    let tmp1 = n.checked_sub(reserved).ok_or(FsError::NoSpace)?;
    let tmp2 = (256 * spc + num_fats) / 2;
    let fat_size = tmp1.div_ceil(tmp2);
    let data_start = reserved + num_fats * fat_size;
    let clusters = n.checked_sub(data_start).ok_or(FsError::NoSpace)? / spc;
    if clusters < 65525 {
        return Err(FsError::NoSpace);
    }

    let mut label11 = [b' '; 11];
    for (i, c) in label.bytes().filter(|c| c.is_ascii_graphic() || *c == b' ').take(11).enumerate() {
        label11[i] = c.to_ascii_uppercase();
    }

    let mut bs = [0u8; SECTOR];
    bs[0..3].copy_from_slice(&[0xeb, 0x58, 0x90]);
    bs[3..11].copy_from_slice(b"MAYOS   ");
    wr16(&mut bs, 11, 512);
    bs[13] = spc as u8;
    wr16(&mut bs, 14, reserved as u16);
    bs[16] = num_fats as u8;
    bs[21] = 0xf8;
    wr16(&mut bs, 24, 63);
    wr16(&mut bs, 26, 255);
    wr32(&mut bs, 32, n);
    wr32(&mut bs, 36, fat_size);
    wr32(&mut bs, 44, 2); // root directory cluster
    wr16(&mut bs, 48, 1); // FSInfo sector
    wr16(&mut bs, 50, 6); // backup boot sector
    bs[64] = 0x80;
    bs[66] = 0x29;
    wr32(&mut bs, 67, volume_id);
    bs[71..82].copy_from_slice(&label11);
    bs[82..90].copy_from_slice(b"FAT32   ");
    // Boot code: print nothing, just halt if someone tries to boot this.
    bs[90..94].copy_from_slice(&[0xfa, 0xf4, 0xeb, 0xfd]);
    wr16(&mut bs, 510, 0xaa55);

    let mut info = [0u8; SECTOR];
    wr32(&mut info, 0, 0x4161_5252);
    wr32(&mut info, 484, 0x6141_7272);
    wr32(&mut info, 488, clusters - 1); // cluster 2 holds the root directory
    wr32(&mut info, 492, 3);
    wr32(&mut info, 508, 0xaa55_0000);

    let io = |r: core::result::Result<(), ()>| r.map_err(|_| FsError::Io);

    // Clear the reserved area and both FATs.
    let zeros = vec![0u8; 64 * SECTOR];
    let mut s = 0u32;
    while s < data_start {
        let count = (data_start - s).min(64);
        io(dev.write(s as u64, &zeros[..count as usize * SECTOR]))?;
        s += count;
    }
    io(dev.write(0, &bs))?;
    io(dev.write(1, &info))?;
    io(dev.write(6, &bs))?;
    io(dev.write(7, &info))?;

    let mut fat0 = [0u8; SECTOR];
    wr32(&mut fat0, 0, 0x0fff_fff8);
    wr32(&mut fat0, 4, 0x0fff_ffff);
    wr32(&mut fat0, 8, 0x0fff_ffff); // root directory: end of chain
    for f in 0..num_fats {
        io(dev.write((reserved + f * fat_size) as u64, &fat0))?;
    }

    // Empty root directory holding just the volume label.
    let mut root = vec![0u8; spc as usize * SECTOR];
    root[..11].copy_from_slice(&label11);
    root[11] = ATTR_VOLUME_ID;
    io(dev.write(data_start as u64, &root))?;
    io(dev.flush())
}
