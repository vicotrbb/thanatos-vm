//! Virtio over MMIO (virtio 1.2 §4.2, version 2 registers). A [`Slot`] holds the registers and
//! virtqueues of one [`Device`], which sees only its queues and guest memory.
use crate::GuestMemory;
use std::sync::{Arc, Mutex};
use virtio_queue::{Queue, QueueT};

const MAGIC: u32 = 0x7472_6976; // "virt"
pub const VERSION_1: u64 = 1 << 32;
const FEATURES_OK: u32 = 8;
const DRIVER_OK: u32 = 4;
const USED_BUFFER: u32 = 1;
const CONFIG: u64 = 0x100;
pub const QUEUE_SIZE: u16 = 256;

/// Drives one interrupt line.
pub type Irq = Arc<dyn Fn(u32, bool) + Send + Sync>;

pub trait Device: Send + 'static {
    const TYPE: u32;
    const QUEUES: usize;
    /// Device-specific feature bits; the transport adds VIRTIO_F_VERSION_1.
    fn features(&self) -> u64;
    fn config(&self) -> &[u8];
    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}
    /// Serves the driver's notification; returns whether the device used buffers.
    fn notify(&mut self, queue: usize, queues: &mut [Queue], memory: &GuestMemory) -> bool;
    fn reset(&mut self) {}
}

/// A window on the bus as the vCPU sees it.
pub trait Mmio: Send + Sync {
    fn read(&self, offset: u64, data: &mut [u8]);
    fn write(&self, offset: u64, data: &[u8]);
}

/// One device on the bus, shared by the vCPU and device threads.
pub struct Slot<D> {
    state: Mutex<State<D>>,
    memory: GuestMemory,
    irq: Irq,
    line: u32,
}

struct State<D> {
    device: D,
    queues: Vec<Queue>,
    status: u32,
    interrupt: u32,
    features_select: u32,
    driver_select: u32,
    driver_features: u64,
    queue_select: u32,
}

fn fresh_queues(count: usize) -> Vec<Queue> {
    (0..count)
        .map(|_| Queue::new(QUEUE_SIZE).expect("256 is a power of two"))
        .collect()
}

impl<D: Device> Slot<D> {
    pub fn new(device: D, memory: GuestMemory, irq: Irq, line: u32) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                device,
                queues: fresh_queues(D::QUEUES),
                status: 0,
                interrupt: 0,
                features_select: 0,
                driver_select: 0,
                driver_features: 0,
                queue_select: 0,
            }),
            memory,
            irq,
            line,
        })
    }

    /// Device work outside a driver notification, such as a received frame; `work` returns
    /// whether it used buffers. Does nothing until the driver is ready.
    pub fn run(&self, work: impl FnOnce(&mut D, &mut [Queue], &GuestMemory) -> bool) {
        let mut state = self.state.lock().unwrap();
        let state = &mut *state;
        if state.status & DRIVER_OK != 0 && work(&mut state.device, &mut state.queues, &self.memory)
        {
            self.raise(state);
        }
    }

    /// Access to the device regardless of driver state.
    pub fn with<R>(&self, work: impl FnOnce(&mut D) -> R) -> R {
        work(&mut self.state.lock().unwrap().device)
    }

    fn raise(&self, state: &mut State<D>) {
        state.interrupt |= USED_BUFFER;
        (self.irq)(self.line, true);
    }
}

impl<D: Device> Mmio for Slot<D> {
    fn read(&self, offset: u64, data: &mut [u8]) {
        let state = self.state.lock().unwrap();
        if offset >= CONFIG {
            let config = state.device.config();
            for (index, byte) in data.iter_mut().enumerate() {
                let at = (offset - CONFIG) as usize + index;
                *byte = config.get(at).copied().unwrap_or(0);
            }
            return;
        }
        let value = state.register(offset);
        for (byte, value) in data.iter_mut().zip(value.to_le_bytes()) {
            *byte = value;
        }
    }

    fn write(&self, offset: u64, data: &[u8]) {
        let mut state = self.state.lock().unwrap();
        let state = &mut *state;
        if offset >= CONFIG {
            state.device.write_config(offset - CONFIG, data);
            return;
        }
        // Registers are 32-bit.
        let Ok(bytes) = <[u8; 4]>::try_from(data) else {
            return;
        };
        let value = u32::from_le_bytes(bytes);
        match offset {
            0x014 => state.features_select = value,
            0x020 => state.set_driver_features(value),
            0x024 => state.driver_select = value,
            0x030 => state.queue_select = value,
            0x050 => {
                let queue = value as usize;
                let ready = state.queues.get(queue).is_some_and(|q| q.ready());
                if ready
                    && state.status & DRIVER_OK != 0
                    && state.device.notify(queue, &mut state.queues, &self.memory)
                {
                    self.raise(state);
                }
            }
            0x064 => {
                state.interrupt &= !value;
                if state.interrupt == 0 {
                    (self.irq)(self.line, false);
                }
            }
            0x070 => {
                if value == 0 {
                    (self.irq)(self.line, false);
                }
                state.set_status(value);
            }
            _ => state.write_queue(offset, value, &self.memory),
        }
    }
}

impl<D: Device> State<D> {
    fn register(&self, offset: u64) -> u32 {
        let queue = self.queues.get(self.queue_select as usize);
        match offset {
            0x000 => MAGIC,
            0x004 => 2,
            0x008 => D::TYPE,
            0x00c => u32::from_le_bytes(*b"THNT"),
            0x010 => match self.features_select {
                0 => self.features() as u32,
                1 => (self.features() >> 32) as u32,
                _ => 0,
            },
            0x034 => queue.map_or(0, |_| QUEUE_SIZE.into()),
            0x044 => queue.is_some_and(|q| q.ready()).into(),
            0x060 => self.interrupt,
            0x070 => self.status,
            // SHMLen of a shared memory region the device lacks (§4.2.2).
            0x0b0 | 0x0b4 => u32::MAX,
            _ => 0,
        }
    }

    fn features(&self) -> u64 {
        VERSION_1 | self.device.features()
    }

    fn set_driver_features(&mut self, value: u32) {
        match self.driver_select {
            0 => self.driver_features = self.driver_features & !0xffff_ffff | u64::from(value),
            1 => self.driver_features = self.driver_features & 0xffff_ffff | u64::from(value) << 32,
            _ => (),
        }
    }

    fn set_status(&mut self, value: u32) {
        if value == 0 {
            self.queues = fresh_queues(D::QUEUES);
            self.status = 0;
            self.interrupt = 0;
            self.features_select = 0;
            self.driver_select = 0;
            self.driver_features = 0;
            self.queue_select = 0;
            self.device.reset();
            return;
        }
        let acceptable =
            self.driver_features & !self.features() == 0 && self.driver_features & VERSION_1 != 0;
        // Refusing FEATURES_OK is how a device rejects the driver's feature set (§3.1.1).
        self.status = if acceptable {
            value
        } else {
            value & !FEATURES_OK
        };
    }

    fn write_queue(&mut self, offset: u64, value: u32, memory: &GuestMemory) {
        let Some(queue) = self.queues.get_mut(self.queue_select as usize) else {
            return;
        };
        match offset {
            0x038 => queue.set_size(value.try_into().unwrap_or(0)),
            // An invalid ring stays not ready, so no device ever walks it.
            0x044 => {
                // `is_valid` is false for a queue not yet ready, so the order matters.
                queue.set_ready(value == 1);
                if !queue.is_valid(memory) {
                    queue.set_ready(false);
                }
            }
            0x080 => queue.set_desc_table_address(Some(value), None),
            0x084 => queue.set_desc_table_address(None, Some(value)),
            0x090 => queue.set_avail_ring_address(Some(value), None),
            0x094 => queue.set_avail_ring_address(None, Some(value)),
            0x0a0 => queue.set_used_ring_address(Some(value), None),
            0x0a4 => queue.set_used_ring_address(None, Some(value)),
            _ => (),
        }
    }
}

#[cfg(test)]
pub mod test_support {
    use super::*;
    use virtio_queue::{desc::RawDescriptor, mock::MockSplitQueue};
    use vm_memory::{GuestAddress, GuestMemoryMmap};

    pub fn memory() -> GuestMemory {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 1 << 22)]).unwrap()
    }

    /// A queue over fake guest memory with chains built from (address, length, device-writable).
    pub fn queue_with<'a>(
        memory: &'a GuestMemory,
        base: u64,
        chains: &[&[(u64, u32, bool)]],
    ) -> (MockSplitQueue<'a, GuestMemory>, Queue) {
        let mock = MockSplitQueue::create(memory, GuestAddress(base), QUEUE_SIZE);
        let mut index = 0u16;
        let mut descriptors = Vec::new();
        for chain in chains {
            for (position, &(address, length, writable)) in chain.iter().enumerate() {
                let mut flags = 0;
                if writable {
                    flags |= virtio_bindings::bindings::virtio_ring::VRING_DESC_F_WRITE as u16;
                }
                let more = position + 1 < chain.len();
                if more {
                    flags |= virtio_bindings::bindings::virtio_ring::VRING_DESC_F_NEXT as u16;
                }
                let next = if more { index + 1 } else { 0 };
                descriptors.push(RawDescriptor::from(
                    virtio_queue::desc::split::Descriptor::new(address, length, flags, next),
                ));
                index += 1;
            }
        }
        mock.build_multiple_desc_chains(&descriptors).unwrap();
        let queue = mock.create_queue().unwrap();
        (mock, queue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Counter(u32);

    impl Device for Counter {
        const TYPE: u32 = 99;
        const QUEUES: usize = 1;
        fn features(&self) -> u64 {
            1
        }
        fn config(&self) -> &[u8] {
            b"cfg"
        }
        fn notify(&mut self, _: usize, _: &mut [Queue], _: &GuestMemory) -> bool {
            self.0 += 1;
            true
        }
        fn reset(&mut self) {
            self.0 = 0;
        }
    }

    fn read(slot: &Slot<Counter>, offset: u64) -> u32 {
        let mut data = [0; 4];
        slot.read(offset, &mut data);
        u32::from_le_bytes(data)
    }

    fn write(slot: &Slot<Counter>, offset: u64, value: u32) {
        slot.write(offset, &value.to_le_bytes());
    }

    #[test]
    fn negotiates_notifies_interrupts_and_resets() {
        let line = Arc::new(AtomicBool::new(false));
        let seen = line.clone();
        let irq: Irq = Arc::new(move |_, level| seen.store(level, Ordering::SeqCst));
        let slot = Slot::new(Counter(0), test_support::memory(), irq, 5);
        assert_eq!(read(&slot, 0), MAGIC);
        assert_eq!(read(&slot, 4), 2);
        assert_eq!(read(&slot, 8), 99);
        write(&slot, 0x14, 1);
        assert_eq!(read(&slot, 0x10), 1);

        // A driver without VERSION_1 is refused at FEATURES_OK.
        write(&slot, 0x70, FEATURES_OK);
        assert_eq!(read(&slot, 0x70) & FEATURES_OK, 0);
        write(&slot, 0x24, 1);
        write(&slot, 0x20, 1);
        write(&slot, 0x70, FEATURES_OK);
        assert_eq!(read(&slot, 0x70) & FEATURES_OK, FEATURES_OK);

        write(&slot, 0x38, 16);
        write(&slot, 0x80, 0x1000);
        write(&slot, 0x90, 0x2000);
        write(&slot, 0xa0, 0x3000);
        write(&slot, 0x44, 1);
        assert_eq!(read(&slot, 0x44), 1);

        // Before DRIVER_OK a notification does nothing; a wild queue index never panics.
        write(&slot, 0x50, 0);
        assert!(!line.load(Ordering::SeqCst));
        write(&slot, 0x70, FEATURES_OK | DRIVER_OK);
        write(&slot, 0x50, 7);
        write(&slot, 0x50, 0);
        assert!(line.load(Ordering::SeqCst));
        assert_eq!(read(&slot, 0x60), USED_BUFFER);
        write(&slot, 0x64, USED_BUFFER);
        assert!(!line.load(Ordering::SeqCst));

        let mut config = [0; 2];
        slot.read(0x101, &mut config);
        assert_eq!(&config, b"fg");

        write(&slot, 0x70, 0);
        assert_eq!(read(&slot, 0x44), 0);
        assert_eq!(slot.with(|d| d.0), 0);
    }

    #[test]
    fn a_ring_outside_guest_memory_never_becomes_ready() {
        let irq: Irq = Arc::new(|_, _| ());
        let slot = Slot::new(Counter(0), test_support::memory(), irq, 5);
        write(&slot, 0x38, 16);
        write(&slot, 0x80, 0x7fff_f000);
        write(&slot, 0x44, 1);
        assert_eq!(read(&slot, 0x44), 0);
    }
}
