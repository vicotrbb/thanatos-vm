# Thanatos VM

One small Rust binary that runs a Linux VM with a screen, a network and a guest agent, on KVM (Linux x86_64 and aarch64) and Hypervisor.framework (macOS 15+ on Apple silicon). The design is in [DESIGN.md](DESIGN.md).

## Install

Needs Rust (the repository pins the toolchain) and [just](https://github.com/casey/just). Building a guest image needs Docker or Podman.

```sh
git clone https://github.com/vicotrbb/thanatos-vm && cd thanatos-vm
just check
cargo build --release -p thanatos-cli
export PATH="$PWD/target/release:$PATH"
```

## Doctor

```sh
thanatos doctor          # says whether a VM can be created here and what to fix
thanatos doctor --sign   # macOS: ad-hoc signs the binary with the hypervisor entitlement
```

On macOS, sign again after every build. On Linux, your user needs access to `/dev/kvm`.

## Build an image

```sh
just image                     # the desktop profile for this machine's architecture
just image x86_64 minimal      # another architecture or the smaller profile
```

This writes `~/.thanatos/images/<arch>/{Image|bzImage,root.ext4}`. Run it from the checkout. A kernel already there is reused; delete it to rebuild.

## Run

```sh
thanatos vm create dev
thanatos run ~/.thanatos/vms/dev/vm.toml &
```

The serial console goes to stdout. `~/.thanatos/vms/dev/vm.toml` holds the VM's settings ([DESIGN 4.12](DESIGN.md)). The guest has users `root` and `user` with no password, reachable only through this machine's console and API.

## Exec

```sh
thanatos exec dev -- uname -a
thanatos file put dev ./local.txt /tmp/local.txt
thanatos file get dev /tmp/local.txt ./back.txt
thanatos stop dev              # asks the guest to power off; --force stops it now
```

## Screenshot

```sh
thanatos screenshot dev > shot.png
thanatos key dev 125+28        # Linux keycodes: Super+Enter opens a terminal in sway
```

## VNC

```sh
thanatos display dev vnc       # then point any VNC client at 127.0.0.1:5901
thanatos display dev headless
```

An address other machines can reach needs a password: `thanatos display dev vnc 0.0.0.0:5901 <password>`. VNC authentication is weak by nature, so prefer an SSH tunnel.

## Network

The guest gets 10.0.2.15 behind user-mode NAT, with outbound policy and forwards in `[network]`:

```toml
[network]
default = "allow"
deny = ["10.0.0.0/8", "192.168.0.0/16", "169.254.0.0/16"]
allow = ["192.168.1.10/32:443/tcp"]
forward = ["127.0.0.1:2222->22"]
```

The guest cannot reach this machine's own services, through 10.0.2.2 or 127.0.0.1, unless a rule allows it: `allow = ["10.0.2.2/32:8000/tcp"]`.

## API

Everything above is JSON lines on `~/.thanatos/vms/<name>/api.sock`, mode 0600. The types with their documentation are in [crates/vmm/src/api.rs](crates/vmm/src/api.rs), and [docs/api.schema.json](docs/api.schema.json) is the schema for other languages.

```sh
echo '{"id":1,"request":"status"}' | nc -U ~/.thanatos/vms/dev/api.sock
```

## License

Apache-2.0 OR MIT.
