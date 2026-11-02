//! `thanatos-guest`: serves exec, file copy, poweroff and heartbeat on vsock port 1, one request
//! per connection. Runs as root in the guest; a convenience, not a security boundary.
#[cfg(target_os = "linux")]
mod agent;

#[cfg(target_os = "linux")]
fn main() -> std::io::Result<()> {
    agent::serve()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("thanatos-guest runs inside a Linux guest");
    std::process::exit(1);
}
