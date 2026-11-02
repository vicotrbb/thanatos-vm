//! KVM on Linux.
use crate::{Exit, GuestMemory, Hypervisor, Unavailable, Vcpu, Vm};
use kvm_bindings::kvm_userspace_memory_region;
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};
use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use vm_memory::{GuestMemoryBackend, GuestMemoryRegion};

const KICK_SIGNAL: i32 = libc::SIGUSR1;

pub struct Host;

impl Hypervisor for Host {
    type Vm = KvmVm;

    fn available() -> Result<(), Unavailable> {
        let unusable = |reason: String| {
            Unavailable {
            reason,
            fix: "enable virtualization and join the device's group: sudo usermod -aG kvm $USER, or pass --device /dev/kvm to the container".into(),
        }
        };
        let kvm = Kvm::new().map_err(|e| unusable(format!("cannot open /dev/kvm: {e}")))?;
        kvm.create_vm()
            .map(drop)
            .map_err(|e| unusable(format!("KVM_CREATE_VM: {e}")))
    }

    fn create_vm(memory: &GuestMemory, vcpus: u32) -> io::Result<KvmVm> {
        let kvm = Kvm::new()?;
        let fd = kvm.create_vm()?;
        for (slot, region) in memory.iter().enumerate() {
            let host = region.as_ptr() as u64;
            let mapping = kvm_userspace_memory_region {
                slot: slot as u32,
                flags: 0,
                guest_phys_addr: region.start_addr().0,
                memory_size: region.len(),
                userspace_addr: host,
            };
            // SAFETY: the mapping belongs to `memory`, whose clones the machine keeps for the
            // VM's whole life, and it covers exactly the region's length.
            unsafe { fd.set_user_memory_region(mapping)? };
        }
        arch::setup_vm(&fd)?;
        install_kick_handler()?;
        Ok(KvmVm {
            kvm,
            fd,
            vcpus,
            threads: Arc::default(),
            stopped: AtomicBool::new(false),
            // A unit struct on x86_64 only.
            #[allow(clippy::default_constructed_unit_structs)]
            arch: Mutex::new(arch::VmState::default()),
        })
    }
}

extern "C" fn on_kick(_: i32) {}

/// KVM_RUN returns EINTR only if the signal has a handler rather than the default action.
fn install_kick_handler() -> io::Result<()> {
    // SAFETY: the handler is async-signal-safe, it does nothing.
    let previous = unsafe { libc::signal(KICK_SIGNAL, on_kick as *const () as libc::sighandler_t) };
    if previous == libc::SIG_ERR {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub struct KvmVm {
    #[cfg_attr(target_arch = "aarch64", allow(dead_code))]
    kvm: Kvm,
    fd: VmFd,
    #[cfg_attr(target_arch = "x86_64", allow(dead_code))]
    vcpus: u32,
    threads: Arc<Mutex<Vec<libc::pthread_t>>>,
    stopped: AtomicBool,
    #[cfg_attr(target_arch = "x86_64", allow(dead_code))]
    arch: Mutex<arch::VmState>,
}

impl Vm for KvmVm {
    type Vcpu<'a> = KvmVcpu<'a>;

    fn create_vcpu(&self, id: u32) -> io::Result<KvmVcpu<'_>> {
        let fd = self.fd.create_vcpu(id.into())?;
        arch::setup_vcpu(self, &fd, id)?;
        // SAFETY: pthread_self has no preconditions.
        self.threads
            .lock()
            .unwrap()
            .push(unsafe { libc::pthread_self() });
        Ok(KvmVcpu {
            vm: self,
            fd,
            buffer: [0; 8],
        })
    }

    fn set_irq(&self, line: u32, level: bool) -> io::Result<()> {
        self.fd.set_irq_line(arch::irq_number(line), level)?;
        Ok(())
    }

    fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        // A signal landing between the flag check and KVM_RUN is lost, so keep kicking until all
        // vCPUs are gone. Detached because the caller may itself be a vCPU thread.
        let threads = self.threads.clone();
        std::thread::spawn(move || {
            loop {
                let threads = threads.lock().unwrap();
                if threads.is_empty() {
                    return;
                }
                for &thread in threads.iter() {
                    // SAFETY: a vCPU removes its thread under this lock before the thread ends,
                    // so every id in the list is live.
                    unsafe { libc::pthread_kill(thread, KICK_SIGNAL) };
                }
                drop(threads);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        });
    }
}

pub struct KvmVcpu<'a> {
    vm: &'a KvmVm,
    fd: VcpuFd,
    buffer: [u8; 8],
}

impl Drop for KvmVcpu<'_> {
    fn drop(&mut self) {
        // SAFETY: pthread_self has no preconditions.
        let me = unsafe { libc::pthread_self() };
        self.vm
            .threads
            .lock()
            .unwrap()
            .retain(|&thread| thread != me);
    }
}

impl Vcpu for KvmVcpu<'_> {
    fn set_boot_state(&mut self, entry: u64, arg: u64) -> io::Result<()> {
        arch::set_boot_state(&self.fd, entry, arg)
    }

    fn run(&mut self) -> io::Result<Exit<'_>> {
        if self.vm.stopped.load(Ordering::SeqCst) {
            return Ok(Exit::Stopped);
        }
        match self.fd.run() {
            Ok(VcpuExit::MmioRead(address, data)) => Ok(Exit::Mmio {
                address,
                data,
                write: false,
            }),
            Ok(VcpuExit::MmioWrite(address, data)) => {
                let length = data.len().min(8);
                self.buffer[..length].copy_from_slice(&data[..length]);
                Ok(Exit::Mmio {
                    address,
                    data: &mut self.buffer[..length],
                    write: true,
                })
            }
            Ok(VcpuExit::IoIn(port, data)) => Ok(Exit::PortIo {
                port,
                data,
                write: false,
            }),
            Ok(VcpuExit::IoOut(port, data)) => {
                let length = data.len().min(8);
                self.buffer[..length].copy_from_slice(&data[..length]);
                Ok(Exit::PortIo {
                    port,
                    data: &mut self.buffer[..length],
                    write: true,
                })
            }
            Ok(VcpuExit::Hlt | VcpuExit::Intr) => Ok(Exit::Again),
            Ok(VcpuExit::Shutdown) => Ok(Exit::Shutdown),
            #[cfg(target_arch = "aarch64")]
            Ok(VcpuExit::SystemEvent(kind, _)) => Ok(match kind {
                kvm_bindings::KVM_SYSTEM_EVENT_RESET => Exit::Reset,
                _ => Exit::Shutdown,
            }),
            Ok(other) => Err(io::Error::other(format!("unexpected vCPU exit {other:?}"))),
            // A waiting vCPU returns EAGAIN when its INIT arrives, and any vCPU EINTR on a kick.
            Err(e) if matches!(e.errno(), libc::EAGAIN | libc::EINTR) => Ok(Exit::Again),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(target_arch = "x86_64")]
mod arch {
    use super::*;
    use kvm_bindings::{KVM_MAX_CPUID_ENTRIES, kvm_pit_config, kvm_regs};

    #[derive(Default)]
    pub struct VmState;

    pub fn irq_number(line: u32) -> u32 {
        line
    }

    pub fn setup_vm(fd: &VmFd) -> io::Result<()> {
        // Intel needs a three-page TSS region below 4 GiB, outside RAM and the MMIO windows.
        fd.set_tss_address(0xfffb_d000)?;
        fd.create_irq_chip()?;
        fd.create_pit2(kvm_pit_config::default())?;
        Ok(())
    }

    pub fn setup_vcpu(vm: &KvmVm, fd: &VcpuFd, id: u32) -> io::Result<()> {
        let mut cpuid = vm.kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)?;
        for entry in cpuid.as_mut_slice() {
            match entry.function {
                // The initial APIC ID the MP table announces, and the hypervisor bit.
                1 => {
                    entry.ebx = entry.ebx & 0x00ff_ffff | id << 24;
                    entry.ecx |= 1 << 31;
                }
                0xb | 0x1f => entry.edx = id,
                _ => (),
            }
        }
        fd.set_cpuid2(&cpuid)?;
        Ok(())
    }

    pub fn set_boot_state(fd: &VcpuFd, entry: u64, arg: u64) -> io::Result<()> {
        let mut sregs = fd.get_sregs()?;
        let code = kvm_bindings::kvm_segment {
            limit: 0xffff_ffff,
            selector: 0x10,
            type_: 0xb,
            present: 1,
            s: 1,
            l: 1,
            g: 1,
            ..Default::default()
        };
        let data = kvm_bindings::kvm_segment {
            selector: 0x18,
            type_: 0x3,
            db: 1,
            l: 0,
            ..code
        };
        sregs.cs = code;
        (sregs.ds, sregs.es, sregs.fs, sregs.gs, sregs.ss) = (data, data, data, data, data);
        sregs.gdt.base = crate::X86_GDT;
        sregs.gdt.limit = 31;
        sregs.cr3 = crate::X86_PML4;
        sregs.cr4 |= 0x20; // PAE
        sregs.cr0 |= 0x8000_0001; // PG | PE
        sregs.efer |= 0x500; // LMA | LME
        fd.set_sregs(&sregs)?;
        fd.set_regs(&kvm_regs {
            rip: entry,
            rsi: arg,
            rsp: 0x8ff0,
            rbp: 0x8ff0,
            rflags: 0x2,
            ..Default::default()
        })?;
        Ok(())
    }
}

#[cfg(target_arch = "aarch64")]
mod arch {
    use super::*;

    #[derive(Default)]
    pub struct VmState;

    pub fn irq_number(_: u32) -> u32 {
        unimplemented!("step 4")
    }
    pub fn setup_vm(_: &VmFd) -> io::Result<()> {
        Err(io::Error::other("step 4"))
    }
    pub fn setup_vcpu(_: &KvmVm, _: &VcpuFd, _: u32) -> io::Result<()> {
        Err(io::Error::other("step 4"))
    }
    pub fn set_boot_state(_: &VcpuFd, _: u64, _: u64) -> io::Result<()> {
        Err(io::Error::other("step 4"))
    }
}
