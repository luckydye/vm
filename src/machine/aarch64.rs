use super::{Boot, checked_end};
use crate::{hypervisor, memory::GuestMemory};
use std::{
    fs::File,
    io::{self, Read},
};
use vm_fdt::{FdtWriter, FdtWriterResult};

/// The kernel's console and early console on the PL011.
pub const CONSOLE_ARGS: &str = "console=ttyAMA0 earlycon=pl011,mmio32,0x09000000";

pub const RAM_START: u64 = 0x40000000;
pub const KERNEL_TEXT_OFFSET: u64 = 0x0;
pub const IMAGE_START: u64 = RAM_START + KERNEL_TEXT_OFFSET;
pub const INITRD_START: u64 = 0x48000000;
pub const DTB_START: u64 = 0x4F000000;
pub const GICD_START: u64 = 0x08000000;
pub const GICR_START: u64 = 0x080A0000;
pub const UART_START: u64 = 0x09000000;
pub const UART_SIZE: u64 = 0x1000;
pub const UART_IRQ: u32 = 1;

pub const VIRTBLK_START: u64 = 0x0a000000;
pub const VIRTBLK_SIZE: u64 = 0x1000;
pub const VIRTBLK_IRQ: u32 = 32;

pub const VIRTNET_START: u64 = 0x0a001000;
pub const VIRTNET_SIZE: u64 = 0x1000;
pub const VIRTNET_IRQ: u32 = 33;

pub const VIRTGPU_START: u64 = 0x0a002000;
pub const VIRTGPU_SIZE: u64 = 0x1000;
pub const VIRTGPU_IRQ: u32 = 34;

pub const VIRTINPUT_KEYBOARD_START: u64 = 0x0a003000;
pub const VIRTINPUT_KEYBOARD_SIZE: u64 = 0x1000;
pub const VIRTINPUT_KEYBOARD_IRQ: u32 = 35;

pub const VIRTINPUT_TABLET_START: u64 = 0x0a004000;
pub const VIRTINPUT_TABLET_SIZE: u64 = 0x1000;
pub const VIRTINPUT_TABLET_IRQ: u32 = 36;

pub const VIRTSND_START: u64 = 0x0a005000;
pub const VIRTSND_SIZE: u64 = 0x1000;
pub const VIRTSND_IRQ: u32 = 37;

pub const VIRTCONSOLE_START: u64 = 0x0a006000;
pub const VIRTCONSOLE_SIZE: u64 = 0x1000;
pub const VIRTCONSOLE_IRQ: u32 = 38;

pub const VIRTINPUT_MOUSE_START: u64 = 0x0a007000;
pub const VIRTINPUT_MOUSE_SIZE: u64 = 0x1000;
pub const VIRTINPUT_MOUSE_IRQ: u32 = 39;

/// virtio-fs shares occupy consecutive slots from here.
pub const VIRTFS_START: u64 = 0x0a008000;
pub const VIRTFS_SIZE: u64 = 0x1000;
pub const VIRTFS_IRQ: u32 = 40;
pub const MAX_SHARES: usize = 8;

/// After the last virtio-fs slot.
pub const VIRTBALLOON_START: u64 = VIRTFS_START + MAX_SHARES as u64 * VIRTFS_SIZE;
pub const VIRTBALLOON_SIZE: u64 = 0x1000;
pub const VIRTBALLOON_IRQ: u32 = VIRTFS_IRQ + MAX_SHARES as u32;

const GIC_PHANDLE: u32 = 1;
const CLOCK_PHANDLE: u32 = 2;

/// Guest RAM is one region from `RAM_START`.
pub fn ram_regions(memory_size: usize) -> Vec<(u64, usize)> {
    vec![(RAM_START, memory_size)]
}

/// Loads the kernel `Image`, initrd and FDT; the boot CPU starts in the kernel with the FDT in x0.
pub fn load(
    mem: &GuestMemory,
    memory_size: usize,
    vcpus: usize,
    kernel_args: &str,
    kernel: &[u8],
    initrd: &[u8],
    shares: usize,
) -> io::Result<Boot> {
    let (gicd_size, gicr_size) = hypervisor::gic_sizes(vcpus)?;
    assert!(GICR_START > GICD_START + gicd_size);
    assert!(UART_START >= GICR_START + gicr_size);

    let initrd_range = Some((INITRD_START, checked_end(INITRD_START, initrd.len(), "initrd")));
    let dtb = build_fdt(
        memory_size as u64,
        vcpus,
        kernel_args,
        initrd_range,
        gicd_size,
        gicr_size,
        shares,
    )
    .unwrap_or_else(|error| panic!("failed to build guest FDT: {error}"));

    validate_boot_layout(memory_size, kernel.len(), initrd, dtb.len());

    mem.write(IMAGE_START, kernel)?;
    mem.write(INITRD_START, initrd)?;
    mem.write(DTB_START, &dtb)?;
    Ok(Boot {
        entry: IMAGE_START,
        arg: DTB_START,
    })
}

fn validate_boot_layout(memory_size: usize, image_size: usize, initrd: &[u8], dtb_size: usize) {
    let ram_end = checked_end(RAM_START, memory_size, "guest RAM");
    let image_end = checked_end(IMAGE_START, image_size, "kernel");
    if image_end > INITRD_START {
        panic!("kernel ends at 0x{image_end:x}, overlapping initrd region at 0x{INITRD_START:x}");
    }

    let initrd_end = checked_end(INITRD_START, initrd.len(), "initrd");
    if initrd_end > DTB_START {
        panic!("initrd ends at 0x{initrd_end:x}, overlapping FDT at 0x{DTB_START:x}");
    }

    let dtb_end = checked_end(DTB_START, dtb_size, "FDT");
    if dtb_end > ram_end {
        panic!("FDT ends at 0x{dtb_end:x}, past guest RAM end at 0x{ram_end:x}");
    }
}

fn build_fdt(
    memory_size: u64,
    vcpus: usize,
    kernel_args: &str,
    initrd: Option<(u64, u64)>,
    gicd_size: u64,
    gicr_size: u64,
    shares: usize,
) -> FdtWriterResult<Vec<u8>> {
    assert!(shares <= MAX_SHARES, "at most {MAX_SHARES} shares are supported");
    let mut fdt = FdtWriter::new()?;
    fdt.set_boot_cpuid_phys(0);

    let root = fdt.begin_node("")?;
    fdt.property_string("compatible", "varmint,virt")?;
    fdt.property_u32("#address-cells", 2)?;
    fdt.property_u32("#size-cells", 2)?;
    fdt.property_u32("interrupt-parent", GIC_PHANDLE)?;

    let aliases = fdt.begin_node("aliases")?;
    fdt.property_string("serial0", "/serial@9000000")?;
    fdt.end_node(aliases)?;

    let chosen = fdt.begin_node("chosen")?;
    fdt.property_string("bootargs", kernel_args)?;
    fdt.property_string("stdout-path", "serial0:115200n8")?;
    fdt.property("rng-seed", &rng_seed())?;
    if let Some((start, end)) = initrd {
        fdt.property_u64("linux,initrd-start", start)?;
        fdt.property_u64("linux,initrd-end", end)?;
    }
    fdt.end_node(chosen)?;

    let psci = fdt.begin_node("psci")?;
    fdt.property_string("compatible", "arm,psci-0.2")?;
    fdt.property_string("method", "hvc")?;
    fdt.end_node(psci)?;

    let cpus = fdt.begin_node("cpus")?;
    fdt.property_u32("#address-cells", 1)?;
    fdt.property_u32("#size-cells", 0)?;
    for cpu_id in 0..vcpus {
        let mpidr = hypervisor::mpidr(cpu_id) as u32;
        let cpu = fdt.begin_node(&format!("cpu@{mpidr:x}"))?;
        fdt.property_string("device_type", "cpu")?;
        fdt.property_string("compatible", "arm,armv8")?;
        fdt.property_u32("reg", mpidr)?;
        fdt.property_string("enable-method", "psci")?;
        fdt.end_node(cpu)?;
    }
    fdt.end_node(cpus)?;

    let gic = fdt.begin_node(&format!("interrupt-controller@{GICD_START:x}"))?;
    fdt.property_string("compatible", "arm,gic-v3")?;
    fdt.property_null("interrupt-controller")?;
    fdt.property_u32("#interrupt-cells", 3)?;
    fdt.property_array_u64("reg", &[GICD_START, gicd_size, GICR_START, gicr_size])?;
    fdt.property_phandle(GIC_PHANDLE)?;
    fdt.end_node(gic)?;

    let timer = fdt.begin_node("timer")?;
    fdt.property_string("compatible", "arm,armv8-timer")?;
    fdt.property_array_u32("interrupts", &[1, 13, 0xf08, 1, 14, 0xf08, 1, 11, 0xf08, 1, 10, 0xf08])?;
    fdt.end_node(timer)?;

    let memory = fdt.begin_node(&format!("memory@{RAM_START:x}"))?;
    fdt.property_string("device_type", "memory")?;
    fdt.property_array_u64("reg", &[RAM_START, memory_size])?;
    fdt.end_node(memory)?;

    let clock = fdt.begin_node("clk24m")?;
    fdt.property_string("compatible", "fixed-clock")?;
    fdt.property_u32("#clock-cells", 0)?;
    fdt.property_u32("clock-frequency", 24_000_000)?;
    fdt.property_phandle(CLOCK_PHANDLE)?;
    fdt.end_node(clock)?;

    let uart = fdt.begin_node(&format!("serial@{UART_START:x}"))?;
    fdt.property_string_list("compatible", vec!["arm,pl011".to_owned(), "arm,primecell".to_owned()])?;
    fdt.property_u32("arm,primecell-periphid", 0x0034_1011)?;
    fdt.property_array_u64("reg", &[UART_START, UART_SIZE])?;
    fdt.property_array_u32("interrupts", &[0, UART_IRQ, 4])?;
    fdt.property_array_u32("clocks", &[CLOCK_PHANDLE, CLOCK_PHANDLE])?;
    fdt.property_string_list("clock-names", vec!["uartclk".to_owned(), "apb_pclk".to_owned()])?;
    fdt.property_string("status", "okay")?;
    fdt.end_node(uart)?;

    add_virtio(&mut fdt, "virtio_blk", VIRTBLK_START, VIRTBLK_SIZE, VIRTBLK_IRQ)?;
    add_virtio(&mut fdt, "virtio_net", VIRTNET_START, VIRTNET_SIZE, VIRTNET_IRQ)?;
    add_virtio(&mut fdt, "virtio_gpu", VIRTGPU_START, VIRTGPU_SIZE, VIRTGPU_IRQ)?;
    add_virtio(
        &mut fdt,
        "virtio_input_keyboard",
        VIRTINPUT_KEYBOARD_START,
        VIRTINPUT_KEYBOARD_SIZE,
        VIRTINPUT_KEYBOARD_IRQ,
    )?;
    add_virtio(
        &mut fdt,
        "virtio_input_tablet",
        VIRTINPUT_TABLET_START,
        VIRTINPUT_TABLET_SIZE,
        VIRTINPUT_TABLET_IRQ,
    )?;
    add_virtio(&mut fdt, "virtio_snd", VIRTSND_START, VIRTSND_SIZE, VIRTSND_IRQ)?;
    add_virtio(
        &mut fdt,
        "virtio_console",
        VIRTCONSOLE_START,
        VIRTCONSOLE_SIZE,
        VIRTCONSOLE_IRQ,
    )?;
    add_virtio(
        &mut fdt,
        "virtio_input_mouse",
        VIRTINPUT_MOUSE_START,
        VIRTINPUT_MOUSE_SIZE,
        VIRTINPUT_MOUSE_IRQ,
    )?;

    add_virtio(
        &mut fdt,
        "virtio_balloon",
        VIRTBALLOON_START,
        VIRTBALLOON_SIZE,
        VIRTBALLOON_IRQ,
    )?;

    for index in 0..shares {
        add_virtio(
            &mut fdt,
            "virtio_fs",
            VIRTFS_START + index as u64 * VIRTFS_SIZE,
            VIRTFS_SIZE,
            VIRTFS_IRQ + index as u32,
        )?;
    }

    fdt.end_node(root)?;
    fdt.finish()
}

fn add_virtio(fdt: &mut FdtWriter, name: &str, base: u64, size: u64, interrupt: u32) -> FdtWriterResult<()> {
    let node = fdt.begin_node(&format!("{name}@{base:x}"))?;
    fdt.property_string("compatible", "virtio,mmio")?;
    fdt.property_array_u64("reg", &[base, size])?;
    fdt.property_array_u32("interrupts", &[0, interrupt, 4])?;
    fdt.end_node(node)
}

/// Seeds the guest RNG so nothing in early boot blocks waiting for entropy.
fn rng_seed() -> [u8; 64] {
    let mut seed = [0; 64];
    File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut seed))
        .expect("failed to read /dev/urandom");
    seed
}
