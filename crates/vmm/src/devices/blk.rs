//! virtio-blk (§5.2) over a raw file.
use crate::{GuestMemory, mmio::Device};
use std::{
    fs::File,
    io::{Read, Write},
    os::unix::fs::FileExt,
};
use virtio_queue::{Queue, QueueOwnedT, QueueT};

const F_SEG_MAX: u64 = 1 << 2;
const F_RO: u64 = 1 << 5;
const F_FLUSH: u64 = 1 << 9;
const SECTOR: u64 = 512;
const IN: u32 = 0;
const OUT: u32 = 1;
const FLUSH: u32 = 4;
const GET_ID: u32 = 8;
const OK: u8 = 0;
const IOERR: u8 = 1;
const UNSUPPORTED: u8 = 2;
// A guest chain can claim gigabytes; copying in chunks keeps the host allocation fixed.
const CHUNK: usize = 1 << 20;

pub struct Blk {
    file: File,
    read_only: bool,
    config: [u8; 16],
    id: [u8; 20],
}

impl Blk {
    pub fn new(file: File, read_only: bool, id: &str) -> std::io::Result<Self> {
        let sectors = file.metadata()?.len() / SECTOR;
        let mut config = [0; 16];
        config[..8].copy_from_slice(&sectors.to_le_bytes());
        config[12..16].copy_from_slice(&126u32.to_le_bytes()); // seg_max: queue size minus two
        let mut name = [0; 20];
        let len = id.len().min(20);
        name[..len].copy_from_slice(&id.as_bytes()[..len]);
        Ok(Self {
            file,
            read_only,
            config,
            id: name,
        })
    }

    fn capacity(&self) -> u64 {
        u64::from_le_bytes(self.config[..8].try_into().unwrap())
    }

    fn serve(
        &self,
        kind: u32,
        sector: u64,
        reader: &mut impl Read,
        data: &mut impl Write,
        len: usize,
        incoming: usize,
    ) -> u8 {
        let in_range = |bytes: usize| {
            sector
                .checked_add((bytes as u64).div_ceil(SECTOR))
                .is_some_and(|end| end <= self.capacity())
        };
        let Some(start) = sector.checked_mul(SECTOR) else {
            return IOERR;
        };
        let mut buffer = vec![0; CHUNK.min(len.max(incoming).max(1))];
        match kind {
            IN if in_range(len) => {
                let mut done = 0;
                while done < len {
                    let count = buffer.len().min(len - done);
                    if self
                        .file
                        .read_exact_at(&mut buffer[..count], start + done as u64)
                        .is_err()
                        || data.write_all(&buffer[..count]).is_err()
                    {
                        return IOERR;
                    }
                    done += count;
                }
                OK
            }
            OUT if self.read_only => IOERR,
            OUT => {
                let mut offset = start;
                loop {
                    let count = match reader.read(&mut buffer) {
                        Ok(0) => return OK,
                        Ok(count) => count,
                        Err(_) => return IOERR,
                    };
                    let end = (offset + count as u64).div_ceil(SECTOR);
                    if end > self.capacity()
                        || self.file.write_all_at(&buffer[..count], offset).is_err()
                    {
                        return IOERR;
                    }
                    offset += count as u64;
                }
            }
            FLUSH => match self.file.sync_data() {
                Ok(()) => OK,
                Err(_) => IOERR,
            },
            GET_ID => match data.write_all(&self.id[..len.min(20)]) {
                Ok(()) => OK,
                Err(_) => IOERR,
            },
            IN => IOERR,
            _ => UNSUPPORTED,
        }
    }
}

impl Device for Blk {
    const TYPE: u32 = 2;
    const QUEUES: usize = 1;

    fn features(&self) -> u64 {
        F_SEG_MAX | F_FLUSH | if self.read_only { F_RO } else { 0 }
    }

    fn config(&self) -> &[u8] {
        &self.config
    }

    fn notify(&mut self, _: usize, queues: &mut [Queue], memory: &GuestMemory) -> bool {
        let mut used = Vec::new();
        let Ok(iter) = queues[0].iter(memory) else {
            return false;
        };
        for chain in iter {
            let head = chain.head_index();
            let (Ok(mut reader), Ok(mut writer)) =
                (chain.clone().reader(memory), chain.writer(memory))
            else {
                used.push((head, 0));
                continue;
            };
            // A chain with no room for the header or the status byte gets nothing back.
            let writable = writer.available_bytes();
            let (Ok(kind), Ok(_), Ok(sector), true) = (
                reader.read_obj::<u32>(),
                reader.read_obj::<u32>(),
                reader.read_obj::<u64>(),
                writable > 0,
            ) else {
                used.push((head, 0));
                continue;
            };
            let Ok(mut status) = writer.split_at(writable - 1) else {
                used.push((head, 0));
                continue;
            };
            let incoming = reader.available_bytes();
            let code = self.serve(
                kind,
                sector,
                &mut reader,
                &mut writer,
                writable - 1,
                incoming,
            );
            let _ = status.write_obj(code);
            used.push((
                head,
                (writer.bytes_written() + status.bytes_written()) as u32,
            ));
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

    fn disk(name: &str, contents: &[u8]) -> File {
        let path = std::env::temp_dir().join(format!("thanatos-blk-{name}-{}", std::process::id()));
        std::fs::write(&path, contents).unwrap();
        let file = File::options().read(true).write(true).open(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        file
    }

    fn header(memory: &GuestMemory, at: u64, kind: u32, sector: u64) {
        memory.write_obj(kind, GuestAddress(at)).unwrap();
        memory.write_obj(sector, GuestAddress(at + 8)).unwrap();
    }

    #[test]
    fn reads_writes_flushes_and_reports_errors() {
        let memory = memory();
        let mut contents = vec![7u8; 1024];
        contents[512] = 9;
        let mut blk = Blk::new(disk("rw", &contents), false, "vda").unwrap();
        assert_eq!(blk.capacity(), 2);
        memory
            .write_slice(&[0xab; 512], GuestAddress(0x20_0000))
            .unwrap();
        header(&memory, 0x10_0000, IN, 1);
        header(&memory, 0x10_0100, OUT, 0);
        header(&memory, 0x10_0200, FLUSH, 0);
        header(&memory, 0x10_0300, IN, 2); // past the end
        header(&memory, 0x10_0400, 99, 0);
        let (mock, queue) = queue_with(
            &memory,
            0x1000,
            &[
                &[
                    (0x10_0000, 16, false),
                    (0x30_0000, 512, true),
                    (0x10_0010, 1, true),
                ],
                &[
                    (0x10_0100, 16, false),
                    (0x20_0000, 512, false),
                    (0x10_0110, 1, true),
                ],
                &[(0x10_0200, 16, false), (0x10_0210, 1, true)],
                &[
                    (0x10_0300, 16, false),
                    (0x30_0000, 512, true),
                    (0x10_0310, 1, true),
                ],
                &[(0x10_0400, 16, false), (0x10_0410, 1, true)],
                &[(0x10_0500, 4, false), (0x10_0510, 1, true)], // short header
            ],
        );
        let mut queues = [queue];
        assert!(blk.notify(0, &mut queues, &memory));
        let status = |at| memory.read_obj::<u8>(GuestAddress(at)).unwrap();
        assert_eq!(status(0x30_0000), 9);
        assert_eq!(status(0x10_0010), OK);
        assert_eq!(status(0x10_0110), OK);
        assert_eq!(status(0x10_0210), OK);
        assert_eq!(status(0x10_0310), IOERR);
        assert_eq!(status(0x10_0410), UNSUPPORTED);
        assert_eq!(mock.used().idx().load(), 6);
        let mut back = [0; 1];
        blk.file.read_exact_at(&mut back, 0).unwrap();
        assert_eq!(back, [0xab]);
    }

    #[test]
    fn a_read_only_disk_refuses_writes() {
        let memory = memory();
        let mut blk = Blk::new(disk("ro", &[1; 512]), true, "vda").unwrap();
        assert_ne!(blk.features() & F_RO, 0);
        header(&memory, 0x10_0000, OUT, 0);
        let (_mock, queue) = queue_with(
            &memory,
            0x1000,
            &[&[
                (0x10_0000, 16, false),
                (0x20_0000, 512, false),
                (0x10_0010, 1, true),
            ]],
        );
        let mut queues = [queue];
        blk.notify(0, &mut queues, &memory);
        assert_eq!(
            memory.read_obj::<u8>(GuestAddress(0x10_0010)).unwrap(),
            IOERR
        );
    }
}
