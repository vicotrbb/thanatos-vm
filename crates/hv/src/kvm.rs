//! KVM on Linux.
use crate::{Exit, GuestMemory, Hypervisor, Unavailable, Vcpu, Vm};
use kvm_ioctls::Kvm;
use std::io;

pub struct Host;

impl Hypervisor for Host {
    type Vm = KvmVm;

    fn available() -> Result<(), Unavailable> {
        Kvm::new().map(drop).map_err(|error| Unavailable {
            reason: format!("cannot open /dev/kvm: {error}"),
            fix: "enable virtualization and join the device's group: sudo usermod -aG kvm $USER, or pass --device /dev/kvm to the container".into(),
        })
    }

    fn create_vm(_: &GuestMemory, _: u32) -> io::Result<KvmVm> {
        Err(io::Error::other("not yet"))
    }
}

pub struct KvmVm;

impl Vm for KvmVm {
    type Vcpu<'a> = KvmVcpu;
    fn create_vcpu(&self, _: u32) -> io::Result<KvmVcpu> {
        Err(io::Error::other("not yet"))
    }
    fn set_irq(&self, _: u32, _: bool) -> io::Result<()> {
        Ok(())
    }
    fn stop(&self) {}
}

pub struct KvmVcpu;

impl Vcpu for KvmVcpu {
    fn set_boot_state(&mut self, _: u64, _: u64) -> io::Result<()> {
        Ok(())
    }
    fn run(&mut self) -> io::Result<Exit<'_>> {
        Ok(Exit::Stopped)
    }
}
