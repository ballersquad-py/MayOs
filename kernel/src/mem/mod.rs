pub mod heap;
pub mod paging;
pub mod pmm;

use core::sync::atomic::{AtomicU64, Ordering};

pub const PAGE_SIZE: u64 = 4096;

static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);

/// Physical memory is mapped at a fixed offset (the "higher-half direct map").
#[inline]
pub fn phys_to_virt(phys: u64) -> u64 {
    phys + HHDM_OFFSET.load(Ordering::Relaxed)
}


pub fn init() {
    let hhdm = crate::boot::HHDM.response().expect("no HHDM response").offset;
    HHDM_OFFSET.store(hhdm, Ordering::Relaxed);
    let memmap = crate::boot::MEMMAP.response().expect("no memory map");
    pmm::init(memmap);
    paging::init();
    heap::init();
}

/// A physically contiguous, zeroed buffer usable for device DMA.
pub struct DmaBuf {
    pub phys: u64,
    pub pages: usize,
}

impl DmaBuf {
    pub fn new(bytes: usize) -> DmaBuf {
        DmaBuf::try_new(bytes).expect("out of memory for DMA buffer")
    }

    pub fn try_new(bytes: usize) -> Option<DmaBuf> {
        let pages = bytes.div_ceil(PAGE_SIZE as usize).max(1);
        let phys = pmm::alloc_contiguous(pages)?;
        let b = DmaBuf { phys, pages };
        unsafe { core::ptr::write_bytes(b.virt() as *mut u8, 0, pages * PAGE_SIZE as usize) };
        Some(b)
    }

    pub fn virt(&self) -> u64 {
        phys_to_virt(self.phys)
    }

    pub fn len(&self) -> usize {
        self.pages * PAGE_SIZE as usize
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.virt() as *const u8, self.len()) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.virt() as *mut u8, self.len()) }
    }
}

impl Drop for DmaBuf {
    fn drop(&mut self) {
        pmm::free_contiguous(self.phys, self.pages);
    }
}
