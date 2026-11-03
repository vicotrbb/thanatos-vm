//! Entering the sandbox cannot be undone, so the checks run in a child process.
use std::{path::PathBuf, process::Command};

const CHILD: &str = "THANATOS_SANDBOX_DIR";

#[test]
fn contains_the_process() {
    if let Some(dir) = std::env::var_os(CHILD) {
        return child(PathBuf::from(dir));
    }
    let dir = std::env::temp_dir().join(format!("thanatos-sandbox-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("allowed")).unwrap();
    std::fs::write(dir.join("secret"), "s").unwrap();
    std::fs::write(dir.join("readable"), "r").unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["contains_the_process", "--exact", "--nocapture"])
        .env(CHILD, &dir)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("files contained"), "{output:?}");
    assert!(text.contains("thread started"), "{output:?}");
    assert!(!text.contains("program ran"), "{output:?}");
}

fn child(dir: PathBuf) {
    let allowed = dir.join("allowed");
    thanatos_hv::sandbox::enter(&thanatos_hv::sandbox::Paths {
        directories: vec![allowed.clone()],
        devices: vec!["/dev/urandom".into()],
        read: vec![dir.join("readable")],
    })
    .unwrap();
    assert!(std::fs::read(dir.join("secret")).is_err());
    assert!(std::fs::write(dir.join("new"), "x").is_err());
    assert_eq!(std::fs::read(dir.join("readable")).unwrap(), b"r");
    std::fs::write(allowed.join("file"), "x").unwrap();
    std::fs::create_dir(allowed.join("sub")).unwrap();
    std::fs::rename(allowed.join("file"), allowed.join("sub/moved")).unwrap();
    assert!(std::fs::File::open("/dev/urandom").is_ok());
    println!("files contained");
    std::thread::spawn(|| println!("thread started"))
        .join()
        .unwrap();
    // Either refused or the process is killed, which the parent sees as the line never printing.
    if Command::new("/bin/echo").status().is_ok() {
        println!("program ran");
    }
}
