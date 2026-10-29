//! A PC without PCI or legacy firmware: the kernel boots through its 64-bit
//! entry with a zero page, finds the CPUs and the IOAPIC in ACPI tables, and
//! its virtio-mmio devices on the command line. Powering off or resetting
//! through the ACPI sleep or reset register stops the VM.

use super::{Boot, checked_end};
use crate::memory::GuestMemory;
use std::io;

/// The kernel's console and early console on the 16550 at COM1.
pub const CONSOLE_ARGS: &str = "console=ttyS0 earlycon=uart8250,io,0x3f8 reboot=acpi pci=off";

pub const RAM_START: u64 = 0;
/// RAM above this moves above 4 GiB to leave room for devices, the IOAPIC and the local APIC.
const LOW_RAM_END: u64 = 0xc000_0000;
const HIGH_RAM_START: u64 = 0x1_0000_0000;

pub const UART_PORT: u16 = 0x3f8;
pub const UART_PORT_END: u16 = 0x400;
pub const UART_IRQ: u32 = 4;

/// The FADT's sleep control, sleep status and reset register.
pub const ACPI_PORT: u16 = 0x600;
pub const ACPI_SLEEP_ENABLE: u32 = 1 << 5;
pub const ACPI_RESET_VALUE: u32 = 1;
/// Unused while the FADT declares hardware-reduced ACPI, but the kernel reads it.
const ACPI_SCI_IRQ: u16 = 9;

const DEVICES_START: u64 = 0xd000_0000;

pub const VIRTBLK_START: u64 = DEVICES_START;
pub const VIRTBLK_SIZE: u64 = 0x1000;
pub const VIRTBLK_IRQ: u32 = 5;

pub const VIRTNET_START: u64 = DEVICES_START + 0x1000;
pub const VIRTNET_SIZE: u64 = 0x1000;
pub const VIRTNET_IRQ: u32 = 6;

pub const VIRTGPU_START: u64 = DEVICES_START + 0x2000;
pub const VIRTGPU_SIZE: u64 = 0x1000;
pub const VIRTGPU_IRQ: u32 = 7;

pub const VIRTINPUT_KEYBOARD_START: u64 = DEVICES_START + 0x3000;
pub const VIRTINPUT_KEYBOARD_SIZE: u64 = 0x1000;
pub const VIRTINPUT_KEYBOARD_IRQ: u32 = 10;

pub const VIRTINPUT_TABLET_START: u64 = DEVICES_START + 0x4000;
pub const VIRTINPUT_TABLET_SIZE: u64 = 0x1000;
pub const VIRTINPUT_TABLET_IRQ: u32 = 11;

pub const VIRTSND_START: u64 = DEVICES_START + 0x5000;
pub const VIRTSND_SIZE: u64 = 0x1000;
pub const VIRTSND_IRQ: u32 = 12;

pub const VIRTCONSOLE_START: u64 = DEVICES_START + 0x6000;
pub const VIRTCONSOLE_SIZE: u64 = 0x1000;
pub const VIRTCONSOLE_IRQ: u32 = 13;

pub const VIRTINPUT_MOUSE_START: u64 = DEVICES_START + 0x7000;
pub const VIRTINPUT_MOUSE_SIZE: u64 = 0x1000;
pub const VIRTINPUT_MOUSE_IRQ: u32 = 14;

/// virtio-fs shares occupy consecutive slots and IOAPIC pins from here.
pub const VIRTFS_START: u64 = DEVICES_START + 0x8000;
pub const VIRTFS_SIZE: u64 = 0x1000;
pub const VIRTFS_IRQ: u32 = 16;
pub const MAX_SHARES: usize = 8;

/// After the last virtio-fs slot.
pub const VIRTBALLOON_START: u64 = VIRTFS_START + MAX_SHARES as u64 * VIRTFS_SIZE;
pub const VIRTBALLOON_SIZE: u64 = 0x1000;
pub const VIRTBALLOON_IRQ: u32 = 15;

const IOAPIC_ADDR: u32 = 0xfec0_0000;
const LAPIC_ADDR: u32 = 0xfee0_0000;

/// Boot structures in low memory, where the kernel expects firmware to leave them.
pub const GDT_START: u64 = 0x500;
pub const IDT_START: u64 = 0x520;
const ZERO_PAGE_START: u64 = 0x7000;
pub const BOOT_STACK: u64 = 0x8ff0;
pub const PML4_START: u64 = 0x9000;
pub const PDPTE_START: u64 = 0xa000;
pub const PDE_START: u64 = 0xb000;
const EBDA_START: u64 = 0x9fc00;
const CMDLINE_START: u64 = 0x20000;
const ACPI_START: u64 = 0xe0000;
const KERNEL_START: u64 = 0x10_0000;

const E820_RAM: u32 = 1;
const E820_RESERVED: u32 = 2;

/// Guest RAM below the device hole, and the rest above 4 GiB.
pub fn ram_regions(memory_size: usize) -> Vec<(u64, usize)> {
    let low = memory_size.min(LOW_RAM_END as usize);
    let mut regions = vec![(RAM_START, low)];
    if memory_size > low {
        regions.push((HIGH_RAM_START, memory_size - low));
    }
    regions
}

/// Loads a bzImage, its initrd, command line, zero page and ACPI tables. The
/// boot CPU starts at the kernel's 64-bit entry with the zero page in rsi.
pub fn load(
    mem: &GuestMemory,
    memory_size: usize,
    vcpus: usize,
    kernel_args: &str,
    kernel: &[u8],
    initrd: &[u8],
    shares: usize,
) -> io::Result<Boot> {
    assert!(shares <= MAX_SHARES, "at most {MAX_SHARES} shares are supported");
    let invalid = |message: &str| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("kernel is not a bzImage: {message}"),
        )
    };

    if kernel.len() < 0x1000 || &kernel[0x202..0x206] != b"HdrS" {
        return Err(invalid("missing setup header"));
    }
    let version = u16::from_le_bytes([kernel[0x206], kernel[0x207]]);
    let xloadflags = u16::from_le_bytes([kernel[0x236], kernel[0x237]]);
    if version < 0x020c || xloadflags & 1 == 0 {
        return Err(invalid("no 64-bit entry point"));
    }
    let setup_sects = match kernel[0x1f1] {
        0 => 4,
        sects => sects as usize,
    };
    let setup_size = (setup_sects + 1) * 512;
    let initrd_addr_max = u32::from_le_bytes(kernel[0x22c..0x230].try_into().unwrap()) as u64;
    let cmdline_size = u32::from_le_bytes(kernel[0x238..0x23c].try_into().unwrap()) as usize;

    let mut cmdline = kernel_args.to_owned();
    for (base, irq) in virtio_devices(shares) {
        cmdline.push_str(&format!(" virtio_mmio.device=4K@{base:#x}:{irq}"));
    }
    if cmdline.len() >= cmdline_size {
        return Err(io::Error::other(format!(
            "kernel command line exceeds {cmdline_size} bytes"
        )));
    }
    let mut cmdline = cmdline.into_bytes();
    cmdline.push(0);

    let low_end = memory_size.min(LOW_RAM_END as usize) as u64;
    let initrd_start = (low_end.min(initrd_addr_max + 1) - initrd.len() as u64) & !0xfff;
    let kernel_end = checked_end(KERNEL_START, kernel.len() - setup_size, "kernel");
    if initrd_start < kernel_end {
        panic!("initrd at 0x{initrd_start:x} overlaps the kernel ending at 0x{kernel_end:x}");
    }

    mem.write(KERNEL_START, &kernel[setup_size..])?;
    mem.write(initrd_start, initrd)?;
    mem.write(CMDLINE_START, &cmdline)?;

    let acpi = acpi_tables(vcpus);
    mem.write(ACPI_START, &acpi)?;

    let mut zero_page = vec![0u8; 0x1000];
    let header_end = 0x202 + kernel[0x201] as usize;
    zero_page[0x1f1..header_end].copy_from_slice(&kernel[0x1f1..header_end]);
    zero_page[0x210] = 0xff; // type_of_loader: undefined
    zero_page[0x211] |= 0x80; // loadflags: CAN_USE_HEAP
    zero_page[0x218..0x21c].copy_from_slice(&(initrd_start as u32).to_le_bytes());
    zero_page[0x21c..0x220].copy_from_slice(&(initrd.len() as u32).to_le_bytes());
    zero_page[0x228..0x22c].copy_from_slice(&(CMDLINE_START as u32).to_le_bytes());
    zero_page[0x070..0x078].copy_from_slice(&ACPI_START.to_le_bytes()); // acpi_rsdp_addr

    let mut e820 = vec![
        (0, EBDA_START, E820_RAM),
        (EBDA_START, KERNEL_START - EBDA_START, E820_RESERVED),
        (KERNEL_START, low_end - KERNEL_START, E820_RAM),
    ];
    if memory_size as u64 > low_end {
        e820.push((HIGH_RAM_START, memory_size as u64 - low_end, E820_RAM));
    }
    zero_page[0x1e8] = e820.len() as u8;
    for (index, (addr, size, kind)) in e820.into_iter().enumerate() {
        let entry = 0x2d0 + index * 20;
        zero_page[entry..entry + 8].copy_from_slice(&addr.to_le_bytes());
        zero_page[entry + 8..entry + 16].copy_from_slice(&size.to_le_bytes());
        zero_page[entry + 16..entry + 20].copy_from_slice(&kind.to_le_bytes());
    }
    mem.write(ZERO_PAGE_START, &zero_page)?;

    Ok(Boot {
        entry: KERNEL_START + 0x200,
        arg: ZERO_PAGE_START,
    })
}

/// Base address and IOAPIC pin of each virtio-mmio device.
fn virtio_devices(shares: usize) -> Vec<(u64, u32)> {
    let mut devices = vec![
        (VIRTBLK_START, VIRTBLK_IRQ),
        (VIRTNET_START, VIRTNET_IRQ),
        (VIRTGPU_START, VIRTGPU_IRQ),
        (VIRTINPUT_KEYBOARD_START, VIRTINPUT_KEYBOARD_IRQ),
        (VIRTINPUT_TABLET_START, VIRTINPUT_TABLET_IRQ),
        (VIRTSND_START, VIRTSND_IRQ),
        (VIRTCONSOLE_START, VIRTCONSOLE_IRQ),
        (VIRTINPUT_MOUSE_START, VIRTINPUT_MOUSE_IRQ),
        (VIRTBALLOON_START, VIRTBALLOON_IRQ),
    ];
    for index in 0..shares {
        devices.push((VIRTFS_START + index as u64 * VIRTFS_SIZE, VIRTFS_IRQ + index as u32));
    }
    devices
}

/// RSDP, XSDT, FADT, DSDT and MADT, laid out from `ACPI_START`.
fn acpi_tables(vcpus: usize) -> Vec<u8> {
    const RSDP_LEN: usize = 36;
    const XSDT_LEN: usize = 36 + 2 * 8;
    const FADT_LEN: usize = 276;

    let xsdt_addr = ACPI_START + RSDP_LEN as u64;
    let fadt_addr = xsdt_addr + XSDT_LEN as u64;
    let dsdt_addr = fadt_addr + FADT_LEN as u64;

    // Name (_S5_, Package () { 5 }): the sleep type the kernel writes to power off.
    let dsdt = table(
        b"DSDT",
        2,
        &[0x08, b'_', b'S', b'5', b'_', 0x12, 0x04, 0x01, 0x0a, 0x05],
    );
    let madt_addr = dsdt_addr + dsdt.len() as u64;

    let mut fadt = vec![0u8; FADT_LEN - 36];
    let field = |fadt: &mut Vec<u8>, offset: usize, bytes: &[u8]| {
        fadt[offset - 36..offset - 36 + bytes.len()].copy_from_slice(bytes);
    };
    field(&mut fadt, 40, &(dsdt_addr as u32).to_le_bytes());
    field(&mut fadt, 46, &ACPI_SCI_IRQ.to_le_bytes());
    // IAPC_BOOT_ARCH: no VGA, no CMOS RTC, and no 8042 since its bit is clear.
    field(&mut fadt, 109, &((1u16 << 2) | (1 << 5)).to_le_bytes());
    // Flags: HW_REDUCED_ACPI and RESET_REG_SUP.
    field(&mut fadt, 112, &((1u32 << 20) | (1 << 10)).to_le_bytes());
    field(&mut fadt, 116, &io_port_register(ACPI_PORT));
    field(&mut fadt, 128, &[ACPI_RESET_VALUE as u8]);
    field(&mut fadt, 131, &[5]); // FADT minor version
    field(&mut fadt, 140, &dsdt_addr.to_le_bytes());
    field(&mut fadt, 244, &io_port_register(ACPI_PORT));
    field(&mut fadt, 256, &io_port_register(ACPI_PORT));
    field(&mut fadt, 268, b"VARMINT\0");
    let fadt = table(b"FACP", 6, &fadt);

    let mut madt = Vec::new();
    madt.extend_from_slice(&LAPIC_ADDR.to_le_bytes());
    madt.extend_from_slice(&1u32.to_le_bytes()); // PCAT_COMPAT: there are 8259 PICs
    for cpu in 0..vcpus {
        // Processor local APIC, enabled.
        madt.extend_from_slice(&[0, 8, cpu as u8, cpu as u8, 1, 0, 0, 0]);
    }
    // The IOAPIC, with its pins numbered from GSI 0.
    madt.extend_from_slice(&[1, 12, 0, 0]);
    madt.extend_from_slice(&IOAPIC_ADDR.to_le_bytes());
    madt.extend_from_slice(&0u32.to_le_bytes());
    let madt = table(b"APIC", 5, &madt);

    let mut xsdt = Vec::new();
    xsdt.extend_from_slice(&fadt_addr.to_le_bytes());
    xsdt.extend_from_slice(&madt_addr.to_le_bytes());
    let xsdt = table(b"XSDT", 1, &xsdt);

    let mut rsdp = vec![0u8; RSDP_LEN];
    rsdp[0..8].copy_from_slice(b"RSD PTR ");
    rsdp[9..15].copy_from_slice(b"VARMNT");
    rsdp[15] = 2;
    rsdp[20..24].copy_from_slice(&(RSDP_LEN as u32).to_le_bytes());
    rsdp[24..32].copy_from_slice(&xsdt_addr.to_le_bytes());
    rsdp[8] = checksum(&rsdp[..20]);
    rsdp[32] = checksum(&rsdp);

    assert_eq!(xsdt.len(), XSDT_LEN);
    assert_eq!(fadt.len(), FADT_LEN);
    [rsdp, xsdt, fadt, dsdt, madt].concat()
}

/// An ACPI table: the standard header, then `body`, summing to zero.
fn table(signature: &[u8; 4], revision: u8, body: &[u8]) -> Vec<u8> {
    let mut table = vec![0u8; 36];
    table[0..4].copy_from_slice(signature);
    table[4..8].copy_from_slice(&((36 + body.len()) as u32).to_le_bytes());
    table[8] = revision;
    table[10..16].copy_from_slice(b"VARMNT");
    table[16..24].copy_from_slice(b"VARMINT ");
    table[24..28].copy_from_slice(&1u32.to_le_bytes());
    table[28..32].copy_from_slice(b"VRMT");
    table[32..36].copy_from_slice(&1u32.to_le_bytes());
    table.extend_from_slice(body);
    table[9] = checksum(&table);
    table
}

/// A generic address structure for one byte in I/O space.
fn io_port_register(port: u16) -> [u8; 12] {
    let mut register = [0u8; 12];
    register[0] = 1; // system I/O
    register[1] = 8; // bit width
    register[3] = 1; // byte access
    register[4..12].copy_from_slice(&(port as u64).to_le_bytes());
    register
}

fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |sum, byte| sum.wrapping_sub(*byte))
}
