//! virtio-vsock (§5.10). Streams are host-initiated: [`connect`] returns a local socket joined to
//! a guest port. shortcut: guest-initiated connections are refused, add a Unix socket per port when a guest service needs to call out.
use crate::{
    GuestMemory,
    mmio::{Device, Slot},
};
use std::{
    collections::{HashMap, VecDeque},
    io::{self, Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
        mpsc::{self, Sender},
    },
    thread::Thread,
    time::Duration,
};
use virtio_queue::{Queue, QueueT};

const GUEST_CID: u64 = 3;
const HOST_CID: u64 = 2;
const HEADER: usize = 44;
const STREAM: u16 = 1;
const REQUEST: u16 = 1;
const RESPONSE: u16 = 2;
const RST: u16 = 3;
const SHUTDOWN: u16 = 4;
const RW: u16 = 5;
const CREDIT_UPDATE: u16 = 6;
const CREDIT_REQUEST: u16 = 7;
const SHUTDOWN_RCV: u32 = 1;
const SHUTDOWN_SEND: u32 = 2;
/// What the guest may have in flight to us per stream; a guest exceeding it is reset.
const BUF_ALLOC: u32 = 1 << 20;
/// Host bytes queued for the guest's receive buffers across all streams.
const PENDING_LIMIT: usize = 1 << 20;
/// A guest that offers no receive buffers must not grow host memory with control replies.
const PENDING_PACKETS: usize = 4096;
const RX: usize = 0;
const TX: usize = 1;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Header {
    src_port: u32,
    dst_port: u32,
    len: u32,
    op: u16,
    flags: u32,
    buf_alloc: u32,
    fwd_cnt: u32,
}

impl Header {
    fn encode(&self) -> [u8; HEADER] {
        let mut out = [0; HEADER];
        out[0..8].copy_from_slice(&HOST_CID.to_le_bytes());
        out[8..16].copy_from_slice(&GUEST_CID.to_le_bytes());
        out[16..20].copy_from_slice(&self.src_port.to_le_bytes());
        out[20..24].copy_from_slice(&self.dst_port.to_le_bytes());
        out[24..28].copy_from_slice(&self.len.to_le_bytes());
        out[28..30].copy_from_slice(&STREAM.to_le_bytes());
        out[30..32].copy_from_slice(&self.op.to_le_bytes());
        out[32..36].copy_from_slice(&self.flags.to_le_bytes());
        out[36..40].copy_from_slice(&self.buf_alloc.to_le_bytes());
        out[40..44].copy_from_slice(&self.fwd_cnt.to_le_bytes());
        out
    }

    /// Fields as the guest sent them, or `None` if it is not a stream packet from the guest.
    fn decode(bytes: &[u8; HEADER]) -> Option<Self> {
        let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        let u16_at = |at: usize| u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap());
        let src_cid = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
        let dst_cid = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
        (src_cid == GUEST_CID && dst_cid == HOST_CID && u16_at(28) == STREAM).then(|| Self {
            src_port: u32_at(16),
            dst_port: u32_at(20),
            len: u32_at(24),
            op: u16_at(30),
            flags: u32_at(32),
            buf_alloc: u32_at(36),
            fwd_cnt: u32_at(40),
        })
    }
}

struct Conn {
    to_host: Option<Sender<Vec<u8>>>,
    stream: UnixStream,
    reader: Option<Thread>,
    connected: Option<Sender<bool>>,
    peer_alloc: u32,
    peer_fwd: u32,
    sent: u32,
    received: u32,
    forwarded: Arc<AtomicU32>,
}

impl Conn {
    fn close(&mut self) {
        self.to_host = None;
        let _ = self.stream.shutdown(Shutdown::Read);
        if let Some(connected) = self.connected.take() {
            let _ = connected.send(false);
        }
        if let Some(reader) = &self.reader {
            reader.unpark();
        }
    }
}

/// (host port, guest port)
type Key = (u32, u32);

pub struct Vsock {
    config: [u8; 8],
    conns: HashMap<Key, Conn>,
    pending: VecDeque<(Header, Vec<u8>)>,
    pending_bytes: usize,
    next_port: u32,
}

impl Default for Vsock {
    fn default() -> Self {
        Self {
            config: GUEST_CID.to_le_bytes(),
            conns: HashMap::new(),
            pending: VecDeque::new(),
            pending_bytes: 0,
            next_port: 1 << 16,
        }
    }
}

impl Vsock {
    fn header(&self, key: Key, op: u16, flags: u32) -> Header {
        let fwd_cnt = self
            .conns
            .get(&key)
            .map_or(0, |conn| conn.forwarded.load(Ordering::Relaxed));
        Header {
            src_port: key.0,
            dst_port: key.1,
            op,
            flags,
            buf_alloc: BUF_ALLOC,
            fwd_cnt,
            len: 0,
        }
    }

    fn control(&mut self, key: Key, op: u16, flags: u32) {
        if self.pending.len() < PENDING_PACKETS {
            let header = self.header(key, op, flags);
            self.pending.push_back((header, Vec::new()));
        }
    }

    fn reset(&mut self, key: Key) {
        if let Some(mut conn) = self.conns.remove(&key) {
            conn.close();
        }
        self.control(key, RST, 0);
    }

    /// Queues as much as credit and the pending bound allow. `None` means the stream is gone.
    fn send_data(&mut self, key: Key, data: &[u8]) -> Option<usize> {
        let conn = self.conns.get(&key)?;
        let credit = conn
            .peer_alloc
            .saturating_sub(conn.sent.wrapping_sub(conn.peer_fwd));
        let count = data
            .len()
            .min(credit as usize)
            .min(PENDING_LIMIT.saturating_sub(self.pending_bytes));
        if count == 0 || self.pending.len() >= PENDING_PACKETS {
            return Some(0);
        }
        let mut header = self.header(key, RW, 0);
        header.len = count as u32;
        self.pending.push_back((header, data[..count].to_vec()));
        self.pending_bytes += count;
        let conn = self.conns.get_mut(&key)?;
        conn.sent = conn.sent.wrapping_add(count as u32);
        Some(count)
    }

    /// Moves pending packets into the driver's receive buffers, splitting those that do not fit.
    fn flush(&mut self, queue: &mut Queue, memory: &GuestMemory) -> bool {
        let mut used = false;
        while let Some((mut header, data)) = self.pending.pop_front() {
            let Some(chain) = queue.pop_descriptor_chain(memory) else {
                self.pending.push_front((header, data));
                break;
            };
            let head = chain.head_index();
            let mut written = 0;
            let mut rest = data;
            match chain.writer(memory) {
                Ok(mut writer) if writer.available_bytes() >= HEADER => {
                    let room = writer.available_bytes() - HEADER;
                    let tail = rest.split_off(room.min(rest.len()));
                    let mut part = header;
                    part.len = rest.len() as u32;
                    if writer.write_all(&part.encode()).is_ok() && writer.write_all(&rest).is_ok() {
                        written = HEADER + rest.len();
                        self.pending_bytes -= rest.len();
                    }
                    if written == 0 {
                        self.pending_bytes -= rest.len();
                    }
                    if !tail.is_empty() {
                        header.len = tail.len() as u32;
                        self.pending.push_front((header, tail));
                    }
                }
                // A chain too small for a header carries nothing; the packet waits for the next.
                _ => self.pending.push_front((header, rest)),
            }
            let _ = queue.add_used(memory, head, written as u32);
            used = true;
        }
        for conn in self.conns.values() {
            if let Some(reader) = &conn.reader {
                reader.unpark();
            }
        }
        used
    }

    fn handle(&mut self, header: Header, data: Vec<u8>) {
        let key = (header.dst_port, header.src_port);
        let Some(conn) = self.conns.get_mut(&key) else {
            // Nothing listens here, which is also the answer to any guest-initiated connection.
            if header.op != RST {
                self.control(key, RST, 0);
            }
            return;
        };
        conn.peer_alloc = header.buf_alloc;
        conn.peer_fwd = header.fwd_cnt;
        match header.op {
            RESPONSE => {
                if let Some(connected) = conn.connected.take() {
                    let _ = connected.send(true);
                }
            }
            RW => {
                conn.received = conn.received.wrapping_add(data.len() as u32);
                let forwarded = conn.forwarded.load(Ordering::Relaxed);
                if conn.received.wrapping_sub(forwarded) > BUF_ALLOC {
                    return self.reset(key);
                }
                if let Some(to_host) = &conn.to_host {
                    let _ = to_host.send(data);
                }
            }
            CREDIT_REQUEST => self.control(key, CREDIT_UPDATE, 0),
            CREDIT_UPDATE => (),
            SHUTDOWN => {
                if header.flags & SHUTDOWN_SEND != 0 {
                    conn.to_host = None;
                }
                if header.flags & SHUTDOWN_RCV != 0 && header.flags & SHUTDOWN_SEND != 0 {
                    self.reset(key);
                }
            }
            RST => {
                if let Some(mut conn) = self.conns.remove(&key) {
                    conn.close();
                }
            }
            _ => self.reset(key),
        }
        // Whatever arrived may have opened credit for a stalled reader.
        if let Some(reader) = self.conns.get(&key).and_then(|conn| conn.reader.as_ref()) {
            reader.unpark();
        }
    }
}

impl Device for Vsock {
    const TYPE: u32 = 19;
    const QUEUES: usize = 3;

    fn features(&self) -> u64 {
        0
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
                    let mut bytes = [0; HEADER];
                    if reader.read_exact(&mut bytes).is_ok()
                        && let Some(header) = Header::decode(&bytes)
                    {
                        // The guest's length is a claim: take only what the chain holds, within
                        // what one stream may have in flight.
                        let length = (header.len as usize)
                            .min(reader.available_bytes())
                            .min(BUF_ALLOC as usize);
                        let mut data = vec![0; length];
                        if reader.read_exact(&mut data).is_ok() {
                            self.handle(header, data);
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
        for conn in self.conns.values_mut() {
            conn.close();
        }
        self.conns.clear();
        self.pending.clear();
        self.pending_bytes = 0;
    }
}

/// Opens a stream to a guest port; the returned socket's far end is the guest's accepted peer.
pub fn connect(slot: &Arc<Slot<Vsock>>, port: u32) -> io::Result<UnixStream> {
    let (client, stream) = UnixStream::pair()?;
    let (connected, answer) = mpsc::channel();
    let (to_host, from_guest) = mpsc::channel::<Vec<u8>>();
    let forwarded = Arc::new(AtomicU32::new(0));
    let mut key = None;
    slot.run(|device, queues, memory| {
        let host_port = device.next_port;
        device.next_port = device.next_port.wrapping_add(1).max(1 << 16);
        let this = (host_port, port);
        device.conns.insert(
            this,
            Conn {
                to_host: Some(to_host),
                stream: stream.try_clone().expect("a socket pair clones"),
                reader: None,
                connected: Some(connected),
                peer_alloc: 0,
                peer_fwd: 0,
                sent: 0,
                received: 0,
                forwarded: forwarded.clone(),
            },
        );
        device.control(this, REQUEST, 0);
        key = Some(this);
        device.flush(&mut queues[RX], memory)
    });
    let key = key.ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "vsock not ready"))?;
    match answer.recv_timeout(CONNECT_TIMEOUT) {
        Ok(true) => (),
        outcome => {
            slot.with(|device| {
                if let Some(mut conn) = device.conns.remove(&key) {
                    conn.close();
                }
            });
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                match outcome {
                    Ok(_) => format!("nothing listens on guest vsock port {port}"),
                    Err(_) => format!("guest vsock port {port} did not answer"),
                },
            ));
        }
    }
    let (to_guest, write_stream) = (slot.clone(), stream.try_clone()?);
    let reader = std::thread::spawn(move || read_host(&to_guest, key, stream));
    slot.with(|device| {
        if let Some(conn) = device.conns.get_mut(&key) {
            conn.reader = Some(reader.thread().clone());
        }
    });
    let slot = slot.clone();
    std::thread::spawn(move || write_host(&slot, key, write_stream, &from_guest, &forwarded));
    Ok(client)
}

/// Host bytes to the guest, stalling while the guest has no credit.
fn read_host(slot: &Slot<Vsock>, key: Key, mut stream: UnixStream) {
    let mut buffer = vec![0; 1 << 16];
    loop {
        let count = stream.read(&mut buffer).unwrap_or(0);
        if count == 0 {
            slot.run(|device, queues, memory| {
                if device.conns.contains_key(&key) {
                    device.control(key, SHUTDOWN, SHUTDOWN_SEND);
                }
                device.flush(&mut queues[RX], memory)
            });
            return;
        }
        let mut done = 0;
        while done < count {
            let mut accepted = None;
            slot.run(|device, queues, memory| {
                accepted = device.send_data(key, &buffer[done..count]);
                device.flush(&mut queues[RX], memory)
            });
            match accepted {
                None => return,
                Some(0) => std::thread::park_timeout(Duration::from_millis(100)),
                Some(sent) => done += sent,
            }
        }
    }
}

/// Guest bytes to the host. Runs off the vCPU thread so a slow client cannot stall the guest.
fn write_host(
    slot: &Slot<Vsock>,
    key: Key,
    mut stream: UnixStream,
    from_guest: &mpsc::Receiver<Vec<u8>>,
    forwarded: &AtomicU32,
) {
    for data in from_guest {
        if stream.write_all(&data).is_err() {
            slot.run(|device, queues, memory| {
                device.reset(key);
                device.flush(&mut queues[RX], memory)
            });
            return;
        }
        forwarded.fetch_add(data.len() as u32, Ordering::Relaxed);
        slot.run(|device, queues, memory| {
            if device.conns.contains_key(&key) {
                device.control(key, CREDIT_UPDATE, 0);
            }
            device.flush(&mut queues[RX], memory)
        });
    }
    let _ = stream.shutdown(Shutdown::Write);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mmio::test_support::{memory, queue_with};
    use vm_memory::{Bytes, GuestAddress};

    fn guest_packet(memory: &GuestMemory, at: u64, op: u16, payload: &[u8], claimed: Option<u32>) {
        let header = Header {
            src_port: 1,
            dst_port: 70000,
            len: claimed.unwrap_or(payload.len() as u32),
            op,
            flags: 3,
            buf_alloc: 8,
            fwd_cnt: 0,
        };
        let mut bytes = header.encode();
        bytes[0..8].copy_from_slice(&GUEST_CID.to_le_bytes());
        bytes[8..16].copy_from_slice(&HOST_CID.to_le_bytes());
        memory.write_slice(&bytes, GuestAddress(at)).unwrap();
        memory.write_slice(payload, GuestAddress(at + 44)).unwrap();
    }

    fn conn(stream: UnixStream, to_host: Sender<Vec<u8>>) -> Conn {
        Conn {
            to_host: Some(to_host),
            stream,
            reader: None,
            connected: None,
            peer_alloc: 0,
            peer_fwd: 0,
            sent: 0,
            received: 0,
            forwarded: Arc::default(),
        }
    }

    #[test]
    fn carries_data_within_credit_and_refuses_unknown_streams() {
        let memory = memory();
        let (stream, _other) = UnixStream::pair().unwrap();
        let (to_host, from_guest) = mpsc::channel();
        let mut vsock = Vsock::default();
        vsock.conns.insert((70000, 1), conn(stream, to_host));
        guest_packet(&memory, 0x10_0000, RW, b"hi", None);
        guest_packet(&memory, 0x11_0000, RW, b"yo", Some(u32::MAX)); // lying length
        let mut unknown = [0; 44];
        memory
            .read_slice(&mut unknown, GuestAddress(0x10_0000))
            .unwrap();
        unknown[20..24].copy_from_slice(&9u32.to_le_bytes());
        unknown[30..32].copy_from_slice(&REQUEST.to_le_bytes());
        unknown[24..28].fill(0);
        memory
            .write_slice(&unknown, GuestAddress(0x12_0000))
            .unwrap();
        let (_tx, tx) = queue_with(
            &memory,
            0x1000,
            &[
                &[(0x10_0000, 46, false)],
                &[(0x11_0000, 46, false)],
                &[(0x12_0000, 44, false)],
            ],
        );
        let (rx_mock, rx) = queue_with(&memory, 0x8000, &[&[(0x20_0000, 44 + 4, true)]]);
        let mut queues = [rx, tx];
        assert!(vsock.notify(TX, &mut queues, &memory));
        assert_eq!(from_guest.recv().unwrap(), b"hi");
        assert_eq!(from_guest.recv().unwrap(), b"yo");
        let mut reply = [0; 44];
        memory
            .read_slice(&mut reply, GuestAddress(0x20_0000))
            .unwrap();
        let op = u16::from_le_bytes([reply[30], reply[31]]);
        assert_eq!(op, RST);
        assert_eq!(rx_mock.used().idx().load(), 1);

        // The peer offered 8 bytes of credit: a longer write is cut there and split to fit.
        assert_eq!(vsock.send_data((70000, 1), b"0123456789"), Some(8));
        assert_eq!(vsock.send_data((70000, 1), b"89"), Some(0));
        assert_eq!(vsock.send_data((1, 1), b"x"), None);
    }

    #[test]
    fn a_guest_overrunning_its_credit_is_reset() {
        let (stream, _other) = UnixStream::pair().unwrap();
        let (to_host, _from_guest) = mpsc::channel();
        let mut vsock = Vsock::default();
        vsock.conns.insert((70000, 1), conn(stream, to_host));
        let header = Header {
            src_port: 1,
            dst_port: 70000,
            op: RW,
            ..Header::default()
        };
        vsock.handle(header, vec![0; BUF_ALLOC as usize]);
        assert!(vsock.conns.contains_key(&(70000, 1)));
        vsock.handle(header, vec![0]);
        assert!(!vsock.conns.contains_key(&(70000, 1)));
        assert_eq!(vsock.pending.back().unwrap().0.op, RST);
    }
}
