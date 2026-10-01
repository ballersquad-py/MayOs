/* MayOS vDSO: clock_gettime without entering the kernel.
 * The kernel maps a data page at VDSO_DATA (see proc/linux.rs) holding the
 * TSC calibration and the wall-clock time at boot.
 * Rebuild with ./build.sh (the .so is committed and embedded). */
struct vd { unsigned long tsc_boot, tsc_per_ms, boot_unix_us; };
struct ts { long sec, nsec; };
struct tv { long sec, usec; };
#define VD ((const volatile struct vd *)0x6ffffffe0000UL)

static inline unsigned long rdtsc(void) {
    unsigned lo, hi;
    __asm__ volatile("rdtsc" : "=a"(lo), "=d"(hi));
    return ((unsigned long)hi << 32) | lo;
}

/* Nanoseconds since boot. */
static inline unsigned long uptime_ns(void) {
    unsigned long t = rdtsc(), b = VD->tsc_boot, pm = VD->tsc_per_ms;
    unsigned long d = t > b ? t - b : 0;
    return d / pm * 1000000UL + (d % pm) * 1000000UL / pm;
}

int __vdso_clock_gettime(int clk, struct ts *ts) {
    unsigned long ns;
    if (VD->tsc_per_ms == 0 && clk < 2) {
        long r;
        __asm__ volatile("syscall" : "=a"(r) : "a"(228), "D"((long)clk), "S"(ts) : "rcx", "r11", "memory");
        return (int)r;
    }
    switch (clk) {
    case 0: case 5: case 8: case 11:
        ns = uptime_ns() + VD->boot_unix_us * 1000UL;
        break;
    case 1: case 4: case 6: case 7: case 9:
        ns = uptime_ns();
        break;
    default: {
        /* CPU-time and other clocks: ask the kernel (glibc does not fall
         * back to the system call when the vDSO fails). */
        long r;
        __asm__ volatile("syscall" : "=a"(r) : "a"(228), "D"((long)clk), "S"(ts) : "rcx", "r11", "memory");
        return (int)r;
    }
    }
    ts->sec = ns / 1000000000UL;
    ts->nsec = ns % 1000000000UL;
    return 0;
}

int __vdso_gettimeofday(struct tv *tv, void *tz) {
    (void)tz;
    if (tv) {
        unsigned long us = uptime_ns() / 1000 + VD->boot_unix_us;
        tv->sec = us / 1000000UL;
        tv->usec = us % 1000000UL;
    }
    return 0;
}

long __vdso_time(long *t) {
    long s = (uptime_ns() / 1000 + VD->boot_unix_us) / 1000000UL;
    if (t)
        *t = s;
    return s;
}

int clock_gettime(int, struct ts *) __attribute__((weak, alias("__vdso_clock_gettime")));
int gettimeofday(struct tv *, void *) __attribute__((weak, alias("__vdso_gettimeofday")));
long time(long *) __attribute__((weak, alias("__vdso_time")));
