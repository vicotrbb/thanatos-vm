//! Drives a device the way a guest does: through its MMIO registers, over guest memory holding
//! the fuzzer's bytes. The first bytes are register writes, the rest is RAM.
use std::sync::Arc;
use thanatos_vmm::{
    GuestMemory,
    mmio::{Device, Irq, Mmio, Slot},
};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

const RAM: usize = 1 << 20;
const QUEUE_SPACING: u64 = 0x1_0000;

pub fn drive<D: Device>(device: D, data: &[u8], queues: u32) {
    if data.len() < 32 {
        return;
    }
    let (registers, ram) = data.split_at(32);
    let memory: GuestMemory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), RAM)]).unwrap();
    memory
        .write_slice(&ram[..ram.len().min(RAM)], GuestAddress(0))
        .unwrap();
    let irq: Irq = Arc::new(|_, _| ());
    let slot = Slot::new(device, memory.clone(), irq, 0);
    let write = |offset: u64, value: u32| slot.write(offset, &value.to_le_bytes());
    for status in [1, 3] {
        write(0x70, status);
    }
    write(0x24, 1);
    write(0x20, 1); // VIRTIO_F_VERSION_1
    write(0x70, 11);
    for queue in 0..queues {
        let base = u64::from(queue) * QUEUE_SPACING;
        write(0x30, queue);
        write(0x38, 256);
        write(0x80, base as u32);
        write(0x90, (base + 0x1000) as u32);
        write(0xa0, (base + 0x2000) as u32);
        write(0x44, 1);
        // Publish a fuzzer-chosen number of available buffers.
        let count = u16::from(registers[queue as usize % 32]) % 64;
        let _ = memory.write_obj(count, GuestAddress(base + 0x1002));
    }
    write(0x70, 15);
    // What remains of the header is arbitrary register traffic, kicks included.
    let mut read = [0; 4];
    for pair in registers.chunks_exact(4) {
        let offset = u64::from(pair[0]) * 4;
        let value = u32::from(pair[1]);
        slot.read(offset, &mut read);
        slot.write(offset, &value.to_le_bytes());
        slot.write(0x50, &(u32::from(pair[2]) % queues.max(1)).to_le_bytes());
    }
    for queue in 0..queues {
        slot.write(0x50, &queue.to_le_bytes());
    }
}
