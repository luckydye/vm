use crate::{
    agent, app, audio, clipboard,
    config::{self, Mount, Seed, VmConfig},
    fuse,
    control,
    devices::{HostBackends, Runtime, RuntimeEvent},
    display::{DisplayBuffer, DisplayNotifier},
    hypervisor::Vm,
    instance::{self, Launch},
    machine::{self, MAX_SHARES},
    memory, net,
    protocol::{GuestMount, GuestSeeds},
    registry, seed,
    stdio, telemetry, virtio,
};
use std::{
    collections::BTreeMap,
    fs, io,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock, mpsc::Receiver},
    thread,
};

struct BootPayload {
    kernel: Vec<u8>,
    initrd: Vec<u8>,
}

impl BootPayload {
    fn load(config: &VmConfig) -> Self {
        let kernel = fs::read(&config.kernel)
            .unwrap_or_else(|error| panic!("failed to read kernel {}: {error}", config.kernel.display()));
        let initrd = fs::read(&config.initrd)
            .unwrap_or_else(|error| panic!("failed to read initrd {}: {error}", config.initrd.display()));
        Self { kernel, initrd }
    }
}

/// Where the guest mounts the staged seeds while copying them.
const SEED_MOUNT: &str = "/run/varmint/seed";

/// Directory deleted when an ephemeral VM exits.
static EPHEMERAL_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Ends the VM process, deleting the VM if it is ephemeral.
pub fn shutdown(reason: &'static str) -> ! {
    telemetry::stop(reason);
    if let Some(dir) = EPHEMERAL_DIR.get() {
        if let Err(error) = instance::archive_log(dir) {
            eprintln!("failed to keep the VM log: {error}");
        }
        let _ = fs::remove_dir_all(dir);
    }
    std::process::exit(0)
}

fn vmm_thread(
    vm: &Vm,
    display: &Mutex<DisplayBuffer>,
    runtime_event_rx: Receiver<RuntimeEvent>,
    audio_event_rx: Receiver<audio::BackendEvent>,
    display_notifier: DisplayNotifier,
    config: &VmConfig,
    boot: BootPayload,
    backends: HostBackends,
) -> io::Result<()> {
    let mem = memory::GuestMemory::new(vm, machine::ram_regions(config.memory_size));
    let entry = machine::load(
        &mem,
        config.memory_size,
        config.vcpus,
        &config.kernel_args,
        &boot.kernel,
        &boot.initrd,
        backends.shares.len(),
    )?;
    // The VMM runs until the VM stops; the guest has its own copy now.
    drop(boot);

    Runtime::new(vm, backends, config.vcpus, entry).run(
        &mem,
        display,
        runtime_event_rx,
        audio_event_rx,
        &display_notifier,
    )
}

pub fn run(
    launch: Launch,
    window: bool,
    mounts: &[Mount],
    seeds: &[Seed],
    environment: BTreeMap<String, String>,
) -> io::Result<()> {
    let Launch {
        instance,
        lock: _lock,
        listener,
    } = launch;
    let hardware = instance.hardware().unwrap_or_else(|error| panic!("{error}"));
    let mut config = VmConfig::new(&instance, &hardware);
    if window {
        // Tells the guest to sync the clipboard; headless VMs skip it.
        config.kernel_args.push_str(" varmint.window=1");
    }
    telemetry::start(&instance.name, &hardware, mounts.len(), window);

    if !hardware.persist {
        EPHEMERAL_DIR.set(instance.dir.clone()).unwrap();
    }
    // A panic on any thread ends the VM. Its directory stays for `varmint run` to
    // show the log; the next prune deletes it if the VM is ephemeral.
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_panic(info);
        telemetry::stop("crash");
        std::process::exit(101);
    }));

    if let Some(runtime) = config::runtime_dir() {
        if std::env::var_os("RENDER_SERVER_EXEC_PATH").is_none() {
            let render_server = runtime.join("Helpers/virgl_render_server");
            if render_server.is_file() {
                unsafe {
                    std::env::set_var("RENDER_SERVER_EXEC_PATH", render_server);
                }
            }
        }
    }

    let disk = virtio::Blk::new(&config.disk, config.disk_size)
        .unwrap_or_else(|error| panic!("failed to open VM disk {}: {error}", config.disk.display()));
    let boot = BootPayload::load(&config);

    let vm = Vm::new(config.vcpus).unwrap_or_else(|error| panic!("failed to create the VM: {error}"));

    let display = Mutex::new(DisplayBuffer::new());

    let (runtime_event_tx, runtime_event_rx) = std::sync::mpsc::channel();

    let staged_seeds =
        seed::stage(&instance, seeds).unwrap_or_else(|error| panic!("failed to stage seeds: {error}"));
    let mut mounts = mounts.to_vec();
    if let Some((staging, _)) = &staged_seeds {
        mounts.push(Mount {
            host: staging.clone(),
            guest: SEED_MOUNT.into(),
            readonly: true,
        });
    }
    assert!(mounts.len() <= MAX_SHARES, "at most {MAX_SHARES} shares are supported");

    let shares = mounts
        .iter()
        .enumerate()
        .map(|(index, mount)| {
            let share = fuse::Share::new(&mount.host, mount.readonly)
                .unwrap_or_else(|error| panic!("failed to open share {}: {error}", mount.host.display()));
            virtio::fs_channel(&format!("share{index}"), share)
        })
        .collect();
    let guest_mounts = mounts
        .iter()
        .enumerate()
        .map(|(index, mount)| GuestMount {
            tag: format!("share{index}"),
            path: mount.guest.clone(),
            readonly: mount.readonly,
        })
        .collect();

    let (agent, agent_sink) = agent::start(
        runtime_event_tx.clone(),
        instance::authorized_keys(),
        guest_mounts,
        environment,
        staged_seeds.map(|(_, items)| GuestSeeds {
            mount: SEED_MOUNT.into(),
            items,
        }),
        registry::allowed().unwrap_or_else(|error| panic!("{error}")),
    );

    let net_rx_tx = runtime_event_tx.clone();
    let network = net::start(move |frame| {
        let _ = net_rx_tx.send(RuntimeEvent::NetRx(frame));
    })
    .unwrap_or_else(|error| panic!("failed to start networking: {error}"));

    // Only a VM with a window plays on the host's speakers; every VM streams its
    // audio to control clients.
    let audio_listeners = Arc::new(audio::Listeners::default());
    let (audio_event_tx, audio_event_rx) = std::sync::mpsc::channel();
    let (_audio_backend, period_sink) = audio::Backend::new(window, Arc::clone(&audio_listeners), move |event| {
        let _ = audio_event_tx.send(event);
    })
    .unwrap_or_else(|error| panic!("failed to start audio output: {error}"));

    // Only a VM with a window syncs the host clipboard; headless isolates only get
    // clipboard images from interactive `varmint exec` sessions.
    let clipboard = window.then(|| {
        let clipboard_change_tx = runtime_event_tx.clone();
        clipboard::start(move |payload| {
            let _ = clipboard_change_tx.send(RuntimeEvent::Clipboard(payload));
        })
    });

    let serial_rx_tx = runtime_event_tx.clone();
    let serial = stdio::start(move |byte| {
        let _ = serial_rx_tx.send(RuntimeEvent::UartRx(byte));
    });

    let backends = HostBackends {
        disk,
        net: network,
        audio: period_sink,
        clipboard,
        agent: agent_sink,
        serial,
        shares,
    };

    let control_context = control::Context {
        display: &display,
        agent: &agent,
        audio: &audio_listeners,
        runtime_tx: runtime_event_tx.clone(),
        display_size: (config.width, config.height),
    };

    let event_loop = window.then(app::event_loop);
    let display_notifier = match &event_loop {
        Some(event_loop) => DisplayNotifier::Window(event_loop.create_proxy()),
        None => DisplayNotifier::Headless,
    };

    thread::scope(|scope| {
        let vm = &vm;
        let display = &display;
        let config = &config;
        scope.spawn(move || {
            vmm_thread(
                vm,
                display,
                runtime_event_rx,
                audio_event_rx,
                display_notifier,
                config,
                boot,
                backends,
            )
            .unwrap_or_else(|error| panic!("VM failed: {error:?}"));
            shutdown("exited")
        });

        match event_loop {
            Some(event_loop) => {
                let control_context = &control_context;
                scope.spawn(move || control::serve(listener, control_context));
                app::run(event_loop, display, runtime_event_tx, (config.width, config.height));
                shutdown("window-closed")
            }
            None => {
                runtime_event_tx
                    .send(RuntimeEvent::DisplayResized {
                        physical_width: config.width,
                        physical_height: config.height,
                        logical_width: config.width,
                        logical_height: config.height,
                        refresh_millihertz: 60_000,
                    })
                    .expect("runtime event pump exited");
                control::serve(listener, &control_context)
            }
        }
    })
}
