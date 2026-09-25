//! Minimal ACPI table parsing: MADT (interrupt controllers) and FADT
//! (power management, for shutdown).

use alloc::vec::Vec;

use crate::mem::paging;

pub struct IoApic {
    pub phys: u64,
    pub gsi_base: u32,
}

pub struct IrqOverride {
    pub irq: u8,
    pub gsi: u32,
    pub flags: u16,
}

pub struct AcpiInfo {
    pub lapic_phys: u64,
    pub ioapics: Vec<IoApic>,
    pub overrides: Vec<IrqOverride>,
    pub cpu_count: usize,
    pub pm1a_cnt: Option<u16>,
    pub slp_typ_s5: Option<u16>,
}

unsafe fn read<T: Copy>(addr: u64) -> T {
    unsafe { core::ptr::read_unaligned(addr as *const T) }
}

/// Map a table and return its virtual address and total length.
fn map_table(phys: u64) -> (u64, usize) {
    let v = paging::map_mmio(phys, 36);
    let len = unsafe { read::<u32>(v + 4) } as usize;
    let v = paging::map_mmio(phys, len);
    (v, len)
}

pub fn parse(rsdp_phys: u64) -> AcpiInfo {
    let mut info = AcpiInfo {
        lapic_phys: 0xfee0_0000,
        ioapics: Vec::new(),
        overrides: Vec::new(),
        cpu_count: 0,
        pm1a_cnt: None,
        slp_typ_s5: None,
    };
    let rsdp = paging::map_mmio(rsdp_phys, 36);
    let revision: u8 = unsafe { read(rsdp + 15) };
    let (root, entry_size) = if revision >= 2 {
        (unsafe { read::<u64>(rsdp + 24) }, 8)
    } else {
        ((unsafe { read::<u32>(rsdp + 16) }) as u64, 4)
    };
    let (root_v, root_len) = map_table(root);
    let count = (root_len - 36) / entry_size;
    for i in 0..count {
        let p = root_v + 36 + (i * entry_size) as u64;
        let phys = if entry_size == 8 { unsafe { read::<u64>(p) } } else { (unsafe { read::<u32>(p) }) as u64 };
        let (t, len) = map_table(phys);
        let sig: [u8; 4] = unsafe { read(t) };
        match &sig {
            b"APIC" => parse_madt(&mut info, t, len),
            b"FACP" => parse_fadt(&mut info, t, len),
            _ => {}
        }
    }
    info
}

fn parse_madt(info: &mut AcpiInfo, t: u64, len: usize) {
    info.lapic_phys = unsafe { read::<u32>(t + 36) } as u64;
    let mut off = 44usize;
    while off + 2 <= len {
        let kind: u8 = unsafe { read(t + off as u64) };
        let elen: u8 = unsafe { read(t + off as u64 + 1) };
        if elen < 2 {
            break;
        }
        let e = t + off as u64;
        unsafe {
            match kind {
                0 => {
                    let flags: u32 = read(e + 4);
                    if flags & 1 != 0 {
                        info.cpu_count += 1;
                    }
                }
                1 => info.ioapics.push(IoApic { phys: read::<u32>(e + 4) as u64, gsi_base: read(e + 8) }),
                2 => info.overrides.push(IrqOverride { irq: read(e + 3), gsi: read(e + 4), flags: read(e + 8) }),
                5 => info.lapic_phys = read(e + 4),
                _ => {}
            }
        }
        off += elen as usize;
    }
}

fn parse_fadt(info: &mut AcpiInfo, t: u64, len: usize) {
    unsafe {
        let pm1a: u32 = read(t + 64);
        if pm1a != 0 {
            info.pm1a_cnt = Some(pm1a as u16);
        }
        let mut dsdt = read::<u32>(t + 40) as u64;
        if len >= 148 {
            let x: u64 = read(t + 140);
            if x != 0 {
                dsdt = x;
            }
        }
        if dsdt == 0 {
            return;
        }
        let (d, dlen) = map_table(dsdt);
        let bytes = core::slice::from_raw_parts(d as *const u8, dlen);
        // Look for the \_S5_ package: NameOp "_S5_" PackageOp PkgLength NumElements SLP_TYPa ...
        if let Some(pos) = bytes.windows(4).position(|w| w == b"_S5_") {
            let mut i = pos + 4;
            if bytes.get(i) != Some(&0x12) {
                return;
            }
            i += 1;
            let lead = bytes[i];
            i += 1 + ((lead >> 6) as usize); // skip PkgLength
            i += 1; // NumElements
            let mut v = bytes[i];
            if v == 0x0a {
                v = bytes[i + 1];
            }
            info.slp_typ_s5 = Some(v as u16);
        }
    }
}
