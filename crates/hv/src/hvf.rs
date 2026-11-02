//! Hypervisor.framework on Apple silicon.
use crate::{Exit, GuestMemory, Hypervisor, Unavailable, Vcpu, Vm};
use applevisor::vm::VirtualMachine;
use std::io;

pub struct Host;

impl Hypervisor for Host {
    type Vm = HvfVm;

    fn available() -> Result<(), Unavailable> {
        VirtualMachine::new()
            .map(drop)
            .map_err(|error| Unavailable {
                reason: format!("hv_vm_create failed: {error}"),
                fix: "sign this binary with the hypervisor entitlement: thanatos doctor --sign"
                    .into(),
            })
    }

    fn create_vm(_: &GuestMemory, _: u32) -> io::Result<HvfVm> {
        Err(io::Error::other("not yet"))
    }
}

pub struct HvfVm;

impl Vm for HvfVm {
    type Vcpu<'a> = HvfVcpu;
    fn create_vcpu(&self, _: u32) -> io::Result<HvfVcpu> {
        Err(io::Error::other("not yet"))
    }
    fn set_irq(&self, _: u32, _: bool) -> io::Result<()> {
        Ok(())
    }
    fn stop(&self) {}
}

pub struct HvfVcpu;

impl Vcpu for HvfVcpu {
    fn set_boot_state(&mut self, _: u64, _: u64) -> io::Result<()> {
        Ok(())
    }
    fn run(&mut self) -> io::Result<Exit<'_>> {
        Ok(Exit::Stopped)
    }
}
