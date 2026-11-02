//! Hypervisor.framework on Apple silicon. PSCI lives here because only this backend sees it: KVM
//! answers PSCI in the kernel.
use crate::{
    Exit, GIC_DISTRIBUTOR, GIC_REDISTRIBUTOR, GuestMemory, Hypervisor, Unavailable, Vcpu, Vm,
};
use applevisor::{
    gic::GicConfig,
    memory::MemPerms,
    vcpu::{ExitReason, Reg, SysReg, VcpuHandle},
    vm::{GicEnabled, VirtualMachine, VirtualMachineConfig, VirtualMachineInstance},
};
use std::{
    io,
    sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use vm_memory::{GuestMemoryBackend, GuestMemoryRegion};

const EC_WFX: u64 = 0x01;
const EC_HVC: u64 = 0x16;
const EC_SMC: u64 = 0x17;
const EC_SYSREG: u64 = 0x18;
const EC_DATA_ABORT: u64 = 0x24;
const FIRST_SPI: u32 = 32;
/// PSTATE at kernel entry: EL1h with all exceptions masked.
const BOOT_CPSR: u64 = 0x3c5;
// shortcut: WFI polls at 1 ms because IPIs bypass the VMM and cannot wake a parked vCPU, raise it or trap SGIs if idle CPU matters.
const WFI_POLL: Duration = Duration::from_millis(1);

const PSCI_VERSION: u64 = 0x8400_0000;
const PSCI_CPU_OFF: u64 = 0x8400_0002;
const PSCI_CPU_ON: u64 = 0x8400_0003;
const PSCI_CPU_ON_64: u64 = 0xc400_0003;
const PSCI_AFFINITY_INFO: u64 = 0x8400_0004;
const PSCI_AFFINITY_INFO_64: u64 = 0xc400_0004;
const PSCI_MIGRATE_INFO_TYPE: u64 = 0x8400_0006;
const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;
const PSCI_SYSTEM_RESET: u64 = 0x8400_0009;
const PSCI_FEATURES: u64 = 0x8400_000a;
const PSCI_NOT_SUPPORTED: u64 = -1i64 as u64;
const PSCI_INVALID: u64 = -2i64 as u64;
const PSCI_ALREADY_ON: u64 = -4i64 as u64;

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

    fn create_vm(memory: &GuestMemory, vcpus: u32) -> io::Result<HvfVm> {
        let mut gic = GicConfig::new();
        gic.set_distributor_base(GIC_DISTRIBUTOR).map_err(other)?;
        gic.set_redistributor_base(GIC_REDISTRIBUTOR)
            .map_err(other)?;
        let vm = VirtualMachine::with_gic(VirtualMachineConfig::new(), gic).map_err(other)?;
        for region in memory.iter() {
            let host = region.as_ptr();
            let flags = u64::from(MemPerms::ReadWriteExec);
            // SAFETY: the mapping belongs to `memory`, whose clones the machine keeps for the
            // VM's whole life, and it covers exactly the region's length.
            let status = unsafe {
                applevisor_sys::hv_vm_map(
                    host.cast(),
                    region.start_addr().0,
                    region.len() as usize,
                    flags,
                )
            };
            if status != 0 {
                return Err(io::Error::other(format!("hv_vm_map: {status:#x}")));
            }
        }
        let power = (0..vcpus)
            .map(|id| Power {
                on: id == 0,
                start: None,
            })
            .collect();
        Ok(HvfVm {
            vm,
            handles: Mutex::new(Vec::new()),
            stopped: AtomicBool::new(false),
            power: Mutex::new(power),
            changed: Condvar::new(),
        })
    }
}

fn other(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

struct Power {
    on: bool,
    /// Entry and context of a CPU_ON not yet applied by the target's thread.
    start: Option<(u64, u64)>,
}

pub struct HvfVm {
    vm: VirtualMachineInstance<GicEnabled>,
    handles: Mutex<Vec<VcpuHandle>>,
    stopped: AtomicBool,
    power: Mutex<Vec<Power>>,
    /// Wakes vCPUs parked in CPU_OFF or WFI.
    changed: Condvar,
}

impl Vm for HvfVm {
    type Vcpu<'a> = HvfVcpu<'a>;

    fn create_vcpu(&self, id: u32) -> io::Result<HvfVcpu<'_>> {
        let vcpu = self.vm.vcpu_create().map_err(other)?;
        // GICv3 routes by affinity, so each redistributor needs its vCPU's MPIDR.
        vcpu.set_sys_reg(SysReg::MPIDR_EL1, id.into())
            .map_err(other)?;
        vcpu.set_trap_debug_reg_accesses(false).map_err(other)?;
        self.handles.lock().unwrap().push(vcpu.get_handle());
        Ok(HvfVcpu {
            vm: self,
            vcpu,
            id: id as usize,
            pending_read: None,
            buffer: [0; 8],
        })
    }

    fn set_irq(&self, line: u32, level: bool) -> io::Result<()> {
        self.vm
            .gic_set_spi(FIRST_SPI + line, level)
            .map_err(other)?;
        self.changed.notify_all();
        Ok(())
    }

    fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        let _guard = self.power.lock().unwrap();
        self.changed.notify_all();
        let _ = self.vm.vcpus_exit(&self.handles.lock().unwrap());
    }
}

pub struct HvfVcpu<'a> {
    vm: &'a HvfVm,
    vcpu: applevisor::vcpu::Vcpu,
    id: usize,
    /// Register and width awaiting the data of an MMIO read.
    pending_read: Option<(u8, usize)>,
    buffer: [u8; 8],
}

impl Vcpu for HvfVcpu<'_> {
    fn set_boot_state(&mut self, entry: u64, arg: u64) -> io::Result<()> {
        self.enter(entry, arg)
    }

    fn run(&mut self) -> io::Result<Exit<'_>> {
        if let Some((register, length)) = self.pending_read.take() {
            let mut value = [0; 8];
            value[..length].copy_from_slice(&self.buffer[..length]);
            self.set_x(register, u64::from_le_bytes(value))?;
            self.advance()?;
        }
        self.wait_until_on()?;
        if self.vm.stopped.load(Ordering::SeqCst) {
            return Ok(Exit::Stopped);
        }
        self.vcpu.run().map_err(other)?;
        let exit = self.vcpu.get_exit_info();
        match exit.reason {
            ExitReason::CANCELED => Ok(Exit::Again),
            ExitReason::VTIMER_ACTIVATED => {
                Err(io::Error::other("vtimer exit with an in-kernel GIC"))
            }
            ExitReason::EXCEPTION => {
                self.exception(exit.exception.syndrome, exit.exception.physical_address)
            }
            ExitReason::UNKNOWN => Err(io::Error::other("unknown vCPU exit")),
        }
    }
}

impl HvfVcpu<'_> {
    fn enter(&self, entry: u64, arg: u64) -> io::Result<()> {
        self.vcpu.set_reg(Reg::PC, entry).map_err(other)?;
        self.vcpu.set_reg(Reg::X0, arg).map_err(other)?;
        self.vcpu.set_reg(Reg::CPSR, BOOT_CPSR).map_err(other)
    }

    fn advance(&self) -> io::Result<()> {
        let pc = self.vcpu.get_reg(Reg::PC).map_err(other)?;
        self.vcpu.set_reg(Reg::PC, pc + 4).map_err(other)
    }

    fn x(&self, register: u8) -> io::Result<u64> {
        if register == 31 {
            return Ok(0); // XZR
        }
        self.vcpu.get_reg(gpr(register)).map_err(other)
    }

    fn set_x(&self, register: u8, value: u64) -> io::Result<()> {
        if register == 31 {
            return Ok(());
        }
        self.vcpu.set_reg(gpr(register), value).map_err(other)
    }

    /// Parks while powered off, and applies a CPU_ON on this vCPU's own thread as the framework
    /// requires.
    fn wait_until_on(&self) -> io::Result<()> {
        let mut power = self.vm.power.lock().unwrap();
        while !power[self.id].on && !self.vm.stopped.load(Ordering::SeqCst) {
            power = self.vm.changed.wait(power).unwrap();
        }
        if let Some((entry, context)) = power[self.id].start.take() {
            self.enter(entry, context)?;
        }
        Ok(())
    }

    fn exception(&mut self, syndrome: u64, address: u64) -> io::Result<Exit<'_>> {
        let class = syndrome >> 26;
        match class {
            EC_DATA_ABORT => self.data_abort(syndrome, address),
            EC_WFX => {
                self.advance()?;
                let power = self.vm.power.lock().unwrap();
                let _ = self.vm.changed.wait_timeout(power, WFI_POLL).unwrap();
                Ok(Exit::Again)
            }
            EC_HVC | EC_SMC => {
                // HVC reports the next instruction as PC, SMC the trapping one.
                if class == EC_SMC {
                    self.advance()?;
                }
                self.psci()
            }
            EC_SYSREG => {
                // Debug and implementation registers the guest probes read as zero, ignore writes.
                if syndrome & 1 == 1 {
                    self.set_x(((syndrome >> 5) & 0x1f) as u8, 0)?;
                }
                self.advance()?;
                Ok(Exit::Again)
            }
            _ => {
                let pc = self.vcpu.get_reg(Reg::PC).map_err(other)?;
                Err(io::Error::other(format!(
                    "unhandled guest exception: syndrome {syndrome:#x} at pc {pc:#x}"
                )))
            }
        }
    }

    fn data_abort(&mut self, syndrome: u64, address: u64) -> io::Result<Exit<'_>> {
        // Without a valid syndrome the access would have to be decoded from the instruction, and
        // Linux's MMIO accessors never produce one.
        if syndrome & (1 << 24) == 0 {
            return Err(io::Error::other(format!(
                "MMIO at {address:#x} without a valid syndrome: {syndrome:#x}"
            )));
        }
        let length = 1usize << ((syndrome >> 22) & 3);
        let register = ((syndrome >> 16) & 0x1f) as u8;
        let write = syndrome & (1 << 6) != 0;
        if write {
            let value = self.x(register)?;
            self.buffer = value.to_le_bytes();
            self.advance()?;
        } else {
            self.buffer = [0; 8];
            self.pending_read = Some((register, length));
        }
        Ok(Exit::Mmio {
            address,
            data: &mut self.buffer[..length],
            write,
        })
    }

    fn psci(&mut self) -> io::Result<Exit<'_>> {
        let function = self.x(0)?;
        let result = match function {
            PSCI_VERSION => 0x0001_0000, // 1.0
            PSCI_MIGRATE_INFO_TYPE => 2, // no migration needed
            PSCI_FEATURES => match self.x(1)? {
                PSCI_VERSION
                | PSCI_CPU_OFF
                | PSCI_CPU_ON
                | PSCI_CPU_ON_64
                | PSCI_AFFINITY_INFO
                | PSCI_AFFINITY_INFO_64
                | PSCI_SYSTEM_OFF
                | PSCI_SYSTEM_RESET
                | PSCI_FEATURES
                | PSCI_MIGRATE_INFO_TYPE => 0,
                _ => PSCI_NOT_SUPPORTED,
            },
            PSCI_CPU_ON | PSCI_CPU_ON_64 => {
                let (target, entry, context) = (self.x(1)?, self.x(2)?, self.x(3)?);
                let mut power = self.vm.power.lock().unwrap();
                let result = match power.get_mut(target as usize) {
                    None => PSCI_INVALID,
                    Some(cpu) if cpu.on => PSCI_ALREADY_ON,
                    Some(cpu) => {
                        cpu.on = true;
                        cpu.start = Some((entry, context));
                        0
                    }
                };
                self.vm.changed.notify_all();
                result
            }
            PSCI_CPU_OFF => {
                self.vm.power.lock().unwrap()[self.id].on = false;
                return Ok(Exit::Again);
            }
            PSCI_AFFINITY_INFO | PSCI_AFFINITY_INFO_64 => {
                let target = self.x(1)? as usize;
                match self.vm.power.lock().unwrap().get(target) {
                    None => PSCI_INVALID,
                    Some(cpu) => u64::from(!cpu.on),
                }
            }
            PSCI_SYSTEM_OFF => return Ok(Exit::Shutdown),
            PSCI_SYSTEM_RESET => return Ok(Exit::Reset),
            _ => PSCI_NOT_SUPPORTED,
        };
        self.set_x(0, result)?;
        Ok(Exit::Again)
    }
}

fn gpr(register: u8) -> Reg {
    const ALL: [Reg; 31] = [
        Reg::X0,
        Reg::X1,
        Reg::X2,
        Reg::X3,
        Reg::X4,
        Reg::X5,
        Reg::X6,
        Reg::X7,
        Reg::X8,
        Reg::X9,
        Reg::X10,
        Reg::X11,
        Reg::X12,
        Reg::X13,
        Reg::X14,
        Reg::X15,
        Reg::X16,
        Reg::X17,
        Reg::X18,
        Reg::X19,
        Reg::X20,
        Reg::X21,
        Reg::X22,
        Reg::X23,
        Reg::X24,
        Reg::X25,
        Reg::X26,
        Reg::X27,
        Reg::X28,
        Reg::FP,
        Reg::LR,
    ];
    ALL[register as usize]
}
