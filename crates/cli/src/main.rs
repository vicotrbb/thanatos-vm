//! `thanatos`: thin commands over the machine and its API.
use std::process::{Command, ExitCode};
use thanatos_hv::{Host, Hypervisor};

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
        _ => Err("usage: thanatos doctor [--sign]".to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
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
