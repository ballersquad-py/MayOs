//! Integration tests against real FAT32 images.
//!
//! Images are created with `mkfs.fat`, populated with mtools, modified by
//! this crate and then checked with `fsck.fat -n` and read back with mtools,
//! so both directions are verified against independent implementations.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use fat32::{BlockDevice, FatFs, FsError};

struct FileDisk(File);

impl BlockDevice for FileDisk {
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), ()> {
        self.0.seek(SeekFrom::Start(lba * 512)).map_err(|_| ())?;
        self.0.read_exact(buf).map_err(|_| ())
    }
    fn write(&mut self, lba: u64, buf: &[u8]) -> Result<(), ()> {
        self.0.seek(SeekFrom::Start(lba * 512)).map_err(|_| ())?;
        self.0.write_all(buf).map_err(|_| ())
    }
}

fn have(tool: &str) -> bool {
    Command::new("sh").arg("-c").arg(format!("command -v {tool}")).output().map(|o| o.status.success()).unwrap_or(false)
}

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn new_image(size_mb: u32) -> Option<PathBuf> {
    if !have("mkfs.fat") || !have("mcopy") || !have("fsck.fat") {
        eprintln!("skipping: dosfstools/mtools not installed");
        return None;
    }
    let dir = std::env::temp_dir().join("mayos-fat32-tests");
    fs::create_dir_all(&dir).unwrap();
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = dir.join(format!("img-{}-{}.img", std::process::id(), n));
    let _ = fs::remove_file(&path);
    let st = Command::new("mkfs.fat")
        .args(["-F", "32", "-s", "1", "-n", "MAYOS", "-C"])
        .arg(&path)
        .arg((size_mb * 1024).to_string())
        .output()
        .unwrap();
    assert!(st.status.success(), "mkfs.fat failed: {}", String::from_utf8_lossy(&st.stderr));
    Some(path)
}

fn mount(path: &Path) -> FatFs<FileDisk> {
    let f = OpenOptions::new().read(true).write(true).open(path).unwrap();
    FatFs::mount(FileDisk(f)).expect("mount failed")
}

fn fsck(path: &Path) {
    let out = Command::new("fsck.fat").arg("-n").arg(path).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "fsck.fat reported problems:\n{text}");
    for bad in ["Free cluster summary wrong", "FATs differ", "Reclaimed", "orphan", "Dirty bit"] {
        assert!(!text.contains(bad), "fsck.fat: {bad}\n{text}");
    }
}

fn mtype(img: &Path, path: &str) -> Vec<u8> {
    let out = Command::new("mtype")
        .env("MTOOLS_SKIP_CHECK", "1")
        .arg("-i")
        .arg(img)
        .arg(format!("::{path}"))
        .output()
        .unwrap();
    assert!(out.status.success(), "mtype {path} failed: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

fn mcopy_in(img: &Path, host: &Path, dest: &str) {
    let out = Command::new("mcopy")
        .env("MTOOLS_SKIP_CHECK", "1")
        .arg("-i")
        .arg(img)
        .arg(host)
        .arg(format!("::{dest}"))
        .output()
        .unwrap();
    assert!(out.status.success(), "mcopy failed: {}", String::from_utf8_lossy(&out.stderr));
}

fn mls(img: &Path, dir: &str) -> String {
    let out = Command::new("mdir")
        .env("MTOOLS_SKIP_CHECK", "1")
        .arg("-b")
        .arg("-i")
        .arg(img)
        .arg(format!("::{dir}"))
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn reads_files_written_by_mtools() {
    let Some(img) = new_image(64) else { return };
    let tmp = img.with_extension("src");
    fs::create_dir_all(&tmp).unwrap();
    let small = tmp.join("Hello World.txt");
    fs::write(&small, b"hi from mtools\n").unwrap();
    let big = tmp.join("big.bin");
    let big_data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 + i / 13) as u8).collect();
    fs::write(&big, &big_data).unwrap();
    mcopy_in(&img, &small, "/Hello World.txt");
    mcopy_in(&img, &big, "/big.bin");

    let mut fs = mount(&img);
    let names: Vec<String> = fs.read_dir("/").unwrap().into_iter().map(|e| e.name).collect();
    assert!(names.contains(&"Hello World.txt".to_string()), "{names:?}");
    assert_eq!(fs.read_file("/hello world.TXT").unwrap(), b"hi from mtools\n");
    assert_eq!(fs.read_file("/big.bin").unwrap(), big_data);
    assert_eq!(fs.volume_label(), "MAYOS");
    fs::remove_dir_all(&tmp).unwrap();
    fs::remove_file(&img).unwrap();
}

#[test]
fn writes_are_valid_for_fsck_and_mtools() {
    let Some(img) = new_image(64) else { return };
    {
        let mut fs = mount(&img);
        fs.create_dir("/docs").unwrap();
        fs.create_dir("/docs/Deeply Nested Folder").unwrap();
        fs.write_file("/docs/Deeply Nested Folder/A long file name.txt", b"long name contents").unwrap();
        fs.write_file("/README.TXT", b"short name").unwrap();
        fs.write_file("/mixed.Case", b"mixed").unwrap();
        let big: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
        fs.write_file("/docs/big data.bin", &big).unwrap();
        // Overwrite with smaller and larger content.
        fs.write_file("/README.TXT", b"replaced").unwrap();
        fs.write_file("/mixed.Case", &vec![b'x'; 5000]).unwrap();
        fs.create_file("/empty file").unwrap();
        assert_eq!(fs.create_file("/empty file"), Err(FsError::AlreadyExists));
    }
    fsck(&img);
    assert_eq!(mtype(&img, "/docs/Deeply Nested Folder/A long file name.txt"), b"long name contents");
    assert_eq!(mtype(&img, "/README.TXT"), b"replaced");
    assert_eq!(mtype(&img, "/mixed.Case"), vec![b'x'; 5000]);
    let big: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
    assert_eq!(mtype(&img, "/docs/big data.bin"), big);
    let listing = mls(&img, "/");
    assert!(listing.contains("empty file"), "{listing}");
    fs::remove_file(&img).unwrap();
}

#[test]
fn many_files_grow_directories() {
    let Some(img) = new_image(64) else { return };
    {
        let mut fs = mount(&img);
        fs.create_dir("/many").unwrap();
        // 512-byte clusters hold 16 slots; long names use 3-4 slots each, so
        // this forces the directory to grow across many clusters.
        for i in 0..200 {
            fs.write_file(&format!("/many/file with a long name number {i}.txt"), format!("{i}").as_bytes()).unwrap();
        }
        let entries = fs.read_dir("/many").unwrap();
        assert_eq!(entries.len(), 200);
        for i in (0..200).step_by(2) {
            fs.remove(&format!("/many/file with a long name number {i}.txt")).unwrap();
        }
        // Freed slots get reused.
        for i in 0..50 {
            fs.write_file(&format!("/many/again {i}"), b"x").unwrap();
        }
        assert_eq!(fs.read_dir("/many").unwrap().len(), 150);
    }
    fsck(&img);
    assert_eq!(mtype(&img, "/many/file with a long name number 199.txt"), b"199");
    assert_eq!(mtype(&img, "/many/again 49"), b"x");
    fs::remove_file(&img).unwrap();
}

#[test]
fn short_name_collisions_get_unique_aliases() {
    let Some(img) = new_image(64) else { return };
    {
        let mut fs = mount(&img);
        for i in 0..12 {
            fs.write_file(&format!("/Document number {i}.txt"), b"d").unwrap();
        }
        assert_eq!(fs.read_dir("/").unwrap().len(), 12);
    }
    fsck(&img);
    fs::remove_file(&img).unwrap();
}

#[test]
fn rename_move_and_delete() {
    let Some(img) = new_image(64) else { return };
    {
        let mut fs = mount(&img);
        fs.create_dir("/a").unwrap();
        fs.create_dir("/a/sub").unwrap();
        fs.write_file("/a/sub/file.txt", b"content").unwrap();
        fs.create_dir("/b").unwrap();

        fs.rename("/a/sub/file.txt", "/a/sub/Renamed File.txt").unwrap();
        assert_eq!(fs.read_file("/a/sub/Renamed File.txt").unwrap(), b"content");
        assert_eq!(fs.stat("/a/sub/file.txt").unwrap_err(), FsError::NotFound);

        // Case-only rename.
        fs.rename("/a/sub/Renamed File.txt", "/a/sub/renamed file.TXT").unwrap();
        assert_eq!(fs.read_dir("/a/sub").unwrap()[0].name, "renamed file.TXT");

        // Move a directory to another parent: ".." must follow.
        fs.rename("/a/sub", "/b/moved").unwrap();
        assert_eq!(fs.read_file("/b/moved/renamed file.TXT").unwrap(), b"content");
        assert_eq!(fs.rename("/b", "/b/moved/inside"), Err(FsError::InvalidPath));

        assert_eq!(fs.remove("/b"), Err(FsError::DirectoryNotEmpty));
        fs.copy_file("/b/moved/renamed file.TXT", "/copy.txt").unwrap();
        fs.remove_all("/b").unwrap();
        assert_eq!(fs.stat("/b").unwrap_err(), FsError::NotFound);
        assert_eq!(fs.read_file("/copy.txt").unwrap(), b"content");
        fs.remove("/a").unwrap();
        assert_eq!(fs.read_dir("/").unwrap().len(), 1);
    }
    fsck(&img);
    fs::remove_file(&img).unwrap();
}

#[test]
fn moved_directory_parent_link_is_valid() {
    let Some(img) = new_image(64) else { return };
    {
        let mut fs = mount(&img);
        fs.create_dir("/x").unwrap();
        fs.create_dir("/x/y").unwrap();
        fs.create_dir("/z").unwrap();
        fs.rename("/x/y", "/z/y").unwrap();
        fs.write_file("/z/y/f", b"1").unwrap();
    }
    // fsck validates ".." entries point at the real parent.
    fsck(&img);
    assert_eq!(mtype(&img, "/z/y/f"), b"1");
    fs::remove_file(&img).unwrap();
}

#[test]
fn free_space_is_tracked_and_reported() {
    let Some(img) = new_image(64) else { return };
    let before;
    {
        let mut fs = mount(&img);
        before = fs.stats().free_clusters;
        fs.write_file("/f", &vec![1u8; 10 * 512]).unwrap();
        assert_eq!(fs.stats().free_clusters, before - 10);
        fs.remove("/f").unwrap();
        assert_eq!(fs.stats().free_clusters, before);
        fs.write_file("/g", &vec![1u8; 3 * 512 + 1]).unwrap();
    }
    // Remount recomputes the count from the FAT.
    let fs = mount(&img);
    assert_eq!(fs.stats().free_clusters, before - 4);
    drop(fs);
    fsck(&img);
    fs::remove_file(&img).unwrap();
}

#[test]
fn out_of_space_is_an_error_not_corruption() {
    let Some(img) = new_image(40) else { return };
    {
        let mut fs = mount(&img);
        let free = fs.stats().free_bytes() as usize;
        assert_eq!(fs.write_file("/huge", &vec![0u8; free + 4096]), Err(FsError::NoSpace));
        assert!(fs.stat("/huge").is_err());
        fs.write_file("/fits", &vec![7u8; free / 2]).unwrap();
    }
    fsck(&img);
    fs::remove_file(&img).unwrap();
}

#[test]
fn invalid_names_are_rejected() {
    let Some(img) = new_image(40) else { return };
    let mut fs = mount(&img);
    assert_eq!(fs.write_file("/bad:name", b""), Err(FsError::InvalidName));
    assert_eq!(fs.create_dir("/what?"), Err(FsError::InvalidName));
    assert_eq!(fs.write_file("/missing/dir/file", b""), Err(FsError::NotFound));
    fs.write_file("/plain", b"x").unwrap();
    assert_eq!(fs.write_file("/plain/child", b""), Err(FsError::NotADirectory));
    drop(fs);
    fs::remove_file(&img).unwrap();
}

#[test]
fn format_creates_a_valid_volume() {
    if !have("fsck.fat") {
        return;
    }
    let dir = std::env::temp_dir().join("mayos-fat32-tests");
    fs::create_dir_all(&dir).unwrap();
    for (mb, label) in [(40u64, "SMALL"), (600, "MAYOS DATA")] {
        let path = dir.join(format!("fmt-{}-{}.img", std::process::id(), mb));
        let f = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
        f.set_len(mb * 1024 * 1024).unwrap();
        let mut disk = FileDisk(f);
        fat32::format(&mut disk, mb * 2048, label, 0x1234_5678).unwrap();
        drop(disk);
        fsck(&path);
        {
            let mut fs = mount(&path);
            assert_eq!(fs.volume_label(), label);
            assert!(fs.read_dir("/").unwrap().is_empty(), "label entry must not show as a file");
            fs.create_dir("/config").unwrap();
            fs.write_file("/config/settings.ini", b"volume = 50\n").unwrap();
        }
        fsck(&path);
        assert_eq!(mtype(&path, "/config/settings.ini"), b"volume = 50\n");
        fs::remove_file(&path).unwrap();
    }
    // Too small for FAT32.
    let path = dir.join(format!("fmt-tiny-{}.img", std::process::id()));
    let f = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
    f.set_len(8 * 1024 * 1024).unwrap();
    assert_eq!(fat32::format(&mut FileDisk(f), 8 * 2048, "X", 1), Err(FsError::NoSpace));
    fs::remove_file(&path).unwrap();
}

/// Wrap a FAT32 volume in a partition table (like a Windows-made VHD).
fn partitioned(kind: &str) -> Option<PathBuf> {
    let vol = new_image(64)?;
    let data = fs::read(&vol).unwrap();
    fs::remove_file(&vol).unwrap();
    let start = 2048usize;
    let mut disk = vec![0u8; start * 512 + data.len() + 34 * 512];
    disk[start * 512..start * 512 + data.len()].copy_from_slice(&data);
    let sectors = (data.len() / 512) as u32;
    if kind == "mbr" {
        disk[446 + 4] = 0x0c;
        disk[446 + 8..446 + 12].copy_from_slice(&(start as u32).to_le_bytes());
        disk[446 + 12..446 + 16].copy_from_slice(&sectors.to_le_bytes());
    } else {
        disk[446 + 4] = 0xee;
        disk[446 + 8..446 + 12].copy_from_slice(&1u32.to_le_bytes());
        let h = 512;
        disk[h..h + 8].copy_from_slice(b"EFI PART");
        disk[h + 72..h + 80].copy_from_slice(&2u64.to_le_bytes());
        disk[h + 80..h + 84].copy_from_slice(&128u32.to_le_bytes());
        disk[h + 84..h + 88].copy_from_slice(&128u32.to_le_bytes());
        // Entry 1 (entry 0 left empty): basic data partition.
        let e = 1024 + 128;
        disk[e..e + 16].copy_from_slice(&[0xa2, 0xa0, 0xd0, 0xeb, 0xe5, 0xb9, 0x33, 0x44, 0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26, 0x99, 0xc7]);
        disk[e + 32..e + 40].copy_from_slice(&(start as u64).to_le_bytes());
        disk[e + 40..e + 48].copy_from_slice(&((start + sectors as usize - 1) as u64).to_le_bytes());
    }
    disk[510] = 0x55;
    disk[511] = 0xaa;
    let path = vol.with_extension(kind);
    fs::write(&path, disk).unwrap();
    Some(path)
}

#[test]
fn mounts_mbr_and_gpt_partitions() {
    for kind in ["mbr", "gpt"] {
        let Some(path) = partitioned(kind) else { return };
        let mut fs = mount(&path);
        fs.write_file("/from windows.txt", kind.as_bytes()).unwrap();
        assert_eq!(fs.read_file("/FROM WINDOWS.TXT").unwrap(), kind.as_bytes());
        drop(fs);
        fs::remove_file(&path).unwrap();
    }
}

#[test]
fn file_handles_read_at_and_append() {
    let Some(img) = new_image(64) else { return };
    let data: Vec<u8> = (0..2_345_678u32).map(|i| (i * 31 + i / 7) as u8).collect();
    {
        let mut fs = mount(&img);
        fs.create_dir("/up").unwrap();
        let mut w = fs.create_writer("/up/streamed file.bin").unwrap();
        // Uneven chunk sizes cross sector and cluster boundaries.
        let mut p = 0;
        let mut k = 0;
        while p < data.len() {
            let n = [1, 511, 512, 513, 4096, 70_000, 3][k % 7].min(data.len() - p);
            fs.append(&mut w, &data[p..p + n]).unwrap();
            p += n;
            k += 1;
        }
        fs.finish_writer(&w).unwrap();
        let h = fs.open_file("/up/streamed file.bin").unwrap();
        assert_eq!(h.size, data.len() as u64);
        for (off, len) in [(0usize, 10usize), (511, 2), (4000, 100_000), (2_345_000, 5000), (1_000_001, 1)] {
            let mut buf = vec![0u8; len];
            let n = fs.read_at(&h, off as u64, &mut buf).unwrap();
            let want = &data[off..(off + len).min(data.len())];
            assert_eq!(&buf[..n], want, "read_at {off} {len}");
        }
    }
    fsck(&img);
    assert_eq!(mtype(&img, "/up/streamed file.bin"), data);
    fs::remove_file(&img).unwrap();
}
