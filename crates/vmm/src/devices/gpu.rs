//! virtio-gpu (§5.7), the 2D command set: resources in VMM memory, one scanout, no virgl or blobs.
use crate::{GuestMemory, mmio::Device, screen::Screen};
use std::{
    collections::HashMap,
    io::{Read, Write},
    sync::Arc,
};
use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestAddress};

const GET_DISPLAY_INFO: u32 = 0x100;
const RESOURCE_CREATE_2D: u32 = 0x101;
const RESOURCE_UNREF: u32 = 0x102;
const SET_SCANOUT: u32 = 0x103;
const RESOURCE_FLUSH: u32 = 0x104;
const TRANSFER_TO_HOST_2D: u32 = 0x105;
const RESOURCE_ATTACH_BACKING: u32 = 0x106;
const RESOURCE_DETACH_BACKING: u32 = 0x107;
const OK_NODATA: u32 = 0x1100;
const OK_DISPLAY_INFO: u32 = 0x1101;
const ERR_UNSPEC: u32 = 0x1200;
const ERR_OUT_OF_MEMORY: u32 = 0x1201;
const ERR_INVALID_SCANOUT: u32 = 0x1202;
const ERR_INVALID_RESOURCE: u32 = 0x1203;
const ERR_INVALID_PARAMETER: u32 = 0x1205;
const FLAG_FENCE: u32 = 1;
const HEADER: usize = 24;
const MAX_SCANOUTS: usize = 16;
/// All resources of one guest together; a guest asking for more is told it is out of memory.
const MAX_RESOURCE_BYTES: u64 = 256 << 20;
const MAX_BACKING_ENTRIES: u32 = 1 << 16;
const CONTROL: usize = 0;

#[derive(Clone, Copy)]
struct Rect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl Rect {
    fn inside(&self, width: u32, height: u32) -> bool {
        self.x
            .checked_add(self.width)
            .is_some_and(|end| end <= width)
            && self
                .y
                .checked_add(self.height)
                .is_some_and(|end| end <= height)
    }
}

/// Where each format keeps red, green and blue within a pixel.
fn channels(format: u32) -> Option<[usize; 3]> {
    match format {
        1 | 2 => Some([2, 1, 0]),    // B8G8R8A8, B8G8R8X8
        3 | 4 => Some([1, 2, 3]),    // A8R8G8B8, X8R8G8B8
        67 | 134 => Some([0, 1, 2]), // R8G8B8A8, R8G8B8X8
        68 | 121 => Some([3, 2, 1]), // X8B8G8R8, A8B8G8R8
        _ => None,
    }
}

struct Resource {
    width: u32,
    height: u32,
    format: u32,
    pixels: Vec<u8>,
    backing: Vec<(u64, u32)>,
}

pub struct Gpu {
    config: [u8; 16],
    screen: Arc<Screen>,
    resources: HashMap<u32, Resource>,
    used_bytes: u64,
    scanout: Option<u32>,
}

impl Gpu {
    pub fn new(screen: Arc<Screen>) -> Self {
        let mut config = [0; 16];
        config[8] = 1; // num_scanouts
        Self {
            config,
            screen,
            resources: HashMap::new(),
            used_bytes: 0,
            scanout: None,
        }
    }

    fn command(&mut self, request: &[u8], memory: &GuestMemory) -> (u32, Vec<u8>) {
        // RESOURCE_ATTACH_BACKING's list follows its two fixed words.
        let extra = request.get(HEADER + 8..).unwrap_or(&[]);
        let word = |index: usize| {
            let at = HEADER + index * 4;
            request
                .get(at..at + 4)
                .map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()))
        };
        let rect = |first: usize| Rect {
            x: word(first),
            y: word(first + 1),
            width: word(first + 2),
            height: word(first + 3),
        };
        let kind = u32::from_le_bytes(request[..4].try_into().unwrap());
        match kind {
            GET_DISPLAY_INFO => {
                let (width, height) = self.screen.with(|f| (f.width, f.height));
                let mut info = vec![0; MAX_SCANOUTS * 24];
                info[8..12].copy_from_slice(&width.to_le_bytes());
                info[12..16].copy_from_slice(&height.to_le_bytes());
                info[16] = 1; // enabled
                (OK_DISPLAY_INFO, info)
            }
            RESOURCE_CREATE_2D => {
                let (id, format, width, height) = (word(0), word(1), word(2), word(3));
                let size = (u64::from(width) * u64::from(height)).saturating_mul(4);
                if id == 0 || channels(format).is_none() || size == 0 {
                    return (ERR_INVALID_PARAMETER, Vec::new());
                }
                if self.resources.contains_key(&id) {
                    return (ERR_INVALID_RESOURCE, Vec::new());
                }
                if size > MAX_RESOURCE_BYTES - self.used_bytes {
                    return (ERR_OUT_OF_MEMORY, Vec::new());
                }
                self.used_bytes += size;
                self.resources.insert(
                    id,
                    Resource {
                        width,
                        height,
                        format,
                        pixels: vec![0; size as usize],
                        backing: Vec::new(),
                    },
                );
                (OK_NODATA, Vec::new())
            }
            RESOURCE_UNREF => match self.resources.remove(&word(0)) {
                Some(resource) => {
                    self.used_bytes -= resource.pixels.len() as u64;
                    if self.scanout == Some(word(0)) {
                        self.scanout = None;
                    }
                    (OK_NODATA, Vec::new())
                }
                None => (ERR_INVALID_RESOURCE, Vec::new()),
            },
            SET_SCANOUT => {
                let area = rect(0);
                let (scanout, id) = (word(4), word(5));
                if scanout != 0 {
                    return (ERR_INVALID_SCANOUT, Vec::new());
                }
                if id == 0 {
                    self.scanout = None;
                    return (OK_NODATA, Vec::new());
                }
                let Some(resource) = self.resources.get(&id) else {
                    return (ERR_INVALID_RESOURCE, Vec::new());
                };
                if !area.inside(resource.width, resource.height) {
                    return (ERR_INVALID_PARAMETER, Vec::new());
                }
                self.scanout = Some(id);
                (OK_NODATA, Vec::new())
            }
            RESOURCE_ATTACH_BACKING => {
                let (id, count) = (word(0), word(1));
                let Some(resource) = self.resources.get_mut(&id) else {
                    return (ERR_INVALID_RESOURCE, Vec::new());
                };
                if count > MAX_BACKING_ENTRIES || extra.len() < count as usize * 16 {
                    return (ERR_INVALID_PARAMETER, Vec::new());
                }
                resource.backing = extra
                    .chunks_exact(16)
                    .take(count as usize)
                    .map(|entry| {
                        (
                            u64::from_le_bytes(entry[..8].try_into().unwrap()),
                            u32::from_le_bytes(entry[8..12].try_into().unwrap()),
                        )
                    })
                    .collect();
                (OK_NODATA, Vec::new())
            }
            RESOURCE_DETACH_BACKING => match self.resources.get_mut(&word(0)) {
                Some(resource) => {
                    resource.backing.clear();
                    (OK_NODATA, Vec::new())
                }
                None => (ERR_INVALID_RESOURCE, Vec::new()),
            },
            TRANSFER_TO_HOST_2D => {
                let area = rect(0);
                let offset = u64::from(word(4)) | u64::from(word(5)) << 32;
                let Some(resource) = self.resources.get_mut(&word(6)) else {
                    return (ERR_INVALID_RESOURCE, Vec::new());
                };
                if !area.inside(resource.width, resource.height) {
                    return (ERR_INVALID_PARAMETER, Vec::new());
                }
                match resource.transfer(area, offset, memory) {
                    Some(()) => (OK_NODATA, Vec::new()),
                    None => (ERR_UNSPEC, Vec::new()),
                }
            }
            RESOURCE_FLUSH => {
                let (area, id) = (rect(0), word(4));
                let Some(resource) = self.resources.get(&id) else {
                    return (ERR_INVALID_RESOURCE, Vec::new());
                };
                if !area.inside(resource.width, resource.height) {
                    return (ERR_INVALID_PARAMETER, Vec::new());
                }
                if self.scanout == Some(id) {
                    resource.present(area, &self.screen);
                }
                (OK_NODATA, Vec::new())
            }
            _ => (ERR_UNSPEC, Vec::new()),
        }
    }
}

impl Resource {
    /// Copies a rectangle from the guest pages behind this resource. `None` when the backing is
    /// shorter than the transfer or points outside guest RAM.
    fn transfer(&mut self, area: Rect, offset: u64, memory: &GuestMemory) -> Option<()> {
        let stride = u64::from(self.width) * 4;
        let row_bytes = area.width as usize * 4;
        for row in 0..u64::from(area.height) {
            let source = offset.checked_add(row.checked_mul(stride)?)?;
            let start = (u64::from(area.y) + row) * stride + u64::from(area.x) * 4;
            let target = self
                .pixels
                .get_mut(start as usize..start as usize + row_bytes)?;
            read_backing(&self.backing, source, target, memory)?;
        }
        Some(())
    }

    fn present(&self, area: Rect, screen: &Screen) {
        let Some([red, green, blue]) = channels(self.format) else {
            return;
        };
        screen.update(|frame| {
            let (width, height) = (self.width, self.height);
            if (frame.width, frame.height) != (width, height) {
                frame.resize(width, height);
            }
            for y in area.y as usize..(area.y + area.height) as usize {
                let start = y * width as usize * 4 + area.x as usize * 4;
                let end = start + area.width as usize * 4;
                let source = self.pixels[start..end].chunks_exact(4);
                for (out, pixel) in frame.pixels[start..end].chunks_exact_mut(4).zip(source) {
                    out[0] = pixel[red];
                    out[1] = pixel[green];
                    out[2] = pixel[blue];
                }
            }
        });
    }
}

fn read_backing(
    backing: &[(u64, u32)],
    mut offset: u64,
    mut target: &mut [u8],
    memory: &GuestMemory,
) -> Option<()> {
    for &(address, length) in backing {
        if target.is_empty() {
            break;
        }
        if offset >= u64::from(length) {
            offset -= u64::from(length);
            continue;
        }
        let count = target.len().min((u64::from(length) - offset) as usize);
        let (now, rest) = target.split_at_mut(count);
        memory
            .read_slice(now, GuestAddress(address.checked_add(offset)?))
            .ok()?;
        target = rest;
        offset = 0;
    }
    target.is_empty().then_some(())
}

impl Device for Gpu {
    const TYPE: u32 = 16;
    const QUEUES: usize = 2;

    fn features(&self) -> u64 {
        0
    }

    fn config(&self) -> &[u8] {
        &self.config
    }

    fn notify(&mut self, queue: usize, queues: &mut [Queue], memory: &GuestMemory) -> bool {
        let mut used = false;
        while let Some(chain) = queues[queue].pop_descriptor_chain(memory) {
            let head = chain.head_index();
            let mut written = 0;
            // The cursor queue's commands are accepted unread: the screenshot is the scanout,
            // and sinks draw their own pointer.
            if queue == CONTROL
                && let (Ok(mut reader), Ok(mut writer)) =
                    (chain.clone().reader(memory), chain.writer(memory))
            {
                let length = reader.available_bytes();
                // A command is its fixed part plus the backing list; both come from the guest.
                let mut request =
                    vec![0; length.min(HEADER + 64 + MAX_BACKING_ENTRIES as usize * 16)];
                if length >= HEADER && reader.read_exact(&mut request).is_ok() {
                    let (kind, body) = self.command(&request, memory);
                    let mut reply = vec![0; HEADER];
                    reply[..4].copy_from_slice(&kind.to_le_bytes());
                    if request[4..8] == FLAG_FENCE.to_le_bytes() {
                        reply[4..16].copy_from_slice(&request[4..16]);
                    }
                    reply.extend_from_slice(&body);
                    if writer.write_all(&reply).is_ok() {
                        written = reply.len();
                    }
                }
            }
            let _ = queues[queue].add_used(memory, head, written as u32);
            used = true;
        }
        used
    }

    fn reset(&mut self) {
        self.resources.clear();
        self.used_bytes = 0;
        self.scanout = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mmio::test_support::{memory, queue_with};

    /// A command at `at` with its reply at `at + 0x1000`.
    fn command(memory: &GuestMemory, at: u64, kind: u32, words: &[u32]) -> Vec<(u64, u32, bool)> {
        memory.write_obj(kind, GuestAddress(at)).unwrap();
        for (index, word) in words.iter().enumerate() {
            memory
                .write_obj(*word, GuestAddress(at + 24 + index as u64 * 4))
                .unwrap();
        }
        vec![
            (at, 24 + words.len() as u32 * 4, false),
            (at + 0x1000, 1024, true),
        ]
    }

    fn status(memory: &GuestMemory, at: u64) -> u32 {
        memory.read_obj(GuestAddress(at + 0x1000)).unwrap()
    }

    #[test]
    fn a_flushed_rectangle_reaches_the_screen_in_rgba() {
        let memory = memory();
        let screen = Arc::new(Screen::new(2, 2));
        let mut gpu = Gpu::new(screen.clone());
        // B8G8R8X8: the backing's last pixel of the 2x2 resource is blue=1, green=2, red=3.
        memory
            .write_slice(&[1, 2, 3, 0], GuestAddress(0x30_0000 + 12))
            .unwrap();
        let mut attach = vec![1, 1];
        attach.extend([0x30_0000, 0, 16, 0]);
        let chains = [
            command(&memory, 0x10_0000, RESOURCE_CREATE_2D, &[1, 2, 2, 2]),
            command(&memory, 0x10_2000, RESOURCE_ATTACH_BACKING, &attach),
            command(&memory, 0x10_4000, SET_SCANOUT, &[0, 0, 2, 2, 0, 1]),
            command(
                &memory,
                0x10_6000,
                TRANSFER_TO_HOST_2D,
                &[0, 0, 2, 2, 0, 0, 1, 0],
            ),
            command(&memory, 0x10_8000, RESOURCE_FLUSH, &[0, 0, 2, 2, 1, 0]),
            command(&memory, 0x10_a000, GET_DISPLAY_INFO, &[]),
        ];
        let chains: Vec<&[_]> = chains.iter().map(Vec::as_slice).collect();
        let (_mock, queue) = queue_with(&memory, 0x1000, &chains);
        let mut queues = [queue];
        assert!(gpu.notify(CONTROL, &mut queues, &memory));
        for at in [0x10_0000, 0x10_2000, 0x10_4000, 0x10_6000, 0x10_8000] {
            assert_eq!(status(&memory, at), OK_NODATA, "{at:#x}");
        }
        assert_eq!(status(&memory, 0x10_a000), OK_DISPLAY_INFO);
        screen.with(|frame| assert_eq!(frame.pixels[12..16], [3, 2, 1, 255]));
    }

    #[test]
    fn hostile_commands_get_errors_not_host_memory() {
        let memory = memory();
        let mut gpu = Gpu::new(Arc::new(Screen::new(2, 2)));
        let huge = u32::MAX;
        let mut attach = vec![1, 1];
        attach.extend([0xffff_0000, 0xffff, 4096, 0]); // backing far outside guest RAM
        let chains = [
            command(&memory, 0x10_0000, RESOURCE_CREATE_2D, &[9, 2, huge, huge]),
            command(&memory, 0x10_2000, RESOURCE_CREATE_2D, &[1, 2, 16, 16]),
            command(&memory, 0x10_4000, RESOURCE_ATTACH_BACKING, &attach),
            command(
                &memory,
                0x10_6000,
                TRANSFER_TO_HOST_2D,
                &[0, 0, 16, 16, 0, 0, 1, 0],
            ),
            // The rectangle overflows u32 and must not wrap into range.
            command(
                &memory,
                0x10_8000,
                TRANSFER_TO_HOST_2D,
                &[huge, 0, 2, 1, 0, 0, 1, 0],
            ),
            command(&memory, 0x10_a000, RESOURCE_FLUSH, &[0, 0, 17, 1, 1, 0]),
            command(&memory, 0x10_c000, SET_SCANOUT, &[0, 0, 1, 1, 3, 1]),
            command(&memory, 0x10_e000, RESOURCE_UNREF, &[42]),
            command(&memory, 0x11_0000, 0xdead, &[]),
        ];
        let chains: Vec<&[_]> = chains.iter().map(Vec::as_slice).collect();
        let (_mock, queue) = queue_with(&memory, 0x1000, &chains);
        let mut queues = [queue];
        gpu.notify(CONTROL, &mut queues, &memory);
        assert_eq!(status(&memory, 0x10_0000), ERR_OUT_OF_MEMORY);
        assert_eq!(status(&memory, 0x10_2000), OK_NODATA);
        assert_eq!(status(&memory, 0x10_6000), ERR_UNSPEC);
        assert_eq!(status(&memory, 0x10_8000), ERR_INVALID_PARAMETER);
        assert_eq!(status(&memory, 0x10_a000), ERR_INVALID_PARAMETER);
        assert_eq!(status(&memory, 0x10_c000), ERR_INVALID_SCANOUT);
        assert_eq!(status(&memory, 0x10_e000), ERR_INVALID_RESOURCE);
        assert_eq!(status(&memory, 0x11_0000), ERR_UNSPEC);
    }
}
