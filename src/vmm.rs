use crate::{
    agent, audio, clipboard,
    config::{self, Mount, Seed, VmConfig},
    fuse,
    control,
    devices::{HostBackends, Runtime, RuntimeEvent},
    display::DisplayBuffer,
    hypervisor::Vm,
    instance::{self, Launch},
    machine::{self, MAX_SHARES},
    memory, net,
    protocol::{GuestMount, GuestSeeds},
    registry, seed,
    ssh_config, stdio,
    store::Grant,
    suspend::Suspend,
    telemetry, virtio,
};
use std::{
    collections::BTreeMap,
    fs, io,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock, mpsc::Receiver},
    thread,
    time::Duration,
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

/// Directory of the VM, whose ssh host is removed when it exits.
static SSH_HOST_DIR: OnceLock<PathBuf> = OnceLock::new();

fn unregister_ssh_host() {
    if let Some(dir) = SSH_HOST_DIR.get() {
        if let Err(error) = ssh_config::unregister(dir) {
            eprintln!("failed to remove the ssh host of the VM: {error}");
        }
    }
}

/// Ends the VM process, deleting the VM if it is ephemeral.
pub fn shutdown(reason: &'static str) -> ! {
    telemetry::stop(reason);
    unregister_ssh_host();
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
    config: &VmConfig,
    boot: BootPayload,
    backends: HostBackends,
    suspend: &Suspend,
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
        suspend,
    )
}

/// `idle_suspend` is how long the VM waits without control clients before it
/// suspends, `None` to never suspend.
pub fn run(
    launch: Launch,
    idle_suspend: Option<Duration>,
    mounts: &[Mount],
    seeds: &[Seed],
    grants: Vec<Grant>,
    environment: BTreeMap<String, String>,
) -> io::Result<()> {
    let Launch {
        instance,
        lock: _lock,
        listener,
    } = launch;
    let hardware = instance.hardware().unwrap_or_else(|error| panic!("{error}"));
    let config = VmConfig::new(&instance, &hardware);
    telemetry::start(&instance.name, &hardware, mounts.len());

    if !hardware.persist {
        EPHEMERAL_DIR.set(instance.dir.clone()).unwrap();
    }
    // A panic on any thread ends the VM. Its directory stays for `vm run` to
    // show the log; the next prune deletes it if the VM is ephemeral.
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_panic(info);
        telemetry::crash();
        unregister_ssh_host();
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

    // Registering also removes the host a killed run of this VM left behind.
    SSH_HOST_DIR.set(instance.dir.clone()).unwrap();
    if let Err(error) = ssh_config::register(&instance.dir) {
        panic!("failed to update {}: {error}", ssh_config::config_path().display());
    }

    let boot = BootPayload::load(&config);

    let vm = Vm::new(config.vcpus).unwrap_or_else(|error| panic!("failed to create the VM: {error}"));

    let display = Mutex::new(DisplayBuffer::new());

    let (runtime_event_tx, runtime_event_rx) = std::sync::mpsc::channel();

    let blk_io_tx = runtime_event_tx.clone();
    let disk = virtio::Blk::new(&config.disk, config.disk_size, move |completion| {
        let _ = blk_io_tx.send(RuntimeEvent::BlkIo(completion));
    })
    .unwrap_or_else(|error| panic!("failed to open VM disk {}: {error}", config.disk.display()));

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
        instance.name.clone(),
        grants,
    );

    let net_rx_tx = runtime_event_tx.clone();
    let network = net::start(move |frame| {
        let _ = net_rx_tx.send(RuntimeEvent::NetRx(frame));
    })
    .unwrap_or_else(|error| panic!("failed to start networking: {error}"));

    // The VM plays to control clients, such as `vm ui`, which play it on their speakers.
    let audio_listeners = Arc::new(audio::Listeners::default());
    let (audio_event_tx, audio_event_rx) = std::sync::mpsc::channel();
    let (_audio_backend, period_sink) = audio::Backend::new(false, Arc::clone(&audio_listeners), move |event| {
        let _ = audio_event_tx.send(event);
    })
    .unwrap_or_else(|error| panic!("failed to start audio output: {error}"));

    let clipboard = Arc::new(clipboard::Guest::new(runtime_event_tx.clone()));

    let serial_rx_tx = runtime_event_tx.clone();
    let serial = stdio::start(move |byte| {
        let _ = serial_rx_tx.send(RuntimeEvent::UartRx(byte));
    });

    let backends = HostBackends {
        disk,
        net: network,
        audio: period_sink,
        clipboard: clipboard.clone(),
        agent: agent_sink,
        serial,
        shares,
    };

    let suspend = Suspend::new();

    let control_context = control::Context {
        suspend: &suspend,
        display: &display,
        agent: &agent,
        clipboard: &clipboard,
        audio: &audio_listeners,
        runtime_tx: runtime_event_tx.clone(),
        display_size: (config.width, config.height),
    };


    thread::scope(|scope| {
        let vm = &vm;
        let display = &display;
        let config = &config;
        let suspend = &suspend;
        if let Some(timeout) = idle_suspend {
            scope.spawn(move || suspend.watch(timeout));
        }
        scope.spawn(move || {
            vmm_thread(
                vm,
                display,
                runtime_event_rx,
                audio_event_rx,
                config,
                boot,
                backends,
                suspend,
            )
            .unwrap_or_else(|error| panic!("VM failed: {error:?}"));
            shutdown("exited")
        });

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
    })
}
