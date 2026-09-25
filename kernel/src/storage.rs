//! Disk discovery and "Set up this disk for MayOS".
//!
//! At boot the main volume is chosen in this order:
//! 1. a virtio disk (QEMU development setup),
//! 2. a SATA/IDE disk that was set up for MayOS (has `/config/disk.id`),
//! 3. the RAM disk built into the ISO.
//!
//! Other FAT32 disks (for example a VHD prepared in Windows) are mounted
//! as `/disk1`, `/disk2`, ... Disks without a FAT32 volume are kept aside
//! so the user can set them up from Settings → Storage.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::drivers::{ahci, ide, pci};
use crate::fs::{self, Disk};
use crate::sync::{Mutex, Spin};

pub const MARKER: &str = "/config/disk.id";

pub struct BlankDisk {
    pub name: String,
    pub model: String,
    pub bytes: u64,
    dev: Option<Disk>,
}

/// Disks that have no FAT32 volume yet.
static BLANK: Mutex<Vec<BlankDisk>> = Mutex::new(Vec::new());

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SetupState {
    Idle,
    Running(String),
    Done(String),
    Failed(String),
}

static SETUP: Spin<SetupState> = Spin::new(SetupState::Idle);

pub fn setup_state() -> SetupState {
    SETUP.lock().clone()
}

fn set_state(s: SetupState) {
    *SETUP.lock() = s;
}

pub struct DiskSummary {
    pub name: String,
    pub model: String,
    pub bytes: u64,
}

pub fn blank_disks() -> Vec<DiskSummary> {
    BLANK.lock().iter().map(|b| DiskSummary { name: b.name.clone(), model: b.model.clone(), bytes: b.bytes }).collect()
}

/// Probe SATA and IDE disks and mount what we find.
pub fn init() {
    let mut found: Vec<(String, String, Disk)> = Vec::new();
    for d in pci::devices() {
        if d.class == 0x01 && d.subclass == 0x06 {
            for disk in ahci::probe(&d) {
                crate::kprintln!("disk: {} \"{}\" {} MiB", disk.name, disk.model, disk.sectors / 2048);
                found.push((disk.name.clone(), disk.model.clone(), Disk::Ahci(disk)));
            }
        }
    }
    for disk in ide::probe() {
        crate::kprintln!("disk: {} \"{}\" {} MiB", disk.name, disk.model, disk.sectors / 2048);
        found.push((disk.name.clone(), disk.model.clone(), Disk::Ide(disk)));
    }

    let root_persistent = fs::root_is_persistent();
    let mut next = 1;
    for (name, model, dev) in found {
        let bytes = dev.sectors() * 512;
        match fs::open_volume(dev) {
            Ok(mut vol) => {
                let is_mayos = vol.exists(MARKER);
                if is_mayos && !root_persistent && !fs::root_is_persistent() {
                    // The ISO may be newer than the disk: bring its programs
                    // and help files up to date before switching over.
                    fs::mount_volume(UPDATE_POINT, vol);
                    let updated = update_system_files();
                    let vol = fs::take_mount(UPDATE_POINT).expect("disk mounted a moment ago");
                    fs::mount_volume("/", vol);
                    crate::kprintln!("disk: {} is the MayOS disk; mounted at / ({} system files updated)", name, updated);
                } else {
                    let point = format!("/disk{}", next);
                    next += 1;
                    fs::mount_volume(&point, vol);
                    crate::kprintln!("disk: {} ({}) mounted at {}", name, model, point);
                }
            }
            Err((e, dev)) => {
                crate::kprintln!("disk: {} has no FAT32 volume ({}); available for setup", name, e);
                BLANK.lock().push(BlankDisk { name, model, bytes, dev: Some(dev) });
            }
        }
    }
}

const UPDATE_POINT: &str = "/mayos-update";
/// Folders that belong to the system: refreshed from the ISO at every boot.
/// Everything else on the disk is the user's and is left alone.
const SYSTEM_DIRS: &[&str] = &["/bin", "/docs"];

/// Copy new or changed files from the ISO's system folders onto the MayOS
/// disk (mounted at `UPDATE_POINT`). Returns how many files were written.
fn update_system_files() -> usize {
    let mut n = 0;
    for dir in SYSTEM_DIRS {
        let _ = update_dir(dir, &format!("{}{}", UPDATE_POINT, dir), &mut n);
    }
    n
}

fn update_dir(from: &str, to: &str, n: &mut usize) -> fs::Result<()> {
    if !fs::is_dir(from) {
        return Ok(());
    }
    if !fs::exists(to) {
        fs::create_dir(to)?;
    }
    for e in fs::read_dir(from)? {
        let (src, dst) = (fs::join(from, &e.name), fs::join(to, &e.name));
        if e.is_dir {
            update_dir(&src, &dst, n)?;
            continue;
        }
        let new = fs::read_file(&src)?;
        let same = fs::stat(&dst).map(|d| d.size as usize == new.len()).unwrap_or(false)
            && fs::read_file(&dst).map(|old| old == new).unwrap_or(false);
        if !same {
            fs::write_file(&dst, &new)?;
            *n += 1;
        }
    }
    Ok(())
}

/// What to set up.
#[derive(Clone)]
pub enum Target {
    /// Format a blank disk (by name).
    Blank(String),
    /// Use an already mounted FAT32 disk, keeping its files.
    Mounted(String),
}

/// Run the setup on a background thread; progress is in `setup_state()`.
pub fn start_setup(target: Target) {
    if matches!(setup_state(), SetupState::Running(_)) {
        return;
    }
    set_state(SetupState::Running(String::from("Starting\u{2026}")));
    let boxed = alloc::boxed::Box::new(target);
    crate::proc::sched::spawn_kernel("disk-setup", setup_thread, alloc::boxed::Box::into_raw(boxed) as usize);
}

extern "C" fn setup_thread(arg: usize) {
    let target = unsafe { alloc::boxed::Box::from_raw(arg as *mut Target) };
    match run_setup(*target) {
        Ok(msg) => set_state(SetupState::Done(msg)),
        Err(msg) => {
            crate::kprintln!("disk setup failed: {}", msg);
            set_state(SetupState::Failed(msg));
        }
    }
}

fn run_setup(target: Target) -> Result<String, String> {
    let point = match target {
        Target::Blank(name) => {
            let mut dev = {
                let mut b = BLANK.lock();
                let i = b.iter().position(|d| d.name == name).ok_or("disk not found")?;
                b[i].dev.take().ok_or("disk is busy")?
            };
            set_state(SetupState::Running(String::from("Formatting\u{2026}")));
            let sectors = dev.sectors();
            let id = crate::arch::cpu::rdtsc() as u32;
            if let Err(e) = fat32::format(&mut dev, sectors, "MAYOS", id) {
                let msg = format!("formatting failed: {}", e);
                if let Some(d) = BLANK.lock().iter_mut().find(|d| d.name == name) {
                    d.dev = Some(dev);
                }
                return Err(msg);
            }
            let vol = fs::open_volume(dev).map_err(|(e, _)| format!("cannot open the new volume: {}", e))?;
            BLANK.lock().retain(|d| d.name != name);
            fs::mount_volume("/newdisk", vol);
            String::from("/newdisk")
        }
        Target::Mounted(point) => point,
    };

    // Copy everything from the current main volume that isn't there yet.
    set_state(SetupState::Running(String::from("Copying files\u{2026}")));
    let mut copied = 0;
    copy_missing("/", &point, &mut copied).map_err(|e| format!("copying failed: {}", e))?;
    if !fs::is_dir(&format!("{}/config", point)) {
        fs::create_dir(&format!("{}/config", point)).map_err(|e| format!("{}", e))?;
    }
    // Settings may have changed since boot: write the current ones.
    let settings = crate::settings::serialize(&crate::settings::get());
    fs::write_file(&format!("{}{}", point, crate::settings::PATH), settings.as_bytes()).map_err(|e| format!("{}", e))?;
    let id = format!("MayOS disk\nset up at uptime {} ms\n", crate::time::uptime_ms());
    fs::write_file(&format!("{}{}", point, MARKER), id.as_bytes()).map_err(|e| format!("{}", e))?;

    // Switch: the disk becomes `/`, the old main volume goes away.
    let vol = fs::take_mount(&point).ok_or("the disk disappeared")?;
    fs::mount_volume("/", vol);
    Ok(format!("MayOS now runs from this disk ({} files copied). Your changes are saved.", copied))
}

/// Copy files and folders from `from` into `to` that don't exist there.
fn copy_missing(from: &str, to: &str, copied: &mut usize) -> fs::Result<()> {
    for e in fs::read_dir(from)? {
        let src = fs::join(from, &e.name);
        // Skip other disks' mount points (they show up in "/").
        if fs::is_mount_point(&src) || src == to {
            continue;
        }
        let dst = fs::join(to, &e.name);
        if e.is_dir {
            if !fs::exists(&dst) {
                fs::create_dir(&dst)?;
            }
            copy_missing(&src, &dst, copied)?;
        } else if !fs::exists(&dst) {
            fs::write_file(&dst, &fs::read_file(&src)?)?;
            *copied += 1;
        }
    }
    Ok(())
}
