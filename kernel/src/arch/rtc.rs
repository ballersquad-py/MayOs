//! CMOS real-time clock (wall-clock date and time).

use super::cpu::{inb, outb};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

fn read_reg(r: u8) -> u8 {
    unsafe {
        outb(0x70, r);
        inb(0x71)
    }
}

fn updating() -> bool {
    read_reg(0x0a) & 0x80 != 0
}

fn raw() -> [u8; 7] {
    while updating() {}
    [read_reg(0x00), read_reg(0x02), read_reg(0x04), read_reg(0x07), read_reg(0x08), read_reg(0x09), read_reg(0x32)]
}

pub fn now() -> DateTime {
    // Read twice until stable to avoid tearing across an update.
    let mut a = raw();
    loop {
        let b = raw();
        if a == b {
            break;
        }
        a = b;
    }
    let status_b = read_reg(0x0b);
    let bcd = status_b & 0x04 == 0;
    let conv = |v: u8| if bcd { (v & 0x0f) + (v >> 4) * 10 } else { v };
    let mut hour = a[2];
    let pm = hour & 0x80 != 0;
    hour = conv(hour & 0x7f);
    if status_b & 0x02 == 0 && pm {
        hour = (hour % 12) + 12;
    }
    let century = if a[6] != 0 { conv(a[6]) as u16 } else { 20 };
    DateTime {
        second: conv(a[0]),
        minute: conv(a[1]),
        hour,
        day: conv(a[3]),
        month: conv(a[4]),
        year: century * 100 + conv(a[5]) as u16,
    }
}
