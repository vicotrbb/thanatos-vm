//! The control API: JSON lines on a mode-0600 Unix socket, one thread per connection.
use crate::{
    api::{
        AGENT_PORT, AgentRequest, Body, Event, ImageFormat, InputEvent, Reply, Request, VncOptions,
        read_frame, write_frame,
    },
    config::{Config, expand},
    devices::{input::ABS_MAX, vsock},
    machine::Control,
    rfb::Vnc,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{self, Read, Write},
    net::Shutdown,
    os::unix::{
        fs::{DirBuilderExt, FileTypeExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const MAX_LINE: usize = 1 << 20;

/// A token's far end, waiting for its attach.
enum Channel {
    Console,
    Stream(UnixStream),
    Bytes(Vec<u8>),
}

/// How long a slow subscriber can hold up the line before it is dropped.
const EVENT_WRITE_TIMEOUT: Duration = Duration::from_secs(1);
const AGENT_POLL_BOOTING: Duration = Duration::from_millis(500);
const AGENT_POLL_RUNNING: Duration = Duration::from_secs(5);

/// A connection's write side: a reply or an event goes out whole.
type Writer = Arc<Mutex<UnixStream>>;

fn write_line(writer: &Writer, value: &impl serde::Serialize) -> io::Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    writer.lock().unwrap().write_all(&line)
}

/// The subscribers of the event stream.
#[derive(Default)]
pub struct Events(Mutex<Vec<Writer>>);

impl Events {
    pub fn emit(&self, event: &Event) {
        self.0
            .lock()
            .unwrap()
            .retain(|writer| write_line(writer, event).is_ok());
    }

    fn subscribe(&self, writer: Writer) -> io::Result<()> {
        writer
            .lock()
            .unwrap()
            .set_write_timeout(Some(EVENT_WRITE_TIMEOUT))?;
        self.0.lock().unwrap().push(writer);
        Ok(())
    }
}

struct Server {
    control: Control,
    started: Instant,
    /// shortcut: a token nobody attaches stays until the VM ends, expire them if a client churns.
    tokens: Mutex<HashMap<String, Channel>>,
    stopping: Mutex<bool>,
    agent_alive: Mutex<bool>,
    agent_enabled: bool,
    vcpus: u32,
    memory_mib: u64,
    vnc_bind: String,
    #[cfg(feature = "window")]
    name: String,
    display: Arc<Mutex<Display>>,
}

/// The mode, and the running sink: dropping a sink stops it.
#[derive(Default)]
struct Display {
    mode: String,
    vnc: Option<Vnc>,
    #[cfg(feature = "window")]
    window: Option<crate::window::Window>,
}

pub fn socket_path(config: &Config) -> PathBuf {
    match &config.api.socket {
        Some(path) => expand(path),
        None => expand(Path::new(&format!(
            "~/.thanatos/vms/{}/api.sock",
            config.name
        ))),
    }
}

/// Binds in a private directory and renames into place, so the socket is never reachable with
/// wider permissions. A live VM's socket is refused; a stale one is replaced.
fn bind(path: &Path) -> io::Result<UnixListener> {
    let is_socket = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket());
    if is_socket {
        if UnixStream::connect(path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("{} belongs to a running VM", path.display()),
            ));
        }
        std::fs::remove_file(path)?;
    }
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let private = parent.join(format!(".api-{}", std::process::id()));
    std::fs::DirBuilder::new().mode(0o700).create(&private)?;
    let inner = private.join("s");
    let bound = UnixListener::bind(&inner).and_then(|listener| {
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&inner, path)?;
        Ok(listener)
    });
    let _ = std::fs::remove_dir_all(&private);
    bound
}

pub fn serve(path: &Path, config: &Config, control: Control) -> io::Result<()> {
    let listener = bind(path)?;
    control.events.emit(&Event::Boot);
    let server = Arc::new(Server {
        control,
        started: Instant::now(),
        tokens: Mutex::default(),
        stopping: Mutex::new(false),
        agent_alive: Mutex::new(false),
        agent_enabled: config.agent.enabled,
        vcpus: config.vcpus,
        memory_mib: config.memory_mib,
        vnc_bind: config.display.vnc_bind.clone(),
        #[cfg(feature = "window")]
        name: config.name.clone(),
        display: Arc::new(Mutex::new(Display {
            mode: "headless".into(),
            ..Display::default()
        })),
    });
    server.set_display(&config.display.mode, None)?;
    if config.agent.enabled {
        let server = server.clone();
        std::thread::spawn(move || server.watch_agent());
    }
    std::thread::spawn(move || {
        for connection in listener.incoming().flatten() {
            let server = server.clone();
            std::thread::spawn(move || {
                let _ = server.connection(connection);
            });
        }
    });
    Ok(())
}

/// Reads to a newline, a byte at a time so nothing past it is consumed: after an attach line the
/// same connection carries raw bytes. `None` at end of stream.
pub fn read_line(input: &mut impl Read) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    let mut byte = [0];
    loop {
        if input.read(&mut byte)? == 0 {
            return Ok(None);
        }
        if byte[0] == b'\n' {
            return String::from_utf8(line)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
        }
        if line.len() >= MAX_LINE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
        }
        line.push(byte[0]);
    }
}

fn token() -> io::Result<String> {
    let mut bytes = [0; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn splice(from: UnixStream, to: UnixStream) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let (mut from_ref, mut to_ref) = (&from, &to);
        let _ = io::copy(&mut from_ref, &mut to_ref);
        let _ = to.shutdown(Shutdown::Write);
    })
}

impl Server {
    /// Polls the agent, reporting only when its liveness changes.
    fn watch_agent(&self) {
        loop {
            let alive = heartbeat(&self.control).is_ok();
            let mut known = self.agent_alive.lock().unwrap();
            if *known != alive {
                *known = alive;
                drop(known);
                self.control.events.emit(&Event::Agent { alive });
            } else {
                drop(known);
            }
            std::thread::sleep(if alive {
                AGENT_POLL_RUNNING
            } else {
                AGENT_POLL_BOOTING
            });
        }
    }

    fn set_display(&self, mode: &str, vnc: Option<VncOptions>) -> io::Result<()> {
        let mut next = Display {
            mode: mode.into(),
            ..Display::default()
        };
        match mode {
            "headless" => (),
            "vnc" => {
                let options = vnc.unwrap_or(VncOptions {
                    bind: None,
                    password: None,
                });
                let bind = options.bind.unwrap_or_else(|| self.vnc_bind.clone());
                // The old listener must let go first in case the address is the same.
                self.display.lock().unwrap().vnc = None;
                next.vnc = Some(Vnc::start(
                    &bind,
                    options.password,
                    self.control.screen.clone(),
                    self.control.input.clone(),
                )?);
            }
            #[cfg(feature = "window")]
            "window" => {
                // Closing the window leaves the VM running, headless.
                let display = self.display.clone();
                let events = self.control.events.clone();
                // Not under the lock: the window's thread takes it when the window closes.
                next.window = Some(crate::window::open(
                    format!("Thanatos: {}", self.name),
                    self.control.screen.clone(),
                    self.control.input.clone(),
                    Box::new(move || {
                        *display.lock().unwrap() = Display {
                            mode: "headless".into(),
                            ..Display::default()
                        };
                        events.emit(&Event::DisplayMode {
                            mode: "headless".into(),
                        });
                    }),
                )?);
            }
            #[cfg(not(feature = "window"))]
            "window" => {
                return Err(io::Error::other(
                    "this build has no window sink: build with --features window",
                ));
            }
            other => return Err(io::Error::other(format!("unknown display mode {other:?}"))),
        }
        *self.display.lock().unwrap() = next;
        self.control
            .events
            .emit(&Event::DisplayMode { mode: mode.into() });
        Ok(())
    }

    fn inject(&self, events: &[InputEvent]) {
        let input = &self.control.input;
        let (width, height) = self.control.screen.with(|f| (f.width, f.height));
        let scale = |value: u32, size: u32| {
            value.min(size.saturating_sub(1)) * ABS_MAX / size.saturating_sub(1).max(1)
        };
        for event in events {
            match *event {
                InputEvent::Key { code, down } => input.key(code, down),
                InputEvent::PointerMove { x, y } => {
                    input.pointer(scale(x, width), scale(y, height))
                }
                InputEvent::Button { button, down } => input.button(button, down),
                InputEvent::Scroll { clicks } => input.scroll(clicks),
            }
        }
    }

    fn connection(&self, mut connection: UnixStream) -> io::Result<()> {
        let writer: Writer = Arc::new(Mutex::new(connection.try_clone()?));
        let mut first = true;
        while let Some(line) = read_line(&mut connection)? {
            let value: Result<Value, _> = serde_json::from_str(&line);
            if first
                && let Ok(Value::String(token)) =
                    value.as_ref().map(|v| v["attach"].clone()).map_err(|_| ())
            {
                return self.attach(&token, connection);
            }
            first = false;
            let reply = match serde_json::from_str::<Request>(&line) {
                Ok(request) => {
                    let subscribing = request.body == Body::Events;
                    if subscribing {
                        self.control.events.subscribe(writer.clone())?;
                    }
                    let (result, error) = match self.handle(request.body) {
                        Ok(result) => (Some(result), None),
                        Err(error) => (None, Some(error.to_string())),
                    };
                    Reply {
                        id: request.id,
                        result,
                        error,
                    }
                }
                Err(error) => Reply {
                    id: value.ok().and_then(|v| v["id"].as_u64()).unwrap_or(0),
                    result: None,
                    error: Some(format!("bad request: {error}")),
                },
            };
            write_line(&writer, &reply)?;
        }
        Ok(())
    }

    fn attach(&self, token: &str, mut connection: UnixStream) -> io::Result<()> {
        let channel = self.tokens.lock().unwrap().remove(token);
        match channel {
            None => {
                let _ = writeln!(connection, "{}", json!({ "error": "unknown token" }));
                Ok(())
            }
            Some(Channel::Bytes(bytes)) => connection.write_all(&bytes),
            Some(Channel::Stream(stream)) => {
                let (back_from, back_to) = (stream.try_clone()?, connection.try_clone()?);
                let there = splice(connection, stream);
                let back = splice(back_from, back_to);
                let _ = there.join();
                let _ = back.join();
                Ok(())
            }
            Some(Channel::Console) => {
                self.control.console.add_output(connection.try_clone()?);
                let mut buffer = [0; 4096];
                loop {
                    let count = connection.read(&mut buffer)?;
                    if count == 0 {
                        // Later guest output finds this socket closed and drops it.
                        return connection.shutdown(Shutdown::Both);
                    }
                    self.control.console.input(&buffer[..count]);
                }
            }
        }
    }

    fn issue(&self, channel: Channel) -> io::Result<Value> {
        let token = token()?;
        self.tokens.lock().unwrap().insert(token.clone(), channel);
        Ok(json!({ "token": token }))
    }

    fn agent(&self, request: &AgentRequest) -> io::Result<UnixStream> {
        let mut stream = vsock::connect(&self.control.vsock, AGENT_PORT)?;
        write_frame(&mut stream, request, &[])?;
        Ok(stream)
    }

    fn forward(
        &self,
        change: impl FnOnce(&thanatos_net::Net) -> Result<(), String>,
    ) -> io::Result<Value> {
        let net = self.control.network.as_ref().ok_or_else(|| {
            io::Error::other("forwards do not apply to a tap: the operator's network does")
        })?;
        change(net).map_err(io::Error::other)?;
        Ok(json!({}))
    }

    fn handle(&self, body: Body) -> io::Result<Value> {
        match body {
            Body::Status => {
                let state = if *self.stopping.lock().unwrap() {
                    "stopping"
                } else if self.agent_enabled && !*self.agent_alive.lock().unwrap() {
                    "booting"
                } else {
                    "running"
                };
                Ok(json!({
                    "state": state,
                    "uptime_seconds": self.started.elapsed().as_secs(),
                    "vcpus": self.vcpus,
                    "memory_mib": self.memory_mib,
                    "devices": ["blk", "rng", "vsock", "net", "gpu", "input"],
                    "display_mode": self.display.lock().unwrap().mode,
                    "agent_alive": *self.agent_alive.lock().unwrap(),
                }))
            }
            Body::Screenshot { format } => {
                let (width, height, bytes) = match format {
                    ImageFormat::Png => {
                        let png = self.control.screen.png()?;
                        let (width, height) = self.control.screen.with(|f| (f.width, f.height));
                        (width, height, png)
                    }
                    ImageFormat::Raw => self
                        .control
                        .screen
                        .with(|f| (f.width, f.height, f.pixels.clone())),
                };
                let mut reply = self.issue(Channel::Bytes(bytes))?;
                reply["width"] = width.into();
                reply["height"] = height.into();
                Ok(reply)
            }
            Body::Input { events } => {
                self.inject(&events);
                Ok(json!({}))
            }
            Body::Display { mode, vnc } => {
                self.set_display(&mode, vnc)?;
                let bound = self.display.lock().unwrap().vnc.as_ref().map(Vnc::address);
                Ok(json!({ "mode": mode, "address": bound.map(|a| a.to_string()) }))
            }
            Body::ForwardAdd { spec } => self.forward(|net| net.add_forward(&spec)),
            Body::ForwardRemove { spec } => self.forward(|net| net.remove_forward(&spec)),
            Body::Events => Ok(json!({})),
            Body::Stop { graceful, timeout } => {
                *self.stopping.lock().unwrap() = true;
                let stop = self.control.stop.clone();
                if !graceful {
                    stop();
                    return Ok(json!({}));
                }
                let asked = self.agent(&AgentRequest::Poweroff);
                std::thread::spawn(move || {
                    if asked.is_ok() {
                        std::thread::sleep(Duration::from_secs(timeout));
                    }
                    stop();
                });
                Ok(json!({}))
            }
            Body::Console => self.issue(Channel::Console),
            Body::Vsock { port } => {
                let stream = vsock::connect(&self.control.vsock, port)?;
                self.issue(Channel::Stream(stream))
            }
            Body::Exec {
                argv,
                env,
                cwd,
                stdin,
            } => {
                let stream = self.agent(&AgentRequest::Exec {
                    argv,
                    env,
                    cwd,
                    stdin,
                })?;
                self.issue(Channel::Stream(stream))
            }
            Body::FilePut { path, mode } => {
                let stream = self.agent(&AgentRequest::FilePut { path, mode })?;
                self.issue(Channel::Stream(stream))
            }
            Body::FileGet { path } => {
                let stream = self.agent(&AgentRequest::FileGet { path })?;
                self.issue(Channel::Stream(stream))
            }
        }
    }
}

/// Asks the agent whether it is there.
pub fn heartbeat(control: &Control) -> io::Result<()> {
    let mut stream = vsock::connect(&control.vsock, AGENT_PORT)?;
    write_frame(&mut stream, &AgentRequest::Heartbeat, &[])?;
    read_frame::<crate::api::AgentFrame>(&mut stream)?
        .map(|_| ())
        .ok_or_else(|| io::Error::other("agent closed the connection"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("th-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn socket_is_private_a_live_one_refused_and_a_stale_one_replaced() {
        let path = dir("bind").join("api.sock");
        let live = bind(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(bind(&path).unwrap_err().kind(), io::ErrorKind::AddrInUse);
        drop(live);
        bind(&path).unwrap();
    }

    #[test]
    fn a_line_stops_at_its_newline_and_a_huge_one_is_refused() {
        let mut input: &[u8] = b"one\nraw";
        assert_eq!(read_line(&mut input).unwrap().unwrap(), "one");
        assert_eq!(input, b"raw");
        let huge = vec![b'x'; MAX_LINE + 1];
        assert!(read_line(&mut huge.as_slice()).is_err());
    }
}
