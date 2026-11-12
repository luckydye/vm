use crate::{
    machine::{Boot, GICD_START, GICR_START},
    memory::GuestMemory,
};
use kvm_bindings::*;
use kvm_ioctls::{DeviceFd, Kvm, VcpuFd, VmFd};
use std::io;

const GICD_SIZE: u64 = 0x1_0000;
const GICR_STRIDE: u64 = 0x2_0000;
/// SGIs and PPIs, then the SPIs the machine uses.
const GIC_IRQS: u32 = 32 + 64;
/// EL1h with all interrupts masked.
const PSTATE_EL1H_DAIF_MASKED: u64 = 0x3c5;

/// The GICv3, which must live as long as the VM.
pub struct State {
    _gic: DeviceFd,
}

/// The sizes of the GIC distributor and of the redistributors for `vcpus`.
pub fn gic_sizes(vcpus: usize) -> io::Result<(u64, u64)> {
    Ok((GICD_SIZE, GICR_STRIDE * vcpus as u64))
}

/// The MPIDR KVM gives vCPU `vcpu_id`, which the FDT names it by.
pub fn mpidr(vcpu_id: usize) -> u64 {
    let vcpu_id = vcpu_id as u64;
    (((vcpu_id >> 4) & 0xff) << 8) | (vcpu_id & 0xf)
}

pub fn irq_number(spi: u32) -> u32 {
    (KVM_ARM_IRQ_TYPE_SPI << KVM_ARM_IRQ_TYPE_SHIFT) | (32 + spi)
}

/// Creates the vCPUs with in-kernel PSCI, secondaries powered off until the
/// guest starts them, then the GICv3.
pub fn create(_kvm: &Kvm, vm: &VmFd, vcpus: usize) -> super::Result<(Vec<VcpuFd>, State)> {
    let mut init = kvm_vcpu_init::default();
    vm.get_preferred_target(&mut init)?;
    init.features[0] |= 1 << KVM_ARM_VCPU_PSCI_0_2;

    let fds = (0..vcpus)
        .map(|id| {
            let vcpu = vm.create_vcpu(id as u64)?;
            let mut init = init;
            if id != 0 {
                init.features[0] |= 1 << KVM_ARM_VCPU_POWER_OFF;
            }
            vcpu.vcpu_init(&init)?;
            Ok(vcpu)
        })
        .collect::<super::Result<Vec<_>>>()?;

    let mut device = kvm_create_device {
        type_: kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3,
        fd: 0,
        flags: 0,
    };
    let gic = vm.create_device(&mut device)?;
    let set = |group: u32, attr: u64, value: Option<&u64>| {
        gic.set_device_attr(&kvm_device_attr {
            flags: 0,
            group,
            attr,
            addr: value.map_or(0, |value| value as *const u64 as u64),
        })
    };
    set(
        KVM_DEV_ARM_VGIC_GRP_ADDR,
        KVM_VGIC_V3_ADDR_TYPE_DIST as u64,
        Some(&GICD_START),
    )?;
    set(
        KVM_DEV_ARM_VGIC_GRP_ADDR,
        KVM_VGIC_V3_ADDR_TYPE_REDIST as u64,
        Some(&GICR_START),
    )?;
    let irqs = GIC_IRQS as u64;
    set(KVM_DEV_ARM_VGIC_GRP_NR_IRQS, 0, Some(&irqs))?;
    set(KVM_DEV_ARM_VGIC_GRP_CTRL, KVM_DEV_ARM_VGIC_CTRL_INIT as u64, None)?;

    Ok((fds, State { _gic: gic }))
}

/// Points the boot vCPU at the kernel; PSCI starts the others.
pub fn configure(vcpu: &VcpuFd, id: usize, boot: &Boot, _mem: &GuestMemory) -> super::Result<()> {
    if id != 0 {
        return Ok(());
    }
    vcpu.set_one_reg(core_reg(PSTATE_OFFSET), &PSTATE_EL1H_DAIF_MASKED.to_le_bytes())?;
    vcpu.set_one_reg(core_reg(PC_OFFSET), &boot.entry.to_le_bytes())?;
    vcpu.set_one_reg(core_reg(0), &boot.arg.to_le_bytes())?;
    Ok(())
}

/// Offsets into `struct kvm_regs`: x0..x30, sp, pc, pstate.
const PC_OFFSET: u64 = 32 * 8;
const PSTATE_OFFSET: u64 = 33 * 8;

fn core_reg(offset: u64) -> u64 {
    KVM_REG_ARM64 | KVM_REG_SIZE_U64 | KVM_REG_ARM_CORE as u64 | (offset / 4)
}
