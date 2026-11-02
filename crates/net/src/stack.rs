//! One thread owns the interface; every host socket has its own blocking threads that talk to it
//! through one event channel, so an idle network wakes nothing.
use crate::{
    policy::{Policy, Protocol},
    wire::{DNS_IP, GATEWAY_IP, GATEWAY_MAC, GUEST_IP, dhcp_reply, prohibited_frame, udp_frame},
};
use smoltcp::{
    iface::{Config, Interface, SocketHandle, SocketSet},
    phy::{self, Device, DeviceCapabilities, Medium},
    socket::tcp,
    time::{Duration as SmolDuration, Instant as SmolInstant},
    wire::{
        EthernetAddress, EthernetFrame, EthernetProtocol, HardwareAddress, IpCidr,
        IpListenEndpoint, IpProtocol, Ipv4Address, Ipv4Packet, TcpPacket, UdpPacket,
    },
};
use std::{
    collections::{HashMap, VecDeque},
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, Sender, SyncSender, channel, sync_channel},
    },
    time::{Duration, Instant},
};

const SOCKET_BUFFER: usize = 1 << 18;
const CHUNK: usize = 1 << 16;
/// Bounds what a guest can make the host hold open.
const MAX_TCP_FLOWS: usize = 1024;
const MAX_UDP_FLOWS: usize = 256;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const UDP_IDLE: Duration = Duration::from_secs(60);
const FRAME_BACKLOG: usize = 4096;
const FIRST_LOCAL_PORT: u16 = 49152;

pub struct Settings {
    pub default: String,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub forward: Vec<String>,
    /// Given each denied destination, for the event stream.
    pub on_denied: Box<dyn Fn(String) + Send>,
}

type TcpKey = (Ipv4Address, u16, Ipv4Address, u16);

enum Event {
    Frame(Vec<u8>),
    Connected(TcpKey, Vec<u8>, io::Result<TcpStream>),
    FromHost(SocketHandle, Vec<u8>),
    HostClosed(SocketHandle),
    Written(SocketHandle),
    Inbound(TcpStream, u16),
    Datagram((u16, Ipv4Address, u16), Vec<u8>),
    UdpIdle((u16, Ipv4Address, u16)),
    AddForward(String, Sender<Result<(), String>>),
    RemoveForward(String, Sender<Result<(), String>>),
}

/// The handle the device and the API hold. Dropping the last one ends the stack.
#[derive(Clone)]
pub struct Net {
    events: SyncSender<Event>,
}

impl Net {
    /// `to_guest` receives each ethernet frame for the guest, on the stack's thread.
    pub fn start(
        settings: Settings,
        guest_mac: [u8; 6],
        to_guest: impl FnMut(Vec<u8>) + Send + 'static,
    ) -> Result<Self, String> {
        let policy = Policy::new(&settings.default, &settings.allow, &settings.deny)?;
        for spec in &settings.forward {
            parse_forward(spec)?;
        }
        let (events, receive) = sync_channel(FRAME_BACKLOG);
        let net = Self { events };
        let on_denied = settings.on_denied;
        let own = net.events.clone();
        let mut stack = Stack::new(policy, guest_mac, Box::new(to_guest), on_denied, own);
        for spec in &settings.forward {
            stack.add_forward(spec)?;
        }
        std::thread::spawn(move || stack.run(&receive));
        Ok(net)
    }

    /// A frame from the guest. Dropped when the stack is behind, as a full NIC ring would.
    pub fn frame(&self, frame: Vec<u8>) {
        let _ = self.events.try_send(Event::Frame(frame));
    }

    /// Opens a `127.0.0.1:2222->22` listener.
    pub fn add_forward(&self, spec: &str) -> Result<(), String> {
        self.ask(|answer| Event::AddForward(spec.into(), answer))
    }

    pub fn remove_forward(&self, spec: &str) -> Result<(), String> {
        self.ask(|answer| Event::RemoveForward(spec.into(), answer))
    }

    fn ask(&self, event: impl FnOnce(Sender<Result<(), String>>) -> Event) -> Result<(), String> {
        let (answer, wait) = channel();
        self.events
            .send(event(answer))
            .map_err(|_| "the network has stopped")?;
        wait.recv().map_err(|_| "the network has stopped")?
    }
}

fn parse_forward(spec: &str) -> Result<(SocketAddr, u16), String> {
    let bad = || format!("bad forward {spec:?}, expected 127.0.0.1:2222->22");
    let (host, guest) = spec.split_once("->").ok_or_else(bad)?;
    Ok((
        host.parse().map_err(|_| bad())?,
        guest.parse().map_err(|_| bad())?,
    ))
}

/// smoltcp's hardware: frames in from the guest, frames out through a callback.
struct Wire {
    incoming: VecDeque<Vec<u8>>,
    to_guest: Box<dyn FnMut(Vec<u8>) + Send>,
}

struct RxToken(Vec<u8>);
struct TxToken<'a>(&'a mut Box<dyn FnMut(Vec<u8>) + Send>);

impl phy::RxToken for RxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl phy::TxToken for TxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, length: usize, f: F) -> R {
        let mut frame = vec![0; length];
        let result = f(&mut frame);
        (self.0)(frame);
        result
    }
}

impl Device for Wire {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken<'a>;

    fn receive(&mut self, _: SmolInstant) -> Option<(RxToken, TxToken<'_>)> {
        let frame = self.incoming.pop_front()?;
        Some((RxToken(frame), TxToken(&mut self.to_guest)))
    }

    fn transmit(&mut self, _: SmolInstant) -> Option<TxToken<'_>> {
        Some(TxToken(&mut self.to_guest))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut capabilities = DeviceCapabilities::default();
        capabilities.medium = Medium::Ethernet;
        capabilities.max_transmission_unit = 1514;
        capabilities
    }
}

struct Flow {
    to_host: Option<Sender<Vec<u8>>>,
    /// The writer thread holds a chunk; reading more would only queue it in memory.
    writing: bool,
    to_guest: VecDeque<u8>,
    /// Lets the reader thread take the next chunk once this one is in the socket.
    credit: SyncSender<()>,
    host_closed: bool,
    close_sent: bool,
    left_listen: bool,
    stream: TcpStream,
}

struct Stack {
    policy: Policy,
    guest_mac: EthernetAddress,
    iface: Interface,
    wire: Wire,
    sockets: SocketSet<'static>,
    flows: HashMap<SocketHandle, Flow>,
    keys: HashMap<TcpKey, Option<SocketHandle>>,
    udp: HashMap<(u16, Ipv4Address, u16), Arc<UdpSocket>>,
    forwards: HashMap<String, (Arc<AtomicBool>, SocketAddr)>,
    events: SyncSender<Event>,
    on_denied: Box<dyn Fn(String) + Send>,
    started: Instant,
    next_port: u16,
}

impl Stack {
    fn new(
        policy: Policy,
        guest_mac: [u8; 6],
        to_guest: Box<dyn FnMut(Vec<u8>) + Send>,
        on_denied: Box<dyn Fn(String) + Send>,
        events: SyncSender<Event>,
    ) -> Self {
        let mut wire = Wire {
            incoming: VecDeque::new(),
            to_guest,
        };
        let config = Config::new(HardwareAddress::Ethernet(GATEWAY_MAC));
        let mut iface = Interface::new(config, &mut wire, SmolInstant::from_millis(0));
        iface.update_ip_addrs(|addresses| {
            let _ = addresses.push(IpCidr::new(GATEWAY_IP.into(), 24));
            let _ = addresses.push(IpCidr::new(DNS_IP.into(), 24));
        });
        // With this route and any_ip the interface accepts traffic for every destination, which
        // is what lets a socket pose as whichever host the guest dialled.
        let _ = iface.routes_mut().add_default_ipv4_route(GATEWAY_IP);
        iface.set_any_ip(true);
        Self {
            policy,
            guest_mac: EthernetAddress(guest_mac),
            iface,
            wire,
            sockets: SocketSet::new(Vec::new()),
            flows: HashMap::new(),
            keys: HashMap::new(),
            udp: HashMap::new(),
            forwards: HashMap::new(),
            events,
            on_denied,
            started: Instant::now(),
            next_port: FIRST_LOCAL_PORT,
        }
    }

    fn run(&mut self, events: &Receiver<Event>) {
        let mut delay = None;
        loop {
            let first = match delay {
                Some(delay) => events.recv_timeout(delay).ok(),
                // Nothing scheduled: sleep until something happens. Every sender gone ends it.
                None => match events.recv() {
                    Ok(event) => Some(event),
                    Err(_) => return,
                },
            };
            if let Some(event) = first {
                self.event(event);
            }
            while let Ok(event) = events.try_recv() {
                self.event(event);
            }
            delay = self.settle();
        }
    }

    /// Polls and pumps until no flow can make progress; returns when smoltcp next needs a poll.
    fn settle(&mut self) -> Option<Duration> {
        loop {
            let now = SmolInstant::from_millis(self.started.elapsed().as_millis() as i64);
            self.iface.poll(now, &mut self.wire, &mut self.sockets);
            let pumped = self.pump();
            if !pumped && self.wire.incoming.is_empty() {
                let now = SmolInstant::from_millis(self.started.elapsed().as_millis() as i64);
                return self
                    .iface
                    .poll_delay(now, &self.sockets)
                    .map(|delay: SmolDuration| Duration::from_millis(delay.total_millis()));
            }
        }
    }

    fn pump(&mut self) -> bool {
        let mut progress = false;
        let mut finished = Vec::new();
        for (&handle, flow) in &mut self.flows {
            let socket = self.sockets.get_mut::<tcp::Socket>(handle);
            if !flow.writing && flow.to_host.is_some() && socket.can_recv() {
                let mut chunk = vec![0; CHUNK];
                if let Ok(count) = socket.recv_slice(&mut chunk) {
                    chunk.truncate(count);
                    if let Some(to_host) = &flow.to_host {
                        flow.writing = to_host.send(chunk).is_ok();
                    }
                    progress = true;
                }
            }
            if !flow.to_guest.is_empty() && socket.can_send() {
                let (front, _) = flow.to_guest.as_slices();
                if let Ok(count) = socket.send_slice(front) {
                    flow.to_guest.drain(..count);
                    progress |= count > 0;
                    if flow.to_guest.is_empty() {
                        let _ = flow.credit.try_send(());
                    }
                }
            }
            use tcp::State::{CloseWait, Closed, Closing, LastAck, Listen, TimeWait};
            // The guest finished sending and everything it sent has been taken.
            let guest_done = matches!(
                socket.state(),
                CloseWait | Closing | LastAck | TimeWait | Closed
            );
            if guest_done && !socket.can_recv() && !flow.writing {
                flow.to_host = None;
            }
            if flow.host_closed && flow.to_guest.is_empty() && !flow.close_sent {
                socket.close();
                flow.close_sent = true;
                progress = true;
            }
            // A SYN-received connection the guest reset falls back to listening, for nobody.
            let abandoned = socket.state() == Listen && flow.left_listen;
            flow.left_listen |= socket.state() != Listen;
            if !socket.is_open() || abandoned {
                finished.push(handle);
            }
        }
        for handle in finished {
            if let Some(flow) = self.flows.remove(&handle) {
                let _ = flow.stream.shutdown(Shutdown::Both);
            }
            self.keys.retain(|_, value| *value != Some(handle));
            self.sockets.remove(handle);
        }
        progress
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Frame(frame) => self.frame(frame),
            Event::Connected(key, syn, stream) => {
                self.keys.remove(&key);
                if let Ok(stream) = stream
                    && let Ok(handle) = self.listen(key, stream)
                {
                    self.keys.insert(key, Some(handle));
                }
                // Replayed either way: a listener accepts it, and no listener answers RST.
                self.wire.incoming.push_back(syn);
            }
            Event::FromHost(handle, data) => {
                if let Some(flow) = self.flows.get_mut(&handle) {
                    flow.to_guest.extend(data);
                }
            }
            Event::HostClosed(handle) => {
                if let Some(flow) = self.flows.get_mut(&handle) {
                    flow.host_closed = true;
                }
            }
            Event::Written(handle) => {
                if let Some(flow) = self.flows.get_mut(&handle) {
                    flow.writing = false;
                }
            }
            Event::Inbound(stream, guest_port) => {
                let _ = self.dial_guest(stream, guest_port);
            }
            Event::Datagram(key, payload) => {
                let frame = udp_frame(self.guest_mac, (key.1, key.2), (GUEST_IP, key.0), &payload);
                (self.wire.to_guest)(frame);
            }
            Event::UdpIdle(key) => {
                self.udp.remove(&key);
            }
            Event::AddForward(spec, answer) => {
                let _ = answer.send(self.add_forward(&spec));
            }
            Event::RemoveForward(spec, answer) => {
                let _ = answer.send(self.remove_forward(&spec));
            }
        }
    }

    fn deny(&self, what: &str, address: Ipv4Address, port: u16) {
        (self.on_denied)(format!("{what} {address}:{port}"));
    }

    /// Sorts one guest frame into what the stack handles by hand and what smoltcp sees.
    fn frame(&mut self, frame: Vec<u8>) {
        let Ok(ethernet) = EthernetFrame::new_checked(&frame) else {
            return;
        };
        if ethernet.ethertype() != EthernetProtocol::Ipv4 {
            self.wire.incoming.push_back(frame);
            return;
        }
        let Ok(ip) = Ipv4Packet::new_checked(ethernet.payload()) else {
            return;
        };
        let (source, destination) = (ip.src_addr(), ip.dst_addr());
        match ip.next_header() {
            IpProtocol::Udp => {
                if let Ok(udp) = UdpPacket::new_checked(ip.payload()) {
                    let ports = (udp.src_port(), udp.dst_port());
                    let datagram = udp.payload().to_vec();
                    let quoted = ip.into_inner().to_vec();
                    self.udp_datagram(source, destination, ports, &datagram, &quoted);
                }
            }
            // Only the gateway answers pings; there are no raw sockets to send others with.
            IpProtocol::Icmp if destination != GATEWAY_IP => (),
            IpProtocol::Tcp if source == GUEST_IP => {
                if let Ok(tcp) = TcpPacket::new_checked(ip.payload()) {
                    let key = (source, tcp.src_port(), destination, tcp.dst_port());
                    if tcp.syn() && !tcp.ack() && !self.keys.contains_key(&key) {
                        return self.new_connection(key, frame);
                    }
                    // A retransmitted SYN while the host connect is out would be reset.
                    if self.keys.get(&key).is_some_and(Option::is_none) {
                        return;
                    }
                    self.wire.incoming.push_back(frame);
                }
            }
            IpProtocol::Tcp => (),
            _ => self.wire.incoming.push_back(frame),
        }
    }

    fn new_connection(&mut self, key: TcpKey, syn: Vec<u8>) {
        let (_, _, destination, port) = key;
        let allowed = self.policy.permits(destination, port, Protocol::Tcp);
        if !allowed {
            self.deny("tcp", destination, port);
        }
        if !allowed || self.keys.len() >= MAX_TCP_FLOWS {
            // No listener, so smoltcp answers RST.
            self.wire.incoming.push_back(syn);
            return;
        }
        self.keys.insert(key, None);
        let target = host_address(destination, port);
        let events = self.events.clone();
        std::thread::spawn(move || {
            let stream = TcpStream::connect_timeout(&target, CONNECT_TIMEOUT);
            let _ = events.send(Event::Connected(key, syn, stream));
        });
    }

    fn tcp_socket() -> tcp::Socket<'static> {
        let mut socket = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0; SOCKET_BUFFER]),
            tcp::SocketBuffer::new(vec![0; SOCKET_BUFFER]),
        );
        socket.set_nagle_enabled(false);
        socket
    }

    fn listen(&mut self, key: TcpKey, stream: TcpStream) -> io::Result<SocketHandle> {
        let (_, _, destination, port) = key;
        let mut socket = Self::tcp_socket();
        socket
            .listen(IpListenEndpoint {
                addr: Some(destination.into()),
                port,
            })
            .map_err(io::Error::other)?;
        self.attach(socket, stream)
    }

    fn dial_guest(&mut self, stream: TcpStream, guest_port: u16) -> io::Result<()> {
        if self.flows.len() >= MAX_TCP_FLOWS {
            return Err(io::Error::other("too many flows"));
        }
        let local_port = self.next_port;
        self.next_port = self.next_port.checked_add(1).unwrap_or(FIRST_LOCAL_PORT);
        let mut socket = Self::tcp_socket();
        socket
            .connect(
                self.iface.context(),
                (GUEST_IP, guest_port),
                (GATEWAY_IP, local_port),
            )
            .map_err(io::Error::other)?;
        self.attach(socket, stream)?;
        Ok(())
    }

    /// Joins a socket to its host stream with one reader and one writer thread.
    fn attach(
        &mut self,
        socket: tcp::Socket<'static>,
        stream: TcpStream,
    ) -> io::Result<SocketHandle> {
        let (mut reader, mut writer) = (stream.try_clone()?, stream.try_clone()?);
        let _ = stream.set_nodelay(true);
        let handle = self.sockets.add(socket);
        let (to_host, chunks) = channel::<Vec<u8>>();
        let (credit, wait) = sync_channel(1);
        self.flows.insert(
            handle,
            Flow {
                to_host: Some(to_host),
                writing: false,
                to_guest: VecDeque::new(),
                credit,
                host_closed: false,
                close_sent: false,
                left_listen: false,
                stream,
            },
        );
        let events = self.events.clone();
        std::thread::spawn(move || {
            let mut buffer = vec![0; CHUNK];
            loop {
                let count = reader.read(&mut buffer).unwrap_or(0);
                if count == 0 {
                    let _ = events.send(Event::HostClosed(handle));
                    return;
                }
                let data = buffer[..count].to_vec();
                if events.send(Event::FromHost(handle, data)).is_err() || wait.recv().is_err() {
                    return;
                }
            }
        });
        let events = self.events.clone();
        std::thread::spawn(move || {
            for chunk in chunks {
                if writer.write_all(&chunk).is_err() {
                    let _ = writer.shutdown(Shutdown::Both);
                }
                if events.send(Event::Written(handle)).is_err() {
                    return;
                }
            }
            let _ = writer.shutdown(Shutdown::Write);
        });
        Ok(handle)
    }

    fn udp_datagram(
        &mut self,
        source: Ipv4Address,
        destination: Ipv4Address,
        (source_port, port): (u16, u16),
        datagram: &[u8],
        quoted: &[u8],
    ) {
        if port == 67 {
            if let Some(reply) = dhcp_reply(datagram, self.guest_mac) {
                (self.wire.to_guest)(reply);
            }
            return;
        }
        if source != GUEST_IP {
            return;
        }
        // DNS at the resolver address is the service the network provides, not an outbound
        // connection, so the host's resolver is reachable whatever the policy says.
        let target = if destination == DNS_IP && port == 53 {
            SocketAddr::from((host_resolver(), 53))
        } else if !self.policy.permits(destination, port, Protocol::Udp) {
            self.deny("udp", destination, port);
            let reply = prohibited_frame(self.guest_mac, destination, quoted);
            (self.wire.to_guest)(reply);
            return;
        } else {
            host_address(destination, port)
        };
        let key = (source_port, destination, port);
        if !self.udp.contains_key(&key) {
            if self.udp.len() >= MAX_UDP_FLOWS {
                return;
            }
            let Ok(socket) = open_udp(target) else {
                return;
            };
            let socket = Arc::new(socket);
            self.udp.insert(key, socket.clone());
            let events = self.events.clone();
            std::thread::spawn(move || {
                let mut buffer = vec![0; 65536];
                while let Ok(count) = socket.recv(&mut buffer) {
                    let datagram = buffer[..count].to_vec();
                    if events.send(Event::Datagram(key, datagram)).is_err() {
                        return;
                    }
                }
                let _ = events.send(Event::UdpIdle(key));
            });
        }
        if let Some(socket) = self.udp.get(&key) {
            let _ = socket.send(datagram);
        }
    }

    fn add_forward(&mut self, spec: &str) -> Result<(), String> {
        let (host, guest_port) = parse_forward(spec)?;
        if self.forwards.contains_key(spec) {
            return Err(format!("forward {spec} already exists"));
        }
        let listener = TcpListener::bind(host).map_err(|e| format!("forward {spec}: {e}"))?;
        let bound = listener.local_addr().map_err(|e| e.to_string())?;
        let closed = Arc::new(AtomicBool::new(false));
        self.forwards.insert(spec.into(), (closed.clone(), bound));
        let events = self.events.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if closed.load(Ordering::SeqCst) {
                    return;
                }
                if let Ok(stream) = stream
                    && events.send(Event::Inbound(stream, guest_port)).is_err()
                {
                    return;
                }
            }
        });
        Ok(())
    }

    fn remove_forward(&mut self, spec: &str) -> Result<(), String> {
        let (closed, bound) = self
            .forwards
            .remove(spec)
            .ok_or_else(|| format!("no forward {spec}"))?;
        closed.store(true, Ordering::SeqCst);
        // accept has no timeout; one last connection wakes it to see the flag.
        let _ = TcpStream::connect(bound);
        Ok(())
    }
}

fn open_udp(target: SocketAddr) -> io::Result<UdpSocket> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.connect(target)?;
    socket.set_read_timeout(Some(UDP_IDLE))?;
    Ok(socket)
}

/// The gateway stands for the host itself, as with slirp.
fn host_address(destination: Ipv4Address, port: u16) -> SocketAddr {
    let address = if destination == GATEWAY_IP {
        Ipv4Addr::LOCALHOST
    } else {
        destination
    };
    SocketAddr::from((address, port))
}

fn host_resolver() -> IpAddr {
    let text = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    text.lines()
        .filter_map(|line| line.trim().strip_prefix("nameserver"))
        .filter_map(|address| address.trim().parse::<Ipv4Addr>().ok())
        .next()
        .map_or(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), IpAddr::V4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use smoltcp::{
        iface::Config as GuestConfig,
        socket::tcp::{Socket, SocketBuffer},
    };
    use std::sync::mpsc::Receiver;

    const GUEST_MAC: [u8; 6] = [2, 0, 0, 0, 0, 1];

    /// A second smoltcp interface plays the guest, exchanging real frames with the stack.
    struct Guest {
        net: Net,
        frames: Receiver<Vec<u8>>,
        iface: Interface,
        wire: GuestWire,
        sockets: SocketSet<'static>,
        started: Instant,
    }

    #[derive(Default)]
    struct GuestWire {
        incoming: VecDeque<Vec<u8>>,
        outgoing: Vec<Vec<u8>>,
    }

    struct GuestTx<'a>(&'a mut Vec<Vec<u8>>);

    impl phy::TxToken for GuestTx<'_> {
        fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, length: usize, f: F) -> R {
            let mut frame = vec![0; length];
            let result = f(&mut frame);
            self.0.push(frame);
            result
        }
    }

    impl Device for GuestWire {
        type RxToken<'a> = RxToken;
        type TxToken<'a> = GuestTx<'a>;
        fn receive(&mut self, _: SmolInstant) -> Option<(RxToken, GuestTx<'_>)> {
            let frame = self.incoming.pop_front()?;
            Some((RxToken(frame), GuestTx(&mut self.outgoing)))
        }
        fn transmit(&mut self, _: SmolInstant) -> Option<GuestTx<'_>> {
            Some(GuestTx(&mut self.outgoing))
        }
        fn capabilities(&self) -> DeviceCapabilities {
            let mut capabilities = DeviceCapabilities::default();
            capabilities.medium = Medium::Ethernet;
            capabilities.max_transmission_unit = 1514;
            capabilities
        }
    }

    impl Guest {
        fn new(allow: &[&str], denied: Sender<String>) -> Self {
            let (out, frames) = channel();
            let settings = Settings {
                default: "allow".into(),
                allow: allow.iter().map(ToString::to_string).collect(),
                deny: vec!["192.0.2.0/24".into()],
                forward: Vec::new(),
                on_denied: Box::new(move |what| {
                    let _ = denied.send(what);
                }),
            };
            let net = Net::start(settings, GUEST_MAC, move |frame| {
                let _ = out.send(frame);
            })
            .unwrap();
            let mut wire = GuestWire::default();
            let config = GuestConfig::new(HardwareAddress::Ethernet(EthernetAddress(GUEST_MAC)));
            let mut iface = Interface::new(config, &mut wire, SmolInstant::from_millis(0));
            iface.update_ip_addrs(|addresses| {
                let _ = addresses.push(IpCidr::new(GUEST_IP.into(), 24));
            });
            iface
                .routes_mut()
                .add_default_ipv4_route(GATEWAY_IP)
                .unwrap();
            Self {
                net,
                frames,
                iface,
                wire,
                sockets: SocketSet::new(Vec::new()),
                started: Instant::now(),
            }
        }

        fn poll(&mut self) {
            let now = SmolInstant::from_millis(self.started.elapsed().as_millis() as i64);
            self.iface.poll(now, &mut self.wire, &mut self.sockets);
            for frame in self.wire.outgoing.drain(..) {
                self.net.frame(frame);
            }
            while let Ok(frame) = self.frames.recv_timeout(Duration::from_millis(1)) {
                self.wire.incoming.push_back(frame);
            }
        }

        fn dial(&mut self, to: Ipv4Address, port: u16) -> SocketHandle {
            let mut socket = Socket::new(
                SocketBuffer::new(vec![0; 4096]),
                SocketBuffer::new(vec![0; 4096]),
            );
            socket
                .connect(self.iface.context(), (to, port), (GUEST_IP, 40000))
                .unwrap();
            self.sockets.add(socket)
        }

        fn poll_until(&mut self, mut done: impl FnMut(&mut Self) -> bool) {
            let started = Instant::now();
            while !done(self) {
                assert!(started.elapsed() < Duration::from_secs(10), "timed out");
                self.poll();
            }
        }
    }

    #[test]
    fn a_guest_connection_reaches_the_host_and_back() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut question = [0; 4];
            stream.read_exact(&mut question).unwrap();
            assert_eq!(&question, b"ping");
            stream.write_all(b"pong").unwrap();
        });
        let (denied, _) = channel();
        let mut guest = Guest::new(&["10.0.2.2/32"], denied);
        let handle = guest.dial(GATEWAY_IP, port);
        guest.poll_until(|g| g.sockets.get::<Socket>(handle).can_send());
        guest
            .sockets
            .get_mut::<Socket>(handle)
            .send_slice(b"ping")
            .unwrap();
        guest.poll_until(|g| g.sockets.get::<Socket>(handle).can_recv());
        let mut answer = [0; 4];
        guest
            .sockets
            .get_mut::<Socket>(handle)
            .recv_slice(&mut answer)
            .unwrap();
        assert_eq!(&answer, b"pong");
        // The host closed: the guest sees the end of the stream.
        guest.poll_until(|g| !g.sockets.get::<Socket>(handle).may_recv());
    }

    #[test]
    fn denied_and_loopback_destinations_are_reset_and_reported() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (denied, reports) = channel();
        let mut guest = Guest::new(&[], denied);
        let reset = guest.dial(Ipv4Address::new(192, 0, 2, 1), 80);
        guest.poll_until(|g| !g.sockets.get::<Socket>(reset).is_open());
        assert_eq!(reports.try_recv().unwrap(), "tcp 192.0.2.1:80");
        // The host's own services are closed to the guest by default, even with default allow.
        let mut other = Socket::new(
            SocketBuffer::new(vec![0; 4096]),
            SocketBuffer::new(vec![0; 4096]),
        );
        other
            .connect(guest.iface.context(), (GATEWAY_IP, port), (GUEST_IP, 40001))
            .unwrap();
        let other = guest.sockets.add(other);
        guest.poll_until(|g| !g.sockets.get::<Socket>(other).is_open());
        assert_eq!(reports.try_recv().unwrap(), format!("tcp 10.0.2.2:{port}"));
        listener.set_nonblocking(true).unwrap();
        assert!(listener.accept().is_err(), "a socket opened anyway");
    }

    #[test]
    fn a_forward_dials_the_guest_port() {
        let (denied, _) = channel();
        let mut guest = Guest::new(&[], denied);
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = probe.local_addr().unwrap();
        drop(probe);
        let spec = format!("{host}->8080");
        guest.net.add_forward(&spec).unwrap();
        assert!(guest.net.add_forward(&spec).is_err());
        let mut socket = Socket::new(
            SocketBuffer::new(vec![0; 4096]),
            SocketBuffer::new(vec![0; 4096]),
        );
        socket.listen(8080).unwrap();
        let listening = guest.sockets.add(socket);
        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(host).unwrap();
            stream.write_all(b"hi").unwrap();
            let mut back = [0; 2];
            stream.read_exact(&mut back).unwrap();
            back
        });
        guest.poll_until(|g| g.sockets.get::<Socket>(listening).can_recv());
        let mut heard = [0; 2];
        guest
            .sockets
            .get_mut::<Socket>(listening)
            .recv_slice(&mut heard)
            .unwrap();
        assert_eq!(&heard, b"hi");
        guest
            .sockets
            .get_mut::<Socket>(listening)
            .send_slice(b"ok")
            .unwrap();
        while !client.is_finished() {
            guest.poll();
        }
        assert_eq!(&client.join().unwrap(), b"ok");
        guest.net.remove_forward(&spec).unwrap();
        assert!(guest.net.remove_forward(&spec).is_err());
    }
}
