//! `thanatos`: thin commands over the machine and its API.
use std::{
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    sync::Arc,
};
use thanatos_hv::{Host, Hypervisor};
use thanatos_vmm::{
    api::{self, AgentFrame, Body, ImageFormat, InputEvent, Reply, Request, VncOptions},
    config::{Config, expand},
    console::Console,
    machine, server,
};

const ROOT_DISK_BYTES: u64 = 16 << 30;

const ENTITLEMENTS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>com.apple.security.hypervisor</key><true/></dict></plist>
"#;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["doctor"] => doctor(),
        ["doctor", "--sign"] => sign().and_then(|()| reexec_doctor()),
        ["run", path] => run(path, None),
        ["run", path, "--display", mode] => run(path, Some(mode)),
        ["image", "build", rest @ ..] => image_build(rest),
        ["vm", "create", name] => vm_create(name),
        ["exec", "--socket", name, "--", argv @ ..] | ["exec", name, "--", argv @ ..]
            if !argv.is_empty() =>
        {
            return exec(name, argv).unwrap_or_else(|message| {
                eprintln!("{message}");
                ExitCode::FAILURE
            });
        }
        ["status", name] => show(name, Body::Status),
        ["screenshot", name] => screenshot(name),
        ["display", name, "headless"] => display(name, "headless", None),
        ["display", name, "window"] => display(name, "window", None),
        ["display", name, "vnc"] => display(name, "vnc", None),
        ["display", name, "vnc", bind] => display(name, "vnc", Some((*bind).into())),
        ["display", name, "vnc", bind, password] => display_with_password(name, bind, password),
        ["key", name, code] => key(name, code),
        ["console", name] => console(name),
        ["forward", "add", name, spec] => show(name, Body::ForwardAdd { spec: (*spec).into() }),
        ["forward", "remove", name, spec] => show(name, Body::ForwardRemove { spec: (*spec).into() }),
        ["events", name] => events(name),
        ["stop", name] => stop(name, true),
        ["stop", name, "--force"] => stop(name, false),
        ["file", "put", name, local, guest] => file_put(name, local, guest),
        ["file", "get", name, guest, local] => file_get(name, guest, local),
        _ => Err("usage: thanatos doctor [--sign] | run <vm.toml> [--display headless|window|vnc] | image build [--arch A] [--profile P] | vm create <name> | exec <name> -- <argv...> | stop <name> [--force] | file put <name> <local> <guest> | file get <name> <guest> <local> | status <name> | screenshot <name> | display <name> headless|window|vnc [bind] [password] | key <name> <code[+code...]> | console <name> | forward add|remove <name> <spec> | events <name>".to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

/// Windows live on the main thread on macOS, so the VM runs beside it.
fn run(path: &str, display: Option<&str>) -> Result<(), String> {
    let (path, display) = (path.to_owned(), display.map(str::to_owned));
    thanatos_vmm::window::host(move || {
        let ended = run_machine(&path, display);
        if let Err(message) = &ended {
            eprintln!("{message}");
        }
        std::process::exit(ended.is_err().into());
    });
    Ok(())
}

fn run_machine(path: &str, display: Option<String>) -> Result<(), String> {
    let mut config = Config::load(path.as_ref()).map_err(|e| e.to_string())?;
    if let Some(mode) = display {
        config.display.mode = mode;
    }
    let console = Console::default();
    console.add_output(std::io::stdout());
    let socket = server::socket_path(&config);
    let events = Arc::new(server::Events::default());
    let ended = machine::run(&config, console, events.clone(), |control| {
        // A VM nothing can reach or stop should not keep running.
        let stop = control.stop.clone();
        if let Err(error) = server::serve(&socket, &config, control) {
            eprintln!("control API {}: {error}", socket.display());
            stop();
        }
    });
    if let Ok(ended) = &ended {
        events.emit(&api::Event::Shutdown {
            status: format!("{ended:?}").to_lowercase(),
        });
    }
    let _ = std::fs::remove_file(&socket);
    ended.map_err(|e| e.to_string())?;
    Ok(())
}

/// A VM by name, or by socket path for a VM with its own `[api] socket`.
fn connect(target: &str) -> Result<UnixStream, String> {
    let path = if target.contains('/') {
        PathBuf::from(target)
    } else {
        home_path(&format!("~/.thanatos/vms/{target}/api.sock"))
    };
    UnixStream::connect(&path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Sends one request on a new connection and returns its result.
fn request(name: &str, body: Body) -> Result<serde_json::Value, String> {
    let mut socket = connect(name)?;
    let line = serde_json::to_string(&Request { id: 1, body }).map_err(|e| e.to_string())?;
    writeln!(socket, "{line}").map_err(|e| e.to_string())?;
    let line = server::read_line(&mut socket)
        .map_err(|e| e.to_string())?
        .ok_or("the VM closed the connection")?;
    let reply: Reply = serde_json::from_str(&line).map_err(|e| e.to_string())?;
    match reply.error {
        Some(error) => Err(error),
        None => Ok(reply.result.unwrap_or_default()),
    }
}

/// Opens a binary channel from a token result.
fn attach(name: &str, result: &serde_json::Value) -> Result<UnixStream, String> {
    let token = result["token"].as_str().ok_or("no token in the reply")?;
    let mut socket = connect(name)?;
    writeln!(socket, "{}", serde_json::json!({ "attach": token })).map_err(|e| e.to_string())?;
    Ok(socket)
}

fn exec(name: &str, argv: &[&str]) -> Result<ExitCode, String> {
    let result = request(
        name,
        Body::Exec {
            argv: argv.iter().map(ToString::to_string).collect(),
            env: Default::default(),
            cwd: None,
            stdin: true,
        },
    )?;
    let mut socket = attach(name, &result)?;
    let mut to_guest = socket.try_clone().map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        let mut buffer = vec![0; 1 << 16];
        while let Ok(count @ 1..) = std::io::stdin().read(&mut buffer) {
            if api::write_frame(&mut to_guest, &AgentFrame::Stdin, &buffer[..count]).is_err() {
                return;
            }
        }
        let _ = api::write_frame(&mut to_guest, &AgentFrame::Stdin, &[]);
    });
    loop {
        let Some((frame, body)) =
            api::read_frame::<AgentFrame>(&mut socket).map_err(|e| e.to_string())?
        else {
            return Err("the agent closed the stream before the exit status".into());
        };
        match frame {
            AgentFrame::Stdout => {
                let mut out = std::io::stdout();
                out.write_all(&body)
                    .and_then(|()| out.flush())
                    .map_err(|e| e.to_string())?;
            }
            AgentFrame::Stderr => std::io::stderr()
                .write_all(&body)
                .map_err(|e| e.to_string())?,
            AgentFrame::Exit { code } => return Ok(ExitCode::from(code as u8)),
            AgentFrame::Error { message } => return Err(message),
            AgentFrame::Stdin | AgentFrame::Data | AgentFrame::Done => (),
        }
    }
}

fn show(name: &str, body: Body) -> Result<(), String> {
    println!("{}", request(name, body)?);
    Ok(())
}

/// Writes a PNG to stdout.
fn screenshot(name: &str) -> Result<(), String> {
    let format = ImageFormat::Png;
    let result = request(name, Body::Screenshot { format })?;
    let mut socket = attach(name, &result)?;
    std::io::copy(&mut socket, &mut std::io::stdout())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn display(name: &str, mode: &str, bind: Option<String>) -> Result<(), String> {
    let vnc = bind.map(|bind| VncOptions {
        bind: Some(bind),
        password: None,
    });
    let mode = mode.into();
    show(name, Body::Display { mode, vnc })
}

fn display_with_password(name: &str, bind: &str, password: &str) -> Result<(), String> {
    let vnc = Some(VncOptions {
        bind: Some(bind.into()),
        password: Some(password.into()),
    });
    let mode = "vnc".into();
    show(name, Body::Display { mode, vnc })
}

/// Presses keys together and releases them in reverse: `125+28` is Super+Enter.
fn key(name: &str, codes: &str) -> Result<(), String> {
    let codes: Vec<u16> = codes
        .split('+')
        .map(|code| code.parse().map_err(|_| format!("bad keycode {code:?}")))
        .collect::<Result<_, _>>()?;
    let press = codes
        .iter()
        .map(|&code| InputEvent::Key { code, down: true });
    let release = codes
        .iter()
        .rev()
        .map(|&code| InputEvent::Key { code, down: false });
    let events = press.chain(release).collect();
    request(name, Body::Input { events }).map(|_| ())
}

fn console(name: &str) -> Result<(), String> {
    let result = request(name, Body::Console)?;
    let mut socket = attach(name, &result)?;
    let mut to_guest = socket.try_clone().map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin(), &mut to_guest);
    });
    std::io::copy(&mut socket, &mut std::io::stdout())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Prints events until the VM ends.
fn events(name: &str) -> Result<(), String> {
    let mut socket = connect(name)?;
    let line = serde_json::to_string(&Request {
        id: 1,
        body: Body::Events,
    });
    writeln!(socket, "{}", line.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    std::io::copy(&mut socket, &mut std::io::stdout())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn stop(name: &str, graceful: bool) -> Result<(), String> {
    let timeout = 30;
    request(name, Body::Stop { graceful, timeout }).map(|_| ())
}

/// The agent's answer that ends a file transfer.
fn finished(frame: AgentFrame) -> Result<bool, String> {
    match frame {
        AgentFrame::Done => Ok(true),
        AgentFrame::Error { message } => Err(message),
        _ => Ok(false),
    }
}

fn file_put(name: &str, local: &str, guest: &str) -> Result<(), String> {
    let text = |e: std::io::Error| e.to_string();
    let mut file = std::fs::File::open(local).map_err(|e| format!("{local}: {e}"))?;
    let mode = file.metadata().map_err(text)?.permissions().mode() & 0o777;
    let result = request(
        name,
        Body::FilePut {
            path: guest.into(),
            mode: Some(mode),
        },
    )?;
    let mut socket = attach(name, &result)?;
    let mut buffer = vec![0; api::MAX_BODY];
    loop {
        let count = file.read(&mut buffer).map_err(text)?;
        api::write_frame(&mut socket, &AgentFrame::Data, &buffer[..count]).map_err(text)?;
        if count == 0 {
            break;
        }
    }
    while let Some((frame, _)) = api::read_frame::<AgentFrame>(&mut socket).map_err(text)? {
        if finished(frame)? {
            return Ok(());
        }
    }
    Err("the agent closed the stream before finishing".into())
}

fn file_get(name: &str, guest: &str, local: &str) -> Result<(), String> {
    let text = |e: std::io::Error| e.to_string();
    let result = request(name, Body::FileGet { path: guest.into() })?;
    let mut socket = attach(name, &result)?;
    // The first frame decides: a missing guest file must not truncate the local one.
    let mut file = None;
    while let Some((frame, body)) = api::read_frame::<AgentFrame>(&mut socket).map_err(text)? {
        let done = finished(frame)?;
        let out = match &mut file {
            Some(out) => out,
            None => file.insert(std::fs::File::create(local).map_err(|e| format!("{local}: {e}"))?),
        };
        out.write_all(&body).map_err(text)?;
        if done {
            return Ok(());
        }
    }
    Err("the agent closed the stream before finishing".into())
}

fn image_arch() -> &'static str {
    std::env::consts::ARCH
}

fn home_path(path: &str) -> PathBuf {
    expand(Path::new(path))
}

/// Runs the recipe in a container. shortcut: must run from a checkout since the recipe and the agent's source live there, embed them when the CLI ships on its own.
fn image_build(args: &[&str]) -> Result<(), String> {
    let (mut arch, mut profile) = (image_arch(), "desktop");
    for pair in args.chunks(2) {
        match pair {
            ["--arch", value] => arch = value,
            ["--profile", value] => profile = value,
            _ => return Err(format!("image build: bad arguments {args:?}")),
        }
    }
    let platform = match arch {
        "x86_64" => "linux/amd64",
        "aarch64" => "linux/arm64",
        _ => return Err(format!("unknown arch {arch}")),
    };
    if !matches!(profile, "minimal" | "desktop") {
        return Err(format!("unknown profile {profile}"));
    }
    let source = std::env::current_dir().map_err(|e| e.to_string())?;
    if !source.join("image/build.sh").exists() {
        return Err("run image build from a Thanatos checkout".into());
    }
    let out = home_path(&format!("~/.thanatos/images/{arch}"));
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let runtime = ["docker", "podman"]
        .into_iter()
        .find(|tool| {
            Command::new(tool)
                .arg("--version")
                .output()
                .is_ok_and(|o| o.status.success())
        })
        .ok_or("image build needs docker or podman")?;
    let status = Command::new(runtime)
        .args(["run", "--rm", "--privileged", "--platform", platform, "-v"])
        .arg(format!("{}:/src:ro", source.display()))
        .arg("-v")
        .arg(format!("{}:/out", out.display()))
        .args(["debian:trixie", "/src/image/build.sh", arch, profile])
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("image build failed: {status}"));
    }
    println!("{}", out.display());
    Ok(())
}

/// Lays out a VM directory from the current image. The root disk is a sparse copy grown so the
/// guest's first-boot resize has room.
fn vm_create(name: &str) -> Result<(), String> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!("bad VM name {name:?}"));
    }
    let arch = image_arch();
    let kernel = if arch == "x86_64" { "bzImage" } else { "Image" };
    let image = home_path(&format!("~/.thanatos/images/{arch}/root.ext4"));
    if !image.exists() {
        return Err(format!("{} is missing: run just image", image.display()));
    }
    let dir = home_path(&format!("~/.thanatos/vms/{name}"));
    if dir.exists() {
        return Err(format!("{} already exists", dir.display()));
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let root = dir.join("root.ext4");
    let flag = if cfg!(target_os = "macos") {
        "-c"
    } else {
        "--reflink=auto"
    };
    let copied = Command::new("cp").arg(flag).arg(&image).arg(&root).status();
    if !copied.is_ok_and(|status| status.success()) {
        std::fs::copy(&image, &root).map_err(|e| e.to_string())?;
    }
    let file = std::fs::File::options()
        .write(true)
        .open(&root)
        .map_err(|e| e.to_string())?;
    file.set_len(ROOT_DISK_BYTES).map_err(|e| e.to_string())?;
    let config = format!(
        "name = \"{name}\"\nkernel = \"~/.thanatos/images/{arch}/{kernel}\"\n\n[[disk]]\npath = \"~/.thanatos/vms/{name}/root.ext4\"\n"
    );
    std::fs::write(dir.join("vm.toml"), config).map_err(|e| e.to_string())?;
    println!("{}", dir.join("vm.toml").display());
    Ok(())
}

fn doctor() -> Result<(), String> {
    match Host::available() {
        Ok(()) => {
            println!("hypervisor: usable");
            Ok(())
        }
        Err(unavailable) => Err(format!(
            "hypervisor: unusable: {}\nfix: {}",
            unavailable.reason, unavailable.fix
        )),
    }
}

/// A running process keeps the signature it started with, so only a new one sees the entitlement.
fn reexec_doctor() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let status = Command::new(exe).arg("doctor").status();
    match status.map_err(|e| e.to_string())?.success() {
        true => Ok(()),
        false => Err("still unusable after signing".into()),
    }
}

/// Ad-hoc signs this binary in place; macOS refuses `hv_vm_create` without the entitlement.
fn sign() -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err("--sign applies only to macOS".into());
    }
    let error = |e: std::io::Error| e.to_string();
    let path = std::env::temp_dir().join(format!("thanatos-{}.entitlements", std::process::id()));
    std::fs::write(&path, ENTITLEMENTS).map_err(error)?;
    let status = Command::new("codesign")
        .args(["--force", "--sign", "-", "--entitlements"])
        .arg(&path)
        .arg(std::env::current_exe().map_err(error)?)
        .status()
        .map_err(error);
    let _ = std::fs::remove_file(&path);
    if status?.success() {
        Ok(())
    } else {
        Err("codesign failed".into())
    }
}
