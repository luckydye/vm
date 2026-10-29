use crate::{
    machine::{BOOT_STACK, Boot, GDT_START, IDT_START, PDE_START, PDPTE_START, PML4_START},
    memory::GuestMemory,
};
use kvm_bindings::*;
use kvm_ioctls::{Kvm, VcpuFd, VmFd};

/// Three pages below 4 GiB, outside guest RAM, for the TSS VMX needs in real mode.
const TSS_ADDRESS: usize = 0xfffb_d000;

/// Null, 64-bit code, data and TSS descriptors.
const GDT: [u64; 4] = [0, 0x00af_9b00_0000_ffff, 0x00cf_9300_0000_ffff, 0x008f_8b00_0000_ffff];
const CODE_SELECTOR: u16 = 1 << 3;
const DATA_SELECTOR: u16 = 2 << 3;
const TSS_SELECTOR: u16 = 3 << 3;

const CR0_PE: u64 = 1;
const CR0_PG: u64 = 1 << 31;
const CR4_PAE: u64 = 1 << 5;
const EFER_LME: u64 = 1 << 8;
const EFER_LMA: u64 = 1 << 10;

const APIC_LVT0: usize = 0x350;
const APIC_LVT1: usize = 0x360;
const APIC_MODE_NMI: u32 = 4;
const APIC_MODE_EXTINT: u32 = 7;

pub struct State;

pub fn irq_number(pin: u32) -> u32 {
    pin
}

/// Creates the in-kernel PIC, IOAPIC and PIT, then the vCPUs with the host's
/// CPU features and their own APIC IDs. Secondaries wait for the boot CPU's
/// INIT and SIPI in the kernel.
pub fn create(kvm: &Kvm, vm: &VmFd, vcpus: usize) -> super::Result<(Vec<VcpuFd>, State)> {
    vm.set_tss_address(TSS_ADDRESS)?;
    vm.create_irq_chip()?;
    vm.create_pit2(kvm_pit_config {
        flags: KVM_PIT_SPEAKER_DUMMY,
        ..Default::default()
    })?;
    let supported = kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)?;

    let fds = (0..vcpus)
        .map(|id| {
            let vcpu = vm.create_vcpu(id as u64)?;
            let mut cpuid = supported.clone();
            for entry in cpuid.as_mut_slice() {
                match entry.function {
                    1 => entry.ebx = (entry.ebx & 0x00ff_ffff) | ((id as u32) << 24),
                    0xb | 0x1f => entry.edx = id as u32,
                    _ => {}
                }
            }
            vcpu.set_cpuid2(&cpuid)?;

            // LINT0 takes the PIC's interrupts and LINT1 NMIs, as firmware leaves them.
            let mut lapic = vcpu.get_lapic()?;
            for (offset, mode) in [(APIC_LVT0, APIC_MODE_EXTINT), (APIC_LVT1, APIC_MODE_NMI)] {
                let value = u32::from_le_bytes(std::array::from_fn(|index| lapic.regs[offset + index] as u8));
                let value = (value & !0x700) | (mode << 8);
                for (index, byte) in value.to_le_bytes().into_iter().enumerate() {
                    lapic.regs[offset + index] = byte as _;
                }
            }
            vcpu.set_lapic(&lapic)?;
            Ok(vcpu)
        })
        .collect::<super::Result<Vec<_>>>()?;

    Ok((fds, State))
}

/// Puts the boot vCPU in long mode at the kernel's 64-bit entry, with the
/// first GiB identity mapped.
pub fn configure(vcpu: &VcpuFd, id: usize, boot: &Boot, mem: &GuestMemory) -> super::Result<()> {
    if id != 0 {
        return Ok(());
    }
    let write = |addr: u64, value: u64| {
        mem.write(addr, &value.to_le_bytes())
            .expect("boot structures are outside guest RAM")
    };
    for (index, descriptor) in GDT.iter().enumerate() {
        write(GDT_START + index as u64 * 8, *descriptor);
    }
    write(IDT_START, 0);
    write(PML4_START, PDPTE_START | 0x3);
    write(PDPTE_START, PDE_START | 0x3);
    for index in 0..512 {
        // Present, writable 2 MiB pages.
        write(PDE_START + index * 8, (index << 21) | 0x83);
    }

    let mut sregs = vcpu.get_sregs()?;
    let segment = |selector: u16, type_: u8, s: u8, l: u8, db: u8| kvm_segment {
        base: 0,
        limit: 0xf_ffff,
        selector,
        type_,
        present: 1,
        dpl: 0,
        db,
        s,
        l,
        g: 1,
        avl: 0,
        unusable: 0,
        padding: 0,
    };
    sregs.cs = segment(CODE_SELECTOR, 0xb, 1, 1, 0);
    let data = segment(DATA_SELECTOR, 0x3, 1, 0, 1);
    sregs.ds = data;
    sregs.es = data;
    sregs.fs = data;
    sregs.gs = data;
    sregs.ss = data;
    sregs.tr = segment(TSS_SELECTOR, 0xb, 0, 0, 0);
    sregs.gdt.base = GDT_START;
    sregs.gdt.limit = (GDT.len() * 8 - 1) as u16;
    sregs.idt.base = IDT_START;
    sregs.idt.limit = 7;
    sregs.cr0 |= CR0_PE | CR0_PG;
    sregs.cr3 = PML4_START;
    sregs.cr4 |= CR4_PAE;
    sregs.efer |= EFER_LME | EFER_LMA;
    vcpu.set_sregs(&sregs)?;

    vcpu.set_fpu(&kvm_fpu {
        fcw: 0x37f,
        mxcsr: 0x1f80,
        ..Default::default()
    })?;

    vcpu.set_regs(&kvm_regs {
        rflags: 0x2,
        rip: boot.entry,
        rsp: BOOT_STACK,
        rbp: BOOT_STACK,
        rsi: boot.arg,
        ..Default::default()
    })
}
