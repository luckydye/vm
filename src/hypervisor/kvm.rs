//! Linux KVM with its in-kernel interrupt controllers: the GICv3 on arm64,
//! the PIC, IOAPIC and local APICs on x86-64.

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "aarch64")]
use aarch64 as arch;
#[cfg(target_arch = "aarch64")]
pub use aarch64::{gic_sizes, mpidr};
#[cfg(target_arch = "x86_64")]
mod x86_64;
use crate::{devices::Devices, machine::Boot, memory::GuestMemory};
use kvm_bindings::kvm_userspace_memory_region;
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};
use std::{
    io,
    sync::{Arc, Mutex},
    thread,
};
#[cfg(target_arch = "x86_64")]
use x86_64 as arch;

type Result<T> = std::result::Result<T, kvm_ioctls::Error>;

#[derive(Clone)]
pub struct Vm(Arc<Inner>);

struct Inner {
    vm: VmFd,
    /// Created with the VM, so the interrupt controller is complete before any device raises a line.
    vcpus: Mutex<Vec<VcpuFd>>,
    /// Guest address and size of each memory slot; `None` once unmapped.
    slots: Mutex<Vec<Option<(u64, usize)>>>,
    _arch: arch::State,
}

pub struct IrqLine {
    vm: Arc<Inner>,
    irq: u32,
    level: bool,
}

fn io_error(error: kvm_ioctls::Error) -> io::Error {
    io::Error::from_raw_os_error(error.errno())
}

impl Vm {
    pub fn new(vcpus: usize) -> io::Result<Self> {
        let kvm = Kvm::new().map_err(|error| {
            io::Error::new(
                io_error(error).kind(),
                format!("cannot open /dev/kvm: {}", io_error(error)),
            )
        })?;
        let max_vcpus = kvm.get_max_vcpus();
        if vcpus > max_vcpus {
            panic!("vcpus={vcpus} exceeds the KVM limit of {max_vcpus}");
        }
        let vm = kvm.create_vm().map_err(io_error)?;
        let (vcpus, arch) = arch::create(&kvm, &vm, vcpus).map_err(io_error)?;
        Ok(Self(Arc::new(Inner {
            vm,
            vcpus: Mutex::new(vcpus),
            slots: Mutex::new(Vec::new()),
            _arch: arch,
        })))
    }

    /// The level-triggered interrupt line `irq`: a GIC SPI on arm64, an IOAPIC pin on x86-64.
    pub fn irq_line(&self, irq: u32) -> IrqLine {
        IrqLine {
            vm: Arc::clone(&self.0),
            irq: arch::irq_number(irq),
            level: false,
        }
    }

    /// Maps `size` bytes of host memory at `guest_addr`; both must be page aligned.
    /// KVM cannot map memory that is not executable.
    pub fn map(&self, host_addr: *mut u8, guest_addr: u64, size: usize, _executable: bool) -> io::Result<()> {
        let mut slots = self.0.slots.lock().unwrap();
        let slot = slots.iter().position(Option::is_none).unwrap_or(slots.len());
        let region = kvm_userspace_memory_region {
            slot: slot as u32,
            flags: 0,
            guest_phys_addr: guest_addr,
            memory_size: size as u64,
            userspace_addr: host_addr as u64,
        };
        unsafe { self.0.vm.set_user_memory_region(region) }.map_err(io_error)?;
        if slot == slots.len() {
            slots.push(None);
        }
        slots[slot] = Some((guest_addr, size));
        Ok(())
    }

    pub fn unmap(&self, guest_addr: u64, size: usize) -> io::Result<()> {
        let mut slots = self.0.slots.lock().unwrap();
        let slot = slots
            .iter()
            .position(|slot| *slot == Some((guest_addr, size)))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("nothing is mapped at {guest_addr:#x}")))?;
        let region = kvm_userspace_memory_region {
            slot: slot as u32,
            flags: 0,
            guest_phys_addr: guest_addr,
            memory_size: 0,
            userspace_addr: 0,
        };
        unsafe { self.0.vm.set_user_memory_region(region) }.map_err(io_error)?;
        slots[slot] = None;
        Ok(())
    }

    /// Releases the host pages behind page-aligned guest RAM; KVM drops its
    /// mappings of them and the guest reads zeroes there afterwards.
    pub fn discard(&self, host_addr: *mut u8, _guest_addr: u64, len: usize) -> io::Result<()> {
        if unsafe { libc::madvise(host_addr.cast(), len, libc::MADV_DONTNEED) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Runs every vCPU on its own thread until the guest powers off.
    pub fn run(&self, boot: Boot, vcpus: usize, mem: &GuestMemory, devices: &Devices) -> io::Result<()> {
        let fds = std::mem::take(&mut *self.0.vcpus.lock().unwrap());
        assert_eq!(fds.len(), vcpus, "the vCPUs already ran");
        thread::scope(|scope| {
            let threads: Vec<_> = fds
                .into_iter()
                .enumerate()
                .map(|(id, mut vcpu)| {
                    let boot = &boot;
                    thread::Builder::new()
                        .name(format!("vcpu{id}"))
                        .spawn_scoped(scope, move || -> io::Result<()> {
                            arch::configure(&vcpu, id, boot, mem).map_err(io_error)?;
                            run_loop(&mut vcpu, mem, devices)
                        })
                        .expect("failed to spawn vCPU thread")
                })
                .collect();
            threads
                .into_iter()
                .try_for_each(|thread| thread.join().expect("vCPU thread panicked"))
        })
    }
}

impl IrqLine {
    pub fn set(&mut self, level: bool) {
        if level == self.level {
            return;
        }

        self.vm.vm.set_irq_line(self.irq, level).unwrap();
        self.level = level;
    }
}

fn run_loop(vcpu: &mut VcpuFd, mem: &GuestMemory, devices: &Devices) -> io::Result<()> {
    loop {
        let exit = match vcpu.run() {
            Ok(exit) => exit,
            Err(error) if error.errno() == libc::EINTR || error.errno() == libc::EAGAIN => continue,
            Err(error) => return Err(io_error(error)),
        };
        match exit {
            VcpuExit::MmioRead(addr, data) => {
                let value = mmio(devices, addr, false, data.len(), 0, mem);
                let len = data.len();
                data.copy_from_slice(&value.to_le_bytes()[..len]);
            }
            VcpuExit::MmioWrite(addr, data) => {
                let mut bytes = [0; 8];
                bytes[..data.len()].copy_from_slice(data);
                mmio(devices, addr, true, data.len(), u64::from_le_bytes(bytes), mem);
            }
            #[cfg(target_arch = "x86_64")]
            VcpuExit::IoIn(port, data) => {
                let value = devices.handle_pio(port, false, 0);
                let len = data.len();
                data.copy_from_slice(&value.to_le_bytes()[..len]);
            }
            #[cfg(target_arch = "x86_64")]
            VcpuExit::IoOut(port, data) => {
                let mut bytes = [0; 4];
                bytes[..data.len()].copy_from_slice(data);
                devices.handle_pio(port, true, u32::from_le_bytes(bytes));
            }
            // PSCI SYSTEM_OFF and SYSTEM_RESET, or an x86 triple fault: reboots
            // end the VM too; `varmint run` starts a persistent VM again.
            VcpuExit::SystemEvent(..) | VcpuExit::Shutdown => crate::vmm::shutdown("guest-poweroff"),
            VcpuExit::Intr => {}
            exit => panic!("unexpected vCPU exit: {exit:?}"),
        }
    }
}

/// A guest access to device memory. On x86-64 addresses with no device read
/// as all ones, as on a PC; on arm64 they are a bug in the machine model.
fn mmio(devices: &Devices, addr: u64, is_write: bool, size: usize, value: u64, mem: &GuestMemory) -> u64 {
    match devices.handle_mmio(addr, is_write, size, value, mem) {
        Ok(read) => read.unwrap_or(0),
        Err(()) if cfg!(target_arch = "x86_64") => u64::MAX,
        Err(()) => panic!(
            "unhandled MMIO {} of {size} bytes at {addr:#x}",
            if is_write { "write" } else { "read" }
        ),
    }
}
