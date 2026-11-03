//! The seam that makes Thanatos portable: three traits, one backend per host.
use std::io;

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod hvf;
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod kvm;

#[allow(unsafe_code)]
pub mod sandbox;

#[cfg(target_os = "macos")]
pub use hvf::Host;
#[cfg(target_os = "linux")]
pub use kvm::Host;
#[cfg(target_os = "linux")]
pub use kvm::open_tap;

/// Guest RAM. Devices touch it only through `vm-memory`'s checked accessors.
pub type GuestMemory = vm_memory::GuestMemoryMmap<()>;

/// Where the x86_64 boot loader puts the tables `set_boot_state` points the vCPU at.
pub const X86_GDT: u64 = 0x500;
pub const X86_PML4: u64 = 0x9000;
/// aarch64 GICv3 frames, which the device tree and the backends must agree on.
pub const GIC_DISTRIBUTOR: u64 = 0x0800_0000;
pub const GIC_REDISTRIBUTOR: u64 = 0x080a_0000;

/// Why no VM can be created here, and what to do about it.
#[derive(Debug)]
pub struct Unavailable {
    pub reason: String,
    pub fix: String,
}

pub trait Hypervisor {
    type Vm: Vm + 'static;
    /// What `thanatos doctor` reports.
    fn available() -> Result<(), Unavailable>;
    fn create_vm(memory: &GuestMemory, vcpus: u32) -> io::Result<Self::Vm>;
}

pub trait Vm: Send + Sync {
    type Vcpu<'a>: Vcpu
    where
        Self: 'a;
    /// Called on the thread that will run the vCPU, one id at a time, in order from 0.
    fn create_vcpu(&self, id: u32) -> io::Result<Self::Vcpu<'_>>;
    /// Level of a device interrupt line (the x86 GSI or the aarch64 SPI offset), from any thread.
    fn set_irq(&self, line: u32, level: bool) -> io::Result<()>;
    /// Makes every `run`, present or future, return `Exit::Stopped`.
    fn stop(&self);
}

pub trait Vcpu {
    /// Entry for the boot processor: `entry` is the PC, `arg` the zero page or the DTB address.
    fn set_boot_state(&mut self, entry: u64, arg: u64) -> io::Result<()>;
    fn run(&mut self) -> io::Result<Exit<'_>>;
}

pub enum Exit<'a> {
    /// Nothing for the caller to do; run again.
    Again,
    /// On a read the handler fills `data`, which the next `run` hands to the guest.
    Mmio {
        address: u64,
        data: &'a mut [u8],
        write: bool,
    },
    PortIo {
        port: u16,
        data: &'a mut [u8],
        write: bool,
    },
    /// Guest powered off.
    Shutdown,
    /// Guest asked to reboot; one process is one boot, so the caller ends too.
    Reset,
    Stopped,
}
