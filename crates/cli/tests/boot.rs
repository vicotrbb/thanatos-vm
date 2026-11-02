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
    let dir = std::env::temp_dir().join(format!("thanatos-boot-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let root = dir.join("root.ext4");
    std::fs::copy(images.join("root.ext4"), &root).unwrap();
    let config = dir.join("vm.toml");
    std::fs::write(
        &config,
        format!(
            "name = \"boot\"\nkernel = \"{}\"\n\n[[disk]]\npath = \"{}\"\n",
            images.join(kernel).display(),
            root.display()
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
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
    booted.expect("no login prompt on the serial console within 180s");
}
