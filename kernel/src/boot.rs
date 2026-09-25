//! Limine boot protocol requests.
//!
//! The bootloader scans the `.limine_requests` section for these structures
//! and fills in their `response` pointers before jumping to `kmain`.

use core::cell::UnsafeCell;
use core::ptr;

const COMMON_MAGIC: [u64; 2] = [0xc7b1dd30df4c8b88, 0x0a82e883a194f07b];

#[repr(C)]
pub struct Request<R: 'static> {
    id: [u64; 4],
    revision: u64,
    response: UnsafeCell<*const R>,
}

unsafe impl<R> Sync for Request<R> {}

impl<R> Request<R> {
    const fn new(a: u64, b: u64) -> Self {
        Request {
            id: [COMMON_MAGIC[0], COMMON_MAGIC[1], a, b],
            revision: 0,
            response: UnsafeCell::new(ptr::null()),
        }
    }

    pub fn response(&self) -> Option<&'static R> {
        let p = unsafe { ptr::read_volatile(self.response.get()) };
        unsafe { p.as_ref() }
    }
}

#[repr(C)]
pub struct BaseRevision(UnsafeCell<[u64; 3]>);
unsafe impl Sync for BaseRevision {}

impl BaseRevision {
    pub fn is_supported(&self) -> bool {
        unsafe { ptr::read_volatile(&(*self.0.get())[2]) == 0 }
    }
}

#[repr(C)]
pub struct Marker<const N: usize>([u64; N]);

#[used]
#[unsafe(link_section = ".limine_requests_start")]
static START_MARKER: Marker<4> =
    Marker([0xf6b8f4b39de7d1ae, 0xfab91a6940fcb9cf, 0x785c6ed015d3e316, 0x181e920a7852b9d9]);

#[used]
#[unsafe(link_section = ".limine_requests_end")]
static END_MARKER: Marker<2> = Marker([0xadc0e0531bb10d03, 0x9572709f31764c62]);

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static BASE_REVISION: BaseRevision =
    BaseRevision(UnsafeCell::new([0xf9562b2d5c95a6c8, 0x6a7b384944536bdc, 3]));

// ---------------------------------------------------------------------------
// Response layouts
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct HhdmResponse {
    pub revision: u64,
    pub offset: u64,
}

#[repr(C)]
pub struct Framebuffer {
    pub address: *mut u8,
    pub width: u64,
    pub height: u64,
    pub pitch: u64,
    pub bpp: u16,
    pub memory_model: u8,
    pub red_mask_size: u8,
    pub red_mask_shift: u8,
    pub green_mask_size: u8,
    pub green_mask_shift: u8,
    pub blue_mask_size: u8,
    pub blue_mask_shift: u8,
}

#[repr(C)]
pub struct FramebufferResponse {
    pub revision: u64,
    pub count: u64,
    pub framebuffers: *const *const Framebuffer,
}

impl FramebufferResponse {
    pub fn first(&self) -> Option<&'static Framebuffer> {
        if self.count == 0 {
            return None;
        }
        unsafe { (*self.framebuffers).as_ref() }
    }
}

pub const MEMMAP_USABLE: u64 = 0;
pub const MEMMAP_ACPI_RECLAIMABLE: u64 = 2;
pub const MEMMAP_BOOTLOADER_RECLAIMABLE: u64 = 5;
pub const MEMMAP_EXECUTABLE: u64 = 6;
pub const MEMMAP_FRAMEBUFFER: u64 = 7;

#[repr(C)]
pub struct MemmapEntry {
    pub base: u64,
    pub length: u64,
    pub kind: u64,
}

#[repr(C)]
pub struct MemmapResponse {
    pub revision: u64,
    pub count: u64,
    pub entries: *const *const MemmapEntry,
}

impl MemmapResponse {
    pub fn entries(&self) -> impl Iterator<Item = &'static MemmapEntry> + '_ {
        (0..self.count as usize).map(move |i| unsafe { &**self.entries.add(i) })
    }
}

#[repr(C)]
pub struct RsdpResponse {
    pub revision: u64,
    /// Physical address with base revision 3.
    pub address: u64,
}

#[repr(C)]
pub struct CmdlineResponse {
    pub revision: u64,
    pub cmdline: *const u8,
}

impl CmdlineResponse {
    pub fn as_str(&self) -> &'static str {
        if self.cmdline.is_null() {
            return "";
        }
        unsafe {
            let mut len = 0;
            while *self.cmdline.add(len) != 0 {
                len += 1;
            }
            core::str::from_utf8(core::slice::from_raw_parts(self.cmdline, len)).unwrap_or("")
        }
    }
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static HHDM: Request<HhdmResponse> = Request::new(0x48dcf1cb8ad2b852, 0x63984e959a98244b);

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static FRAMEBUFFER: Request<FramebufferResponse> =
    Request::new(0x9d5827dcd881dd75, 0xa3148604f6fab11b);

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static MEMMAP: Request<MemmapResponse> = Request::new(0x67cf3d9d378a806f, 0xe304acdfc50c3c62);

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static RSDP: Request<RsdpResponse> = Request::new(0xc5e77b6b397e7b43, 0x27637845accdcf3c);

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static CMDLINE: Request<CmdlineResponse> = Request::new(0x4b161536e598651e, 0xb390ad4a2f1f303a);

pub fn cmdline() -> &'static str {
    CMDLINE.response().map(|r| r.as_str()).unwrap_or("")
}
