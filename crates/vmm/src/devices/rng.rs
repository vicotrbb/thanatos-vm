//! virtio-rng (§5.4): fills what the driver offers from the host's generator.
use crate::{GuestMemory, mmio::Device};
use std::{fs::File, io::Read};
use virtio_queue::{Queue, QueueOwnedT, QueueT};

const CHUNK: usize = 4096;

pub struct Rng(File);

impl Rng {
    pub fn new() -> std::io::Result<Self> {
        File::open("/dev/urandom").map(Self)
    }
}

impl Device for Rng {
    const TYPE: u32 = 4;
    const QUEUES: usize = 1;

    fn features(&self) -> u64 {
        0
    }

    fn config(&self) -> &[u8] {
        &[]
    }

    fn notify(&mut self, _: usize, queues: &mut [Queue], memory: &GuestMemory) -> bool {
        let mut used = Vec::new();
        let Ok(iter) = queues[0].iter(memory) else {
            return false;
        };
        for chain in iter {
            let head = chain.head_index();
            let mut written = 0;
            if let Ok(mut writer) = chain.writer(memory) {
                // One chunk per chain: the driver only needs some bytes, and its size is its own.
                let mut bytes = [0; CHUNK];
                let count = CHUNK.min(writer.available_bytes());
                if self.0.read_exact(&mut bytes[..count]).is_ok()
                    && std::io::Write::write_all(&mut writer, &bytes[..count]).is_ok()
                {
                    written = count;
                }
            }
            used.push((head, written as u32));
        }
        let any = !used.is_empty();
        for (head, length) in used {
            let _ = queues[0].add_used(memory, head, length);
        }
        any
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mmio::test_support::{memory, queue_with};
    use vm_memory::{Bytes, GuestAddress};

    #[test]
    fn fills_the_offered_buffer_and_nothing_past_it() {
        let memory = memory();
        let (mock, queue) = queue_with(&memory, 0x1000, &[&[(0x10_0000, 64, true)]]);
        let mut queues = [queue];
        assert!(Rng::new().unwrap().notify(0, &mut queues, &memory));
        let mut bytes = [0; 65];
        memory
            .read_slice(&mut bytes, GuestAddress(0x10_0000))
            .unwrap();
        assert!(bytes[..64].iter().any(|&b| b != 0));
        assert_eq!(bytes[64], 0);
        assert_eq!(mock.used().idx().load(), 1);
    }
}
