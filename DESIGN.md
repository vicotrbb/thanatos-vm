# Thanatos VM: design specification

Date: 2026-11-02. Status: design for review, nothing built yet. Author: Victor Bona with Claude.

Thanatos is a standalone, open-source virtual machine monitor written in Rust. It boots an unmodified Linux guest, with a persistent disk, a network, a screen, a keyboard and a pointer, at native speed on every host that has a hardware hypervisor: Linux with KVM, macOS on Apple silicon with Hypervisor.framework, and Windows with the Windows Hypervisor Platform. It runs as a plain process, inside a container, or inside a pod, from one binary and one configuration file, and it needs no root.

This document is the specification a builder plans from. Where it records a choice the user made in conversation, it says so. Where it records a choice Claude made as a default, it is marked **(my call)** so the user can overrule it while reading.

## 1. Intent

### 1.1 The problem

Every small Rust VMM in existence, Firecracker, Cloud Hypervisor, Cubebot's `apps/sandbox`, answers the question "what is the smallest safe KVM monitor". Firecracker and Cloud Hypervisor run on x86_64 and aarch64 Linux; none of them runs on Windows, only libkrun has a Hypervisor.framework backend for macOS, and none of them gives the guest a screen that the host owns. People who need "a Linux machine with a desktop that I can start anywhere, drive programmatically, and trust as a boundary" fall back to QEMU, which is two million lines of C, or to containers, which are not a boundary against a kernel exploit.

### 1.2 What Thanatos is

One Rust binary, `thanatos`, that:

- boots a stock Linux kernel and a stock distribution root filesystem, so "install apps" means `apt install`;
- runs the guest at native speed on KVM, Hypervisor.framework or Windows Hypervisor Platform, whichever the host has;
- gives the guest a disk, a network, a vsock control channel, a serial console, a screen, a keyboard and a pointer, through virtio devices the VMM owns;
- exposes everything a program needs to drive the guest, start, stop, exec, copy files, screenshot, inject input, forward ports, as a local API, a CLI and a Rust library;
- runs as a process with no privileges beyond the hypervisor device, so it works as a child process of another program, inside a container, or in a pod.

### 1.3 What it is not

- Not an emulator. Where no hypervisor exists, Thanatos says so and refuses. See 3.3.
- Not a hypervisor. It uses the host's.
- Not an orchestrator. One process runs one VM. Whoever needs many VMs starts many processes.
- Not a guest operating system. The guest is unmodified Linux plus one small optional agent.

### 1.4 Why now

The user's project Cubebot runs each AI agent in a KVM guest of a 5,000-line in-house VMM, and that VMM runs only on x86_64 Linux with `/dev/kvm`. Agents cannot wake on Macs, arm64 hosts, or managed clusters without nested virtualization. Cubebot's own 2026-11-01 design lists "an aarch64 VMM port, a macOS Hypervisor.framework backend, a userspace guest network" as later, independent steps. Thanatos is those steps, built as a general product rather than a Cubebot feature, because every program that wants a disposable trusted Linux desktop has the same need. Cubebot is the first consumer and the proof that the device set is enough. It is not a dependency and does not shape the API.

## 2. Goals, non-goals, anti-goals

### 2.1 Goals (version 1)

1. Boot an unmodified Linux 6.12 LTS kernel with a Debian root filesystem on all three of: Linux x86_64 with KVM, Linux aarch64 with KVM, macOS arm64 with Hypervisor.framework. Windows x86_64 with WHP is version 1.1 (see 2.4).
2. Guest architecture equals host architecture. Two guest images, x86_64 and aarch64, built from one recipe. The user chose native speed wherever a hypervisor exists over a single emulated image.
3. Devices: virtio-blk, virtio-net, virtio-vsock, virtio-rng, virtio-gpu (2D), virtio-input (keyboard, absolute pointer), and a serial console. All virtio over the MMIO transport.
4. Networking without root through a user-mode TCP/IP stack in the VMM, with an outbound policy and inbound port forwards. A tap backend on Linux for those who have the capability.
5. A screen the host owns: the guest's scanout lives in VMM memory, screenshots are read from it, input enters only through the VMM. Display sinks: headless, a built-in VNC server, and an optional native window.
6. A control API on a local socket, a CLI over it, and a Rust library that the CLI is written against.
7. An optional guest agent, `thanatos-guest`, a static binary that offers exec, file copy and clean shutdown over vsock.
8. A reproducible guest image build, `thanatos image build`, that produces a kernel and a root image for either architecture, from any host that can run a container.
9. Runs unprivileged as a bare process on each host, and unchanged inside a Docker container or a Kubernetes pod that passes the hypervisor device through.
10. Boot to a login prompt in under one second on KVM and under two seconds on Hypervisor.framework, on a developer laptop, with a 2 GiB guest.

### 2.2 Non-goals (version 1)

- Software CPU emulation, RISC-V guests, or any guest whose architecture differs from the host's.
- Intel Macs. Apple announced at WWDC 2025 that macOS 26 is the last release for Intel hardware, so an x86 Hypervisor.framework backend would serve a platform in its security-updates-only window.
- Snapshot and restore. The hypervisor trait leaves room for it (see 4.2); nothing in version 1 implements it.
- 3D acceleration, virgl, audio, USB, PCI passthrough, nested virtualization inside the guest.
- Live migration, multiple VMs per process, a daemon, a scheduler.
- A Windows or macOS guest.
- qcow2 or any image format other than raw files.

### 2.3 Anti-goals

Thanatos has failed, even if it works, if:

- it needs root or an administrator on any supported host for its default configuration;
- the guest can alter the frames a screenshot returns or write into the input queues that the API feeds;
- a host with no hypervisor silently runs the guest in some weaker mode;
- the device model grows past what a reviewer can read in a day, or `unsafe` escapes the hypervisor backends and guest memory;
- a consumer must link Thanatos's internals to drive a VM instead of using the API.

### 2.4 Platform matrix

| Host | Hypervisor | Guest arch | Version | Notes |
| --- | --- | --- | --- | --- |
| Linux x86_64 | KVM | x86_64 | 1.0 | `/dev/kvm` access, normally group `kvm`. In containers and pods via device passthrough. |
| Linux aarch64 | KVM | aarch64 | 1.0 | Same. Hosts with a GICv3 only: arm64 servers and Linux VMs on Apple silicon with nested virtualization. Raspberry Pi 5 has a GICv2 and is not supported. |
| macOS 15+ arm64 | Hypervisor.framework | aarch64 | 1.0 | Binary signed with the `com.apple.security.hypervisor` entitlement. macOS 15 is required for `hv_gic_create`. No root. |
| Windows 11 x86_64 | Windows Hypervisor Platform | x86_64 | 1.1 | The "Windows Hypervisor Platform" optional feature enabled once by an administrator. |
| Windows 11 arm64 | Windows Hypervisor Platform | aarch64 | later | WHP reports arm64 support through `WHvGetCapability`; untested until someone has the hardware. |
| Anything without a hypervisor | none | | never | `thanatos doctor` explains why. |

## 3. Decisions the user made

These were chosen in conversation on 2026-11-01 and 2026-11-02.

1. **Rust, standalone, general purpose.** Thanatos is its own open-source project with its own API. Cubebot is an example consumer.
2. **Own parts, reuse the low-level core.** The hypervisor bindings, guest memory abstraction, device tree writer and TCP/IP stack come from existing crates. The device model, display, control protocol, networking policy, image tooling and CLI are Thanatos's own.
3. **Native speed wherever a hypervisor exists.** Hardware virtualization with a matching-architecture guest on every host, rather than one emulated image everywhere.
4. **Runs as a process.** The VMM is a plain process; containers and pods are placements of it, not requirements. This means no tap device by default and a user-mode network stack instead.
5. **Real apps.** The guest is a stock distribution with a package manager, not a custom OS.

### 3.1 Decisions made here as defaults (my call)

Each is a sensible default that the user has not explicitly approved. Overrule any of them in review.

- Debian trixie as the base root filesystem, Linux 6.12 LTS as the kernel.
- virtio over MMIO rather than PCI.
- rust-vmm's `virtio-queue` and `vm-memory` for the virtqueue and guest memory rather than a hand-written queue.
- `smoltcp` as the TCP/IP stack under the user-mode network.
- No async runtime. Threads and blocking I/O.
- Headless and VNC display sinks in the core binary; a native window behind a Cargo feature that is off by default.
- Windows deferred to 1.1 so that 1.0 is three backends on two kernels, not four on three.
- Where no hypervisor exists, refuse. The user chose native speed; whether a slow emulated fallback is ever wanted is an open question for them (see 12).
- License: Apache-2.0 OR MIT, the Rust ecosystem default. Open question for the user (see 12).

## 4. Architecture

### 4.1 Shape

```
                 ┌───────────────────────────── thanatos process ─────────────────────────────┐
                 │                                                                            │
   thanatos CLI  │  control API (Unix socket / named pipe, JSON lines)                        │
   or any        │        │                                                                   │
   program ──────┼──► Machine ──► memory (one region) ── boot loader (x86_64 | aarch64)       │
                 │        │                                                                   │
                 │        ├──► vCPU threads ──► Hypervisor backend (kvm | hvf | whp)          │
                 │        │         │ MMIO exits                                              │
                 │        ├──► virtio-mmio bus ─┬─ blk ─── raw image files                    │
                 │        │                     ├─ net ─── user-mode stack ──► host sockets   │
                 │        │                     │          or tap (Linux)                     │
                 │        │                     ├─ vsock ── API streams, guest agent           │
                 │        │                     ├─ rng                                        │
                 │        │                     ├─ gpu ─── framebuffer ──► headless | VNC |   │
                 │        │                     │                          window             │
                 │        │                     └─ input ◄── API, VNC, window                 │
                 │        └──► serial console ──► stdout / API                                │
                 └────────────────────────────────────────────────────────────────────────────┘
                                                        │
                                         Linux guest: kernel + Debian + thanatos-guest (optional)
```

One process is one VM. The process owns the guest's memory, runs one host thread per vCPU, and services device I/O on a few device threads. Everything a consumer does goes through the control API; the CLI is a thin client of it; the Rust library is what the CLI and the API server are written against.

### 4.2 Crates

A Cargo workspace. Names are the published crate names.

| Crate | Path | Owns | `unsafe` |
| --- | --- | --- | --- |
| `thanatos-hv` | `crates/hv` | The `Hypervisor`, `Vm` and `Vcpu` traits and the three backends `kvm`, `hvf`, `whp`, selected by `cfg(target_os)`. Guest memory mapping. | yes, confined here |
| `thanatos-vmm` | `crates/vmm` | `Machine`: memory layout, boot loaders, virtio-mmio bus, all devices, display sinks, control API server, configuration. | no |
| `thanatos-net` | `crates/net` | The user-mode network: a `smoltcp` interface fed by virtio-net frames, NAT to host sockets, DNS forwarding, policy, port forwards. | no |
| `thanatos-cli` | `crates/cli` | The binary, installed as `thanatos`: `run`, `doctor`, `image`, and the client commands. The crate name `thanatos` is already taken on crates.io. | no |
| `thanatos-guest` | `crates/guest` | The guest agent, built static for `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`. | no |

Workspace lints set `unsafe_code = "forbid"` everywhere except `thanatos-hv`, which sets `deny` and allows it per module.

**The hypervisor trait** is the seam that makes everything else portable. In outline:

```rust
pub trait Hypervisor {
    fn available() -> Result<(), Unavailable>;           // what `doctor` reports
    fn create_vm(&self, memory: &GuestMemory, arch: ArchConfig) -> Result<Box<dyn Vm>>;
}
pub trait Vm: Send + Sync {
    fn create_vcpu(&self, id: u32) -> Result<Box<dyn Vcpu>>;
    fn set_irq(&self, line: u32, level: bool) -> Result<()>;   // thread-safe
    fn stop(&self);                                           // kick every vCPU out of run
}
pub trait Vcpu: Send {
    fn set_boot_state(&mut self, boot: BootState) -> Result<()>;  // entry point, registers, per arch
    fn run(&mut self) -> Result<Exit>;
}
pub enum Exit {
    Mmio { address: u64, data: &mut [u8], write: bool },
    PortIo { port: u16, data: &mut [u8], write: bool },   // x86_64 only
    Halt, Shutdown, Reset, Stopped,
    Psci(PsciCall),                                      // aarch64 on hvf and whp; kvm handles PSCI in-kernel
}
```

Snapshot and restore is left room for by two future methods, `Vcpu::state()` and `Vcpu::set_state()`, that no version 1 code calls. Nothing else in the design depends on them.

### 4.3 Threads and synchronization (my call)

- One host thread per vCPU, parked in the backend's run call. A MMIO exit is handled on that thread: the bus finds the device by address, the device takes its own mutex, handles the register access, and returns. A virtqueue notification marks work and wakes the device's thread.
- One thread per device backend that blocks on I/O: disk, network, vsock, and the display sink. The gpu, input, rng and serial devices need no thread of their own.
- Device threads raise interrupts through `Vm::set_irq`, which is thread-safe on all three backends: `KVM_IRQ_LINE` and `hv_gic_set_spi` are safe to call from any thread, and the WHP backend routes the line through its own IOAPIC model, behind a mutex, before calling `WHvRequestInterrupt` (see 4.6).
- Device-to-thread wakeups use a channel and a condition variable from `std`. No `tokio`, no `mio`. The Linux KVM backend may later register `ioeventfd` and `irqfd` to avoid the round trip through the vCPU thread; it is an optimization, not a design element.
- Stopping the VM: `Vm::stop` makes every run call return `Exit::Stopped`. On KVM that is a signal to the vCPU thread, on HVF `hv_vcpus_exit`, on WHP `WHvCancelRunVirtualProcessor`.

### 4.4 Guest memory

One contiguous region allocated by the host, `mmap` anonymous on Unix and `VirtualAlloc` on Windows, mapped into the guest at the architecture's RAM base. `vm-memory` provides the `GuestMemoryMmap` type and the bounds-checked accessors the devices use. The devices never hold raw pointers into guest memory; they read and write through `vm-memory` with the guest-physical addresses the virtqueue hands them.

Guest physical layouts, fixed per architecture:

| | x86_64 | aarch64 |
| --- | --- | --- |
| RAM base | 0 | 0x8000_0000 |
| RAM size | configured, up to the MMIO hole, then above 4 GiB | configured |
| virtio-mmio windows | 0xd000_0000 + n·0x1000, IRQ 5 + n via IOAPIC | 0x0a00_0000 + n·0x200, SPI 32 + n |
| Serial | I/O port 0x3f8, IRQ 4 | PL011 at 0x0900_0000, SPI 1 |
| Interrupt controller | LAPIC and IOAPIC: both in-kernel on KVM; on WHP the LAPIC is the platform's and the IOAPIC is the VMM's | GICv3: distributor 0x0800_0000, redistributors 0x080a_0000 |
| Kernel load | bzImage per the Linux x86 boot protocol | Image at RAM base + 0x80000, DTB near the top of RAM |

The aarch64 layout mirrors QEMU's `virt` machine, which the Linux kernel, the GIC code in every hypervisor, and existing documentation all assume. The x86_64 layout mirrors Firecracker's.

### 4.5 Boot

**x86_64.** Load the bzImage at the 64-bit entry point with no BIOS and no UEFI, exactly as Firecracker does: the kernel's protected-mode code at 0x100000, the zero page with the command line and the E820 map, an identity page table, a GDT, and an MP table in the EBDA so the kernel finds the vCPUs. CPUID comes from the hypervisor's supported set with the hypervisor bit set. Time is kvmclock on KVM and the TSC with the hypervisor's reference time on WHP. Devices are announced by the kernel command line, `virtio_mmio.device=4K@0xd0000000:5` and so on. There is no ACPI; the guest shuts down by the agent's request or by `reboot`, which triple-faults into a `Shutdown` exit.

**aarch64.** Load the `Image` at RAM base + 0x80000 with a device tree built by `vm-fdt` that describes memory, the vCPUs with PSCI as the enable method, the GICv3, the architected timer, the PL011 and the virtio-mmio nodes with their SPIs. Registers at entry: x0 = DTB address, PC = Image load address. PSCI on KVM is handled in the kernel. On HVF and WHP, PSCI calls arrive as HVC exits and the VMM implements `PSCI_VERSION`, `PSCI_FEATURES`, `CPU_ON`, `CPU_OFF`, `AFFINITY_INFO`, `SYSTEM_OFF` and `SYSTEM_RESET`, which is the whole set Linux uses. `SYSTEM_OFF` is the clean shutdown path on aarch64.

**Both.** The root filesystem is a raw ext4 image found by the kernel as `/dev/vda`; there is no initramfs. The command line defaults to `console=<serial> root=/dev/vda rw` plus the virtio-mmio lines on x86_64, and the configuration may append to it.

### 4.6 Hypervisor backends

**KVM (Linux).** Through `kvm-ioctls` and `kvm-bindings`. x86_64: `KVM_CREATE_IRQCHIP`, `KVM_CREATE_PIT2`, `KVM_SET_CPUID2` from `KVM_GET_SUPPORTED_CPUID`, MSRs and registers set for the 64-bit entry. aarch64: `KVM_ARM_PREFERRED_TARGET` then `KVM_ARM_VCPU_INIT` with the PSCI 0.2 feature, a GICv3 through `KVM_CREATE_DEVICE` with its distributor and redistributor addresses and `KVM_DEV_ARM_VGIC_CTRL_INIT` after the vCPUs exist, and the timer interrupt wired through the in-kernel vGIC. Interrupts through `KVM_IRQ_LINE`.

**Hypervisor.framework (macOS arm64).** Through `applevisor` and `applevisor-sys`, version 1.0. `hv_vm_create`, `hv_vm_map` for the memory region, `hv_gic_create` before any vCPU exists (macOS 15 requirement) with the same distributor and redistributor addresses as the device tree, then `hv_vcpu_create` on each vCPU thread (the framework ties a vCPU to the thread that created it). Exits: data aborts become `Exit::Mmio`, HVC exits with a PSCI function id become `Exit::Psci`, `HV_EXIT_REASON_VTIMER_ACTIVATED` is masked and the timer interrupt delivered through the GIC, `HV_EXIT_REASON_CANCELED` becomes `Stopped`. Device interrupts through `hv_gic_set_spi`. `applevisor` 1.0 exposes the GIC in its `gic` module behind its `macos-15-0` feature (its default feature targets macOS 26); Thanatos enables `macos-15-0` so the binary runs on macOS 15 and later. The binary is signed with an entitlements file holding `com.apple.security.hypervisor`; `cargo build` output is signed ad hoc by `thanatos doctor --sign` or the release script, and the release artifact is signed and notarized.

**Windows Hypervisor Platform (Windows x86_64, version 1.1).** Through the `windows` crate's `Win32_System_Hypervisor` feature, which binds `WinHvPlatform.dll` directly. `WHvCreatePartition`, processor count and the emulated local APIC as partition properties, `WHvSetupPartition`, `WHvMapGpaRange` for memory, one `WHvCreateVirtualProcessor` per vCPU, `WHvRunVirtualProcessor` with memory-access and I/O-port exits mapped to `Exit::Mmio` and `Exit::PortIo`. WHP emulates only the local APIC, not an IOAPIC, PIT or i8259, so the backend carries its own IOAPIC model, a few hundred lines, that turns `set_irq` into `WHvRequestInterrupt` with the routed vector, and the guest gets its TSC frequency from the hypervisor CPUID leaf instead of a PIT. This is the bulk of step 10. The `whpx` crate on crates.io has fifty downloads and one release; the `windows` crate is maintained by Microsoft, so Thanatos uses it directly.

### 4.7 Virtio

All devices sit on the virtio-mmio transport, version 2 of the register layout from virtio 1.2 section 4.2. MMIO was chosen over PCI because it is a few hundred lines with no bus enumeration, config space or capability lists, the guest finds the devices through the device tree on aarch64 and the command line on x86_64, and Firecracker and Cubebot have both shown it carries a full workload. The cost is that no device can use MSI-X; each has one interrupt line, which is adequate for six devices.

The virtqueue is rust-vmm's `virtio-queue` over `vm-memory`. It is the one piece of guest-facing parsing every device goes through, it is fuzzed upstream and used by Cloud Hypervisor, and owning it would add 300 lines of the most security-relevant code in the project for no gain. (my call)

Each device implements one trait: feature bits, configuration space, a queue-notification handler, and reset. Devices see only their queues and guest memory, never the hypervisor.

| Device | Spec section | Queues | Backend | Notes |
| --- | --- | --- | --- | --- |
| blk | 5.2 | 1 | raw file, `pread`/`pwrite`, `fdatasync` on flush | `read_only` per disk; discard and write-zeroes later |
| net | 5.1 | rx, tx | `thanatos-net` user-mode stack, or a tap on Linux | MAC from config or derived from the VM name; offloads off in version 1 |
| vsock | 5.10 | rx, tx, event | host side in the VMM: streams multiplexed over the control API, and Unix socket per port on demand | guest CID 3, host CID 2 |
| rng | 5.4 | 1 | host `getrandom` | 60 lines; without it the guest waits for entropy before sshd and TLS work |
| gpu | 5.7 | control, cursor | the framebuffer in VMM memory | 2D command set only: resource create, attach backing, transfer to host, set scanout, flush, cursor; no virgl, no blobs; one scanout, 1280x800 default, EDID-less |
| input | 5.8 | event, status | the input arbiter | two devices: a keyboard and an absolute pointer (tablet) |

The serial console is a 16550 on x86_64 and a PL011 on aarch64, each about 100 lines, writing to stdout or to an API stream and reading from the API.

### 4.8 Networking

**Default: user-mode.** `thanatos-net` runs a `smoltcp` interface whose "hardware" is the virtio-net queues. The guest is given 10.0.2.15/24 with gateway 10.0.2.2 and DNS 10.0.2.3, the slirp conventions, through a tiny DHCP responder on the gateway so the stock distribution's networking works unconfigured. Guest TCP connections to the outside terminate in `smoltcp` and are proxied to host `TcpStream`s; UDP flows are proxied with a per-flow timeout; DNS to 10.0.2.3 is forwarded to the host's resolver; ICMP echo is answered for the gateway only. The host sees ordinary outbound sockets from the `thanatos` process, so host firewalls, VPNs and proxies apply.

`smoltcp` was chosen because it is the one maintained pure-Rust stack with ten million downloads and no C dependency; its current README lists TCP window scaling, delayed acknowledgements and CUBIC or Reno congestion control as implemented, and selective acknowledgements and timestamps as not. The missing SACK costs throughput on lossy links, not on the loopback and LAN paths most guests use. The builder must confirm the throughput target in section 7 before the API freezes. (my call)

**Policy.** The configuration carries an outbound policy evaluated in the VMM, before any host socket is opened: a list of allow and deny rules over destination CIDR, port range and protocol, and a default. A denied connection gets a TCP reset or ICMP unreachable in the guest, and a line in the event stream naming the destination. This is the enforcement point that replaces a pod NetworkPolicy when Thanatos runs as a bare process, and it applies identically in every placement.

**Port forwards.** `host:port -> guest:port` entries in the configuration, and `forward add` on the API, open host listeners that the stack connects to the guest.

**Linux tap.** On Linux, `network.backend = "tap"` with a named tap device uses it directly, for operators who have `CAP_NET_ADMIN` and want the guest on a real bridge. Policy and forwards do not apply; the operator's network does.

### 4.9 Control API

A local stream socket: a Unix socket on Linux and macOS, a named pipe on Windows, at a path in the configuration, created mode 0600. The protocol is JSON lines: one request object per line with an `id`, one response per line with the same `id`, plus unsolicited `event` lines. Binary channels, a vsock stream, the serial console, a VNC session, are opened by a request that returns a token, then attached by a second connection whose first line is `{"attach": "<token>"}`, after which the connection carries raw bytes.

Requests in version 1:

| Request | Does |
| --- | --- |
| `status` | state (`booting`, `running`, `stopping`, `stopped`), uptime, vCPUs, memory, devices, display mode |
| `stop { graceful: bool, timeout }` | graceful asks the guest agent to `poweroff`, then after the timeout, or when `graceful` is false, stops the VM |
| `console` | token for a bidirectional serial stream |
| `vsock { port }` | token for a stream the VMM connects to that guest port |
| `exec { argv, env, cwd, stdin: bool }` | via the guest agent; returns a token; the stream carries a framed stdin, stdout, stderr and exit status |
| `file put / get` | via the guest agent |
| `screenshot { format: "png" | "raw" }` | the host's framebuffer copy, as last flushed by the guest |
| `input { events: [...] }` | key down and up by Linux keycode, pointer move absolute, button down and up, scroll |
| `display { mode: "headless" | "vnc" | "window", vnc: { bind, password } }` | switch the sink at runtime |
| `forward add / remove` | network port forwards |
| `events` | subscribe: boot, shutdown, exit status, network policy denials, display mode changes, agent liveness |

Every request and event is defined once as Rust types in `thanatos-vmm::api`, serialized with `serde`, and the CLI and library share them. A JSON Schema is generated from them for consumers in other languages.

### 4.10 Guest agent

`thanatos-guest` is a static musl binary, under a megabyte, that the image build installs as a systemd service listening on vsock port 1. It serves `exec`, `file put`, `file get`, `poweroff` and a liveness heartbeat. It is optional: a guest without it still boots, runs, shows a screen and takes input; only `exec`, file copy and graceful stop need it. Its protocol is length-prefixed frames with a JSON header and a byte body, defined in the same `api` module.

The agent runs as root in the guest, as any init-started service does. It is a convenience for the host, not a security boundary; the VM is the boundary.

### 4.11 Display and input

The virtio-gpu device keeps the guest's current scanout as an RGBA buffer in VMM memory, updated when the guest issues `TRANSFER_TO_HOST_2D` and `RESOURCE_FLUSH`. The VMM, not the guest, decides what a screenshot is: it is whatever the guest last presented. Input events reach the guest's virtio-input devices only through the API and the active display sink; nothing in the guest can write into those queues.

The promise is exactly that, and no more: a screenshot is the host's copy of the scanout, and everything the host injected is everything that arrived on the virtio-input devices. Software inside the guest can still synthesize input for itself, by writing to `/dev/input/event*` as root or by asking its own compositor, as it can on any computer. A consumer that must tell a person's input from a program's, or keep a program off the controls, does that above the API with its own connections and its own in-guest arrangement. Cubebot's arbiter with two X servers is one such arrangement; it runs unchanged inside a Thanatos guest. (my call: the weak guarantee, stated plainly, over an unachievable strong one)

Display sinks, chosen in configuration and switchable at runtime:

- **headless**: no output; screenshots and input through the API only. The default.
- **vnc**: a built-in RFB 3.8 server on a configured bind address with optional VNC authentication, raw and CopyRect encodings in version 1, ZRLE later. Any VNC client, including noVNC in a browser, shows the screen and sends input. The server is Thanatos's own, about 600 lines, because every VNC crate on crates.io is either a client or unmaintained. (my call)
- **window** (Cargo feature `window`, off by default): a native window through `winit` and `softbuffer`. Off by default so the core binary has no windowing dependencies and builds on headless CI.

Inside the guest, the stock kernel's `virtio_gpu` driver provides `/dev/dri/card0` and a framebuffer console, and `virtio_input` provides evdev devices. Any Wayland compositor or X server that works on a plain DRM device works unchanged: the image recipe ships a minimal session (see 5) and the user can `apt install` another.

### 4.12 Configuration

One TOML file per VM, passed to `thanatos run`. Every field has a default except `kernel` and the first disk.

```toml
name = "dev"                       # used for the socket name and the MAC
vcpus = 2
memory_mib = 2048
kernel = "~/.thanatos/images/aarch64/Image"
cmdline_extra = "quiet"            # appended to the generated command line

[[disk]]
path = "~/.thanatos/vms/dev/root.ext4"
read_only = false

[[disk]]
path = "~/.thanatos/vms/dev/data.ext4"
create_mib = 8192                  # created sparse and ext4-formatted if missing (needs the guest to format; see 5.3)

[network]
backend = "user"                   # or "tap"
default = "allow"                  # or "deny"
deny = ["10.0.0.0/8", "192.168.0.0/16", "169.254.0.0/16"]
allow = ["192.168.1.10/32:443/tcp"]
forward = ["127.0.0.1:2222->22", "127.0.0.1:5900->5900"]

[display]
mode = "vnc"                       # headless | vnc | window
width = 1280
height = 800
vnc_bind = "127.0.0.1:5901"

[api]
socket = "~/.thanatos/vms/dev/api.sock"

[agent]
enabled = true                     # expect thanatos-guest on vsock port 1
```

The same structure is the `Config` type in the library, so a consumer embedding Thanatos builds it in code.

## 5. Guest images

### 5.1 Recipe

`thanatos image build --arch aarch64|x86_64` produces `Image` or `bzImage` and `root.ext4` under `~/.thanatos/images/<arch>/`. It runs inside a container (Docker or Podman, whichever is present, with `--platform` for cross-architecture builds through binfmt), so it works on macOS and Linux alike and produces byte-identical output for the same inputs. The recipe is a shell script and a kernel configuration fragment per architecture, checked into `image/`, with pinned kernel version, Debian snapshot date and package list.

- **Kernel:** Linux 6.12 LTS, built from kernel.org with checksum verification, `defconfig` plus a fragment that turns on virtio-mmio, virtio blk, net, vsock, rng, gpu and input, DRM with the fbdev console, ext4, overlayfs, netfilter for the guest's own firewall, and turns off module loading (`CONFIG_MODULES=n`) so the image carries exactly one kernel and no `/lib/modules`. uinput is off in the default configuration, which removes the easiest in-guest input injection path; it is a convenience for consumers that build attribution above the API (see 4.11), not a guarantee.
- **Root:** Debian trixie built with `mmdebstrap`, with `systemd`, `openssh-server` (disabled by default), `ca-certificates`, `curl`, `thanatos-guest` as a service, a user `user` with sudo, and the desktop profile below. Output is a raw ext4 image with no partition table, sized to contents plus headroom, resized online by the guest on first boot to fill its disk.
- **Desktop profile:** `sway` as the Wayland compositor with `foot` as the terminal and `chromium`, or `weston` where a user prefers it, started on tty1 by a systemd user session. Both run on a plain DRM device with no GPU acceleration. The profile is one package list; a `--profile minimal` build omits it.

### 5.2 Bring your own

Any kernel that has the virtio drivers and any root filesystem boot the same way; the recipe is a convenience. The documentation lists the kernel options a custom kernel needs.

### 5.3 Disks

A VM's root disk is a copy of the base image, made at `thanatos vm create` with a reflink where the filesystem supports it and a sparse copy otherwise, so a hundred VMs share nothing at runtime but cost little at rest. Extra disks are created sparse by the VMM; the guest agent formats them on first boot when the configuration asks. The VMM never formats a disk itself, so it needs no `mkfs` on the host.

## 6. Security model

- **The VM is the boundary.** Guest code, including root in the guest, cannot read VMM memory, host files, or other VMs. This holds exactly as well as the host hypervisor holds, which is the same assumption every cloud makes.
- **Attack surface is the device model.** The guest controls every byte in every virtqueue. All device code is safe Rust over bounds-checked `vm-memory` accessors, and each device has a fuzz target in `fuzz/` driven by `cargo-fuzz` with a fake guest memory, run in CI.
- **`unsafe` is confined** to `thanatos-hv`: hypervisor calls and the memory mapping. Every `unsafe` block has a comment naming the invariant it relies on.
- **Host privileges:** none beyond opening the hypervisor device. No capabilities, no setuid, no helpers. The tap backend is the only exception, and it is opt-in.
- **Process hardening (version 1.1):** a seccomp filter on Linux after the VM is set up, following Firecracker's allow list.
- **Screen and input integrity:** a screenshot is the VMM's own copy of the scanout, and the virtio-input queues are written only by the VMM. Thanatos guarantees the guest cannot alter the one or write into the other. It does not, and cannot, stop guest software from synthesizing input inside the guest; attribution between a person and a program is built above the API (4.11).
- **Network:** policy is enforced in the VMM before a host socket is opened, so it cannot be bypassed from inside the guest.
- **API socket** is mode 0600 in a directory the user owns. There is no network listener except VNC, which is off by default and binds loopback by default.

## 7. Performance

Targets on a developer laptop, 2 vCPUs, 2 GiB:

| Measure | KVM | HVF | How measured |
| --- | --- | --- | --- |
| `thanatos run` to login prompt | < 1 s | < 2 s | serial timestamp |
| Disk sequential read, 1 MiB blocks | > 70 % of host | > 70 % of host | `dd` in guest vs host |
| User-mode net, single TCP stream to host | > 300 MB/s | > 300 MB/s | `iperf3` guest to host |
| Desktop 1280x800 over VNC on loopback | 30 fps on a full-screen update | same | synthetic flush loop |
| Idle VM host CPU | < 2 % | < 2 % | `top` |

The table is advisory, measured on the minimal image profile (the desktop profile boots a full session and is not expected to meet the boot target). The release gate in section 8 is that the numbers are measured and recorded, not that they are met; the user decides whether a miss blocks a release. The known soft spot is the user-mode network, which copies every byte twice.

### 7.1 Measured, 2026-11-02

Apple M5 Pro, macOS 26, HVF, 2 vCPUs, 2 GiB. KVM was not measured: this machine reaches it only through a nested VM, and CI runners would not be representative.

| Measure | Target | Result | How |
| --- | --- | --- | --- |
| Run to login prompt | < 2 s | **Missed.** 4.5 s (systemd's own report, minimal profile, 4 GiB disk); 15 s on first boot of a `vm create` desktop VM, whose 16 GiB resize is most of that | serial, `systemd-analyze` line |
| Disk sequential read, 1 MiB | > 70 % of host | **Missed.** 2.8 GB/s against 10.8 GB/s host, 26 %, both from host cache | `dd` with `iflag=direct` in guest, `dd` on host |
| User-mode net, TCP to host | > 300 MB/s | Met. 768 MB/s | `dd` to `/dev/tcp/10.0.2.2` into `nc`, not `iperf3` |
| VNC on loopback at 1280x800 | 30 fps | Met for the server: 902 full-screen raw updates/s. 56 updates/s on a scrolling terminal, limited by the guest | a small RFB client; no synthetic guest flush loop |
| Idle host CPU | < 2 % | Met. 0.3 % | `top` |

## 8. Testing and qualification

- **Unit:** every device against a fake guest memory and a scripted virtqueue; the boot loaders against golden zero pages and device trees; the network stack against recorded packet exchanges; the API types against their JSON schema.
- **Fuzz:** each device's queue handler, the virtio-mmio register handler, the RFB parser and the API parser.
- **Boot matrix:** an integration test that boots the current image to the agent's heartbeat and runs `exec true`, on every platform in the matrix. GitHub's Linux runners expose KVM, so Linux x86_64 runs on every push; Linux aarch64 and macOS arm64 need self-hosted runners because GitHub's macOS arm64 runners are virtual machines without nested virtualization. The user's Apple M5 Pro machine is the first macOS runner, and a Linux VM on it with nested virtualization enabled (M3 or later with macOS 15 or later) is the first Linux aarch64 runner and the development loop for step 4. A Windows machine for 1.1 is an open question (section 12).
- **Release gate:** boot matrix green, fuzzers clean for an hour, performance table measured and recorded, `cargo deny` and `cargo audit` clean.
- **Desktop check:** boot the desktop image, take a screenshot, assert the wallpaper's checksum, inject a key sequence that opens the terminal, screenshot again, assert a change.

## 9. Repository layout

```
thanatos-vm/
  Cargo.toml               workspace, shared lints, pinned toolchain
  DESIGN.md                this document
  crates/hv/               thanatos-hv: traits, kvm/, hvf/, whp/
  crates/vmm/              thanatos-vmm: machine, boot/, devices/, display/, net glue, api/, config
  crates/net/              thanatos-net
  crates/cli/              thanatos
  crates/guest/            thanatos-guest
  image/                   build.sh, kernel/<arch>.config, rootfs/packages.txt, profiles/
  fuzz/                    cargo-fuzz targets
  tests/                   boot matrix and desktop check
  docs/                    user guide, API reference generated from the types
```

## 10. Order of work

Each step ends with something that runs and a test that proves it. Nothing later depends on an unproven earlier step.

1. **Workspace and `doctor`.** Crate skeletons, lints, the `Hypervisor` trait, and `thanatos doctor` that reports on each host whether a VM can be created, which checks `/dev/kvm` permissions, the macOS entitlement, or the Windows feature, and says what to fix.
2. **KVM x86_64 boots to a serial shell.** Memory, boot loader, serial, virtio-mmio with blk only, the image recipe's minimal profile. Cubebot's `apps/sandbox/src/kvm` at `~/Code/Personal/cubebot` is the user's own Apache-2.0 code covering exactly this step: its bzImage loader, MP table, 16550 and virtio-mmio transport carry over nearly as they are; its block, net and vsock devices are rewritten against `virtio-queue` rather than its hand-written queue. Thanatos's API is its own; Cubebot will implement its `Sandbox` interface on top of the Thanatos API and is not a compatibility target.
3. **Hypervisor.framework aarch64 boots to a serial shell.** The `hvf` backend, the aarch64 boot loader and device tree, PSCI, the GIC, PL011. libkrun's HVF backend is the prior art to read first. This is the step that proves the trait is right; expect the trait to change here and nowhere later.
4. **KVM aarch64.** Mostly configuration of what step 3 built; the `KVM_CREATE_DEVICE` GICv3 sequence and FDT layout are lifted from Firecracker's and Cloud Hypervisor's aarch64 code, which run on arm64 today. Proves two backends share one arch loader.
5. **vsock, rng and the guest agent.** `exec`, file copy, graceful stop, through the API.
6. **User-mode networking.** `apt install` works from inside the guest on every platform. Policy and forwards.
7. **Screen and input.** virtio-gpu, virtio-input, headless screenshots, the VNC server, the desktop profile. The desktop check passes.
8. **Control API and CLI complete.** Every request in 4.9; JSON schema; library documentation.
9. **Qualification.** Boot matrix in CI, fuzzers, performance table, `cargo deny`. Tag 1.0.
10. **Windows.** The `whp` backend and the x86_64 loader's WHP paths. Tag 1.1.
11. **Later, independent:** seccomp, snapshot and restore, ZRLE, native window polish, Windows arm64, a tap-less bridge on macOS through `vmnet` for those who want the guest on the LAN.

## 11. Constraints

- Rust stable, pinned in `rust-toolchain.toml`. No nightly features.
- Dependencies, locked: `kvm-ioctls`, `kvm-bindings`, `vm-memory`, `vm-fdt`, `virtio-queue`, `virtio-bindings`, `linux-loader` (bzImage and Image parsing), `applevisor` and `applevisor-sys`, `windows` with the hypervisor feature, `smoltcp`, `serde` and `serde_json`, `nix` or `libc`, `png` for screenshots, and `winit` and `softbuffer` behind the `window` feature. Adding a dependency is a design change recorded in this document.
- No async runtime, no C dependencies, no build-time code generation beyond `serde` derives.
- `unsafe` only in `thanatos-hv`.
- Cyclomatic complexity and function length limits set by `clippy.toml` at the workspace root.
- The guest image recipe is reproducible: same inputs, same bytes.
- Everything a consumer can do is reachable through the control API; the library adds nothing the API cannot do, so a consumer in another language is never second-class.

## 12. Open questions for the user

Answers change what gets built. Everything else in this document a builder can settle.

1. **Emulation fallback.** Is "no hypervisor, no VM" acceptable forever, or should the `Hypervisor` trait be kept emulator-shaped so a slow software backend can be added later? The trait as drawn can take one; keeping it that way costs nothing now but rules out hypervisor-specific shortcuts later.
2. **License.** Apache-2.0 OR MIT is assumed. Cubebot is Apache-2.0, so porting its sandbox code into an Apache-2.0 OR MIT project is compatible; the user owns both in any case.
3. **Debian trixie as the default image.** Alpine would be a fifth of the size and boot faster; Debian has the broadest desktop and application coverage on both architectures. The recipe can build either, but one is the default that gets tested.
4. **Windows at 1.1.** Confirmed as acceptable, or does the user need Windows in the first release? Either way a Windows 11 machine with the Hypervisor Platform feature is needed as a test runner before step 10 can be scheduled.
5. **macOS distribution.** A notarized release binary needs a paid Apple Developer account and its certificate in the release pipeline. Without one, downloaded binaries are blocked by Gatekeeper and the macOS install story is "build it yourself and let `thanatos doctor --sign` sign it ad hoc". Does the user have, or want to get, a Developer ID?

## 13. Left to the builder

Guest physical addresses within the ranges in 4.4; the exact `Exit` enum shape; the device-thread wakeup mechanism; the DHCP responder's lease shape; the VNC server's authentication scheme beyond VNC auth; the agent's frame format; the API's token format; the image's package list beyond what 5.1 names; `clippy.toml` thresholds; CI runner labels; the name of the default VM directory; how `thanatos vm create` names and finds images; whether `doctor --sign` or a separate script signs the macOS binary.

## 14. Prior art and references

- Firecracker: the proof that a minimal KVM monitor with virtio-mmio, a direct 64-bit kernel entry and no BIOS boots a full Linux in about 125 ms. Thanatos's x86_64 and aarch64 memory layouts and boot paths follow it.
- QEMU's `virt` machine: the aarch64 layout the Linux kernel, the GIC drivers and the HVF GIC in macOS 15 all expect.
- Cloud Hypervisor: the GICv3 and FDT code ported from Firecracker that documents the `KVM_CREATE_DEVICE` sequence.
- libkrun: a Rust VMM with both a KVM and a Hypervisor.framework backend for aarch64; the closest existing prior art for step 3.
- Cubebot `apps/sandbox`: 5,300 lines of the user's own KVM x86_64 VMM with virtio blk, net and vsock over MMIO, a 16550, and a direct bzImage loader; the starting point for step 2.
- slirp and `gvproxy`: the user-mode networking conventions, 10.0.2.0/24 with the gateway at .2 and DNS at .3, that every guest image already expects.
- Apple: [hv_gic_create](https://developer.apple.com/documentation/hypervisor/hv_gic_create(_:)) is macOS 15+, must be called after `hv_vm_create` and before any vCPU; one GIC per VM. Nested virtualization is M3 or later with macOS 15, which is what a Linux VM on a Mac needs to run KVM itself.
- Microsoft: [Windows Hypervisor Platform API](https://learn.microsoft.com/en-us/virtualization/api/hypervisor-platform/hypervisor-platform), available since the April 2018 update; arm64 support reported by `WHvGetCapability`; on arm64 the partition must use the emulated GIC.
- Crates, versions as of 2026-11-02 on crates.io: `kvm-ioctls` 0.25.1, `kvm-bindings` 0.14.2, `vm-memory` 0.18.0, `vm-fdt` 0.3.0, `virtio-queue` 0.18.0, `virtio-bindings` 0.2.7, `linux-loader` 0.14.0, `applevisor` 1.0.0, `windows` 0.62.2, `smoltcp` 0.14.0.
- Apple's WWDC 2025 announcement that macOS 26 is the last release for Intel Macs, the reason there is no x86 Hypervisor.framework backend.

## Builder's calls

- 2026-11-02: no `linux-loader`; bzImage and arm64 Image headers are parsed by hand in `boot/`, fewer lines than adapting to it.
- 2026-11-02: amends 4.3: `blk` is served synchronously on the vCPU thread that kicks it.
- 2026-11-02: amends 4.4: the arm64 Image loads at RAM base + its header's `text_offset`, as the boot protocol requires, not a fixed +0x80000.
- 2026-11-02: PSCI lives inside the HVF backend, so it is not in the `Exit` enum.
- 2026-11-02: section 11 has no TOML crate; `config.rs` parses the subset into a JSON tree for serde.
- 2026-11-02: amends 4.12: `[network] tap` names the device for the `tap` backend.
- 2026-11-02: `image build` runs from a checkout, since the recipe lives there.
- 2026-11-02: `vm create` grows the root disk sparse to 16 GiB; the guest resizes on first boot.
- 2026-11-02: no `clippy.toml` or `cargo deny`: the brief says exactly three gates.
- 2026-11-02: amends 4.5: the guest gets a real-time clock (PL031, CMOS); without one TLS and apt signatures fail.
- 2026-11-02: amends 4.7: vsock refuses guest-initiated connections; no Unix socket per port until a guest service needs one.
- 2026-11-02: amends 4.8: host loopback, 0.0.0.0/8 and 10.0.2.0/24 (the gateway reaches host loopback) open only by an explicit allow, even with default allow.
- 2026-11-02: amends 4.11: VNC sends raw only (CopyRect is optional for a server); a non-loopback address without a password is refused.
- 2026-11-02: amends 4.11: closing the window leaves the VM running, headless; with the feature, `thanatos run` keeps the main thread for the window and creates the event loop only on the first open.
- 2026-11-02: amends 4.9: `screenshot` returns a token for its bytes, as other binary channels do, plus width and height.
- 2026-11-02: amends 4.9 and 4.10: `exec`, `file put` and `file get` tokens carry the agent's frames directly.
- 2026-11-02: amends 5.1: sway starts from an autologin on tty1 through `.bash_profile`, no seatd, a flat wallpaper for the desktop check.
- 2026-11-02: amends 8: fuzzers run a minute through `just fuzz` and not in CI, per the brief's one workflow.
- 2026-11-02: JSON schema is hand-written beside the types since section 11 has no schema crate; a test holds it to one sample per variant.
