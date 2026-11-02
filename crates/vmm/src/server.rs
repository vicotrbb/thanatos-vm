//! The control API: JSON lines on a mode-0600 Unix socket, one thread per connection.
use crate::{
    api::{AGENT_PORT, AgentRequest, Body, Reply, Request, read_frame, write_frame},
    config::{Config, expand},
    devices::vsock,
    machine::Control,
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
}

struct Server {
    control: Control,
    started: Instant,
    /// shortcut: a token nobody attaches stays until the VM ends, expire them if a client churns.
    tokens: Mutex<HashMap<String, Channel>>,
    stopping: Mutex<bool>,
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

pub fn serve(path: &Path, control: Control) -> io::Result<()> {
    let listener = bind(path)?;
    let server = Arc::new(Server {
        control,
        started: Instant::now(),
        tokens: Mutex::default(),
        stopping: Mutex::new(false),
    });
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
    fn connection(&self, mut connection: UnixStream) -> io::Result<()> {
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
            let mut line = serde_json::to_vec(&reply)?;
            line.push(b'\n');
            connection.write_all(&line)?;
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

    fn handle(&self, body: Body) -> io::Result<Value> {
        match body {
            Body::Status => Ok(json!({
                "state": if *self.stopping.lock().unwrap() { "stopping" } else { "running" },
                "uptime_seconds": self.started.elapsed().as_secs(),
            })),
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
