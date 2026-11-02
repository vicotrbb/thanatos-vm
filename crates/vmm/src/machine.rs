//! `Machine`: one process, one VM. Owns memory, the bus, and the vCPU threads.
#[cfg(target_arch = "aarch64")]
use crate::pl011::Pl011;
#[cfg(target_arch = "x86_64")]
use crate::serial::Serial;
use crate::{
    GuestMemory, boot,
    config::{Config, expand},
    console::Console,
    devices::blk::Blk,
    mmio::{Irq, Mmio, Slot},
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
    File::options()
        .read(true)
        .write(!disk.read_only)
        .open(&path)
        .map_err(named)
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

pub fn run(config: &Config, console: Console) -> io::Result<Ended> {
    let kernel_path = expand(&config.kernel);
    let kernel = std::fs::read(&kernel_path)
        .map_err(|e| io::Error::new(e.kind(), format!("kernel {}: {e}", kernel_path.display())))?;
    if config.disk.len() > layout::MAX_DEVICES {
        return Err(io::Error::other("too many disks"));
    }
    let memory = guest_memory(config.memory_mib)?;
    let vm = Arc::new(Host::create_vm(&memory, config.vcpus)?);
    let irq: Irq = {
        let vm = vm.clone();
        Arc::new(move |line, level| {
            let _ = vm.set_irq(line, level);
        })
    };
    let vm = &*vm;

    let mut bus = Bus::default();
    let mut lines = Vec::new();
    for (index, disk) in config.disk.iter().enumerate() {
        let blk = Blk::new(open_disk(disk)?, disk.read_only, &format!("vd{index}"))?;
        let base = layout::MMIO_BASE + index as u64 * layout::MMIO_WINDOW;
        let line = layout::FIRST_IRQ + index as u32;
        bus.add(
            base,
            layout::MMIO_WINDOW,
            Slot::new(blk, memory.clone(), irq.clone(), line),
        );
        lines.push((base, line));
    }

    let (entry, arg, uart) =
        load_guest(config, &memory, &kernel, &lines, &console, &irq, &mut bus)?;
    let bus = &bus;
    let ended = Mutex::new(None);
    std::thread::scope(|scope| -> io::Result<()> {
        let (created, wait) = mpsc::channel();
        for id in 0..config.vcpus {
            let (created, ended, uart, irq) = (created.clone(), &ended, &uart, irq.clone());
            scope.spawn(move || {
                let result = vm.create_vcpu(id).and_then(|mut vcpu| {
                    if id == 0 {
                        vcpu.set_boot_state(entry, arg)?;
                    }
                    let _ = created.send(Ok(()));
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
type PortUart = Arc<Mutex<Serial>>;
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
    Ok((entry, boot::x86_64::ZERO_PAGE, uart))
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
    use boot::aarch64::{SERIAL_BASE, SERIAL_SPI};
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
fn port_io(port: u16, data: &mut [u8], write: bool, serial: &PortUart, irq: &Irq) {
    let offset = port.wrapping_sub(layout::SERIAL_PORT);
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
