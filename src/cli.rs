use crate::{
    clipboard, help, screen, serve,
    config::{self, GUEST_ENVIRONMENT_VARIABLE, Hardware, Mount, Seed},
    instance::{self, Instance, Launch},
    keymap,
    machine::MAX_SHARES,
    protocol::{self, Codec, Frame, InputEvent, Reply, Request, Session, kind},
    virtio::input::keys::{BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, KEY_LEFTSHIFT},
    vmm,
};
use crossterm::terminal;
use std::{
    collections::BTreeMap,
    env,
    fs::{self, File},
    io::{self, BufRead, BufReader, IsTerminal, Read, Write},
    net::TcpListener,
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::{fs::PermissionsExt, net::UnixListener, net::UnixStream, process::CommandExt},
    },
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::Instant,
};

const DEFAULT_HARDWARE: Hardware = Hardware {
    persist: false,
    cpus: 4,
    memory_mib: 6144,
    disk_gib: 64,
    width: 1920,
    height: 1080,
};

fn fail(message: impl std::fmt::Display) -> ! {
    eprintln!("varmint: {message}");
    std::process::exit(1)
}

pub fn main() {
    write_default_commands();
    let args: Vec<String> = env::args().skip(1).collect();
    let Some((command, args)) = args.split_first() else {
        print!("{}", help::overview(&help::commands(&DEFAULT_HARDWARE), &user_command_summaries()));
        return;
    };

    if command == "help" || command == "--help" || command == "-h" {
        match args.first() {
            None => print!("{}", help::overview(&help::commands(&DEFAULT_HARDWARE), &user_command_summaries())),
            Some(name) => print!("{}", help::command(&command_help(name))),
        }
        return;
    }
    let builtin = help::commands(&DEFAULT_HARDWARE).iter().any(|known| known.name == command);
    let before_separator = args.iter().take_while(|arg| *arg != "--");
    if builtin && before_separator.into_iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{}", help::command(&command_help(command)));
        return;
    }

    match command.as_str() {
        "run" => run(args),
        "ls" => list(),
        "stop" => stop_all(args),
        "rm" => remove(args),
        "save" => save(args),
        "exec" => exec(args),
        "ssh" => ssh(args),
        "proxy" => proxy(args),
        "screenshot" => screenshot(args),
        "screen" => screen_command(args),
        "stream" => stream(args),
        "input" => input(args),
        "move" => move_pointer(args),
        "click" => click(args),
        "type" => type_text(args),
        "key" => key(args),
        "scroll" => scroll(args),
        "serve" => serve_command(args),
        "__vmm" => vmm_process(args),
        _ => user_command(command, args),
    }
}

fn user_commands_dir() -> PathBuf {
    instance::home().join("commands")
}

/// Commands a new ~/.varmint/commands starts with; they are the user's to change or delete.
const DEFAULT_COMMANDS: [(&str, &str); 2] =
    [("dev", include_str!("../commands/dev")), ("pull", include_str!("../commands/pull"))];

/// Creates ~/.varmint/commands with the default commands unless it exists. The
/// directory is filled aside and renamed into place, so concurrent varmint
/// processes never see it half written.
fn write_default_commands() {
    let dir = user_commands_dir();
    if dir.exists() {
        return;
    }
    let partial = instance::home().join(format!(".commands-{}", std::process::id()));
    fs::create_dir_all(&partial)
        .unwrap_or_else(|error| fail(format!("failed to create {}: {error}", partial.display())));
    for (name, script) in DEFAULT_COMMANDS {
        let path = partial.join(name);
        fs::write(&path, script).unwrap_or_else(|error| fail(format!("failed to write {}: {error}", path.display())));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|error| fail(format!("failed to make {} executable: {error}", path.display())));
    }
    if let Err(error) = fs::rename(&partial, &dir) {
        fs::remove_dir_all(&partial)
            .unwrap_or_else(|error| fail(format!("failed to remove {}: {error}", partial.display())));
        if !dir.exists() {
            fail(format!("failed to create {}: {error}", dir.display()));
        }
    }
}

/// Executables in ~/.varmint/commands, the user's own varmint commands.
fn user_commands() -> Vec<String> {
    let Ok(entries) = fs::read_dir(user_commands_dir()) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| {
            entry
                .metadata()
                .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        })
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Each user command with its summary: the first comment line after the
/// shebang, or nothing when the file has none.
fn user_command_summaries() -> Vec<(String, String)> {
    user_commands()
        .into_iter()
        .map(|name| {
            let file = File::open(user_commands_dir().join(&name))
                .unwrap_or_else(|error| fail(format!("failed to read command {name}: {error}")));
            let second_line = BufReader::new(file.take(4096)).lines().nth(1).and_then(Result::ok);
            let summary = second_line.and_then(|line| line.strip_prefix("# ").map(str::to_owned)).unwrap_or_default();
            (name, summary)
        })
        .collect()
}

/// Runs ~/.varmint/commands/NAME with the remaining arguments.
fn user_command(name: &str, args: &[String]) -> ! {
    if !user_commands().iter().any(|command| command == name) {
        unknown_command(name);
    }
    let executable = env::current_exe()
        .and_then(|path| path.canonicalize())
        .unwrap_or_else(|error| fail(error));
    let error = Command::new(user_commands_dir().join(name))
        .args(args)
        .env("VARMINT", executable)
        .exec();
    fail(format!("failed to run {}: {error}", user_commands_dir().join(name).display()))
}

fn command_help(name: &str) -> help::Command {
    help::commands(&DEFAULT_HARDWARE)
        .into_iter()
        .find(|command| command.name == name)
        .unwrap_or_else(|| unknown_command(name))
}

fn unknown_command(name: &str) -> ! {
    eprintln!("varmint: unknown command {name:?}\nRun `varmint help` to list the commands.");
    std::process::exit(2)
}

/// Wrong arguments: the command's usage and where to find more.
fn usage_error(command: &str) -> ! {
    eprintln!("varmint: {}", help::usage_error(&command_help(command)));
    std::process::exit(2)
}

fn name_arg<'a>(command: &str, args: &'a [String]) -> &'a str {
    args.first().unwrap_or_else(|| usage_error(command))
}

fn open(name: &str) -> Instance {
    Instance::open(name).unwrap_or_else(|error| fail(error))
}

fn parse<T: std::str::FromStr>(value: Option<&String>, what: &str) -> T {
    value
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| fail(format!("invalid {what}")))
}

const ADJECTIVES: [&str; 64] = [
    "amber", "bold", "brave", "brisk", "calm", "clever", "cosmic", "crisp", "curly", "dapper", "eager", "fancy",
    "fluffy", "fuzzy", "gentle", "giddy", "glossy", "golden", "grumpy", "happy", "hasty", "humble", "icy", "jolly",
    "keen", "lazy", "lively", "lucky", "mellow", "merry", "mighty", "misty", "nimble", "noble", "odd", "plucky",
    "polite", "proud", "quick", "quiet", "rapid", "rusty", "shiny", "shy", "silly", "sleepy", "sly", "smooth",
    "snappy", "sneaky", "sparkly", "speedy", "spicy", "steady", "sunny", "swift", "tidy", "tiny", "witty",
    "wobbly", "zany", "zesty", "breezy", "dusty",
];

const ANIMALS: [&str; 64] = [
    "badger", "beaver", "bison", "bobcat", "camel", "cobra", "condor", "coyote", "crane", "cricket", "dingo",
    "dolphin", "donkey", "eagle", "falcon", "ferret", "finch", "gecko", "gopher", "heron", "hornet", "hyena",
    "ibis", "iguana", "jackal", "jaguar", "koala", "lemur", "lizard", "llama", "lynx", "magpie", "marmot", "mink",
    "moose", "newt", "ocelot", "orca", "osprey", "otter", "panda", "parrot", "pelican", "puffin", "python",
    "quail", "rabbit", "raven", "salmon", "seal", "shrew", "skunk", "sloth", "stoat", "tapir", "toad", "toucan",
    "turtle", "viper", "walrus", "weasel", "wombat", "yak", "zebra",
];

/// A random adjective-animal name such as `swift-otter`.
fn random_name() -> String {
    let random = uuid::Uuid::new_v4().as_u128();
    let adjective = ADJECTIVES[(random % 64) as usize];
    let animal = ANIMALS[(random / 64 % 64) as usize];
    format!("{adjective}-{animal}")
}

fn run(args: &[String]) {
    let started = Instant::now();
    let mut name = None;
    let mut window = false;
    let mut wait_network = false;
    let mut mounts = Vec::new();
    let mut environment = Vec::new();
    let mut seeds = Vec::new();
    let mut hardware = DEFAULT_HARDWARE;
    let mut hardware_flags = false;
    let mut image = None;
    let mut command = Vec::new();

    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            command = args.by_ref().cloned().collect();
            break;
        }
        match arg.as_str() {
            "--window" => window = true,
            "--wait-network" => wait_network = true,
            "--env" => environment.push(args.next().unwrap_or_else(|| usage_error("run")).clone()),
            "--seed" => seeds.push(args.next().unwrap_or_else(|| usage_error("run")).clone()),
            "--mount" => {
                let spec = args.next().unwrap_or_else(|| fail("--mount needs HOST:GUEST[:ro]"));
                mounts.push(Mount::parse(spec).unwrap_or_else(|error| fail(error)));
                // One share is reserved for the seeds.
                if mounts.len() >= MAX_SHARES {
                    fail(format!("at most {} mounts are supported", MAX_SHARES - 1));
                }
            }
            "--base" => {
                image = Some(args.next().unwrap_or_else(|| usage_error("run")).clone());
                hardware_flags = true;
            }
            "--persist" => {
                hardware.persist = true;
                hardware_flags = true;
            }
            "--cpus" => {
                hardware.cpus = parse(args.next(), "--cpus");
                hardware_flags = true;
            }
            "--memory" => {
                hardware.memory_mib = parse(args.next(), "--memory");
                hardware_flags = true;
            }
            "--disk" => {
                hardware.disk_gib = parse(args.next(), "--disk");
                hardware_flags = true;
            }
            "--size" => {
                let size = args.next().unwrap_or_else(|| fail("--size needs WxH"));
                let (width, height) = size.split_once('x').unwrap_or_else(|| fail("--size needs WxH"));
                hardware.width = parse(Some(&width.to_owned()), "--size width");
                hardware.height = parse(Some(&height.to_owned()), "--size height");
                hardware_flags = true;
            }
            _ if name.is_none() && !arg.starts_with('-') => name = Some(arg.clone()),
            _ => usage_error("run"),
        }
    }

    let environment = config::guest_environment(&environment).unwrap_or_else(|error| fail(error));
    let seeds = config::seeds(&seeds).unwrap_or_else(|error| fail(error));
    instance::prune();
    let launch = match name {
        Some(name) if Instance::open(&name).is_ok() => {
            if hardware_flags {
                fail(format!("VM {name} already exists; its hardware and base cannot be changed"));
            }
            instance::restart(&name)
        }
        Some(name) => instance::create(&name, &hardware, image.as_deref()),
        None => {
            let name = random_name();
            instance::create(&name, &hardware, image.as_deref())
        }
    }
    .unwrap_or_else(|error| fail(error));

    let name = launch.instance.name.clone();
    let log = launch.instance.log();
    spawn_vmm(launch, window, &mounts, &seeds, &environment);

    let instance = open(&name);
    let (_, reply) = request(&instance, &Request::Wait).unwrap_or_else(|_| {
        let log = fs::read_to_string(&log).unwrap_or_default();
        let tail: Vec<&str> = log.lines().rev().take(20).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        fail(format!("VM {name} exited during boot:\n{}", tail.join("\n")))
    });

    if wait_network {
        wait_for_network(&instance);
    }

    eprintln!(
        "{name} ready in {:.2}s (guest boot {:.2}s)",
        started.elapsed().as_secs_f64(),
        reply.boot_ms.expect("wait reply without boot time") as f64 / 1000.0
    );
    if command.is_empty() {
        println!("{name}");
        // stdin, not stdout: `vm=$(varmint run)` in a shell pipes stdout but still copies.
        if io::stdin().is_terminal() {
            objc2::rc::autoreleasepool(|_| clipboard::Pasteboard::general().write_text(name.as_bytes()));
        }
        return;
    }

    // Run the command, then stop the VM; an ephemeral VM is gone afterwards.
    let (session, interactive) = exec_session(command);
    let code = run_session(&instance, session, interactive, true);
    stop(&instance);
    std::process::exit(code);
}

/// Blocks until the guest has a configured link and a running DNS resolver.
/// Both waits are systemd's own, so nothing here polls.
fn wait_for_network(instance: &Instance) {
    let argv = [
        "sudo",
        "sh",
        "-c",
        "/usr/lib/systemd/systemd-networkd-wait-online --any --timeout=30 && systemctl start systemd-resolved.service",
    ];
    let session = Session::Exec {
        argv: argv.map(str::to_owned).to_vec(),
    };
    let code = run_session(instance, session, false, false);
    if code != 0 {
        fail(format!("network in VM {} did not come up (exit code {code})", instance.name));
    }
}

fn spawn_vmm(launch: Launch, window: bool, mounts: &[Mount], seeds: &[Seed], environment: &BTreeMap<String, String>) {
    let lock_fd = launch.lock.as_raw_fd();
    let listen_fd = launch.listener.as_raw_fd();
    set_inheritable(lock_fd, true);
    set_inheritable(listen_fd, true);

    let log = File::create(launch.instance.log()).unwrap_or_else(|error| fail(format!("failed to create log: {error}")));
    // Resolve symlinks: the VM process finds its helpers next to the real binary.
    let executable = env::current_exe()
        .and_then(|path| path.canonicalize())
        .unwrap_or_else(|error| fail(error));
    let mut command = Command::new(executable);
    command
        .arg("__vmm")
        .arg(&launch.instance.name)
        .arg(lock_fd.to_string())
        .arg(listen_fd.to_string())
        .env(GUEST_ENVIRONMENT_VARIABLE, serde_json::to_string(environment).unwrap())
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap_or_else(|error| fail(error)))
        .stderr(log)
        .process_group(0);
    if window {
        command.arg("--window");
    }
    for mount in mounts {
        command.arg("--mount").arg(mount.spec());
    }
    for seed in seeds {
        command.arg("--seed").arg(seed.spec());
    }
    command
        .spawn()
        .unwrap_or_else(|error| fail(format!("failed to start VM process: {error}")));
}

fn set_inheritable(fd: RawFd, inheritable: bool) {
    let flags = if inheritable { 0 } else { libc::FD_CLOEXEC };
    assert!(unsafe { libc::fcntl(fd, libc::F_SETFD, flags) } == 0, "fcntl failed: {}", io::Error::last_os_error());
}

/// The virtio-fs server pins every directory the guest has cached with an open
/// fd, so the default soft limit of 256 runs out on any real project.
fn raise_open_file_limit() {
    let mut per_process: libc::c_int = 0;
    let mut size = size_of::<libc::c_int>();
    let result = unsafe {
        libc::sysctlbyname(
            c"kern.maxfilesperproc".as_ptr(),
            (&raw mut per_process).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    assert!(result == 0, "sysctl kern.maxfilesperproc failed: {}", io::Error::last_os_error());
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    assert!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0, "getrlimit failed: {}", io::Error::last_os_error());
    limit.rlim_cur = limit.rlim_max.min(per_process as libc::rlim_t);
    assert!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } == 0, "setrlimit failed: {}", io::Error::last_os_error());
}

/// Entry point of the detached VM process started by `varmint run`.
fn vmm_process(args: &[String]) {
    let [name, lock_fd, listen_fd, rest @ ..] = args else {
        fail("internal: __vmm NAME LOCK_FD LISTEN_FD [--window] [--mount SPEC]...");
    };
    raise_open_file_limit();
    let mut window = false;
    let mut mounts = Vec::new();
    let mut seeds = Vec::new();
    let mut rest = rest.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--window" => window = true,
            "--mount" => {
                let spec = rest.next().unwrap_or_else(|| fail("internal: --mount needs a spec"));
                mounts.push(Mount::parse(spec).unwrap_or_else(|error| fail(error)));
            }
            "--seed" => {
                let spec = rest.next().unwrap_or_else(|| fail("internal: --seed needs a spec"));
                seeds.push(Seed::parse(spec).unwrap_or_else(|error| fail(error)));
            }
            _ => fail(format!("internal: unexpected argument {arg}")),
        }
    }
    let lock_fd: RawFd = parse(Some(lock_fd), "lock fd");
    let listen_fd: RawFd = parse(Some(listen_fd), "listen fd");

    // Helpers spawned by the VM must not keep the lock or the socket alive.
    set_inheritable(lock_fd, false);
    set_inheritable(listen_fd, false);
    let launch = Launch {
        instance: open(name),
        lock: unsafe { File::from_raw_fd(lock_fd) },
        listener: unsafe { UnixListener::from_raw_fd(listen_fd) },
    };

    let environment = env::var(GUEST_ENVIRONMENT_VARIABLE)
        .ok()
        .and_then(|json| serde_json::from_str::<BTreeMap<String, String>>(&json).ok())
        .unwrap_or_else(|| fail("internal: missing guest environment"));
    // Helpers started by the VM must not see the guest's secrets.
    unsafe { env::remove_var(GUEST_ENVIRONMENT_VARIABLE) };

    vmm::run(launch, window, &mounts, &seeds, environment).unwrap_or_else(|error| panic!("VM failed: {error:?}"));
}

fn list() {
    instance::prune();
    let instances = instance::list();
    // In pipes only the names, one per line, as in `varmint ls | varmint rm`.
    if !io::stdout().is_terminal() {
        for instance in instances {
            println!("{}", instance.name);
        }
        return;
    }
    if instances.is_empty() {
        println!("No VMs. Start one with: varmint run");
        return;
    }
    println!("{:<20} {:<8} {:<8} {:>4} {:>8} {:>8}", "NAME", "STATE", "PERSIST", "CPUS", "MEMORY", "DISK");
    for instance in instances {
        let hardware = instance.hardware().unwrap_or_else(|error| fail(error));
        println!(
            "{:<20} {:<8} {:<8} {:>4} {:>6}Mi {:>6}Gi",
            instance.name,
            if instance.is_running() { "running" } else { "stopped" },
            if hardware.persist { "yes" } else { "no" },
            hardware.cpus,
            hardware.memory_mib,
            hardware.disk_gib,
        );
    }
}

fn stop(instance: &Instance) {
    let persist = instance.hardware().unwrap_or_else(|error| fail(error)).persist;
    let lock = instance.lock_file();
    if persist {
        let code = run_session(instance, Session::Exec { argv: vec!["sync".into()] }, false, false);
        if code != 0 {
            fail(format!("sync in VM {} failed with exit code {code}", instance.name));
        }
    }
    request(instance, &Request::Stop).unwrap_or_else(|error| fail(error));
    // Blocks until the VM process has exited and released its lock.
    lock.lock().unwrap_or_else(|error| fail(error));
}

/// Stops a persistent VM and saves its disk as an image for `run --base`.
fn save(args: &[String]) {
    let [name, image] = args else {
        usage_error("save");
    };
    let instance = open(name);
    if !instance.hardware().unwrap_or_else(|error| fail(error)).persist {
        fail(format!("VM {name} is ephemeral and would be deleted when it stops; only persistent VMs can be saved"));
    }
    if !instance.is_running() {
        fail(format!("VM {name} is stopped; start it with `varmint run {name}` so its identity can be cleared"));
    }
    instance::new_image(image).unwrap_or_else(|error| fail(error));

    // Like the published base image: VMs cloned from the image must not share
    // a machine id or SSH host keys. The guest generates new ones at boot.
    let argv = [
        "sudo",
        "sh",
        "-c",
        "truncate -s 0 /etc/machine-id && rm -f /var/lib/dbus/machine-id /etc/ssh/ssh_host_*",
    ];
    let session = Session::Exec {
        argv: argv.map(str::to_owned).to_vec(),
    };
    let code = run_session(&instance, session, false, false);
    if code != 0 {
        fail(format!("clearing the identity of VM {name} failed with exit code {code}"));
    }
    stop(&instance);
    instance::save(&instance, image).unwrap_or_else(|error| fail(error));
    eprintln!("saved {name} as image {image}; start VMs from it with `varmint run --base {image}`");
}

/// The NAMEs, or without them the names on stdin, one per line, as in `varmint ls | varmint rm`.
fn names_or_stdin(command: &str, args: &[String]) -> Vec<String> {
    let names: Vec<String> = match args {
        [] if io::stdin().is_terminal() => usage_error(command),
        [] => io::stdin().lock().lines().map(|line| line.unwrap_or_else(|error| fail(error))).collect(),
        names => names.to_vec(),
    };
    names.iter().map(|name| name.trim().to_owned()).filter(|name| !name.is_empty()).collect()
}

fn stop_all(args: &[String]) {
    for name in names_or_stdin("stop", args) {
        let instance = open(&name);
        if instance.is_running() {
            stop(&instance);
        }
    }
}

fn remove(args: &[String]) {
    for name in names_or_stdin("rm", args) {
        let instance = open(&name);
        if instance.is_running() {
            stop(&instance);
        }
        if instance.dir.exists() {
            instance::remove(&instance);
        }
    }
}

const DEFAULT_SERVE_ADDRESS: &str = "127.0.0.1:7420";

fn serve_command(args: &[String]) {
    let mut ui = false;
    let (mut cert, mut key, mut address) = (None, None, None);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--ui" => ui = true,
            "--cert" => cert = Some(PathBuf::from(args.next().unwrap_or_else(|| usage_error("serve")))),
            "--key" => key = Some(PathBuf::from(args.next().unwrap_or_else(|| usage_error("serve")))),
            _ if address.is_none() => address = Some(arg.as_str()),
            _ => usage_error("serve"),
        }
    }
    let address = address.unwrap_or(DEFAULT_SERVE_ADDRESS);
    let tls = match (cert, key) {
        (Some(cert), Some(key)) => Some(serve::tls::config(&cert, &key).unwrap_or_else(|error| fail(error))),
        (None, None) => None,
        _ => fail("--cert and --key go together"),
    };
    let token = serve::token().unwrap_or_else(|error| fail(error));
    let listener =
        TcpListener::bind(address).unwrap_or_else(|error| fail(format!("failed to listen on {address}: {error}")));
    // stderr is the event log, so this goes to stdout.
    println!("serving on {address}; clients authenticate with the token in {}", serve::token_path().display());
    if ui {
        let scheme = if tls.is_some() { "https" } else { "http" };
        println!("web UI: {scheme}://{address}/#token={token}");
    }
    serve::serve(listener, &token, ui, tls)
}

fn split_command(command: &str, args: &[String]) -> Vec<String> {
    match args.iter().position(|arg| arg == "--") {
        Some(index) => args[index + 1..].to_vec(),
        None if args.is_empty() => Vec::new(),
        None => usage_error(command),
    }
}

fn pty_session(argv: Vec<String>, interactive: bool) -> Session {
    let (cols, rows) = if interactive { terminal::size().unwrap_or_else(|error| fail(error)) } else { (80, 24) };
    Session::Pty {
        argv,
        cols,
        rows,
        terminal: terminal_environment(),
    }
}

/// Programs like Claude Code only enable features such as Shift+Enter in terminals they recognize.
fn terminal_environment() -> BTreeMap<String, String> {
    let mut environment: BTreeMap<String, String> = ["TERM_PROGRAM", "TERM_PROGRAM_VERSION", "COLORTERM"]
        .into_iter()
        .filter_map(|key| Some((key.to_owned(), env::var(key).ok()?)))
        .collect();
    environment.insert("TERM".into(), env::var("TERM").unwrap_or_else(|_| "xterm-256color".into()));
    environment
}

fn exec(args: &[String]) {
    // Without NAME it comes from stdin, as in `varmint run | varmint exec -- bash`.
    let (name, command) = match args.split_first() {
        Some((name, command)) if name != "--" => (name.clone(), command),
        _ if io::stdin().is_terminal() => usage_error("exec"),
        _ => (name_from_stdin(), args),
    };
    // The pipe only carried the name; in a terminal the session reads the keyboard.
    if io::stdout().is_terminal() {
        let tty = File::open("/dev/tty").unwrap_or_else(|error| fail(format!("failed to open /dev/tty: {error}")));
        assert!(unsafe { libc::dup2(tty.as_raw_fd(), libc::STDIN_FILENO) } != -1, "dup2 failed: {}", io::Error::last_os_error());
    }
    let instance = open(&name);
    let (session, interactive) = exec_session(split_command("exec", command));
    std::process::exit(run_session(&instance, session, interactive, true));
}

/// Like ssh: a terminal at both ends gets one in the guest, so interactive
/// programs work; scripts and pipes keep separate stdout and stderr. Without
/// a command it is a login shell.
fn exec_session(argv: Vec<String>) -> (Session, bool) {
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    let session = match (interactive, argv.is_empty()) {
        (true, _) => pty_session(argv, true),
        (false, true) => Session::Exec {
            argv: vec!["bash".into(), "-l".into()],
        },
        (false, false) => Session::Exec { argv },
    };
    (session, interactive)
}

fn proxy(args: &[String]) {
    let [name, port] = args else { usage_error("proxy") };
    let instance = open(name);
    let port = parse(Some(port), "port");
    std::process::exit(run_session(&instance, Session::Tcp { port }, false, true));
}

fn ssh(args: &[String]) {
    let name = name_arg("ssh", args);
    open(name);
    let key = instance::ssh_key();
    let executable = env::current_exe()
        .and_then(|path| path.canonicalize())
        .unwrap_or_else(|error| fail(error));
    let error = Command::new("ssh")
        .arg("-o")
        .arg(format!("ProxyCommand='{}' proxy {name} 22", executable.display()))
        .args(["-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null", "-o", "LogLevel=ERROR"])
        .arg("-i")
        .arg(key)
        .arg(format!("varmint@{name}"))
        .args(&args[1..])
        .exec();
    fail(format!("failed to run ssh: {error}"))
}

/// Streams stdin/stdout through a guest session and returns its exit code.
fn run_session(instance: &Instance, session: Session, raw: bool, forward_stdin: bool) -> i32 {
    let stream = instance.connect().unwrap_or_else(|error| fail(error));
    let mut writer = stream.try_clone().unwrap_or_else(|error| fail(error));
    protocol::write_json(&mut writer, &Request::Open { session }).unwrap_or_else(|error| fail(error));
    let mut reader = BufReader::new(stream);

    if raw {
        terminal::enable_raw_mode().unwrap_or_else(|error| fail(error));
    }
    let restore = || {
        if raw {
            let _ = terminal::disable_raw_mode();
        }
    };

    if !forward_stdin {
        protocol::write_frame(&mut writer, &Frame::new(0, kind::EOF, Vec::new())).unwrap_or_else(|error| fail(error));
    }
    // Stdin and resize frames come from different threads; whole frames only.
    let writer = Arc::new(Mutex::new(writer));
    if forward_stdin {
        let writer = writer.clone();
        thread::spawn(move || pump_stdin(&writer, raw));
    }
    if raw {
        forward_resizes(writer);
    }

    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    loop {
        let frame = match protocol::read_frame(&mut reader) {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                restore();
                fail("connection to the VM closed")
            }
            Err(error) => {
                restore();
                fail(error)
            }
        };
        match frame.kind {
            kind::DATA => {
                let _ = stdout.write_all(&frame.payload);
                let _ = stdout.flush();
            }
            kind::STDERR => {
                let _ = stderr.write_all(&frame.payload);
            }
            kind::EXIT => {
                restore();
                return i32::from_le_bytes(frame.payload.try_into().expect("malformed exit frame"));
            }
            other => {
                restore();
                fail(format!("unexpected frame kind {other} from the VM"))
            }
        }
    }
}

/// Ctrl+V as a control byte, a CSI u key and an xterm modifyOtherKeys key.
const PASTE_KEYS: [&[u8]; 3] = [b"\x16", b"\x1b[118;5u", b"\x1b[27;5;118~"];

/// With `paste_images`, Ctrl+V first puts the host clipboard image into the guest
/// clipboard, so programs like Claude Code can paste it. Text is pasted by the terminal.
fn pump_stdin(writer: &Mutex<UnixStream>, paste_images: bool) {
    let mut stdin = io::stdin();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let frame = match stdin.read(&mut buffer) {
            Ok(0) | Err(_) => Frame::new(0, kind::EOF, Vec::new()),
            Ok(n) => Frame::new(0, kind::DATA, buffer[..n].to_vec()),
        };
        let eof = frame.kind == kind::EOF;
        let mut writer = writer.lock().unwrap();
        if paste_images
            && PASTE_KEYS.iter().any(|key| frame.payload.windows(key.len()).any(|window| window == *key))
            && protocol::write_frame(&mut *writer, &clipboard_image_frame()).is_err()
        {
            return;
        }
        if protocol::write_frame(&mut *writer, &frame).is_err() || eof {
            return;
        }
    }
}

/// The host clipboard image, or an empty frame that clears the guest clipboard.
fn clipboard_image_frame() -> Frame {
    let mut png = objc2::rc::autoreleasepool(|_| clipboard::Pasteboard::general().read_png()).unwrap_or_default();
    if png.len() > protocol::MAX_PAYLOAD {
        eprint!("varmint: the clipboard image is too large for the VM ({} bytes)\r\n", png.len());
        png.clear();
    }
    Frame::new(0, kind::CLIPBOARD_IMAGE, png)
}

/// Sends the terminal size to the guest pty whenever the window changes.
fn forward_resizes(writer: Arc<Mutex<UnixStream>>) {
    screen::on_window_change(move || {
        let Ok((cols, rows)) = terminal::size() else {
            return;
        };
        let mut payload = cols.to_le_bytes().to_vec();
        payload.extend_from_slice(&rows.to_le_bytes());
        let _ = protocol::write_frame(&mut *writer.lock().unwrap(), &Frame::new(0, kind::RESIZE, payload));
    });
}

fn request(instance: &Instance, request: &Request) -> Result<(BufReader<UnixStream>, Reply), String> {
    let mut stream = instance.connect()?;
    protocol::write_json(&mut stream, request).map_err(|error| error.to_string())?;
    let mut reader = BufReader::new(stream);
    let reply: Reply = protocol::read_json(&mut reader).map_err(|error| error.to_string())?;
    match reply.error {
        Some(error) => Err(error),
        None => Ok((reader, reply)),
    }
}

fn send_input(name: &str, events: Vec<InputEvent>) {
    request(&open(name), &Request::Input { events }).unwrap_or_else(|error| fail(error));
}

fn screenshot(args: &[String]) {
    let name = name_arg("screenshot", args);
    let path = args.get(1).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(format!("{name}.png")));
    let (mut reader, reply) = request(&open(name), &Request::Screenshot).unwrap_or_else(|error| fail(error));
    let mut png = vec![0; reply.png_len.expect("screenshot reply without length")];
    reader.read_exact(&mut png).unwrap_or_else(|error| fail(error));

    if path.as_os_str() == "-" {
        io::stdout().write_all(&png).unwrap_or_else(|error| fail(error));
    } else {
        fs::write(&path, png).unwrap_or_else(|error| fail(format!("failed to write {}: {error}", path.display())));
        println!("{}", path.display());
    }
}

fn screen_command(args: &[String]) {
    if !io::stdout().is_terminal() {
        fail("varmint screen draws into a terminal; use `varmint screenshot` to save images");
    }
    // Without NAME it comes from stdin, as in `varmint run | varmint screen`.
    let (name, options) = match args.split_first() {
        Some((name, options)) if !name.starts_with("--") => (name.clone(), options),
        _ if io::stdin().is_terminal() => usage_error("screen"),
        _ => (name_from_stdin(), args),
    };
    let fps = match options {
        [] => 30,
        [flag, value] if flag == "--fps" => parse(Some(value), "--fps"),
        _ => usage_error("screen"),
    };

    let instance = open(&name);
    let (frames, _) = request(&instance, &Request::Stream {
            fps,
            codec: Codec::Rgb,
            audio: false,
        }).unwrap_or_else(|error| fail(error));
    let (input, _) = request(&instance, &Request::InputStream).unwrap_or_else(|error| fail(error));
    let resize = |width, height| {
        request(&instance, &Request::Resize { width, height })
            .map(drop)
            .map_err(io::Error::other)
    };
    let error = screen::run(frames, input.into_inner(), resize).unwrap_err();
    fail(format!("VM {name}: {error}"))
}

fn stream(args: &[String]) {
    let name = name_arg("stream", args);
    let mut fps = 30;
    let mut audio = true;
    let mut options = args[1..].iter();
    while let Some(option) = options.next() {
        match option.as_str() {
            "--fps" => fps = parse(options.next(), "--fps"),
            "--no-audio" => audio = false,
            _ => usage_error("stream"),
        }
    }

    let (mut media, _) = request(&open(name), &Request::Stream {
            fps,
            codec: Codec::H264,
            audio,
        }).unwrap_or_else(|error| fail(error));
    // Unbuffered, since a frame held back until the next one is a frame late.
    let mut stdout = unsafe { File::from_raw_fd(io::stdout().as_raw_fd()) };
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let read = match media.read(&mut buffer) {
            Ok(0) => std::process::exit(0),
            Ok(read) => read,
            Err(error) => fail(format!("VM {name}: {error}")),
        };
        match stdout.write_all(&buffer[..read]) {
            Ok(()) => {}
            // The reader went away, which is how a stream is meant to end.
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => std::process::exit(0),
            Err(error) => fail(error),
        }
    }
}

fn input(args: &[String]) {
    let [name] = args else { usage_error("input") };
    let (events, _) = request(&open(name), &Request::InputStream).unwrap_or_else(|error| fail(error));
    let mut events = events.into_inner();
    for line in io::stdin().lock().lines() {
        let line = line.unwrap_or_else(|error| fail(error));
        if line.trim().is_empty() {
            continue;
        }
        let event: InputEvent =
            serde_json::from_str(&line).unwrap_or_else(|error| fail(format!("not an input event: {line}: {error}")));
        protocol::write_json(&mut events, &event).unwrap_or_else(|error| fail(format!("VM {name}: {error}")));
    }
}

/// The first line of stdin, which `varmint run` writes once the VM is ready.
fn name_from_stdin() -> String {
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line).unwrap_or_else(|error| fail(error));
    let name = line.trim();
    if name.is_empty() {
        fail("no VM name on stdin");
    }
    name.to_owned()
}

fn move_pointer(args: &[String]) {
    let [name, x, y] = args else { usage_error("move") };
    send_input(
        name,
        vec![InputEvent::Move {
            x: parse(Some(x), "x"),
            y: parse(Some(y), "y"),
        }],
    );
}

fn click(args: &[String]) {
    let name = name_arg("click", args);
    let mut button = BTN_LEFT;
    let mut clicks = 1;
    let mut position = Vec::new();
    for arg in &args[1..] {
        match arg.as_str() {
            "--right" => button = BTN_RIGHT,
            "--middle" => button = BTN_MIDDLE,
            "--double" => clicks = 2,
            _ => position.push(parse::<u32>(Some(arg), "coordinate")),
        }
    }

    let mut events = match position.as_slice() {
        [] => Vec::new(),
        [x, y] => vec![InputEvent::Move { x: *x, y: *y }],
        _ => usage_error("click"),
    };
    for _ in 0..clicks {
        events.push(InputEvent::Button { button, down: true });
        events.push(InputEvent::Button { button, down: false });
    }
    send_input(name, events);
}

fn type_text(args: &[String]) {
    let [name, text] = args else { usage_error("type") };
    let mut events = Vec::new();
    for c in text.chars() {
        let (code, shift) = keymap::char_key(c).unwrap_or_else(|| fail(format!("cannot type {c:?}")));
        if shift {
            events.push(InputEvent::Key {
                code: KEY_LEFTSHIFT,
                down: true,
            });
        }
        events.push(InputEvent::Key { code, down: true });
        events.push(InputEvent::Key { code, down: false });
        if shift {
            events.push(InputEvent::Key {
                code: KEY_LEFTSHIFT,
                down: false,
            });
        }
    }
    send_input(name, events);
}

fn key(args: &[String]) {
    let name = name_arg("key", args);
    if args.len() < 2 {
        usage_error("key");
    }

    let mut events = Vec::new();
    for combo in &args[1..] {
        let codes: Vec<u16> = combo
            .split('+')
            .map(|key| keymap::named_key(key).unwrap_or_else(|| fail(format!("unknown key {key:?}"))))
            .collect();
        events.extend(codes.iter().map(|&code| InputEvent::Key { code, down: true }));
        events.extend(codes.iter().rev().map(|&code| InputEvent::Key { code, down: false }));
    }
    send_input(name, events);
}

fn scroll(args: &[String]) {
    let name = name_arg("scroll", args);
    let value = parse(args.get(1), "scroll amount");
    let horizontal = args[2..].iter().any(|arg| arg == "--horizontal");
    send_input(name, vec![InputEvent::Scroll { value, horizontal }]);
}
