mod events;
mod gpu;

use crate::{
    agent, audio, clipboard,
    display::{DisplayBuffer, DisplayNotifier},
    hypervisor::Vm,
    machine::*,
    memory::GuestMemory,
    net, stdio, uart, virtio,
};
pub use events::{RuntimeEvent, RuntimeInputEvent};
use std::{
    io,
    sync::{Mutex, mpsc::Receiver},
    thread,
};

pub struct HostBackends {
    pub disk: virtio::Blk,
    pub net: net::Backend,
    pub audio: audio::PeriodSink,
    pub clipboard: Option<clipboard::Sink>,
    pub agent: agent::Sink,
    pub serial: stdio::Sink,
    pub shares: Vec<(virtio::Fs, virtio::FsWorker)>,
}

pub struct Devices {
    uart: Mutex<uart::Uart>,
    blk: Mutex<virtio::MmioTransport<virtio::Blk>>,
    net: Mutex<virtio::MmioTransport<virtio::Net>>,
    gpu: gpu::Handle,
    keyboard: Mutex<virtio::MmioTransport<virtio::Input>>,
    tablet: Mutex<virtio::MmioTransport<virtio::Input>>,
    mouse: Mutex<virtio::MmioTransport<virtio::Input>>,
    snd: Mutex<virtio::MmioTransport<virtio::Snd>>,
    console: Mutex<virtio::MmioTransport<virtio::Console>>,
    balloon: Mutex<virtio::MmioTransport<virtio::Balloon>>,
    shares: Vec<Mutex<virtio::MmioTransport<virtio::Fs>>>,
}

pub struct Runtime<'a> {
    vm: &'a Vm,
    vcpus: usize,
    boot: Boot,
    devices: Devices,
    gpu_worker: gpu::Worker,
    share_workers: Vec<virtio::FsWorker>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum MmioDevice {
    #[cfg(target_arch = "aarch64")]
    Uart,
    VirtioBlk,
    VirtioNet,
    VirtioGpu,
    VirtioInputKeyboard,
    VirtioInputTablet,
    VirtioInputMouse,
    VirtioSnd,
    VirtioConsole,
    VirtioBalloon,
}

#[derive(Debug, Copy, Clone)]
struct MmioRoute {
    device: MmioDevice,
    offset: u64,
}

struct MmioRegion {
    base: u64,
    size: u64,
    device: MmioDevice,
}

const MMIO_REGIONS: &[MmioRegion] = &[
    #[cfg(target_arch = "aarch64")]
    MmioRegion {
        base: UART_START,
        size: UART_SIZE,
        device: MmioDevice::Uart,
    },
    MmioRegion {
        base: VIRTBLK_START,
        size: VIRTBLK_SIZE,
        device: MmioDevice::VirtioBlk,
    },
    MmioRegion {
        base: VIRTNET_START,
        size: VIRTNET_SIZE,
        device: MmioDevice::VirtioNet,
    },
    MmioRegion {
        base: VIRTGPU_START,
        size: VIRTGPU_SIZE,
        device: MmioDevice::VirtioGpu,
    },
    MmioRegion {
        base: VIRTINPUT_KEYBOARD_START,
        size: VIRTINPUT_KEYBOARD_SIZE,
        device: MmioDevice::VirtioInputKeyboard,
    },
    MmioRegion {
        base: VIRTINPUT_TABLET_START,
        size: VIRTINPUT_TABLET_SIZE,
        device: MmioDevice::VirtioInputTablet,
    },
    MmioRegion {
        base: VIRTINPUT_MOUSE_START,
        size: VIRTINPUT_MOUSE_SIZE,
        device: MmioDevice::VirtioInputMouse,
    },
    MmioRegion {
        base: VIRTSND_START,
        size: VIRTSND_SIZE,
        device: MmioDevice::VirtioSnd,
    },
    MmioRegion {
        base: VIRTCONSOLE_START,
        size: VIRTCONSOLE_SIZE,
        device: MmioDevice::VirtioConsole,
    },
    MmioRegion {
        base: VIRTBALLOON_START,
        size: VIRTBALLOON_SIZE,
        device: MmioDevice::VirtioBalloon,
    },
];

impl<'a> Runtime<'a> {
    pub fn new(vm: &'a Vm, backends: HostBackends, vcpus: usize, boot: Boot) -> Self {
        let uart = Mutex::new(uart::Uart::new(vm.irq_line(UART_IRQ), backends.serial));

        let blk = Mutex::new(virtio::MmioTransport::new(backends.disk, vm.irq_line(VIRTBLK_IRQ)));

        let net = Mutex::new(virtio::MmioTransport::new(
            virtio::Net::new(backends.net.mac(), backends.net),
            vm.irq_line(VIRTNET_IRQ),
        ));

        let (gpu, gpu_worker) = gpu::channel(vm.irq_line(VIRTGPU_IRQ));

        let keyboard = Mutex::new(virtio::MmioTransport::new(
            virtio::Input::keyboard(),
            vm.irq_line(VIRTINPUT_KEYBOARD_IRQ),
        ));

        let tablet = Mutex::new(virtio::MmioTransport::new(
            virtio::Input::tablet(),
            vm.irq_line(VIRTINPUT_TABLET_IRQ),
        ));

        let mouse = Mutex::new(virtio::MmioTransport::new(
            virtio::Input::mouse(),
            vm.irq_line(VIRTINPUT_MOUSE_IRQ),
        ));

        let snd = Mutex::new(virtio::MmioTransport::new(
            virtio::Snd::new(backends.audio),
            vm.irq_line(VIRTSND_IRQ),
        ));

        let console = Mutex::new(virtio::MmioTransport::new(
            virtio::Console::new(backends.clipboard, backends.agent),
            vm.irq_line(VIRTCONSOLE_IRQ),
        ));

        let balloon = Mutex::new(virtio::MmioTransport::new(
            virtio::Balloon,
            vm.irq_line(VIRTBALLOON_IRQ),
        ));

        let (shares, share_workers): (Vec<_>, Vec<_>) = backends
            .shares
            .into_iter()
            .enumerate()
            .map(|(index, (share, worker))| {
                let transport = Mutex::new(virtio::MmioTransport::new(
                    share,
                    vm.irq_line(VIRTFS_IRQ + index as u32),
                ));
                (transport, worker)
            })
            .unzip();

        Self {
            vm,
            vcpus,
            boot,
            devices: Devices {
                uart,
                blk,
                net,
                gpu,
                keyboard,
                tablet,
                mouse,
                snd,
                console,
                balloon,
                shares,
            },
            gpu_worker,
            share_workers,
        }
    }

    pub fn run(
        self,
        mem: &GuestMemory,
        display: &Mutex<DisplayBuffer>,
        runtime_event_rx: Receiver<RuntimeEvent>,
        audio_event_rx: Receiver<audio::BackendEvent>,
        display_notifier: &DisplayNotifier,
    ) -> io::Result<()> {
        thread::scope(|scope| {
            scope.spawn(|| self.gpu_worker.run(mem, display, display_notifier));

            for (worker, transport) in self.share_workers.into_iter().zip(&self.devices.shares) {
                scope.spawn(move || worker.run(transport, mem));
            }

            scope.spawn(|| {
                let runtime_events = events::RuntimeEventPump::new(mem, &self.devices, runtime_event_rx);
                runtime_events.run();
            });

            scope.spawn(|| {
                let audio_events = events::AudioEventPump::new(mem, &self.devices, audio_event_rx);
                audio_events.run();
            });

            self.vm.run(self.boot, self.vcpus, mem, &self.devices)
        })
    }
}

impl Devices {
    pub fn handle_mmio(
        &self,
        phys_addr: u64,
        is_write: bool,
        size: usize,
        value: u64,
        mem: &GuestMemory,
    ) -> std::result::Result<Option<u64>, ()> {
        let shares_end = VIRTFS_START + self.shares.len() as u64 * VIRTFS_SIZE;
        if (VIRTFS_START..shares_end).contains(&phys_addr) {
            let index = ((phys_addr - VIRTFS_START) / VIRTFS_SIZE) as usize;
            let offset = (phys_addr - VIRTFS_START) % VIRTFS_SIZE;
            return Ok(handle_virtio_mmio(&self.shares[index], offset, size, is_write, value, mem));
        }

        let Some(route) = classify(phys_addr) else {
            return Err(());
        };

        let result = match route.device {
            #[cfg(target_arch = "aarch64")]
            MmioDevice::Uart => {
                if is_write {
                    self.uart.lock().unwrap().write(route.offset, value as u32);
                    None
                } else {
                    Some(self.uart.lock().unwrap().read(route.offset) as u64)
                }
            }
            MmioDevice::VirtioBlk => handle_virtio_mmio(&self.blk, route.offset, size, is_write, value, mem),
            MmioDevice::VirtioNet => handle_virtio_mmio(&self.net, route.offset, size, is_write, value, mem),
            MmioDevice::VirtioGpu => self.gpu.handle_mmio(route.offset, size, is_write, value),
            MmioDevice::VirtioInputKeyboard => {
                handle_virtio_mmio(&self.keyboard, route.offset, size, is_write, value, mem)
            }
            MmioDevice::VirtioInputTablet => handle_virtio_mmio(&self.tablet, route.offset, size, is_write, value, mem),
            MmioDevice::VirtioInputMouse => handle_virtio_mmio(&self.mouse, route.offset, size, is_write, value, mem),
            MmioDevice::VirtioSnd => handle_virtio_mmio(&self.snd, route.offset, size, is_write, value, mem),
            MmioDevice::VirtioConsole => handle_virtio_mmio(&self.console, route.offset, size, is_write, value, mem),
            MmioDevice::VirtioBalloon => handle_virtio_mmio(&self.balloon, route.offset, size, is_write, value, mem),
        };

        Ok(result)
    }

    /// Port I/O: the serial port and the ACPI sleep and reset register. Ports
    /// with nothing behind them read as all ones, like an empty ISA bus.
    #[cfg(target_arch = "x86_64")]
    pub fn handle_pio(&self, port: u16, is_write: bool, value: u32) -> u32 {
        match port {
            UART_PORT..UART_PORT_END => {
                let offset = (port - UART_PORT) as u64;
                if is_write {
                    self.uart.lock().unwrap().write(offset, value);
                    0
                } else {
                    self.uart.lock().unwrap().read(offset)
                }
            }
            // A sleep request or reset: both stop the VM, like PSCI SYSTEM_OFF and SYSTEM_RESET.
            ACPI_PORT if is_write && (value & ACPI_SLEEP_ENABLE != 0 || value == ACPI_RESET_VALUE) => {
                crate::vmm::shutdown("guest-poweroff")
            }
            _ => u32::MAX,
        }
    }
}

fn classify(phys_addr: u64) -> Option<MmioRoute> {
    MMIO_REGIONS.iter().find_map(|region| {
        (region.base..region.base + region.size)
            .contains(&phys_addr)
            .then(|| MmioRoute {
                device: region.device,
                offset: phys_addr - region.base,
            })
    })
}

fn handle_virtio_mmio<D: virtio::Device>(
    dev: &Mutex<virtio::MmioTransport<D>>,
    offset: u64,
    size: usize,
    is_write: bool,
    value: u64,
    mem: &GuestMemory,
) -> Option<u64> {
    let mut dev = dev.lock().unwrap();
    if is_write {
        dev.write(offset, size, value, mem);
        None
    } else {
        Some(dev.read(offset, size))
    }
}
