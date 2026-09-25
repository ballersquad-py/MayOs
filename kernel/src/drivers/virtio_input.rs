//! virtio input driver, used for the QEMU tablet: absolute pointer
//! coordinates mean no mouse grabbing in the QEMU window.

use alloc::string::String;
use alloc::vec::Vec;

use super::pci::PciDevice;
use super::virtio::{Buf, Transport, Virtqueue};
use crate::input::{self, InputEvent};
use crate::mem::DmaBuf;

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const REL_WHEEL: u16 = 8;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;

const CFG_SELECT: usize = 0;
const CFG_SUBSEL: usize = 1;
const CFG_SIZE: usize = 2;
const CFG_DATA: usize = 8;
const CFG_ID_NAME: u8 = 1;
const CFG_ABS_INFO: u8 = 0x12;

const QUEUE_LEN: u16 = 64;

pub struct VirtioInput {
    t: Transport,
    q: Virtqueue,
    events: DmaBuf,
    pub name: String,
    abs_max: (u32, u32),
    pending_abs: (Option<u32>, Option<u32>),
}

impl VirtioInput {
    pub fn new(pci: &PciDevice) -> Option<VirtioInput> {
        let t = Transport::new(pci)?;
        if !t.negotiate(0) {
            return None;
        }
        let q = t.setup_queue(0, QUEUE_LEN)?;
        let events = DmaBuf::new(QUEUE_LEN as usize * 8);
        let mut dev = VirtioInput { t, q, events, name: String::new(), abs_max: (32767, 32767), pending_abs: (None, None) };
        dev.name = dev.config_string(CFG_ID_NAME, 0);
        if let Some(mx) = dev.abs_max(ABS_X) {
            dev.abs_max.0 = mx.max(1);
        }
        if let Some(my) = dev.abs_max(ABS_Y) {
            dev.abs_max.1 = my.max(1);
        }
        for i in 0..dev.q.size() {
            let b = Buf { phys: dev.events.phys + i as u64 * 8, len: 8, device_writes: true };
            dev.q.submit(&[b]);
        }
        dev.t.driver_ok();
        dev.q.notify();
        Some(dev)
    }

    fn select(&self, sel: u8, sub: u8) -> usize {
        self.t.cfg_write8(CFG_SELECT, sel);
        self.t.cfg_write8(CFG_SUBSEL, sub);
        self.t.cfg_read8(CFG_SIZE) as usize
    }

    fn config_string(&self, sel: u8, sub: u8) -> String {
        let n = self.select(sel, sub);
        let bytes: Vec<u8> = (0..n).map(|i| self.t.cfg_read8(CFG_DATA + i)).collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn abs_max(&self, axis: u16) -> Option<u32> {
        if self.select(CFG_ABS_INFO, axis as u8) < 8 {
            return None;
        }
        Some(self.t.cfg_read32(CFG_DATA + 4))
    }

    pub fn is_tablet(&self) -> bool {
        self.select(CFG_ABS_INFO, ABS_X as u8) >= 8
    }

    /// Drain completed events into the global input queue.
    pub fn poll(&mut self) {
        let mut any = false;
        while let Some((id, _)) = self.q.pop_used() {
            let p = self.events.virt() + id as u64 * 8;
            let (kind, code, value) = unsafe {
                (
                    core::ptr::read_volatile(p as *const u16),
                    core::ptr::read_volatile((p + 2) as *const u16),
                    core::ptr::read_volatile((p + 4) as *const u32),
                )
            };
            self.handle(kind, code, value);
            let b = Buf { phys: self.events.phys + id as u64 * 8, len: 8, device_writes: true };
            self.q.submit(&[b]);
            any = true;
        }
        if any {
            self.q.notify();
        }
    }

    fn handle(&mut self, kind: u16, code: u16, value: u32) {
        match kind {
            EV_ABS if code == ABS_X => self.pending_abs.0 = Some(value),
            EV_ABS if code == ABS_Y => self.pending_abs.1 = Some(value),
            EV_KEY => {
                let button = match code {
                    BTN_LEFT => 0,
                    BTN_RIGHT => 1,
                    BTN_MIDDLE => 2,
                    _ => return,
                };
                input::push(InputEvent::MouseButton { button, pressed: value != 0 });
            }
            EV_REL if code == REL_WHEEL => input::push(InputEvent::Wheel(-(value as i32))),
            EV_SYN => {
                if self.pending_abs.0.is_some() || self.pending_abs.1.is_some() {
                    let x = self.pending_abs.0.map(|v| (v as u64 * 65535 / self.abs_max.0 as u64) as u32);
                    let y = self.pending_abs.1.map(|v| (v as u64 * 65535 / self.abs_max.1 as u64) as u32);
                    input::push(InputEvent::MouseAbsolute { x, y });
                    self.pending_abs = (None, None);
                }
            }
            _ => {}
        }
    }
}
