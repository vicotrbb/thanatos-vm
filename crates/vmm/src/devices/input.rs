//! virtio-input (§5.8): a keyboard and a tablet. Only the VMM writes these queues.
use crate::{
    GuestMemory,
    mmio::{Device, Slot},
};
use std::{collections::VecDeque, io::Write, sync::Arc};
use virtio_queue::{Queue, QueueT};

const CONFIG_ID_NAME: u8 = 1;
const CONFIG_EV_BITS: u8 = 0x11;
const CONFIG_ABS_INFO: u8 = 0x12;
const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
const REL_WHEEL: u16 = 8;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
/// The tablet's coordinate range; sinks scale pixels to it.
pub const ABS_MAX: u32 = 32767;
const EVENT_QUEUE: usize = 0;
/// Bytes of the config's union plus its 8-byte header.
const CONFIG: usize = 136;
/// A guest not draining its queue loses the oldest input, not host memory.
const BACKLOG: usize = 1024;

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Keyboard,
    Tablet,
}

pub struct Input {
    kind: Kind,
    config: [u8; CONFIG],
    pending: VecDeque<[u8; 8]>,
}

fn event(kind: u16, code: u16, value: u32) -> [u8; 8] {
    let mut bytes = [0; 8];
    bytes[..2].copy_from_slice(&kind.to_le_bytes());
    bytes[2..4].copy_from_slice(&code.to_le_bytes());
    bytes[4..].copy_from_slice(&value.to_le_bytes());
    bytes
}

fn set_bits(out: &mut [u8], bits: impl IntoIterator<Item = u16>) -> u8 {
    let mut size = 0;
    for bit in bits {
        let byte = bit as usize / 8;
        out[byte] |= 1 << (bit % 8);
        size = size.max(byte + 1);
    }
    size as u8
}

impl Input {
    pub fn new(kind: Kind) -> Self {
        Self {
            kind,
            config: [0; CONFIG],
            pending: VecDeque::new(),
        }
    }

    fn answer(&mut self, select: u8, subsel: u8) {
        self.config = [0; CONFIG];
        self.config[0] = select;
        self.config[1] = subsel;
        let payload = &mut self.config[8..];
        let size = match (select, self.kind, u16::from(subsel)) {
            (CONFIG_ID_NAME, Kind::Keyboard, _) => {
                payload[..16].copy_from_slice(b"thanatos-keybd\0\0");
                14
            }
            (CONFIG_ID_NAME, Kind::Tablet, _) => {
                payload[..16].copy_from_slice(b"thanatos-tablet\0");
                15
            }
            (CONFIG_EV_BITS, Kind::Keyboard, EV_KEY) => set_bits(payload, 1..256),
            (CONFIG_EV_BITS, Kind::Tablet, EV_KEY) => {
                set_bits(payload, [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE])
            }
            (CONFIG_EV_BITS, Kind::Tablet, EV_REL) => set_bits(payload, [REL_WHEEL]),
            (CONFIG_EV_BITS, Kind::Tablet, EV_ABS) => set_bits(payload, [ABS_X, ABS_Y]),
            (CONFIG_ABS_INFO, Kind::Tablet, ABS_X | ABS_Y) => {
                payload[4..8].copy_from_slice(&ABS_MAX.to_le_bytes());
                20
            }
            _ => 0,
        };
        self.config[2] = size;
    }

    fn flush(&mut self, queue: &mut Queue, memory: &GuestMemory) -> bool {
        let mut used = false;
        while !self.pending.is_empty() {
            let Some(chain) = queue.pop_descriptor_chain(memory) else {
                break;
            };
            let head = chain.head_index();
            let mut written = 0;
            if let (Ok(mut writer), Some(bytes)) = (chain.writer(memory), self.pending.pop_front())
                && writer.write_all(&bytes).is_ok()
            {
                written = 8;
            }
            let _ = queue.add_used(memory, head, written);
            used = true;
        }
        used
    }
}

impl Device for Input {
    const TYPE: u32 = 18;
    const QUEUES: usize = 2;

    fn features(&self) -> u64 {
        0
    }

    fn config(&self) -> &[u8] {
        &self.config
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        let (mut select, mut subsel) = (self.config[0], self.config[1]);
        for (index, &byte) in data.iter().enumerate() {
            match offset + index as u64 {
                0 => select = byte,
                1 => subsel = byte,
                _ => (),
            }
        }
        self.answer(select, subsel);
    }

    fn notify(&mut self, queue: usize, queues: &mut [Queue], memory: &GuestMemory) -> bool {
        if queue == EVENT_QUEUE {
            return self.flush(&mut queues[EVENT_QUEUE], memory);
        }
        // LED and similar status from the guest: nothing here has LEDs.
        let mut used = false;
        while let Some(chain) = queues[queue].pop_descriptor_chain(memory) {
            let _ = queues[queue].add_used(memory, chain.head_index(), 0);
            used = true;
        }
        used
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.config = [0; CONFIG];
    }
}

/// The two devices, as the API and the display sinks see them.
#[derive(Clone)]
pub struct InputPorts {
    pub keyboard: Arc<Slot<Input>>,
    pub tablet: Arc<Slot<Input>>,
}

fn send(slot: &Slot<Input>, events: &[[u8; 8]]) {
    slot.with(|device| {
        for event in events {
            if device.pending.len() == BACKLOG {
                device.pending.pop_front();
            }
            device.pending.push_back(*event);
        }
    });
    slot.run(|device, queues, memory| device.flush(&mut queues[EVENT_QUEUE], memory));
}

impl InputPorts {
    /// Key down or up by Linux keycode.
    pub fn key(&self, code: u16, down: bool) {
        let press = event(EV_KEY, code, down.into());
        send(&self.keyboard, &[press, event(EV_SYN, 0, 0)]);
    }

    /// Absolute position as a fraction of the screen, in the tablet's range.
    pub fn pointer(&self, x: u32, y: u32) {
        let (x, y) = (x.min(ABS_MAX), y.min(ABS_MAX));
        send(
            &self.tablet,
            &[
                event(EV_ABS, ABS_X, x),
                event(EV_ABS, ABS_Y, y),
                event(EV_SYN, 0, 0),
            ],
        );
    }

    /// 0 left, 1 middle, 2 right.
    pub fn button(&self, button: u8, down: bool) {
        let code = match button {
            0 => BTN_LEFT,
            1 => BTN_MIDDLE,
            _ => BTN_RIGHT,
        };
        let press = event(EV_KEY, code, down.into());
        send(&self.tablet, &[press, event(EV_SYN, 0, 0)]);
    }

    /// Wheel clicks, positive up.
    pub fn scroll(&self, clicks: i32) {
        let wheel = event(EV_REL, REL_WHEEL, clicks as u32);
        send(&self.tablet, &[wheel, event(EV_SYN, 0, 0)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mmio::test_support::{memory, queue_with};
    use vm_memory::{Bytes, GuestAddress};

    #[test]
    fn the_keyboard_describes_itself_and_delivers_keys() {
        let memory = memory();
        let mut input = Input::new(Kind::Keyboard);
        input.write_config(0, &[CONFIG_EV_BITS]);
        input.write_config(1, &[EV_KEY as u8]);
        assert_eq!(input.config()[2], 32);
        assert_eq!(input.config()[8 + 3] & 0x40, 0x40); // KEY_ESC.. bit 30 is KEY_A
        input.write_config(0, &[CONFIG_EV_BITS]);
        input.write_config(1, &[EV_ABS as u8]);
        assert_eq!(input.config()[2], 0);

        let (mock, queue) = queue_with(&memory, 0x1000, &[&[(0x10_0000, 8, true)]]);
        let mut queues = [queue];
        input.pending.push_back(event(EV_KEY, 30, 1));
        assert!(input.notify(EVENT_QUEUE, &mut queues, &memory));
        let got: [u8; 8] = memory.read_obj(GuestAddress(0x10_0000)).unwrap();
        assert_eq!(got, event(EV_KEY, 30, 1));
        assert_eq!(mock.used().idx().load(), 1);
    }

    #[test]
    fn the_tablet_reports_its_range() {
        let mut input = Input::new(Kind::Tablet);
        input.write_config(0, &[CONFIG_ABS_INFO, ABS_Y as u8]);
        assert_eq!(input.config()[2], 20);
        assert_eq!(input.config()[12..16], ABS_MAX.to_le_bytes());
    }
}
