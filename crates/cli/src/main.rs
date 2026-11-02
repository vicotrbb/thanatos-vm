//! `thanatos`: thin commands over the machine and its API.
use std::{
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};
use thanatos_hv::{Host, Hypervisor};
use thanatos_vmm::{
    config::{Config, expand},
    console::Console,
    machine,
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
        ["run", path] => run(path),
        ["image", "build", rest @ ..] => image_build(rest),
        ["vm", "create", name] => vm_create(name),
        _ => Err("usage: thanatos doctor [--sign] | run <vm.toml> | image build [--arch A] [--profile P] | vm create <name>".to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn run(path: &str) -> Result<(), String> {
    let config = Config::load(path.as_ref()).map_err(|e| e.to_string())?;
    let console = Console::default();
    console.add_output(std::io::stdout());
    machine::run(&config, console).map_err(|e| e.to_string())?;
    Ok(())
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
