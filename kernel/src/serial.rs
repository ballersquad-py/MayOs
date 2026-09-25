//! COM1 serial port: the first output device we have, and the one the
//! automated tests read from.

use core::fmt::{self, Write};

use crate::arch::cpu::{inb, outb};
use crate::sync::Spin;

const COM1: u16 = 0x3f8;

pub struct Serial;

static LOCK: Spin<()> = Spin::new(());

pub fn init() {
    unsafe {
        outb(COM1 + 1, 0x00); // no interrupts
        outb(COM1 + 3, 0x80); // DLAB on
        outb(COM1, 0x01); // 115200 baud
        outb(COM1 + 1, 0x00);
        outb(COM1 + 3, 0x03); // 8N1
        outb(COM1 + 2, 0xc7); // FIFO
        outb(COM1 + 4, 0x0b);
    }
}

fn put(b: u8) {
    unsafe {
        let mut spins = 0;
        while inb(COM1 + 5) & 0x20 == 0 && spins < 100_000 {
            spins += 1;
        }
        outb(COM1, b);
    }
}

impl Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            if b == b'\n' {
                put(b'\r');
            }
            put(b);
        }
        Ok(())
    }
}

pub fn write_fmt(args: fmt::Arguments) {
    let _g = LOCK.lock();
    let _ = Serial.write_fmt(args);
}

/// Bypass the lock (panic path).
pub fn write_fmt_unlocked(args: fmt::Arguments) {
    let _ = Serial.write_fmt(args);
}
