//! `varmint serve`: VM provisioning over TCP, one JSON request and one JSON
//! reply per connection. Requests run the varmint CLI, so a failing request
//! cannot take the server down and behaves exactly like the command.
//!
//! The log on stderr is JSON Lines of CloudEvents 1.0, for programs to act
//! on. Events of a connection carry the extension attributes `peer` (client
//! address) and `requestid`, shared by all events of one connection. Types, with
//! their `data`:
//!
//! - `dev.varmint.serve.request`: the request as received, without its token
//!   and with only the names of `env`
//! - `dev.varmint.serve.exec`: `args`, `exit_code` (null if killed by a
//!   signal) and `duration_ms` of a finished varmint command
//! - `dev.varmint.serve.reply`: the reply as sent
//! - `dev.varmint.serve.error`: `message` of a connection that failed, such
//!   as a client that left before the reply, or of a failed accept, which has
//!   no `peer` or `requestid`

mod forward;
pub mod tls;
mod web;

use crate::{
    config::{GUEST_ENVIRONMENT_VARIABLE, Hardware},
    instance, protocol,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    env,
    fs::{self, OpenOptions},
    io::{self, BufRead, BufReader, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        net::UnixStream,
    },
    path::PathBuf,
    process::{Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// How long a client may take to send its whole request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Requests are a few hundred bytes; a longer line is cut off and fails to parse.
const MAX_REQUEST_BYTES: u64 = 64 * 1024;

/// Pause after a failed accept, so a lasting error such as running out of
/// file descriptors does not spin the loop.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Hardware fields only apply when a VM is created, like the `run` flags.
/// `env` holds the guest variables, which replace the server's own
/// config::PASSED_VARIABLES; they reach `varmint run` in
/// GUEST_ENVIRONMENT_VARIABLE, never its arguments or its own variables.
/// Mounts and seeds are left out on purpose: they name host files, which a
/// remote client must not reach.
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "lowercase", deny_unknown_fields)]
enum Request {
    Run {
        name: Option<String>,
        base: Option<String>,
        #[serde(default)]
        persist: bool,
        cpus: Option<usize>,
        memory_mib: Option<u64>,
        disk_gib: Option<u64>,
        size: Option<(u32, u32)>,
        #[serde(default)]
        wait_network: bool,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    Ls,
    Stop {
        name: String,
    },
    Rm {
        name: String,
    },
    /// The addresses and listening ports of a running VM, asked of the guest.
    Network {
        name: String,
    },
    /// Makes a guest port reachable on the host's loopback, so browsers treat it as `localhost`.
    Forward {
        name: String,
        port: u16,
    },
    Unforward {
        name: String,
        port: u16,
    },
    /// Hands the connection to the VM's control socket with `request` as its
    /// first line, for screen streams, input and shells.
    Attach {
        name: String,
        request: protocol::Request,
    },
}

#[derive(Serialize, Default)]
struct Reply {
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vms: Option<Vec<Vm>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<Host>,
    #[serde(skip_serializing_if = "Option::is_none")]
    network: Option<Network>,
    /// The host port a forward listens on.
    #[serde(skip_serializing_if = "Option::is_none")]
    local_port: Option<u16>,
}

#[derive(Serialize)]
struct Network {
    /// Global IPv4 addresses of the guest.
    addresses: Vec<String>,
    ports: Vec<Port>,
}

#[derive(Serialize, PartialEq, Eq, PartialOrd, Ord)]
struct Port {
    port: u16,
    /// Only reachable from inside the guest, such as through `varmint ssh -L`.
    loopback: bool,
    /// The host port it is forwarded to.
    forwarded: Option<u16>,
}

#[derive(Serialize)]
struct Vm {
    name: String,
    running: bool,
    hardware: Hardware,
    /// What the VM process takes from the host, while it runs.
    memory_bytes: Option<u64>,
    /// CPU time the VM process used since it started, while it runs.
    cpu_ms: Option<u64>,
}

#[derive(Serialize)]
struct Host {
    memory_bytes: u64,
    /// Counted like Activity Monitor's "Memory Used" on macOS: app memory, wired and compressed.
    used_bytes: u64,
    /// CPU time of all cores since boot: busy, and busy plus idle.
    cpu_busy_ms: u64,
    cpu_total_ms: u64,
}

pub fn token_path() -> PathBuf {
    instance::home().join("serve-token")
}

/// The token clients must send, created on first use.
pub fn token() -> Result<String, String> {
    let path = token_path();
    if !path.exists() {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| format!("failed to create {}: {error}", path.display()))?;
        file.write_all(uuid::Uuid::new_v4().simple().to_string().as_bytes())
            .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
    }
    let metadata = fs::metadata(&path).map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "{} must only be readable by you; run: chmod 600 {}",
            path.display(),
            path.display()
        ));
    }
    let token = fs::read_to_string(&path).map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let token = token.trim().to_owned();
    if token.is_empty() {
        return Err(format!("{} is empty", path.display()));
    }
    Ok(token)
}

/// With `ui`, connections that do not start with a JSON request are HTTP,
/// answered by [`web`] with the web UI and its API. With `tls`, every
/// connection is TLS.
pub fn serve(listener: TcpListener, token: &str, ui: bool, tls: Option<Arc<rustls::ServerConfig>>) -> ! {
    thread::scope(|scope| {
        loop {
            let (connection, peer) = match listener.accept() {
                Ok(accepted) => accepted,
                Err(error) => {
                    emit("error", None, json!({ "message": format!("accept failed: {error}") }));
                    thread::sleep(ACCEPT_RETRY_DELAY);
                    continue;
                }
            };
            let client = Client {
                peer,
                request_id: uuid::Uuid::new_v4().to_string(),
            };
            let tls = tls.clone();
            scope.spawn(move || {
                let handled = match tls {
                    Some(tls) => tls::wrap(connection, tls, |connection| handle(connection, &client, token, ui)),
                    None => handle(connection, &client, token, ui),
                };
                if let Err(error) = handled {
                    client.emit("error", json!({ "message": error.to_string() }));
                }
            });
        }
    })
}

struct Client {
    peer: SocketAddr,
    request_id: String,
}

impl Client {
    fn emit(&self, kind: &str, data: Value) {
        emit(kind, Some(self), data);
    }
}

/// Writes one CloudEvent as a line on stderr.
fn emit(kind: &str, client: Option<&Client>, data: Value) {
    let mut event = json!({
        "specversion": "1.0",
        "id": uuid::Uuid::new_v4().to_string(),
        "source": "varmint/serve",
        "type": format!("dev.varmint.serve.{kind}"),
        "time": rfc3339(SystemTime::now()),
        "data": data,
    });
    if let Some(client) = client {
        event["peer"] = json!(client.peer.to_string());
        event["requestid"] = json!(client.request_id);
    }
    protocol::write_json(&mut io::stderr().lock(), &event).expect("failed to write the serve log");
}

/// A connection from a client: a TCP stream, or the plaintext end of a TLS one.
trait Connection: Read + Write + Send + Sized {
    fn try_clone(&self) -> io::Result<Self>;
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
    fn shutdown(&self, how: Shutdown) -> io::Result<()>;
}

impl Connection for TcpStream {
    fn try_clone(&self) -> io::Result<Self> {
        TcpStream::try_clone(self)
    }
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        TcpStream::set_read_timeout(self, timeout)
    }
    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        TcpStream::shutdown(self, how)
    }
}

impl Connection for UnixStream {
    fn try_clone(&self) -> io::Result<Self> {
        UnixStream::try_clone(self)
    }
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        UnixStream::set_read_timeout(self, timeout)
    }
    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        UnixStream::shutdown(self, how)
    }
}

/// Reads from a client until a deadline for the whole request, so a client
/// that sends a byte at a time cannot hold its connection open.
struct Deadline<C> {
    stream: C,
    deadline: Instant,
}

impl<C: Connection> Read for Deadline<C> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(timed_out());
        }
        self.stream.set_read_timeout(Some(remaining))?;
        // An expired read timeout is WouldBlock on macOS.
        self.stream.read(buffer).map_err(|error| match error.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => timed_out(),
            _ => error,
        })
    }
}

fn timed_out() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!("not sent within {}s", REQUEST_TIMEOUT.as_secs()),
    )
}

/// UTC time as RFC 3339 with milliseconds, such as 2026-10-17T12:00:00.123Z.
fn rfc3339(time: SystemTime) -> String {
    let since_epoch = time
        .duration_since(UNIX_EPOCH)
        .expect("host clock is before the unix epoch");
    let seconds = since_epoch.as_secs();
    // Days since 1970-01-01 to a civil date, Howard Hinnant's days_from_civil inverted.
    let days = seconds / 86400 + 719468;
    let era = days / 146097;
    let day_of_era = days - era * 146097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60,
        since_epoch.subsec_millis()
    )
}

fn handle(connection: impl Connection, client: &Client, token: &str, ui: bool) -> io::Result<()> {
    let deadline = Deadline {
        stream: connection.try_clone()?,
        deadline: Instant::now() + REQUEST_TIMEOUT,
    };
    let mut reader = BufReader::new(deadline.take(MAX_REQUEST_BYTES));
    let mut writer = connection;
    if ui && reader.fill_buf()?.first().is_some_and(|&byte| byte != b'{') {
        return web::handle(client, reader, writer, token);
    }
    let request = protocol::read_json(&mut reader).map_err(|error| format!("unreadable request: {error}"));
    let (reply, control) = respond(client, request, token);
    protocol::write_json(&mut writer, &reply)?;
    match control {
        Some(control) => relay(reader, writer, control),
        None => Ok(()),
    }
}

/// Answers a request and logs the reply; for `attach`, also returns the
/// connected control socket.
fn respond(client: &Client, request: Result<Value, String>, token: &str) -> (Reply, Option<UnixStream>) {
    let (reply, control) = match request.and_then(|request| authorize(client, request, token)) {
        Ok(Request::Attach { name, request }) => match attach(&name, &request) {
            Ok(control) => (Ok(Reply::default()), Some(control)),
            Err(error) => (Err(error), None),
        },
        request => (request.and_then(|request| execute(client, request)), None),
    };
    let reply = reply.unwrap_or_else(|error| Reply {
        error: Some(error),
        ..Reply::default()
    });
    client.emit("reply", serde_json::to_value(&reply).expect("a reply serializes"));
    (reply, control)
}

fn authorize(client: &Client, mut request: Value, token: &str) -> Result<Request, String> {
    let given = request.as_object_mut().and_then(|fields| fields.remove("token"));
    let mut logged = request.clone();
    if let Some(env) = logged.get_mut("env") {
        *env = json!(env.as_object().map(|variables| variables.keys().collect::<Vec<_>>()));
    }
    client.emit("request", logged);
    match given
        .as_ref()
        .and_then(Value::as_str)
        .is_some_and(|given| token_matches(given, token))
    {
        true => serde_json::from_value(request).map_err(|error| format!("invalid request: {error}")),
        false => Err("invalid token".to_owned()),
    }
}

/// Compares in constant time, so response times do not reveal the token.
fn token_matches(given: &str, token: &str) -> bool {
    given.len() == token.len() && given.bytes().zip(token.bytes()).fold(0, |diff, (a, b)| diff | (a ^ b)) == 0
}

fn execute(client: &Client, request: Request) -> Result<Reply, String> {
    match request {
        Request::Run {
            name,
            base,
            persist,
            cpus,
            memory_mib,
            disk_gib,
            size,
            wait_network,
            env,
        } => {
            let mut args = vec!["run".to_owned()];
            if let Some(name) = name {
                instance::validate_name(&name, "VM")?;
                args.push(name);
            }
            if let Some(base) = base {
                instance::validate_name(&base, "image")?;
                args.extend(["--base".to_owned(), base]);
            }
            if persist {
                args.push("--persist".into());
            }
            if let Some(cpus) = cpus {
                args.extend(["--cpus".to_owned(), cpus.to_string()]);
            }
            if let Some(memory_mib) = memory_mib {
                args.extend(["--memory".to_owned(), memory_mib.to_string()]);
            }
            if let Some(disk_gib) = disk_gib {
                args.extend(["--disk".to_owned(), disk_gib.to_string()]);
            }
            if let Some((width, height)) = size {
                args.extend(["--size".to_owned(), format!("{width}x{height}")]);
            }
            if wait_network {
                args.push("--wait-network".into());
            }
            let name = varmint(client, &args, &env)?;
            Ok(Reply {
                name: Some(name),
                ..Reply::default()
            })
        }
        Request::Ls => {
            instance::prune();
            let vms = instance::list()
                .into_iter()
                .map(|instance| -> Result<Vm, String> {
                    let usage = usage(&instance)?;
                    Ok(Vm {
                        running: usage.is_some(),
                        hardware: instance.hardware()?,
                        memory_bytes: usage.as_ref().map(|usage| usage.memory_bytes.expect("a usage reply has memory_bytes")),
                        cpu_ms: usage.map(|usage| usage.cpu_ms.expect("a usage reply has cpu_ms")),
                        name: instance.name,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            forward::retain(&vms.iter().filter(|vm| vm.running).map(|vm| vm.name.as_str()).collect::<Vec<_>>());
            Ok(Reply {
                vms: Some(vms),
                host: Some(host()),
                ..Reply::default()
            })
        }
        Request::Stop { name } => {
            instance::validate_name(&name, "VM")?;
            varmint(client, &["stop".to_owned(), name], &BTreeMap::new()).map(|_| Reply::default())
        }
        Request::Rm { name } => {
            instance::validate_name(&name, "VM")?;
            varmint(client, &["rm".to_owned(), name], &BTreeMap::new()).map(|_| Reply::default())
        }
        Request::Network { name } => Ok(Reply {
            network: Some(network(&name)?),
            ..Reply::default()
        }),
        Request::Forward { name, port } => {
            instance::validate_name(&name, "VM")?;
            if !instance::Instance::open(&name)?.is_running() {
                return Err(format!("VM {name} is not running"));
            }
            Ok(Reply {
                local_port: Some(forward::start(&name, port)?),
                ..Reply::default()
            })
        }
        Request::Unforward { name, port } => {
            instance::validate_name(&name, "VM")?;
            forward::stop(&name, port);
            Ok(Reply::default())
        }
        Request::Attach { .. } => unreachable!("attach is handled before execute"),
    }
}

/// Asks the guest of VM `name` for its addresses and listening TCP ports.
fn network(name: &str) -> Result<Network, String> {
    let addresses = guest_output(name, &["ip", "-j", "-4", "addr", "show", "scope", "global"])?;
    let addresses: Vec<Value> = serde_json::from_str(&addresses).map_err(|error| format!("unreadable ip output: {error}"))?;
    let addresses = addresses
        .iter()
        .filter_map(|interface| interface["addr_info"].as_array())
        .flatten()
        .filter_map(|address| address["local"].as_str().map(str::to_owned))
        .collect();

    let sockets = guest_output(name, &["ss", "-H", "-t", "-l", "-n"])?;
    let mut ports: Vec<Port> = sockets
        .lines()
        .filter_map(|line| line.split_whitespace().nth(3))
        .filter_map(|local| local.rsplit_once(':'))
        .filter_map(|(host, port)| {
            Some(Port {
                port: port.parse().ok()?,
                loopback: host.starts_with("127.") || host.starts_with("[::1]"),
                forwarded: None,
            })
        })
        .collect();
    ports.sort();
    // A port open on all addresses and on loopback counts once, as reachable.
    ports.dedup_by(|later, earlier| {
        let same = later.port == earlier.port;
        if same {
            earlier.loopback &= later.loopback;
        }
        same
    });
    let forwarded = forward::ports(name);
    for port in &mut ports {
        port.forwarded = forwarded.get(&port.port).copied();
    }
    Ok(Network { addresses, ports })
}

/// Runs `argv` in the guest of VM `name` and returns its stdout.
fn guest_output(name: &str, argv: &[&str]) -> Result<String, String> {
    let session = protocol::Session::Exec {
        argv: argv.iter().map(|arg| (*arg).to_owned()).collect(),
    };
    let mut control = attach(name, &protocol::Request::Open { session })?;
    control
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| error.to_string())?;
    let mut output = Vec::new();
    loop {
        let frame = protocol::read_frame(&mut control)
            .map_err(|error| format!("failed to ask VM {name} for its network: {error}"))?
            .ok_or_else(|| format!("VM {name} closed the connection"))?;
        match frame.kind {
            protocol::kind::DATA => output.extend_from_slice(&frame.payload),
            protocol::kind::EXIT => {
                return match <[u8; 4]>::try_from(frame.payload.as_slice()).map(i32::from_le_bytes) {
                    Ok(0) => Ok(String::from_utf8_lossy(&output).into_owned()),
                    _ => Err(format!("{} failed in VM {name}", argv[0])),
                };
            }
            _ => {}
        }
    }
}

/// What the process of a VM takes from the host, or None if the VM is not running.
fn usage(instance: &instance::Instance) -> Result<Option<protocol::Reply>, String> {
    if !instance.is_running() {
        return Ok(None);
    }
    let reply = instance.connect().and_then(|mut control| {
        protocol::write_json(&mut control, &protocol::Request::Usage)
            .and_then(|()| protocol::read_json::<protocol::Reply>(&mut BufReader::new(control)))
            .map_err(|error| format!("failed to ask VM {} for its usage: {error}", instance.name))
    });
    match reply {
        Ok(protocol::Reply { error: Some(error), .. }) => {
            Err(format!("VM {} did not report its usage: {error}", instance.name))
        }
        Ok(reply) => Ok(Some(reply)),
        // The VM stopped after the check.
        Err(_) if !instance.is_running() => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "macos")]
fn host() -> Host {
    use std::{mem, ptr, slice};
    let mut memory_bytes: u64 = 0;
    let mut size = size_of::<u64>();
    let result = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&raw mut memory_bytes).cast(),
            &mut size,
            ptr::null_mut(),
            0,
        )
    };
    assert!(result == 0, "sysctl hw.memsize failed: {}", io::Error::last_os_error());
    let mut stats: libc::vm_statistics64 = unsafe { mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    #[allow(deprecated)]
    let result = unsafe {
        libc::host_statistics64(libc::mach_host_self(), libc::HOST_VM_INFO64, (&raw mut stats).cast(), &mut count)
    };
    assert_eq!(result, libc::KERN_SUCCESS, "host_statistics64 failed: {result}");
    let pages = u64::from(stats.internal_page_count) - u64::from(stats.purgeable_count)
        + u64::from(stats.wire_count)
        + u64::from(stats.compressor_page_count);
    let page_bytes = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;

    let mut cpus: libc::natural_t = 0;
    let mut info: libc::processor_info_array_t = ptr::null_mut();
    let mut info_count: libc::mach_msg_type_number_t = 0;
    #[allow(deprecated)]
    let result = unsafe {
        libc::host_processor_info(
            libc::mach_host_self(),
            libc::PROCESSOR_CPU_LOAD_INFO,
            &mut cpus,
            &mut info,
            &mut info_count,
        )
    };
    assert_eq!(result, libc::KERN_SUCCESS, "host_processor_info failed: {result}");
    let loads = unsafe { slice::from_raw_parts(info.cast::<libc::processor_cpu_load_info>(), cpus as usize) };
    let (busy_ticks, total_ticks) = loads.iter().fold((0, 0), |(busy, total), load| {
        let ticks = load.cpu_ticks.map(u64::from);
        let idle = ticks[libc::CPU_STATE_IDLE as usize];
        let all: u64 = ticks.iter().sum();
        (busy + all - idle, total + all)
    });
    #[allow(deprecated)]
    let result = unsafe {
        libc::vm_deallocate(
            libc::mach_task_self(),
            info as libc::vm_address_t,
            info_count as usize * size_of::<libc::integer_t>(),
        )
    };
    assert_eq!(result, libc::KERN_SUCCESS, "vm_deallocate failed: {result}");
    let tick_ms = 1000 / unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64;

    Host {
        memory_bytes,
        used_bytes: pages * page_bytes,
        cpu_busy_ms: busy_ticks * tick_ms,
        cpu_total_ms: total_ticks * tick_ms,
    }
}

/// Memory in use counts what is not available to start programs with, as `free` does.
#[cfg(target_os = "linux")]
fn host() -> Host {
    let meminfo = fs::read_to_string("/proc/meminfo").expect("failed to read /proc/meminfo");
    let kib = |field: &str| -> u64 {
        meminfo
            .lines()
            .find_map(|line| line.strip_prefix(field)?.strip_prefix(':'))
            .and_then(|value| value.trim().trim_end_matches(" kB").parse().ok())
            .unwrap_or_else(|| panic!("/proc/meminfo has no {field}"))
    };
    let memory_bytes = kib("MemTotal") * 1024;
    let used_bytes = memory_bytes - kib("MemAvailable") * 1024;

    // user nice system idle iowait irq softirq steal, in clock ticks.
    let stat = fs::read_to_string("/proc/stat").expect("failed to read /proc/stat");
    let ticks: Vec<u64> = stat
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("cpu "))
        .expect("/proc/stat has no cpu line")
        .split_whitespace()
        .take(8)
        .map(|value| value.parse().expect("malformed /proc/stat"))
        .collect();
    let total_ticks: u64 = ticks.iter().sum();
    let busy_ticks = total_ticks - ticks[3] - ticks[4];
    let tick_ms = 1000 / unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64;

    Host {
        memory_bytes,
        used_bytes,
        cpu_busy_ms: busy_ticks * tick_ms,
        cpu_total_ms: total_ticks * tick_ms,
    }
}

/// Connects to the control socket of VM `name` and sends `request`.
fn attach(name: &str, request: &protocol::Request) -> Result<UnixStream, String> {
    instance::validate_name(name, "VM")?;
    let mut control = instance::Instance::open(name)?.connect()?;
    protocol::write_json(&mut control, request).map_err(|error| error.to_string())?;
    Ok(control)
}

/// Passes bytes between the client and a control socket until either side
/// closes, starting with what the client sent after its request line.
fn relay<C: Connection>(mut reader: BufReader<io::Take<Deadline<C>>>, mut writer: C, mut control: UnixStream) -> io::Result<()> {
    control.write_all(reader.buffer())?;
    reader.consume(reader.buffer().len());
    writer.set_read_timeout(None)?;
    let mut from_client = writer.try_clone()?;
    let mut to_client = control.try_clone()?;
    thread::scope(|scope| {
        scope.spawn(move || {
            let _ = io::copy(&mut to_client, &mut writer);
            let _ = writer.shutdown(Shutdown::Both);
        });
        let result = io::copy(&mut from_client, &mut control);
        let _ = control.shutdown(Shutdown::Write);
        result.map(|_| ())
    })
}

/// Runs a varmint command with `environment` as its guest variables and
/// returns its stdout, or its error message without the `varmint: ` prefix,
/// like the other errors of a reply.
fn varmint(client: &Client, args: &[String], environment: &BTreeMap<String, String>) -> Result<String, String> {
    let executable = env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|error| error.to_string())?;
    let started = Instant::now();
    let output = Command::new(executable)
        .args(args)
        .env(
            GUEST_ENVIRONMENT_VARIABLE,
            serde_json::to_string(environment).expect("variables serialize"),
        )
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("failed to run varmint: {error}"))?;
    client.emit(
        "exec",
        json!({
            "args": args,
            "exit_code": output.status.code(),
            "duration_ms": started.elapsed().as_millis() as u64,
        }),
    );
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr.trim();
        return Err(message.strip_prefix("varmint: ").unwrap_or(message).to_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
