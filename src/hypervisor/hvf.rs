//! Apple Hypervisor.framework on arm64 Macs, with its in-kernel GICv3.

mod psci;
mod sysreg;

use crate::{
    devices::Devices,
    machine::{Boot, GICD_START, GICR_START},
    memory::{BLOCK, GuestMemory},
    suspend::Suspend,
};
use applevisor::{error::HypervisorError, prelude::*};
use applevisor_sys::{HV_MEMORY_EXEC, HV_MEMORY_READ, HV_MEMORY_WRITE, hv_vm_map, hv_vm_protect, hv_vm_unmap};
use std::{
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    thread,
};

/// The vCPUs order memory like x86 (`configure_vcpu`), which FEX in the guest relies on.
pub const KERNEL_ARGS: &str = "varmint.tso=1";

#[derive(Clone)]
pub struct Vm(VirtualMachineInstance<GicEnabled>);

pub struct IrqLine {
    vm: VirtualMachineInstance<GicEnabled>,
    intid: u32,
    level: bool,
}

fn io_error(error: HypervisorError) -> io::Error {
    io::Error::other(format!("{error:?}"))
}

fn check(result: i32, what: &str) -> io::Result<()> {
    if result != 0 {
        return Err(io::Error::other(format!("{what} failed: {result:#x}")));
    }
    Ok(())
}

/// The whole [`BLOCK`]s of `guest_addr..guest_addr + len`.
fn discard_blocks(guest_addr: u64, len: usize) -> (u64, u64) {
    let start = guest_addr.next_multiple_of(BLOCK as u64);
    (start, (guest_addr + len as u64) / BLOCK as u64 * BLOCK as u64)
}

/// Best effort: a memory object whose pages the guest has written to stays.
fn madvise(host_addr: *mut u8, len: u64) -> io::Result<()> {
    if unsafe { libc::madvise(host_addr.cast(), len as usize, libc::MADV_FREE_REUSABLE) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Puts a fresh, zeroed memory object behind the block at `addr`.
fn replace_block(addr: *mut u8, len: usize) {
    let block = unsafe {
        libc::mmap(
            addr.cast(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANON | libc::MAP_PRIVATE | libc::MAP_FIXED,
            -1,
            0,
        )
    };
    assert_eq!(block, addr.cast(), "mmap of a guest RAM block failed: {}", io::Error::last_os_error());
}

/// The sizes of the GIC distributor and of the redistributors for `vcpus`.
pub fn gic_sizes(vcpus: usize) -> io::Result<(u64, u64)> {
    let gicd_size = GicConfig::get_distributor_size().map_err(io_error)? as u64;
    let gicr_region_size = GicConfig::get_redistributor_region_size().map_err(io_error)? as u64;
    let gicr_stride = GicConfig::get_redistributor_size().map_err(io_error)? as u64;
    assert!(gicr_stride != 0);

    let max_vcpus = gicr_region_size / gicr_stride;
    if vcpus as u64 > max_vcpus {
        panic!("vcpus={vcpus} exceeds the Hypervisor.framework GIC limit of {max_vcpus}");
    }
    Ok((gicd_size, gicr_stride * vcpus as u64))
}

impl Vm {
    pub fn new(vcpus: usize) -> io::Result<Self> {
        gic_sizes(vcpus)?;
        let mut gic_config = GicConfig::new();
        gic_config.set_distributor_base(GICD_START).map_err(io_error)?;
        gic_config.set_redistributor_base(GICR_START).map_err(io_error)?;

        let mut vm_cfg = VirtualMachineConfig::new();
        vm_cfg
            .set_ipa_granule(IpaGranule::HV_IPA_GRANULE_4KB)
            .map_err(io_error)?;
        Ok(Self(VirtualMachine::with_gic(vm_cfg, gic_config).map_err(io_error)?))
    }

    /// The level-triggered line of shared peripheral interrupt `irq`.
    pub fn irq_line(&self, irq: u32) -> IrqLine {
        let (spi_int_start, _) = GicConfig::get_spi_interrupt_range().expect("failed to read the GIC SPI range");
        IrqLine {
            vm: self.0.clone(),
            intid: spi_int_start + irq,
            level: false,
        }
    }

    /// Maps `size` bytes of host memory at `guest_addr`; both must be page aligned.
    pub fn map(&self, host_addr: *mut u8, guest_addr: u64, size: usize, executable: bool) -> io::Result<()> {
        let flags = HV_MEMORY_READ | HV_MEMORY_WRITE | if executable { HV_MEMORY_EXEC } else { 0 };
        check(
            unsafe { hv_vm_map(host_addr.cast(), guest_addr, size, flags) },
            "hv_vm_map",
        )
    }

    pub fn unmap(&self, guest_addr: u64, size: usize) -> io::Result<()> {
        check(unsafe { hv_vm_unmap(guest_addr, size) }, "hv_vm_unmap")
    }

    /// Releases the host pages behind page-aligned guest RAM. `madvise` keeps
    /// the pages of a memory object that the guest has written to, so the whole
    /// blocks of the range are given back by replacing their memory; the rest
    /// is only `madvise`d, which is best effort.
    pub fn discard(&self, host_addr: *mut u8, guest_addr: u64, len: usize) -> io::Result<()> {
        let (blocks_start, blocks_end) = discard_blocks(guest_addr, len);
        if blocks_end < blocks_start {
            return madvise(host_addr, len as u64);
        }
        let blocks_len = (blocks_end - blocks_start) as usize;
        if blocks_len > 0 {
            let blocks_host = unsafe { host_addr.add((blocks_start - guest_addr) as usize) };
            self.unmap(blocks_start, blocks_len)?;
            for offset in (0..blocks_len).step_by(BLOCK) {
                // XNU keeps the pages of a block whose mapping fails, so leaving
                // it as it was only costs memory.
                replace_block(unsafe { blocks_host.add(offset) }, BLOCK.min(blocks_len - offset));
            }
            self.map(blocks_host, blocks_start, blocks_len, true)?;
        }
        let released = |from: u64, to: u64| -> io::Result<()> {
            if from < to {
                madvise(unsafe { host_addr.add((from - guest_addr) as usize) }, to - from)?;
            }
            Ok(())
        };
        released(guest_addr, blocks_start)?;
        released(blocks_end, guest_addr + len as u64)?;
        Ok(())
    }

    /// Runs the vCPUs until the boot CPU stops.
    pub fn run(&self, boot: Boot, vcpus: usize, mem: &GuestMemory, devices: &Devices, suspend: &Suspend) -> io::Result<()> {
        CpuRuntime::new(&self.0, boot.entry, boot.arg, vcpus, suspend)
            .and_then(|cpus| cpus.run(&self.0, mem, devices, suspend))
            .map_err(io_error)
    }
}

impl IrqLine {
    pub fn set(&mut self, level: bool) {
        if level == self.level {
            return;
        }

        self.vm.gic_set_spi(self.intid, level).unwrap();
        self.level = level;
    }
}

const BOOT_VCPU_ID: usize = 0;
const FIRST_SECONDARY_VCPU_ID: usize = 1;
const PSTATE_EL1H_DAIF_MASKED: u64 = 0x3c5;
const ESR_EC_HVC_AARCH64: u64 = 0x16;

#[derive(Debug, Clone, Copy)]
struct SecondaryStart {
    entry_point: u64,
    context_id: u64,
}

enum StartResult {
    Started,
    InvalidCpu,
    AlreadyOn,
    Unavailable,
}

struct SecondaryCpu {
    boot_tx: SyncSender<SecondaryStart>,
    online: AtomicBool,
}

struct SecondaryCpus {
    cpus: Vec<SecondaryCpu>,
}

struct CpuRuntime {
    boot_vcpu: Vcpu,
    secondaries: SecondaryCpus,
    secondary_workers: Vec<SecondaryWorker>,
}

struct SecondaryWorker {
    vcpu_id: usize,
    boot_rx: Receiver<SecondaryStart>,
}

impl CpuRuntime {
    fn new(
        vm: &VirtualMachineInstance<GicEnabled>,
        entry_point: u64,
        x0: u64,
        vcpus: usize,
        suspend: &Suspend,
    ) -> Result<Self> {
        let boot_vcpu = vm.vcpu_create()?;
        configure_vcpu(vm, &boot_vcpu, BOOT_VCPU_ID, suspend)?;
        set_entry_state(&boot_vcpu, entry_point, x0)?;

        let (secondaries, secondary_workers) = SecondaryCpus::new(vcpus);

        Ok(Self {
            boot_vcpu,
            secondaries,
            secondary_workers,
        })
    }

    fn run(
        self,
        vm: &VirtualMachineInstance<GicEnabled>,
        mem: &GuestMemory,
        devices: &Devices,
        suspend: &Suspend,
    ) -> Result<()> {
        let Self {
            boot_vcpu,
            secondaries,
            secondary_workers,
        } = self;

        thread::scope(|scope| -> Result<()> {
            for worker in secondary_workers {
                let secondaries = &secondaries;
                scope.spawn(move || worker.run(vm, mem, devices, secondaries, suspend).unwrap());
            }

            run_loop(&boot_vcpu, BOOT_VCPU_ID, mem, devices, &secondaries, suspend)
        })
    }
}

impl SecondaryCpus {
    fn new(vcpus: usize) -> (Self, Vec<SecondaryWorker>) {
        let mut cpus = Vec::new();
        let mut workers = Vec::new();

        for vcpu_id in FIRST_SECONDARY_VCPU_ID..vcpus {
            let (boot_tx, boot_rx) = sync_channel(1);
            cpus.push(SecondaryCpu {
                boot_tx,
                online: AtomicBool::new(false),
            });
            workers.push(SecondaryWorker { vcpu_id, boot_rx });
        }

        (Self { cpus }, workers)
    }

    fn start(&self, mpidr: u64, entry_point: u64, context_id: u64) -> StartResult {
        let Some(cpu) = self.cpu_for_mpidr(mpidr) else {
            return StartResult::InvalidCpu;
        };

        if cpu.online.swap(true, Ordering::SeqCst) {
            return StartResult::AlreadyOn;
        }

        if cpu
            .boot_tx
            .send(SecondaryStart {
                entry_point,
                context_id,
            })
            .is_err()
        {
            cpu.online.store(false, Ordering::SeqCst);
            return StartResult::Unavailable;
        }

        StartResult::Started
    }

    fn stop(&self, vcpu_id: usize) -> bool {
        let Some(cpu) = vcpu_id
            .checked_sub(FIRST_SECONDARY_VCPU_ID)
            .and_then(|index| self.cpus.get(index))
        else {
            return false;
        };

        cpu.online.store(false, Ordering::SeqCst);
        true
    }

    fn cpu_for_mpidr(&self, mpidr: u64) -> Option<&SecondaryCpu> {
        let vcpu_id = (mpidr & 0xff) as usize;
        vcpu_id
            .checked_sub(FIRST_SECONDARY_VCPU_ID)
            .and_then(|index| self.cpus.get(index))
    }
}

impl SecondaryWorker {
    fn run(
        self,
        vm: &VirtualMachineInstance<GicEnabled>,
        mem: &GuestMemory,
        devices: &Devices,
        secondaries: &SecondaryCpus,
        suspend: &Suspend,
    ) -> Result<()> {
        let vcpu = vm.vcpu_create()?;
        configure_vcpu(vm, &vcpu, self.vcpu_id, suspend)?;
        while let Ok(start) = self.boot_rx.recv() {
            set_entry_state(&vcpu, start.entry_point, start.context_id)?;
            run_loop(&vcpu, self.vcpu_id, mem, devices, secondaries, suspend)?;
        }

        Ok(())
    }
}

fn configure_vcpu(vm: &VirtualMachineInstance<GicEnabled>, vcpu: &Vcpu, vcpu_id: usize, suspend: &Suspend) -> Result<()> {
    vcpu.set_sys_reg(SysReg::ACTLR_EL1, 1 << 1)?; // enable TSO
    vcpu.set_sys_reg(SysReg::MPIDR_EL1, mpidr(vcpu_id))?;
    // An exit requested outside `hv_vcpu_run` makes the next run return at once.
    let (vm, handle) = (vm.clone(), vcpu.get_handle());
    suspend.add_vcpu(Box::new(move || {
        vm.vcpus_exit(std::slice::from_ref(&handle))
            .unwrap_or_else(|error| panic!("hv_vcpus_exit failed: {error:?}"))
    }));
    Ok(())
}

fn set_entry_state(vcpu: &Vcpu, entry_point: u64, x0: u64) -> Result<()> {
    vcpu.set_reg(Reg::CPSR, PSTATE_EL1H_DAIF_MASKED)?; // start in EL1
    vcpu.set_reg(Reg::PC, entry_point)?;
    vcpu.set_reg(Reg::X0, x0)?;
    vcpu.set_reg(Reg::X1, 0)?;
    vcpu.set_reg(Reg::X2, 0)?;
    vcpu.set_reg(Reg::X3, 0)?;
    Ok(())
}

/// The MPIDR of vCPU `vcpu_id`, which the FDT names it by.
pub fn mpidr(vcpu_id: usize) -> u64 {
    vcpu_id as u64
}

fn reg_from_rt(rt: u64) -> Option<Reg> {
    match rt {
        0 => Some(Reg::X0),
        1 => Some(Reg::X1),
        2 => Some(Reg::X2),
        3 => Some(Reg::X3),
        4 => Some(Reg::X4),
        5 => Some(Reg::X5),
        6 => Some(Reg::X6),
        7 => Some(Reg::X7),
        8 => Some(Reg::X8),
        9 => Some(Reg::X9),
        10 => Some(Reg::X10),
        11 => Some(Reg::X11),
        12 => Some(Reg::X12),
        13 => Some(Reg::X13),
        14 => Some(Reg::X14),
        15 => Some(Reg::X15),
        16 => Some(Reg::X16),
        17 => Some(Reg::X17),
        18 => Some(Reg::X18),
        19 => Some(Reg::X19),
        20 => Some(Reg::X20),
        21 => Some(Reg::X21),
        22 => Some(Reg::X22),
        23 => Some(Reg::X23),
        24 => Some(Reg::X24),
        25 => Some(Reg::X25),
        26 => Some(Reg::X26),
        27 => Some(Reg::X27),
        28 => Some(Reg::X28),
        29 => Some(Reg::X29),
        30 => Some(Reg::X30),
        31 => None,
        _ => None,
    }
}

fn read_rt(vcpu: &Vcpu, rt: u64) -> Result<u64> {
    Ok(match reg_from_rt(rt) {
        Some(reg) => vcpu.get_reg(reg)?,
        None => 0,
    })
}

fn write_rt(vcpu: &Vcpu, rt: u64, value: u64) -> Result<()> {
    if let Some(reg) = reg_from_rt(rt) {
        vcpu.set_reg(reg, value)?;
    }
    Ok(())
}

fn run_loop(
    vcpu: &Vcpu,
    vcpu_id: usize,
    mem: &GuestMemory,
    devices: &Devices,
    secondaries: &SecondaryCpus,
    suspend: &Suspend,
) -> Result<()> {
    loop {
        suspend.wait_awake();
        vcpu.run()?;
        let exit = vcpu.get_exit_info();

        match exit.reason {
            ExitReason::EXCEPTION => {
                let esr = exit.exception.syndrome;
                let phys_addr = exit.exception.physical_address;
                let pc = vcpu.get_reg(Reg::PC)?;
                let ec = (esr >> 26) & 0b111111;

                match ec {
                    ESR_EC_HVC_AARCH64 => match psci::handle_hvc(vcpu, vcpu_id, secondaries)? {
                        psci::Action::Continue => continue,
                        psci::Action::CpuOff => return Ok(()),
                    },

                    0x18 => {
                        if sysreg::handle_trap(vcpu, esr, pc)? {
                            continue;
                        }

                        let (sysreg, rt, is_read) = sysreg::decode(esr);
                        panic!(
                            "unhandled sysreg trap: {:?}, rt={}, {}, esr=0x{:x}, pc=0x{:x}",
                            sysreg,
                            rt,
                            if is_read { "read/MRS" } else { "write/MSR" },
                            esr,
                            pc
                        );
                    }

                    0x24 | 0x25 => {
                        let is_write = ((esr >> 6) & 1) == 1;
                        let rt = (esr >> 16) & 0b11111;
                        let size = 1usize << ((esr >> 22) & 0b11);

                        let value = if is_write { read_rt(vcpu, rt)? as u32 as u64 } else { 0 };
                        let Ok(read_value) = devices.handle_mmio(phys_addr, is_write, size, value, mem) else {
                            panic!(
                                "unhandled data abort trap: ec={}, rt={}, {}, esr=0x{:x}, pc=0x{:x}, addr=0x{:x}",
                                ec,
                                rt,
                                if is_write { "write" } else { "read" },
                                esr,
                                pc,
                                phys_addr,
                            );
                        };

                        if let Some(read_value) = read_value {
                            write_rt(vcpu, rt, read_value)?;
                        }

                        vcpu.set_reg(Reg::PC, pc + 4)?;
                    }

                    0x20 | 0x21 => {
                        panic!(
                            "instruction abort: fault=0x{:x}, esr=0x{:x}, pc=0x{:x}",
                            exit.exception.physical_address, esr, pc
                        );
                    }

                    _ => {
                        panic!("unexpected exception ec=0x{:x}, esr=0x{:x}, pc=0x{:x}", ec, esr, pc);
                    }
                }
            }
            ExitReason::CANCELED => (),
            _ => eprintln!("unexpected exit reason: {:?}", exit),
        }
    }
}
