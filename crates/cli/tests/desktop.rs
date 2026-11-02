//! DESIGN 8's desktop check, on an image built with the desktop profile.
use std::{
    io::{Read, Write},
    net::TcpStream,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const WALLPAPER: [u8; 3] = [0x20, 0x40, 0x60];
const SUPER: &str = "125+28"; // Super+Enter opens the terminal

fn thanatos(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_thanatos"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// The share of the screen showing the wallpaper.
fn wallpaper_share(socket: &str) -> f64 {
    let shot = thanatos(&["screenshot", socket]);
    assert!(shot.status.success(), "{shot:?}");
    let mut reader = png::Decoder::new(std::io::Cursor::new(shot.stdout))
        .read_info()
        .unwrap();
    let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
    reader.next_frame(&mut pixels).unwrap();
    let same = pixels
        .chunks_exact(4)
        .filter(|p| p[..3] == WALLPAPER)
        .count();
    same as f64 / (pixels.len() / 4) as f64
}

fn wait_for(limit: u64, mut done: impl FnMut() -> bool, what: &str) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < Duration::from_secs(limit), "{what}");
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// A minimal RFB client: asks for the whole screen, then types through VNC and sees it change.
fn vnc_shows_the_screen_and_passes_keys(socket: &str) {
    let reply = thanatos(&["display", socket, "vnc", "127.0.0.1:0"]);
    assert!(reply.status.success(), "{reply:?}");
    let reply: serde_json::Value = serde_json::from_slice(&reply.stdout).unwrap();
    let mut vnc = TcpStream::connect(reply["address"].as_str().unwrap()).unwrap();
    vnc.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut version = [0; 12];
    vnc.read_exact(&mut version).unwrap();
    assert_eq!(&version, b"RFB 003.008\n");
    vnc.write_all(&version).unwrap();
    let mut security = [0; 2];
    vnc.read_exact(&mut security).unwrap();
    assert_eq!(security, [1, 1]);
    vnc.write_all(&[1]).unwrap();
    let mut result = [0; 4];
    vnc.read_exact(&mut result).unwrap();
    assert_eq!(result, [0; 4]);
    vnc.write_all(&[1]).unwrap();
    let mut init = [0; 24];
    vnc.read_exact(&mut init).unwrap();
    let width = u16::from_be_bytes([init[0], init[1]]);
    let height = u16::from_be_bytes([init[2], init[3]]);
    assert_eq!((width, height), (1280, 800));
    let mut name = vec![0; u32::from_be_bytes(init[20..24].try_into().unwrap()) as usize];
    vnc.read_exact(&mut name).unwrap();

    let update = |incremental: u8, vnc: &mut TcpStream| {
        let mut request = vec![3, incremental, 0, 0, 0, 0];
        request.extend_from_slice(&width.to_be_bytes());
        request.extend_from_slice(&height.to_be_bytes());
        vnc.write_all(&request).unwrap();
        let mut header = [0; 16];
        vnc.read_exact(&mut header).unwrap();
        assert_eq!(&header[..4], [0, 0, 0, 1]);
        let w = u16::from_be_bytes([header[8], header[9]]) as usize;
        let h = u16::from_be_bytes([header[10], header[11]]) as usize;
        let mut pixels = vec![0; w * h * 4];
        vnc.read_exact(&mut pixels).unwrap();
        pixels
    };
    let full = update(0, &mut vnc);
    assert_eq!(full.len(), 1280 * 800 * 4);
    // Typing into the terminal over VNC changes the screen: the update only returns if so.
    for keysym in [b'l', b's'] {
        for down in [1, 0] {
            vnc.write_all(&[4, down, 0, 0, 0, 0, 0, keysym]).unwrap();
        }
    }
    assert!(!update(1, &mut vnc).is_empty());
}

#[test]
#[ignore = "needs a hypervisor and a desktop-profile image"]
fn the_desktop_shows_its_wallpaper_and_takes_input() {
    if cfg!(target_os = "macos") {
        assert!(thanatos(&["doctor", "--sign"]).status.success());
    }
    let arch = std::env::consts::ARCH;
    let kernel = if arch == "x86_64" { "bzImage" } else { "Image" };
    let images =
        PathBuf::from(std::env::var("HOME").unwrap()).join(format!(".thanatos/images/{arch}"));
    let dir = PathBuf::from(format!("/tmp/thanatos-desktop-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let root = dir.join("root.ext4");
    std::fs::copy(images.join("root.ext4"), &root).unwrap();
    let socket = dir.join("api.sock");
    let config = dir.join("vm.toml");
    std::fs::write(
        &config,
        format!(
            "name = \"desktop\"\nkernel = \"{}\"\n\n[[disk]]\npath = \"{}\"\n\n[api]\nsocket = \"{}\"\n",
            images.join(kernel).display(),
            root.display(),
            socket.display()
        ),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_thanatos"))
        .arg("run")
        .arg(&config)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut serial = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        let mut buffer = [0; 4096];
        while let Ok(1..) = serial.read(&mut buffer) {}
    });
    let socket = socket.to_str().unwrap().to_string();
    let result = std::panic::catch_unwind(|| {
        wait_for(
            180,
            || {
                std::path::Path::new(&socket).exists()
                    && thanatos(&["screenshot", &socket]).status.success()
                    && wallpaper_share(&socket) > 0.9
            },
            "the wallpaper never appeared",
        );
        // Screenshots of an unchanged screen stay the same, down to the byte.
        std::thread::sleep(Duration::from_secs(1));
        let before = thanatos(&["screenshot", &socket]).stdout;
        assert_eq!(before, thanatos(&["screenshot", &socket]).stdout);
        assert!(thanatos(&["key", &socket, SUPER]).status.success());
        wait_for(
            60,
            || wallpaper_share(&socket) < 0.5,
            "the key did not open a terminal over the wallpaper",
        );
        vnc_shows_the_screen_and_passes_keys(&socket);
    });
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
    result.unwrap();
}
