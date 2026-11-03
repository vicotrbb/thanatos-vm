//! `Machine`: one process, one VM. Owns memory, the bus, and the vCPU threads.
#[cfg(target_arch = "aarch64")]
use crate::pl011::Pl011;
#[cfg(target_arch = "x86_64")]
use crate::serial::Serial;
use crate::{
    GuestMemory,
    api::Event,
    boot,
    config::{Config, expand},
    console::Console,
    devices::{
        blk::Blk,
        gpu::Gpu,
        input::{Input, InputPorts, Kind},
        net::{self, Net},
        rng::Rng,
        vsock::Vsock,
    },
    mmio::{Irq, Mmio, Slot},
    screen::Screen,
    server::Events,
};
use std::{
    fs::File,
    io,
    sync::{Arc, Mutex, mpsc},
};
use thanatos_hv::{Exit, Host, Hypervisor, Vcpu, Vm};
use vm_memory::{GuestAddress, GuestMemoryMmap};

#[cfg(target_arch = "x86_64")]
mod layout {
    pub const MMIO_BASE: u64 = 0xd000_0000;
    pub const MMIO_WINDOW: u64 = 0x1000;
    pub const FIRST_IRQ: u32 = 5;
    pub const MAX_DEVICES: usize = 11; // ISA pins 5-15 in the MP table
    pub const SERIAL_IRQ: u32 = 4;
    pub const SERIAL_PORT: u16 = 0x3f8;
}

#[cfg(target_arch = "aarch64")]
mod layout {
    pub const MMIO_BASE: u64 = 0x0a00_0000;
    pub const MMIO_WINDOW: u64 = 0x200;
    pub const FIRST_IRQ: u32 = 32;
    pub const MAX_DEVICES: usize = 32;
}

/// What the control API drives while the machine runs.
#[derive(Clone)]
pub struct Control {
    pub vsock: Arc<Slot<Vsock>>,
    /// `None` on a tap, where the operator's network applies instead.
    pub network: Option<thanatos_net::Net>,
    pub console: Console,
    pub screen: Arc<Screen>,
    pub input: InputPorts,
    pub events: Arc<Events>,
    pub stop: Arc<dyn Fn() + Send + Sync>,
}

/// Exit status of the guest as the process reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    Shutdown,
    Reset,
    Stopped,
}

fn guest_memory(memory_mib: u64) -> io::Result<GuestMemory> {
    let size = memory_mib << 20;
    let ranges = ram_ranges(size);
    GuestMemoryMmap::from_ranges(&ranges).map_err(io::Error::other)
}

#[cfg(target_arch = "x86_64")]
fn ram_ranges(size: u64) -> Vec<(GuestAddress, usize)> {
    let low = size.min(layout::MMIO_BASE);
    let mut ranges = vec![(GuestAddress(0), low as usize)];
    if size > low {
        ranges.push((GuestAddress(1 << 32), (size - low) as usize));
    }
    ranges
}

#[cfg(target_arch = "aarch64")]
fn ram_ranges(size: u64) -> Vec<(GuestAddress, usize)> {
    vec![(GuestAddress(boot::aarch64::RAM_BASE), size as usize)]
}

fn open_disk(disk: &crate::config::Disk) -> io::Result<File> {
    let path = expand(&disk.path);
    let named = |e: io::Error| io::Error::new(e.kind(), format!("disk {}: {e}", path.display()));
    if let (Some(mib), false) = (disk.create_mib, path.try_exists().map_err(named)?) {
        File::create(&path)
            .map_err(named)?
            .set_len(mib << 20)
            .map_err(named)?;
    }
    let file = File::options()
        .read(true)
        .write(!disk.read_only)
        .open(&path)
        .map_err(named)?;
    // Two writers, or a reader beside a writer, would corrupt the filesystem or read it torn.
    let locked = if disk.read_only {
        file.try_lock_shared()
    } else {
        file.try_lock()
    };
    locked.map_err(|e| match e {
        std::fs::TryLockError::WouldBlock => named(io::Error::new(
            io::ErrorKind::ResourceBusy,
            "in use by another VM",
        )),
        std::fs::TryLockError::Error(e) => named(e),
    })?;
    Ok(file)
}

/// shortcut: the tap backend exists only on Linux, as DESIGN 4.8 specifies.
#[cfg(target_os = "linux")]
fn tap_device(
    config: &Config,
    mac: [u8; 6],
    memory: &GuestMemory,
    irq: &Irq,
    line: u32,
) -> io::Result<Arc<Slot<Net>>> {
    let tap = thanatos_hv::open_tap(&config.network.tap)?;
    let writer = tap.try_clone()?;
    let slot = Slot::new(
        Net::new(mac, net::write_tap(writer)),
        memory.clone(),
        irq.clone(),
        line,
    );
    net::pump_tap(tap, slot.clone());
    Ok(slot)
}

#[cfg(not(target_os = "linux"))]
fn tap_device(
    _: &Config,
    _: [u8; 6],
    _: &GuestMemory,
    _: &Irq,
    _: u32,
) -> io::Result<Arc<Slot<Net>>> {
    Err(io::Error::other(
        "the tap network backend exists only on Linux",
    ))
}

fn network(
    config: &Config,
    memory: &GuestMemory,
    irq: &Irq,
    line: u32,
    events: &Arc<Events>,
) -> io::Result<(Arc<Slot<Net>>, Option<thanatos_net::Net>)> {
    let mac = net::mac_from_name(&config.name);
    if config.network.backend == "tap" {
        return Ok((tap_device(config, mac, memory, irq, line)?, None));
    }
    if config.network.backend != "user" {
        return Err(io::Error::other(format!(
            "unknown network backend {:?}",
            config.network.backend
        )));
    }
    // The stack needs the device to deliver to and the device the stack to send to.
    let stack = Arc::new(std::sync::OnceLock::<thanatos_net::Net>::new());
    let to_stack = stack.clone();
    let slot = Slot::new(
        Net::new(mac, move |frame| {
            if let Some(stack) = to_stack.get() {
                stack.frame(frame);
            }
        }),
        memory.clone(),
        irq.clone(),
        line,
    );
    let settings = thanatos_net::Settings {
        default: config.network.default.clone(),
        allow: config.network.allow.clone(),
        deny: config.network.deny.clone(),
        forward: config.network.forward.clone(),
        on_denied: {
            let events = events.clone();
            Box::new(move |destination| events.emit(&Event::NetworkDenied { destination }))
        },
    };
    let started = thanatos_net::Net::start(settings, mac, net::deliverer(Arc::downgrade(&slot)))
        .map_err(io::Error::other)?;
    let _ = stack.set(started.clone());
    Ok((slot, Some(started)))
}

/// The windows on the bus, in guest-physical order of lookup.
#[derive(Default)]
pub struct Bus(Vec<(u64, u64, Arc<dyn Mmio>)>);

impl Bus {
    fn add(&mut self, base: u64, length: u64, device: Arc<dyn Mmio>) {
        self.0.push((base, length, device));
    }

    fn access(&self, address: u64, data: &mut [u8], write: bool) {
        let Some((base, _, device)) = self
            .0
            .iter()
            .find(|(base, length, _)| (*base..base + length).contains(&address))
        else {
            // No device reads as zero and swallows writes, as an unpopulated bus does.
            if !write {
                data.fill(0);
            }
            return;
        };
        if write {
            device.write(address - base, data);
        } else {
            device.read(address - base, data);
        }
    }
}

/// Disks and the kernel are already open; only these paths are still needed by name.
fn confine(config: &Config) -> io::Result<()> {
    let socket = crate::server::socket_path(config);
    let parent = socket.parent().unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;
    thanatos_hv::sandbox::enter(&thanatos_hv::sandbox::Paths {
        directories: vec![parent.to_path_buf()],
        devices: ["/dev/kvm", "/dev/net/tun", "/dev/urandom"]
            .map(Into::into)
            .to_vec(),
        // AppKit finds the application through its own executable.
        read: ["/etc/resolv.conf".into()]
            .into_iter()
            .chain(std::env::current_exe())
            .collect(),
    })
}

pub fn run(
    config: &Config,
    console: Console,
    events: Arc<Events>,
    ready: impl FnOnce(Control),
) -> io::Result<Ended> {
    let kernel_path = expand(&config.kernel);
    let kernel = std::fs::read(&kernel_path)
        .map_err(|e| io::Error::new(e.kind(), format!("kernel {}: {e}", kernel_path.display())))?;
    // Six more windows: rng, vsock, net, gpu and two inputs.
    if config.disk.len() + 6 > layout::MAX_DEVICES {
        return Err(io::Error::other("too many disks"));
    }
    let disks = config
        .disk
        .iter()
        .map(open_disk)
        .collect::<io::Result<Vec<_>>>()?;
    // Before any thread starts, because threads inherit the confinement on Linux.
    if config.sandbox.enabled {
        confine(config)?;
    }
    let memory = guest_memory(config.memory_mib)?;
    let vm = Arc::new(Host::create_vm(&memory, config.vcpus)?);
    let irq: Irq = {
        let vm = vm.clone();
        Arc::new(move |line, level| {
            let _ = vm.set_irq(line, level);
        })
    };
    let stopper = vm.clone();
    let vm = &*vm;

    let mut bus = Bus::default();
    let mut lines = Vec::new();
    for (index, (file, disk)) in disks.into_iter().zip(&config.disk).enumerate() {
        let blk = Blk::new(file, disk.read_only, &format!("vd{index}"))?;
        let base = layout::MMIO_BASE + index as u64 * layout::MMIO_WINDOW;
        let line = layout::FIRST_IRQ + index as u32;
        bus.add(
            base,
            layout::MMIO_WINDOW,
            Slot::new(blk, memory.clone(), irq.clone(), line),
        );
        lines.push((base, line));
    }

    let mut window = |bus: &mut Bus, device: Arc<dyn Mmio>| {
        let index = lines.len();
        let (base, line) = (
            layout::MMIO_BASE + index as u64 * layout::MMIO_WINDOW,
            layout::FIRST_IRQ + index as u32,
        );
        bus.add(base, layout::MMIO_WINDOW, device);
        lines.push((base, line));
    };
    let line_of = |index: usize| layout::FIRST_IRQ + index as u32;
    let index = config.disk.len();
    let rng = Slot::new(Rng::new()?, memory.clone(), irq.clone(), line_of(index));
    window(&mut bus, rng);
    let vsock = Slot::new(
        Vsock::default(),
        memory.clone(),
        irq.clone(),
        line_of(index + 1),
    );
    window(&mut bus, vsock.clone());
    let (net_slot, network) = network(config, &memory, &irq, line_of(index + 2), &events)?;
    window(&mut bus, net_slot);
    let screen = Arc::new(Screen::new(config.display.width, config.display.height));
    let gpu = Slot::new(
        Gpu::new(screen.clone()),
        memory.clone(),
        irq.clone(),
        line_of(index + 3),
    );
    window(&mut bus, gpu);
    let keyboard = Slot::new(
        Input::new(Kind::Keyboard),
        memory.clone(),
        irq.clone(),
        line_of(index + 4),
    );
    let tablet = Slot::new(
        Input::new(Kind::Tablet),
        memory.clone(),
        irq.clone(),
        line_of(index + 5),
    );
    window(&mut bus, keyboard.clone());
    window(&mut bus, tablet.clone());

    let (entry, arg, uart) =
        load_guest(config, &memory, &kernel, &lines, &console, &irq, &mut bus)?;
    let bus = &bus;
    ready(Control {
        vsock,
        network,
        console: console.clone(),
        screen,
        input: InputPorts { keyboard, tablet },
        events,
        stop: {
            let vm = stopper;
            Arc::new(move || vm.stop())
        },
    });
    let ended = Mutex::new(None);
    // Held until all vCPUs exist: KVM's GIC cannot initialise once one has entered the guest.
    let gate = &Mutex::new(());
    std::thread::scope(|scope| -> io::Result<()> {
        let (created, wait) = mpsc::channel();
        let opened = gate.lock().unwrap();
        for id in 0..config.vcpus {
            let (created, ended, uart, irq) = (created.clone(), &ended, &uart, irq.clone());
            scope.spawn(move || {
                let result = vm.create_vcpu(id).and_then(|mut vcpu| {
                    if id == 0 {
                        vcpu.set_boot_state(entry, arg)?;
                    }
                    let _ = created.send(Ok(()));
                    drop(gate.lock());
                    vcpu_loop(&mut vcpu, bus, uart, &irq)
                });
                let reason = match result {
                    Ok(reason) => reason,
                    Err(error) => {
                        eprintln!("vCPU {id}: {error}");
                        let _ = created.send(Err(()));
                        Ended::Stopped
                    }
                };
                ended.lock().unwrap().get_or_insert(reason);
                vm.stop();
            });
            // One at a time: backends may finish VM setup when the last vCPU exists.
            if wait
                .recv()
                .map_or(true, |created: Result<(), ()>| created.is_err())
            {
                break;
            }
        }
        drop(opened);
        Ok(())
    })?;
    let reason = ended.lock().unwrap().take();
    Ok(reason.unwrap_or(Ended::Stopped))
}

fn with_extra(mut cmdline: String, config: &Config) -> String {
    if !config.cmdline_extra.is_empty() {
        cmdline.push(' ');
        cmdline.push_str(&config.cmdline_extra);
    }
    cmdline
}

#[cfg(target_arch = "x86_64")]
type PortUart = (Arc<Mutex<Serial>>, crate::rtc::Cmos);
#[cfg(target_arch = "aarch64")]
type PortUart = ();

/// Puts the kernel in memory and wires the console's UART; returns the boot processor's entry
/// state and what `port_io` needs.
#[cfg(target_arch = "x86_64")]
fn load_guest(
    config: &Config,
    memory: &GuestMemory,
    kernel: &[u8],
    devices: &[(u64, u32)],
    console: &Console,
    irq: &Irq,
    _: &mut Bus,
) -> io::Result<(u64, u64, PortUart)> {
    let mut cmdline = String::from("console=ttyS0 root=/dev/vda rw reboot=t panic=-1 pci=off");
    for (base, line) in devices {
        cmdline.push_str(&format!(" virtio_mmio.device=4K@{base:#x}:{line}"));
    }
    let cmdline = with_extra(cmdline, config);
    let entry = boot::x86_64::load(memory, kernel, &cmdline, config.vcpus as u8)?;
    let uart = Arc::new(Mutex::new(Serial::new(console.clone())));
    let (kicked, irq) = (uart.clone(), irq.clone());
    console.set_kick(move || irq(layout::SERIAL_IRQ, kicked.lock().unwrap().line()));
    Ok((entry, boot::x86_64::ZERO_PAGE, (uart, Default::default())))
}

#[cfg(target_arch = "aarch64")]
fn load_guest(
    config: &Config,
    memory: &GuestMemory,
    kernel: &[u8],
    devices: &[(u64, u32)],
    console: &Console,
    irq: &Irq,
    bus: &mut Bus,
) -> io::Result<(u64, u64, PortUart)> {
    use boot::aarch64::{RTC_BASE, SERIAL_BASE, SERIAL_SPI};
    let cmdline = with_extra("console=ttyAMA0 root=/dev/vda rw panic=-1".into(), config);
    let windows: Vec<_> = devices
        .iter()
        .map(|&(base, line)| (base, layout::MMIO_WINDOW, line))
        .collect();
    let (entry, dtb) = boot::aarch64::load(memory, kernel, &cmdline, config.vcpus, &windows)?;
    let uart = Arc::new(Pl011::new(console.clone(), irq.clone(), SERIAL_SPI));
    let kicked = uart.clone();
    console.set_kick(move || kicked.update());
    bus.add(SERIAL_BASE, 0x1000, uart);
    bus.add(RTC_BASE, 0x1000, Arc::new(crate::rtc::Pl031));
    Ok((entry, dtb, ()))
}

fn vcpu_loop<V: Vcpu>(vcpu: &mut V, bus: &Bus, uart: &PortUart, irq: &Irq) -> io::Result<Ended> {
    loop {
        match vcpu.run()? {
            Exit::Again => (),
            Exit::Mmio {
                address,
                data,
                write,
            } => bus.access(address, data, write),
            Exit::PortIo { port, data, write } => port_io(port, data, write, uart, irq),
            Exit::Shutdown => return Ok(Ended::Shutdown),
            Exit::Reset => return Ok(Ended::Reset),
            Exit::Stopped => return Ok(Ended::Stopped),
        }
    }
}

#[cfg(target_arch = "aarch64")]
fn port_io(_: u16, _: &mut [u8], _: bool, _: &PortUart, _: &Irq) {}

#[cfg(target_arch = "x86_64")]
fn port_io(port: u16, data: &mut [u8], write: bool, (serial, cmos): &PortUart, irq: &Irq) {
    let offset = port.wrapping_sub(layout::SERIAL_PORT);
    match (port, write) {
        (0x70, true) if !data.is_empty() => return cmos.select(data[0]),
        (0x71, false) => return data.fill(cmos.read()),
        (0x70 | 0x71, _) => return,
        _ => (),
    }
    if offset < 8 && !data.is_empty() {
        let mut uart = serial.lock().unwrap();
        if write {
            uart.write(offset, data[0]);
        } else {
            data[0] = uart.read(offset);
        }
        irq(layout::SERIAL_IRQ, uart.line());
    } else if !write {
        data.fill(0xff); // nothing there, as an empty ISA bus reads
    }
}

#[cfg(test)]
mod disk_lock_tests {
    use super::*;
    use crate::config::Disk;

    #[test]
    fn a_disk_has_one_writer_or_many_readers() {
        let path = std::env::temp_dir().join(format!("thanatos-lock-{}", std::process::id()));
        std::fs::write(&path, [0; 512]).unwrap();
        let disk = |read_only| Disk {
            path: path.clone(),
            read_only,
            create_mib: None,
        };
        let writer = open_disk(&disk(false)).unwrap();
        let busy = open_disk(&disk(false)).unwrap_err();
        assert_eq!(busy.kind(), io::ErrorKind::ResourceBusy);
        assert!(busy.to_string().contains("in use"));
        assert!(open_disk(&disk(true)).is_err());
        drop(writer);
        let (_one, _two) = (
            open_disk(&disk(true)).unwrap(),
            open_disk(&disk(true)).unwrap(),
        );
        assert!(open_disk(&disk(false)).is_err());
        std::fs::remove_file(&path).unwrap();
    }
}
