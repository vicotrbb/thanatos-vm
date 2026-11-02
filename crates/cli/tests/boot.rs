//! Boots the built image and waits for the login prompt on the serial console.
use std::{
    io::Read,
    path::PathBuf,
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};

#[test]
#[ignore = "needs a hypervisor and a built image"]
fn boots_to_a_login_prompt() {
    // Cargo relinks the binary for the test run, which drops the entitlement.
    if cfg!(target_os = "macos") {
        let signed = Command::new(env!("CARGO_BIN_EXE_thanatos"))
            .args(["doctor", "--sign"])
            .status()
            .unwrap();
        assert!(signed.success());
    }
    let arch = std::env::consts::ARCH;
    let kernel = if arch == "x86_64" { "bzImage" } else { "Image" };
    let images =
        PathBuf::from(std::env::var("HOME").unwrap()).join(format!(".thanatos/images/{arch}"));
    // Not temp_dir: on macOS it is too long for a Unix socket path.
    let dir = PathBuf::from(format!("/tmp/thanatos-boot-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let root = dir.join("root.ext4");
    std::fs::copy(images.join("root.ext4"), &root).unwrap();
    let grown = std::fs::File::options().write(true).open(&root).unwrap();
    grown.set_len(4 << 30).unwrap();
    let config = dir.join("vm.toml");
    std::fs::write(
        &config,
        format!(
            "name = \"boot\"\nkernel = \"{}\"\n\n[[disk]]\npath = \"{}\"\n\n[api]\nsocket = \"{}\"\n",
            images.join(kernel).display(),
            root.display(),
            dir.join("api.sock").display()
        ),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_thanatos"))
        .arg("run")
        .arg(&config)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut seen, mut buffer) = (Vec::new(), [0; 4096]);
        while let Ok(count @ 1..) = stdout.read(&mut buffer) {
            print!("{}", String::from_utf8_lossy(&buffer[..count]));
            seen.extend_from_slice(&buffer[..count]);
            if seen.windows(6).any(|w| w == b"login:") {
                let _ = sender.send(());
                return;
            }
        }
    });
    let booted = receiver.recv_timeout(Duration::from_secs(180));
    let finished = booted.is_ok().then(|| {
        exec_works(&config);
        files_copy_both_ways(&config);
        apt_installs_from_the_internet(&config);
        stops_gracefully(&config, &mut child);
    });
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
    booted.expect("no login prompt on the serial console within 180s");
    finished.unwrap();
}

/// The agent starts around the login prompt, so give it a moment.
fn exec_works(config: &std::path::Path) {
    let socket = config.with_file_name("api.sock");
    for _ in 0..60 {
        let output = Command::new(env!("CARGO_BIN_EXE_thanatos"))
            .args(["exec", "--socket"])
            .arg(&socket)
            .args(["--", "sh", "-c", "uname -s; echo err >&2; exit 7"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        if output.status.code() == Some(7) {
            assert_eq!(String::from_utf8_lossy(&output.stdout), "Linux\n");
            assert_eq!(String::from_utf8_lossy(&output.stderr), "err\n");
            return;
        }
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
        std::thread::sleep(Duration::from_secs(1));
    }
    panic!("exec never succeeded");
}

fn thanatos(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_thanatos"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn files_copy_both_ways(config: &std::path::Path) {
    let socket = config.with_file_name("api.sock");
    let socket = socket.to_str().unwrap();
    let (there, back) = (
        config.with_file_name("there"),
        config.with_file_name("back"),
    );
    // Larger than one frame, so the transfer has to be split and rejoined.
    let payload: Vec<u8> = (0..3 << 20).map(|i: u32| (i % 251) as u8).collect();
    std::fs::write(&there, &payload).unwrap();
    let put = thanatos(&[
        "file",
        "put",
        socket,
        there.to_str().unwrap(),
        "/tmp/copied",
    ]);
    assert!(put.status.success(), "{put:?}");
    let get = thanatos(&["file", "get", socket, "/tmp/copied", back.to_str().unwrap()]);
    assert!(get.status.success(), "{get:?}");
    assert!(
        std::fs::read(&back).unwrap() == payload,
        "file changed in transit"
    );
    let missing = thanatos(&[
        "file",
        "get",
        socket,
        "/no/such/file",
        back.to_str().unwrap(),
    ]);
    assert!(!missing.status.success());
    assert!(
        std::fs::read(&back).unwrap() == payload,
        "a failed get touched the local file"
    );
}

fn apt_installs_from_the_internet(config: &std::path::Path) {
    let socket = config.with_file_name("api.sock");
    let script = "apt-get update -qq && apt-get install -y -qq hello >/dev/null && hello";
    let run = thanatos(&[
        "exec",
        "--socket",
        socket.to_str().unwrap(),
        "--",
        "sh",
        "-c",
        script,
    ]);
    let said = String::from_utf8_lossy(&run.stdout);
    assert!(said.contains("Hello, world!"), "{run:?}");
}

fn stops_gracefully(config: &std::path::Path, child: &mut std::process::Child) {
    let socket = config.with_file_name("api.sock");
    let stop = thanatos(&["stop", socket.to_str().unwrap()]);
    assert!(stop.status.success(), "{stop:?}");
    let started = std::time::Instant::now();
    while started.elapsed() < Duration::from_secs(25) {
        if child.try_wait().unwrap().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("the guest did not power off before the stop timeout");
}
