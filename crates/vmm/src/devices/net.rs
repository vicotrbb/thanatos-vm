//! virtio-net (§5.1) with offloads off. A frame's only device-specific part is its 12-byte header.
use crate::{
    GuestMemory,
    mmio::{Device, Slot},
};
use std::{
    collections::VecDeque,
    io::{Read, Write},
    sync::{Arc, Weak},
};
use virtio_queue::{Queue, QueueT};

const F_MAC: u64 = 1 << 5;
const HEADER: usize = 12;
/// Largest frame the guest may hand over without offloads (it negotiated none), with margin for
/// a VLAN tag.
const MAX_FRAME: usize = 1518;
/// What waits for receive buffers; past this frames drop, as on a real NIC.
const BACKLOG: usize = 1024;
const RX: usize = 0;
const TX: usize = 1;

pub struct Net {
    config: [u8; 6],
    to_host: Box<dyn FnMut(Vec<u8>) + Send>,
    to_guest: VecDeque<Vec<u8>>,
}

impl Net {
    pub fn new(mac: [u8; 6], to_host: impl FnMut(Vec<u8>) + Send + 'static) -> Self {
        Self {
            config: mac,
            to_host: Box::new(to_host),
            to_guest: VecDeque::new(),
        }
    }

    pub fn deliver(&mut self, frame: Vec<u8>) {
        if self.to_guest.len() < BACKLOG {
            self.to_guest.push_back(frame);
        }
    }

    pub fn flush(&mut self, queue: &mut Queue, memory: &GuestMemory) -> bool {
        let mut used = false;
        while let Some(frame) = self.to_guest.front() {
            let Some(chain) = queue.pop_descriptor_chain(memory) else {
                break;
            };
            let head = chain.head_index();
            let mut written = 0;
            if let Ok(mut writer) = chain.writer(memory) {
                let mut header = [0; HEADER];
                header[10] = 1; // num_buffers
                if writer.available_bytes() >= HEADER + frame.len()
                    && writer.write_all(&header).is_ok()
                    && writer.write_all(frame).is_ok()
                {
                    written = HEADER + frame.len();
                }
            }
            // A chain too small to hold the frame loses it, which is the driver's own doing.
            self.to_guest.pop_front();
            let _ = queue.add_used(memory, head, written as u32);
            used = true;
        }
        used
    }
}

impl Device for Net {
    const TYPE: u32 = 1;
    const QUEUES: usize = 2;

    fn features(&self) -> u64 {
        F_MAC
    }

    fn config(&self) -> &[u8] {
        &self.config
    }

    fn notify(&mut self, queue: usize, queues: &mut [Queue], memory: &GuestMemory) -> bool {
        let mut used = false;
        if queue == TX {
            while let Some(chain) = queues[TX].pop_descriptor_chain(memory) {
                let head = chain.head_index();
                if let Ok(mut reader) = chain.reader(memory) {
                    let length = reader.available_bytes();
                    if (HEADER..=HEADER + MAX_FRAME).contains(&length) {
                        let mut frame = vec![0; length];
                        if reader.read_exact(&mut frame).is_ok() {
                            frame.drain(..HEADER);
                            (self.to_host)(frame);
                        }
                    }
                }
                let _ = queues[TX].add_used(memory, head, 0);
                used = true;
            }
        }
        self.flush(&mut queues[RX], memory) || used
    }

    fn reset(&mut self) {
        self.to_guest.clear();
    }
}

/// What the network sends the guest goes to the device through a weak link, so the running
/// network does not keep the device alive.
pub fn deliverer(slot: Weak<Slot<Net>>) -> impl FnMut(Vec<u8>) + Send + 'static {
    move |frame| {
        if let Some(slot) = slot.upgrade() {
            slot.with(|device| device.deliver(frame));
            slot.run(|device, queues, memory: &GuestMemory| device.flush(&mut queues[RX], memory));
        }
    }
}

/// Reads frames from a tap until it closes.
pub fn pump_tap(mut tap: std::fs::File, slot: Arc<Slot<Net>>) {
    std::thread::spawn(move || {
        let mut buffer = vec![0; 65536];
        while let Ok(count @ 1..) = tap.read(&mut buffer) {
            let mut deliver = deliverer(Arc::downgrade(&slot));
            deliver(buffer[..count].to_vec());
        }
    });
}

/// The MAC of the guest: from the configuration, or derived from the VM name.
pub fn mac_from_name(name: &str) -> [u8; 6] {
    let hash = name.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    });
    let bytes = hash.to_le_bytes();
    // Locally administered, unicast.
    [0x52, 0x54, bytes[0], bytes[1], bytes[2], bytes[3]]
}

/// Frames from the guest onto a tap.
pub fn write_tap(mut tap: std::fs::File) -> impl FnMut(Vec<u8>) + Send + 'static {
    move |frame| {
        let _ = tap.write_all(&frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mmio::test_support::{memory, queue_with};
    use std::sync::mpsc;
    use vm_memory::{Bytes, GuestAddress};

    #[test]
    fn frames_cross_both_ways_and_oversized_ones_are_dropped() {
        let memory = memory();
        let (sent, heard) = mpsc::channel();
        let mut net = Net::new([2, 0, 0, 0, 0, 1], move |frame| sent.send(frame).unwrap());
        memory
            .write_slice(b"frame", GuestAddress(0x10_0000 + 12))
            .unwrap();
        let (_tx, tx) = queue_with(
            &memory,
            0x1000,
            &[
                &[(0x10_0000, 17, false)],
                &[(0x11_0000, 4, false)],     // shorter than a header
                &[(0x12_0000, 20000, false)], // longer than a frame
            ],
        );
        let (rx_mock, rx) = queue_with(
            &memory,
            0x8000,
            &[&[(0x20_0000, 2048, true)], &[(0x21_0000, 16, true)]],
        );
        let mut queues = [rx, tx];
        assert!(net.notify(TX, &mut queues, &memory));
        assert_eq!(heard.recv().unwrap(), b"frame");
        assert!(heard.try_recv().is_err());
        net.deliver(b"answer".to_vec());
        net.deliver(vec![0; 100]); // does not fit its 16-byte chain
        assert!(net.flush(&mut queues[RX], &memory));
        let mut back = [0; 6];
        memory
            .read_slice(&mut back, GuestAddress(0x20_0000 + 12))
            .unwrap();
        assert_eq!(&back, b"answer");
        assert_eq!(rx_mock.used().idx().load(), 2);
        assert!(net.to_guest.is_empty());
    }

    #[test]
    fn a_name_gives_a_stable_local_unicast_mac() {
        let mac = mac_from_name("dev");
        assert_eq!(mac, mac_from_name("dev"));
        assert_ne!(mac, mac_from_name("other"));
        assert_eq!(mac[0] & 3, 2);
    }
}
