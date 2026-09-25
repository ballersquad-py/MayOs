//! The C library functions QuickJS needs that live on the Rust side:
//! memory, time and maths (from the `libm` crate).

use core::alloc::Layout;

const HEADER: usize = 16;

#[unsafe(no_mangle)]
extern "C" fn malloc(size: usize) -> *mut u8 {
    let Ok(layout) = Layout::from_size_align(size + HEADER, 16) else { return core::ptr::null_mut() };
    let p = unsafe { alloc::alloc::alloc(layout) };
    if p.is_null() {
        return p;
    }
    unsafe {
        *(p as *mut usize) = size;
        p.add(HEADER)
    }
}

#[unsafe(no_mangle)]
extern "C" fn calloc(n: usize, size: usize) -> *mut u8 {
    let total = n.saturating_mul(size);
    let p = malloc(total);
    if !p.is_null() {
        unsafe { core::ptr::write_bytes(p, 0, total) };
    }
    p
}

#[unsafe(no_mangle)]
extern "C" fn free(p: *mut u8) {
    if p.is_null() {
        return;
    }
    unsafe {
        let base = p.sub(HEADER);
        let size = *(base as *const usize);
        alloc::alloc::dealloc(base, Layout::from_size_align_unchecked(size + HEADER, 16));
    }
}

#[unsafe(no_mangle)]
extern "C" fn realloc(p: *mut u8, size: usize) -> *mut u8 {
    if p.is_null() {
        return malloc(size);
    }
    if size == 0 {
        free(p);
        return core::ptr::null_mut();
    }
    unsafe {
        let base = p.sub(HEADER);
        let old = *(base as *const usize);
        let q = alloc::alloc::realloc(base, Layout::from_size_align_unchecked(old + HEADER, 16), size + HEADER);
        if q.is_null() {
            return q;
        }
        *(q as *mut usize) = size;
        q.add(HEADER)
    }
}

#[unsafe(no_mangle)]
extern "C" fn malloc_usable_size(p: *mut u8) -> usize {
    if p.is_null() { 0 } else { unsafe { *(p.sub(HEADER) as *const usize) } }
}

#[unsafe(no_mangle)]
extern "C" fn abort() -> ! {
    panic!("javascript engine aborted");
}

#[unsafe(no_mangle)]
extern "C" fn exit(_: i32) -> ! {
    panic!("javascript engine called exit");
}

#[unsafe(no_mangle)]
extern "C" fn mayos_log(p: *const u8, n: usize) {
    let s = unsafe { core::slice::from_raw_parts(p, n) };
    crate::kprint!("{}", alloc::string::String::from_utf8_lossy(s));
}

#[repr(C)]
struct Timeval {
    sec: i64,
    usec: i64,
}

/// Seconds since 1970 from the CMOS clock.
fn unix_now() -> i64 {
    let t = crate::arch::rtc::now();
    let days = days_from_civil(t.year as i64, t.month as i64, t.day as i64);
    days * 86400 + t.hour as i64 * 3600 + t.minute as i64 * 60 + t.second as i64
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[unsafe(no_mangle)]
extern "C" fn gettimeofday(tv: *mut Timeval, _tz: *mut u8) -> i32 {
    if !tv.is_null() {
        let us = crate::time::uptime_us();
        unsafe {
            (*tv).sec = unix_now();
            (*tv).usec = (us % 1_000_000) as i64;
        }
    }
    0
}

#[repr(C)]
struct Tm {
    sec: i32,
    min: i32,
    hour: i32,
    mday: i32,
    mon: i32,
    year: i32,
    wday: i32,
    yday: i32,
    isdst: i32,
    gmtoff: i64,
    zone: *const u8,
}

#[unsafe(no_mangle)]
extern "C" fn localtime_r(t: *const i64, out: *mut Tm) -> *mut Tm {
    let secs = unsafe { *t };
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    // Civil from days (Howard Hinnant).
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    unsafe {
        *out = Tm {
            sec: (rem % 60) as i32,
            min: (rem / 60 % 60) as i32,
            hour: (rem / 3600) as i32,
            mday: d as i32,
            mon: (m - 1) as i32,
            year: (y - 1900) as i32,
            wday: ((days + 4).rem_euclid(7)) as i32,
            yday: 0,
            isdst: 0,
            gmtoff: 0,
            zone: c"UTC".as_ptr() as *const u8,
        };
    }
    out
}

macro_rules! math1 {
    ($($name:ident),*) => { $(
        #[unsafe(no_mangle)]
        extern "C" fn $name(x: f64) -> f64 { libm::$name(x) }
    )* };
}
macro_rules! math2 {
    ($($name:ident),*) => { $(
        #[unsafe(no_mangle)]
        extern "C" fn $name(x: f64, y: f64) -> f64 { libm::$name(x, y) }
    )* };
}
math1!(acos, acosh, asin, asinh, atan, atanh, cbrt, ceil, cos, cosh, exp, expm1, fabs, floor, log, log10, log1p, log2, round, sin, sinh, sqrt, tan, tanh, trunc);
math2!(atan2, fmod, hypot, pow, fmax, fmin);

#[unsafe(no_mangle)]
extern "C" fn lrint(x: f64) -> i64 {
    libm::rint(x) as i64
}
