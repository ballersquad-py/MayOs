//! A disk image held in memory (loaded by the bootloader as a module).
//! Used when no virtio disk exists, e.g. in VirtualBox or on real
//! hardware. Changes last until the machine is switched off.

pub struct RamDisk {
    base: *mut u8,
    pub sectors: u64,
}

unsafe impl Send for RamDisk {}

impl RamDisk {

    pub fn from_boot_module() -> Option<RamDisk> {
        let m = crate::boot::MODULES.response()?.first()?;
        if m.address.is_null() || m.size < 512 {
            return None;
        }
        Some(RamDisk { base: m.address, sectors: m.size / 512 })
    }
}

impl fat32::BlockDevice for RamDisk {
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), ()> {
        if lba + (buf.len() / 512) as u64 > self.sectors {
            return Err(());
        }
        unsafe { core::ptr::copy_nonoverlapping(self.base.add(lba as usize * 512), buf.as_mut_ptr(), buf.len()) };
        Ok(())
    }

    fn write(&mut self, lba: u64, buf: &[u8]) -> Result<(), ()> {
        if lba + (buf.len() / 512) as u64 > self.sectors {
            return Err(());
        }
        unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), self.base.add(lba as usize * 512), buf.len()) };
        Ok(())
    }
}
