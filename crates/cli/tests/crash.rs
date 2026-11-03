//! Kills the VMM under write load, over and over. Data the guest has flushed must survive and the
//! filesystem must recover. This is a process crash: the host's own cache survives it, so it does
//! not stand in for a power cut.
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const ROUNDS: u32 = 30;

fn exec(config: &Path, script: &str) -> Option<String> {
    let output = Command::new(env!("CARGO_BIN_EXE_thanatos"))
        .args(["exec", "--socket"])
        .arg(config)
        .args(["--", "sh", "-c", script])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success() || !output.stderr.is_empty(),
        "{output:?}"
    );
    output.status.success().then_some(text)
}

/// A failed assertion must not leave a VM running with the disks locked.
struct Vm(Child);

impl Drop for Vm {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(config: &Path, socket: &Path) -> Vm {
    let child = Vm(Command::new(env!("CARGO_BIN_EXE_thanatos"))
        .arg("run")
        .arg(config)
        .args(["--display", "headless"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap());
    let started = Instant::now();
    while exec(socket, "true").is_none() {
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "agent never came up"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    child
}

#[test]
#[ignore = "needs a hypervisor and a built image"]
fn flushed_data_survives_the_vmm_being_killed_with_4k_blocks() {
    survives_kills(4096);
}

// A block size below the page size has its own failure path in ext4's writeback.
#[test]
#[ignore = "needs a hypervisor and a built image"]
fn flushed_data_survives_the_vmm_being_killed_with_1k_blocks() {
    survives_kills(1024);
}

fn survives_kills(block_size: u32) {
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
    let dir = PathBuf::from(format!(
        "/tmp/thanatos-crash-{}-{block_size}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let root = dir.join("root.ext4");
    std::fs::copy(images.join("root.ext4"), &root).unwrap();
    let socket = dir.join("api.sock");
    let config = dir.join("vm.toml");
    std::fs::write(
        &config,
        format!(
            "name = \"crash\"\nvcpus = 2\nkernel = \"{}\"\n\n[[disk]]\npath = \"{}\"\n\n[[disk]]\npath = \"{}\"\ncreate_mib = 256\n\n[api]\nsocket = \"{}\"\n",
            images.join(kernel).display(),
            root.display(),
            dir.join("data.ext4").display(),
            socket.display(),
        ),
    )
    .unwrap();

    let mut child = start(&config, &socket);
    exec(&socket, &format!("mkfs.ext4 -q -b {block_size} /dev/vdb")).expect("format the data disk");
    for round in 1..=ROUNDS {
        // Replays the journal, then checks the result with nothing mounted.
        exec(&socket, "mount /dev/vdb /mnt && umount /mnt")
            .unwrap_or_else(|| panic!("round {round}: the disk does not mount"));
        let fsck = exec(&socket, "e2fsck -fn /dev/vdb 2>&1; echo status=$?").unwrap();
        assert!(
            fsck.ends_with("status=0\n"),
            "round {round}: corrupt:\n{fsck}"
        );
        exec(&socket, "mount /dev/vdb /mnt").expect("mount again");
        if round > 1 {
            let seen = exec(&socket, "cat /mnt/acked").unwrap_or_default();
            assert_eq!(
                seen,
                format!("{}\n", round - 1),
                "round {round}: flushed data lost"
            );
        }
        exec(
            &socket,
            &format!("echo {round} > /mnt/acked.new && sync /mnt/acked.new && mv /mnt/acked.new /mnt/acked && sync /mnt"),
        )
        .expect("write the record");
        // Unflushed writes in flight when the process dies.
        exec(
            &socket,
            "(while :; do dd if=/dev/urandom of=/mnt/noise bs=64k count=64 2>/dev/null; done) >/dev/null 2>&1 &",
        )
        .expect("start the load");
        std::thread::sleep(Duration::from_millis(300 + u64::from(round) * 137));
        drop(child);
        child = start(&config, &socket);
    }
    drop(child);
    std::fs::remove_dir_all(&dir).unwrap();
}
