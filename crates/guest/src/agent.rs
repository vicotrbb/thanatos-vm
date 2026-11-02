use nix::sys::socket::{
    AddressFamily, Backlog, MsgFlags, SockFlag, SockType, VsockAddr, accept, bind, listen, recv,
    send, socket,
};
use std::{
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, RawFd},
        unix::{fs::OpenOptionsExt, process::ExitStatusExt},
    },
    process::{Command, Stdio},
    sync::{Arc, Mutex},
};
use thanatos_vmm::api::{AGENT_PORT, AgentFrame, AgentRequest, MAX_BODY, read_frame, write_frame};

const ANY_CID: u32 = u32::MAX;

/// An accepted socket. nix hands it out as a raw descriptor, which its safe `recv`, `send` and
/// `close` take directly.
struct Conn(RawFd);

impl Drop for Conn {
    fn drop(&mut self) {
        let _ = nix::unistd::close(self.0);
    }
}

impl Read for &Conn {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        Ok(recv(self.0, buffer, MsgFlags::empty())?)
    }
}

impl Write for &Conn {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        Ok(send(self.0, buffer, MsgFlags::MSG_NOSIGNAL)?)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn serve() -> io::Result<()> {
    let listener = socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC,
        None,
    )?;
    bind(listener.as_raw_fd(), &VsockAddr::new(ANY_CID, AGENT_PORT))?;
    listen(&listener, Backlog::new(16)?)?;
    loop {
        let conn = Arc::new(Conn(accept(listener.as_raw_fd())?));
        std::thread::spawn(move || {
            let mut stream: &Conn = &conn;
            if let Err(error) = request(&conn) {
                let message = error.to_string();
                let _ = write_frame(&mut stream, &AgentFrame::Error { message }, &[]);
            }
        });
    }
}

fn request(conn: &Arc<Conn>) -> io::Result<()> {
    let mut stream = &**conn;
    let Some((request, _)) = read_frame::<AgentRequest>(&mut stream)? else {
        return Ok(());
    };
    match request {
        AgentRequest::Heartbeat => write_frame(&mut stream, &AgentFrame::Done, &[]),
        AgentRequest::Poweroff => {
            // Without ACPI an x86 poweroff only halts; the kernel's reboot=t ends the VM instead.
            let verb = if cfg!(target_arch = "x86_64") {
                "reboot"
            } else {
                "poweroff"
            };
            Command::new("systemctl").arg(verb).status()?;
            write_frame(&mut stream, &AgentFrame::Done, &[])
        }
        AgentRequest::FilePut { path, mode } => {
            let mut options = File::options();
            options.write(true).create(true).truncate(true);
            if let Some(mode) = mode {
                options.mode(mode);
            }
            let mut file = options.open(path)?;
            while let Some((frame, body)) = read_frame::<AgentFrame>(&mut stream)? {
                if frame != AgentFrame::Data {
                    return Err(io::Error::other("expected a data frame"));
                }
                if body.is_empty() {
                    file.sync_all()?;
                    return write_frame(&mut stream, &AgentFrame::Done, &[]);
                }
                file.write_all(&body)?;
            }
            Err(io::Error::other("stream ended before the end of the file"))
        }
        AgentRequest::FileGet { path } => {
            let mut file = File::open(path)?;
            let mut buffer = vec![0; MAX_BODY];
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    return write_frame(&mut stream, &AgentFrame::Done, &[]);
                }
                write_frame(&mut stream, &AgentFrame::Data, &buffer[..count])?;
            }
        }
        AgentRequest::Exec {
            argv,
            env,
            cwd,
            stdin,
        } => exec(conn, &argv, &env, cwd.as_deref(), stdin),
    }
}

fn exec(
    conn: &Arc<Conn>,
    argv: &[String],
    env: &std::collections::BTreeMap<String, String>,
    cwd: Option<&str>,
    stdin: bool,
) -> io::Result<()> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| io::Error::other("empty argv"))?;
    let mut command = Command::new(program);
    command
        .args(args)
        .envs(env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if stdin { Stdio::piped() } else { Stdio::null() });
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let mut child = command.spawn()?;
    let (out, err) = (child.stdout.take(), child.stderr.take());
    // Detached: a client that never ends stdin must not hold back the exit status.
    if let Some(mut input) = child.stdin.take() {
        let conn = conn.clone();
        std::thread::spawn(move || {
            let mut stream: &Conn = &conn;
            while let Ok(Some((AgentFrame::Stdin, body))) = read_frame(&mut stream) {
                if body.is_empty() || input.write_all(&body).is_err() {
                    return;
                }
            }
        });
    }
    // Both output pumps write to one socket, and a frame must go out whole.
    let lock = Mutex::new(());
    std::thread::scope(|scope| {
        let pump = |mut source: Box<dyn Read + Send>, frame: AgentFrame| {
            let lock = &lock;
            let conn: &Conn = conn;
            scope.spawn(move || {
                let mut stream = conn;
                let mut buffer = vec![0; 1 << 16];
                while let Ok(count @ 1..) = source.read(&mut buffer) {
                    let _held = lock.lock().unwrap();
                    if write_frame(&mut stream, &frame, &buffer[..count]).is_err() {
                        return;
                    }
                }
            });
        };
        if let Some(out) = out {
            pump(Box::new(out), AgentFrame::Stdout);
        }
        if let Some(err) = err {
            pump(Box::new(err), AgentFrame::Stderr);
        }
    });
    let status = child.wait()?;
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    let mut stream: &Conn = conn;
    write_frame(&mut stream, &AgentFrame::Exit { code }, &[])
}
