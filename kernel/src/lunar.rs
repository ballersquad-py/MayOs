//! `pkg install lunar-client`: Lunar Client's own launcher on MayOS.
//!
//! Lunar ships an Electron app for glibc Linux (an AppImage). MayOS's
//! Linux programs are Alpine (musl) ones, so this installs:
//! - a glibc runtime: the libraries the launcher needs, taken from
//!   Ubuntu 24.04's packages, into /usr/lib/x86_64-linux-gnu (where Ubuntu's
//!   dynamic linker looks); programs started with MAYOS_GLIBC=1 get that
//!   linker instead of Alpine's gcompat shim (see proc::linux);
//! - the launcher itself, downloaded from Lunar's servers and unpacked
//!   from the AppImage's squashfs image into /opt/lunar;
//! - the `lunar-client` command and a menu entry.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::fs;
use crate::pkg::{download, mkdirs, say, tar_entries};
use crate::proc::process::Console;

const UBUNTU: &str = "http://archive.ubuntu.com/ubuntu";
const SUITES: &[&str] = &["noble", "noble-updates", "noble-security"];
const LIB_DIR: &str = "/usr/lib/x86_64-linux-gnu";
const LUNAR_DIR: &str = "/opt/lunar";
const LUNAR_DOWNLOAD: &str = "https://api.lunarclientprod.com/site/download?os=linux";

/// Ubuntu packages holding every library Lunar's launcher loads (worked
/// out by running it on Ubuntu 24.04 and reading /proc/<pid>/maps).
const DEBS: &[&str] = &[
    "libc6", "libgcc-s1", "zlib1g", "libasound2t64", "libatk-bridge2.0-0t64", "libatk1.0-0t64", "libatspi2.0-0t64",
    "libavahi-client3", "libavahi-common3", "libblkid1", "libbrotli1", "libbsd0", "libbz2-1.0", "libcairo-gobject2",
    "libcairo2", "libcap2", "libcom-err2", "libcups2t64", "libdatrie1", "libdbus-1-3", "libdrm2", "libepoxy0",
    "libexpat1", "libffi8", "libfontconfig1", "libfreetype6", "libfribidi0", "libgbm1", "libgcrypt20",
    "libgdk-pixbuf-2.0-0", "libglib2.0-0t64", "libgmp10", "libgnutls30t64", "libgpg-error0", "libgraphite2-3",
    "libgssapi-krb5-2", "libgtk-3-0t64", "libharfbuzz0b", "libhogweed6t64", "libidn2-0", "libjpeg-turbo8",
    "libk5crypto3", "libkeyutils1", "libkrb5-3", "libkrb5support0", "liblz4-1", "liblzma5", "libmd0", "libmount1",
    "libnettle8t64", "libnspr4", "libnss3", "libp11-kit0", "libpango-1.0-0", "libpangocairo-1.0-0",
    "libpangoft2-1.0-0", "libpcre2-8-0", "libpixman-1-0", "libpng16-16t64", "libselinux1", "libsystemd0",
    "libtasn1-6", "libthai0", "libtinfo6", "libudev1", "libunistring5", "libwayland-client0", "libwayland-cursor0",
    "libwayland-egl1", "libx11-6", "libx11-xcb1", "libxau6", "libxcb-render0", "libxcb-shm0", "libxcb1",
    "libxcomposite1", "libxcursor1", "libxdamage1", "libxdmcp6", "libxext6", "libxfixes3", "libxi6",
    "libxinerama1", "libxkbcommon0", "libxrandr2", "libxrender1", "libzstd1",
];

const SCRIPT: &[u8] = br#"#!/bin/sh
# Lunar Client's own launcher on MayOS's glibc runtime. MAYOS_GLIBC=1 is
# inherited by everything it starts, including the Java it downloads.
export MAYOS_GLIBC=1
cd /opt/lunar
exec /opt/lunar/lunarclient --no-sandbox --in-process-gpu --ozone-platform=wayland --disable-gpu "$@"
"#;

const DESKTOP: &[u8] = b"[Desktop Entry]\nType=Application\nName=Lunar Client\nExec=lunar-client\nIcon=/opt/lunar/lunarclient.png\nTerminal=false\n";

pub fn install(c: &Console, cancel: &AtomicBool) -> Result<(), String> {
    say(c, "Lunar Client: glibc runtime from Ubuntu 24.04, launcher from Lunar's servers.\n");
    runtime(c, cancel)?;
    if cancel.load(Ordering::Relaxed) {
        return Err(String::from("cancelled"));
    }
    launcher(c)?;
    let _ = fs::write_file("/usr/bin/lunar-client", SCRIPT);
    let _ = mkdirs("/usr/share/applications");
    let _ = fs::write_file("/usr/share/applications/lunarclient.desktop", DESKTOP);
    say(c, "Done. Start it from the menu (Lunar Client) or with: lunar-client\n");
    Ok(())
}

// --- the glibc runtime ------------------------------------------------------

/// Package name -> pool path, newest suite last wins.
fn ubuntu_index(c: &Console) -> Result<BTreeMap<String, String>, String> {
    let mut map = BTreeMap::new();
    for suite in SUITES {
        say(c, &format!("Reading Ubuntu's package list ({})...\n", suite));
        let gz = download(&format!("{}/dists/{}/main/binary-amd64/Packages.gz", UBUNTU, suite))?;
        let text = crate::pkg::gunzip_all(&gz)?;
        let text = String::from_utf8_lossy(&text);
        let mut name = "";
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("Package: ") {
                name = v.trim();
            } else if let Some(v) = line.strip_prefix("Filename: ")
                && DEBS.contains(&name)
            {
                map.insert(name.to_string(), v.trim().to_string());
            }
        }
    }
    Ok(map)
}

fn runtime(c: &Console, cancel: &AtomicBool) -> Result<(), String> {
    let idx = ubuntu_index(c)?;
    mkdirs(LIB_DIR).map_err(|e| e.to_string())?;
    for (k, name) in DEBS.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(String::from("cancelled"));
        }
        let Some(path) = idx.get(*name) else {
            return Err(format!("Ubuntu has no package {}", name));
        };
        say(c, &format!("[{}/{}] {}\n", k + 1, DEBS.len(), name));
        let deb = download(&format!("{}/{}", UBUNTU, path))?;
        let tar = deb_data(&deb)?;
        let mut files: BTreeMap<String, &[u8]> = BTreeMap::new();
        let mut links: Vec<(String, String)> = Vec::new();
        tar_entries(&tar, |e| {
            let n = e.name.trim_start_matches("./");
            let Some(rest) = n.strip_prefix("usr/lib/x86_64-linux-gnu/").or_else(|| n.strip_prefix("lib/x86_64-linux-gnu/")) else { return };
            // Libraries only (not their plugins' subfolders).
            if rest.contains('/') || !rest.contains(".so") {
                return;
            }
            match e.kind {
                b'0' | 0 => {
                    files.insert(rest.to_string(), e.data);
                }
                b'2' => links.push((rest.to_string(), e.link.rsplit('/').next().unwrap_or("").to_string())),
                _ => {}
            }
        });
        for (n, data) in &files {
            fs::write_file(&format!("{}/{}", LIB_DIR, n), data).map_err(|e| format!("{}: {}", n, e))?;
        }
        // FAT has no symbolic links: the names programs ask for
        // (libfoo.so.1) get a copy of the file they point to.
        for (n, target) in &links {
            let mut t = target.clone();
            for _ in 0..4 {
                match links.iter().find(|(l, _)| *l == t) {
                    Some((_, next)) => t = next.clone(),
                    None => break,
                }
            }
            if let Some(data) = files.get(&t) {
                fs::write_file(&format!("{}/{}", LIB_DIR, n), data).map_err(|e| format!("{}: {}", n, e))?;
            }
        }
    }
    Ok(())
}

/// The data.tar of a .deb (an `ar` archive), decompressed.
fn deb_data(deb: &[u8]) -> Result<Vec<u8>, String> {
    if !deb.starts_with(b"!<arch>\n") {
        return Err(String::from("not a .deb"));
    }
    let mut o = 8;
    while o + 60 <= deb.len() {
        let h = &deb[o..o + 60];
        let name = core::str::from_utf8(&h[0..16]).unwrap_or("").trim().trim_end_matches('/');
        let size: usize = core::str::from_utf8(&h[48..58]).unwrap_or("0").trim().parse().unwrap_or(0);
        let body = deb.get(o + 60..o + 60 + size).ok_or("truncated .deb")?;
        if name.starts_with("data.tar") {
            return match name {
                "data.tar.zst" => zstd(body),
                "data.tar.gz" => crate::pkg::gunzip_all(body),
                "data.tar" => Ok(body.to_vec()),
                _ => Err(format!("{} is not supported", name)),
            };
        }
        o += 60 + size + (size & 1);
    }
    Err(String::from("no data in .deb"))
}

fn zstd(data: &[u8]) -> Result<Vec<u8>, String> {
    use ruzstd::io::Read;
    let mut dec = ruzstd::decoding::StreamingDecoder::new(data).map_err(|e| format!("zstd: {:?}", e))?;
    let mut out = Vec::new();
    let mut buf = alloc::vec![0u8; 256 * 1024];
    loop {
        let n = dec.read(&mut buf).map_err(|e| format!("zstd: {:?}", e))?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

// --- the launcher -----------------------------------------------------------

fn launcher(c: &Console) -> Result<(), String> {
    // The website's download link (it redirects to the newest build; the
    // updater's latest-linux.yml lags behind).
    say(c, "Downloading Lunar Client's launcher from lunarclient.com (about 130 MB)...\n");
    let img = download(LUNAR_DOWNLOAD)?;
    say(c, "Unpacking...\n");
    let _ = fs::remove_all(LUNAR_DIR);
    mkdirs(LUNAR_DIR).map_err(|e| e.to_string())?;
    let n = squashfs_extract(&img, LUNAR_DIR)?;
    say(c, &format!("{} files.\n", n));
    Ok(())
}

// --- squashfs (version 4, gzip), as inside an AppImage ----------------------

struct Sq<'a> {
    d: &'a [u8],
    block_size: usize,
    inodes: Vec<u8>,
    /// Compressed block offset (from the table start) -> decompressed offset.
    inode_map: BTreeMap<u64, usize>,
    dirs: Vec<u8>,
    dir_map: BTreeMap<u64, usize>,
    frags: Vec<(u64, u32)>,
}

fn u16_at(b: &[u8], o: usize) -> usize {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]) as usize).unwrap_or(0)
}
fn u32_at(b: &[u8], o: usize) -> u64 {
    b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as u64).unwrap_or(0)
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    b.get(o..o + 8).map(|s| u64::from_le_bytes(s.try_into().unwrap())).unwrap_or(0)
}

fn zlib(data: &[u8], max: usize) -> Result<Vec<u8>, String> {
    miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(data, max.max(1)).map_err(|_| String::from("squashfs: bad compressed block"))
}

/// A metadata table: every 8 KiB block, decompressed and joined.
fn meta_table(d: &[u8], start: u64, end: u64) -> Result<(Vec<u8>, BTreeMap<u64, usize>), String> {
    let mut out = Vec::new();
    let mut map = BTreeMap::new();
    let mut p = start;
    while p + 2 <= end {
        let h = u16_at(d, p as usize);
        let len = h & 0x7fff;
        let raw = d.get(p as usize + 2..p as usize + 2 + len).ok_or("squashfs: truncated")?;
        map.insert(p - start, out.len());
        if h & 0x8000 != 0 {
            out.extend_from_slice(raw);
        } else {
            out.extend_from_slice(&zlib(raw, 8192)?);
        }
        p += 2 + len as u64;
    }
    Ok((out, map))
}

impl<'a> Sq<'a> {
    fn open(img: &'a [u8]) -> Result<Sq<'a>, String> {
        // The image follows the AppImage's own ELF program.
        let shoff = u64_at(img, 40) as usize;
        let (ent, num) = (u16_at(img, 58), u16_at(img, 60));
        let at = shoff + ent * num;
        let d = img.get(at..).ok_or("not an AppImage")?;
        if !d.starts_with(b"hsqs") {
            return Err(String::from("no squashfs image in the AppImage"));
        }
        if u16_at(d, 20) != 1 {
            return Err(String::from("squashfs: only gzip images are supported"));
        }
        let frag_count = u32_at(d, 16) as usize;
        let (inode_t, dir_t, frag_t) = (u64_at(d, 64), u64_at(d, 72), u64_at(d, 80));
        let export_t = u64_at(d, 88);
        let id_t = u64_at(d, 48);
        let (inodes, inode_map) = meta_table(d, inode_t, dir_t)?;
        // The directory table ends where the next table starts.
        let dir_end = [frag_t, export_t, id_t].into_iter().filter(|&x| x > dir_t && x != u64::MAX).min().unwrap_or(d.len() as u64);
        let (dirs, dir_map) = meta_table(d, dir_t, dir_end)?;
        let mut frags = Vec::new();
        if frag_count > 0 && frag_t != u64::MAX {
            let blocks = (frag_count * 16).div_ceil(8192);
            let mut table = Vec::new();
            for i in 0..blocks {
                let loc = u64_at(d, frag_t as usize + i * 8);
                let (t, _) = meta_table(d, loc, loc + 2 + (u16_at(d, loc as usize) & 0x7fff) as u64)?;
                table.extend_from_slice(&t);
            }
            for i in 0..frag_count {
                frags.push((u64_at(&table, i * 16), u32_at(&table, i * 16 + 8) as u32));
            }
        }
        Ok(Sq { d, block_size: u32_at(d, 12) as usize, inodes, inode_map, dirs, dir_map, frags })
    }

    fn inode_at(&self, block: u64, offset: usize) -> Result<usize, String> {
        self.inode_map.get(&block).map(|b| b + offset).ok_or_else(|| String::from("squashfs: bad inode reference"))
    }

    fn file_data(&self, i: usize, ext: bool) -> Result<Vec<u8>, String> {
        let b = &self.inodes;
        let (start, frag, frag_off, size, list) = if ext {
            (u64_at(b, i + 16), u32_at(b, i + 44), u32_at(b, i + 48), u64_at(b, i + 24) as usize, i + 56)
        } else {
            (u32_at(b, i + 16), u32_at(b, i + 20), u32_at(b, i + 24), u32_at(b, i + 28) as usize, i + 32)
        };
        let bs = self.block_size;
        let nblocks = if frag == 0xffff_ffff { size.div_ceil(bs) } else { size / bs };
        let mut out = Vec::with_capacity(size);
        let mut pos = start as usize;
        for k in 0..nblocks {
            let w = u32_at(b, list + k * 4) as usize;
            let len = w & 0xff_ffff;
            if len == 0 {
                out.resize(out.len() + bs.min(size - out.len()), 0);
                continue;
            }
            let raw = self.d.get(pos..pos + len).ok_or("squashfs: truncated file")?;
            if w & 0x100_0000 != 0 {
                out.extend_from_slice(raw);
            } else {
                out.extend_from_slice(&zlib(raw, bs)?);
            }
            pos += len;
        }
        if frag != 0xffff_ffff {
            let (fstart, fsize) = *self.frags.get(frag as usize).ok_or("squashfs: bad fragment")?;
            let len = (fsize & 0xff_ffff) as usize;
            let raw = self.d.get(fstart as usize..fstart as usize + len).ok_or("squashfs: truncated fragment")?;
            let block = if fsize & 0x100_0000 != 0 { raw.to_vec() } else { zlib(raw, bs)? };
            let need = size - out.len();
            out.extend_from_slice(block.get(frag_off as usize..frag_off as usize + need).ok_or("squashfs: bad fragment")?);
        }
        out.truncate(size);
        Ok(out)
    }

    /// Unpack the directory at inode offset `i` into `to`.
    fn extract_dir(&self, i: usize, to: &str, count: &mut usize, depth: u32) -> Result<(), String> {
        if depth > 32 {
            return Err(String::from("squashfs: too deep"));
        }
        let b = &self.inodes;
        let kind = u16_at(b, i);
        let (start, size, offset) = match kind {
            1 => (u32_at(b, i + 16), u16_at(b, i + 24), u16_at(b, i + 26)),
            8 => (u32_at(b, i + 24), u32_at(b, i + 20) as usize, u16_at(b, i + 38)),
            _ => return Err(String::from("squashfs: not a directory")),
        };
        if size <= 3 {
            return Ok(());
        }
        let base = *self.dir_map.get(&start).ok_or("squashfs: bad directory reference")? + offset;
        let end = base + size - 3;
        let mut p = base;
        while p + 12 <= end {
            let n = u32_at(&self.dirs, p) as usize + 1;
            let iblock = u32_at(&self.dirs, p + 4);
            p += 12;
            for _ in 0..n {
                let ioff = u16_at(&self.dirs, p);
                let name_len = u16_at(&self.dirs, p + 6) + 1;
                let name = String::from_utf8_lossy(self.dirs.get(p + 8..p + 8 + name_len).ok_or("squashfs: bad entry")?).into_owned();
                p += 8 + name_len;
                let ii = self.inode_at(iblock, ioff)?;
                let path = format!("{}/{}", to, name);
                match u16_at(b, ii) {
                    1 | 8 => {
                        let _ = fs::create_dir(&path);
                        self.extract_dir(ii, &path, count, depth + 1)?;
                    }
                    2 | 9 => {
                        let mut data = self.file_data(ii, u16_at(b, ii) == 9)?;
                        if path.ends_with("/app.asar") {
                            show_windows(&mut data);
                        }
                        fs::write_file(&path, &data).map_err(|e| format!("{}: {}", path, e))?;
                        *count += 1;
                    }
                    // Symbolic links (an icon alias): not needed.
                    _ => {}
                }
            }
        }
        Ok(())
    }
}

fn squashfs_extract(img: &[u8], to: &str) -> Result<usize, String> {
    let sq = Sq::open(img)?;
    let root = u64_at(sq.d, 32);
    let ri = sq.inode_at(root >> 16, (root & 0xffff) as usize)?;
    let mut n = 0;
    sq.extract_dir(ri, to, &mut n, 0)?;
    Ok(n)
}

/// Lunar creates its windows hidden and shows them on "ready-to-show",
/// which needs an offscreen paint that never completes under MayOS's
/// compositor. Create the launcher and sign-in windows visible instead
/// (same-length byte patch, so the asar index stays valid).
fn show_windows(data: &mut [u8]) {
    const PATS: [&[u8]; 2] = [
        b"autoHideMenuBar:!0,show:!1",
        b"fullscreenable:process.platform===`darwin`,show:!1",
    ];
    for pat in PATS {
        let mut i = 0;
        while i + pat.len() <= data.len() {
            if &data[i..i + pat.len()] == pat {
                data[i + pat.len() - 1] = b'0';
                i += pat.len();
            } else {
                i += 1;
            }
        }
    }
}
