//! Nodes: the local machine and `vm serve` instances on the network.
//!
//! A node is configured in `~/.vm/nodes`, one line per machine:
//!
//! ```text
//! # name address token [ca]
//! mini 192.168.1.20:7420 3f2a... ~/.vm/tls/mini-ca.pem
//! ```
//!
//! The file holds tokens, so it must be readable only by its owner. The local
//! machine is always available as the bare (unqualified) namespace. A VM on a
//! node is named `NODE/NAME`, which keeps the pool free of collisions and lets
//! every command work with either kind of name.
//!
//! This module is the client side of `vm serve`: it opens the same control
//! socket protocol the CLI already uses for a local VM, through the serve
//! `attach` request. TLS is used when a CA is configured.

use crate::{
    instance,
    protocol::{self, Reply as ControlReply, Request as ControlRequest},
    serve::{self, Host, Reply, Request, Vm},
};
use rustls::{
    ClientConfig, ClientConnection, RootCertStore,
    pki_types::{CertificateDer, ServerName, pem::PemObject},
};
use serde_json::json;
use std::{
    env,
    fs,
    io::{self, BufReader, Read, Write},
    net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const TLS_CHUNK: usize = 16 * 1024;

/// A `vm serve` host from `~/.vm/nodes`.
#[derive(Clone)]
pub(crate) struct Node {
    pub(crate) name: String,
    pub(crate) address: String,
    token: String,
    tls: Option<Arc<ClientConfig>>,
}

/// The local machine or a VM on a node.
pub(crate) enum Target {
    Local(instance::Instance),
    Remote { node: Node, name: String },
}

/// A bidirectional connection to a VM control socket.
pub(crate) struct Control {
    pub(crate) reader: BufReader<Wire>,
    pub(crate) writer: Wire,
    pub(crate) reply: ControlReply,
}

/// The connection a control socket runs on: a Unix socket locally, TCP for a
/// node. TLS connections are wrapped in a Unix socket pair by [`tls_wrap`], so
/// the rest of the CLI sees one cloneable stream either way.
pub(crate) enum Wire {
    Unix(UnixStream),
    Tcp(TcpStream),
}

/// One node's VMs and capacity, for `ls` and placement.
pub(crate) struct Status {
    pub(crate) node: Option<Node>,
    pub(crate) vms: Vec<Vm>,
    pub(crate) host: Host,
}

impl Wire {
    fn try_clone(&self) -> io::Result<Self> {
        match self {
            Self::Unix(stream) => stream.try_clone().map(Self::Unix),
            Self::Tcp(stream) => stream.try_clone().map(Self::Tcp),
        }
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.set_read_timeout(timeout),
            Self::Tcp(stream) => stream.set_read_timeout(timeout),
        }
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.set_write_timeout(timeout),
            Self::Tcp(stream) => stream.set_write_timeout(timeout),
        }
    }
}

impl Read for Wire {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Unix(stream) => stream.read(buffer),
            Self::Tcp(stream) => stream.read(buffer),
        }
    }
}

impl Write for Wire {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Self::Unix(stream) => stream.write(buffer),
            Self::Tcp(stream) => stream.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.flush(),
            Self::Tcp(stream) => stream.flush(),
        }
    }
}

impl Control {
    /// Sets or clears the timeout for the rest of the connection.
    fn set_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.reader.get_ref().set_read_timeout(timeout)?;
        self.writer.set_write_timeout(timeout)
    }
}

impl Target {
    /// Resolves a bare local name or a `NODE/NAME` remote name.
    pub(crate) fn resolve(name: &str) -> Result<Self, String> {
        match name.split_once('/') {
            Some((node_name, vm_name)) => {
                if node_name.is_empty() || vm_name.is_empty() {
                    return Err(format!("invalid VM name {name:?}"));
                }
                let node = Node::find(node_name)?;
                instance::validate_name(vm_name, "VM")?;
                Ok(Self::Remote {
                    node,
                    name: vm_name.to_owned(),
                })
            }
            None => instance::Instance::open(name).map(Self::Local),
        }
    }

    pub(crate) fn name(&self) -> &str {
        match self {
            Self::Local(instance) => &instance.name,
            Self::Remote { name, .. } => name,
        }
    }

    pub(crate) fn qualified_name(&self) -> String {
        match self {
            Self::Local(instance) => instance.name.clone(),
            Self::Remote { node, name } => format!("{}/{name}", node.name),
        }
    }

    pub(crate) fn node(&self) -> Option<&Node> {
        match self {
            Self::Local(_) => None,
            Self::Remote { node, .. } => Some(node),
        }
    }
}

impl Status {
    pub(crate) fn label(&self) -> &str {
        self.node.as_ref().map_or("local", |node| node.name.as_str())
    }

    pub(crate) fn local() -> Result<Self, String> {
        let (vms, host) = serve::local_vms()?;
        Ok(Self { node: None, vms, host })
    }

    pub(crate) fn remote(node: &Node) -> Result<Self, String> {
        let reply = rpc(node, &Request::Ls)?;
        Ok(Self {
            node: Some(node.clone()),
            vms: reply
                .vms
                .ok_or_else(|| format!("node {} sent no VM list", node.name))?,
            host: reply
                .host
                .ok_or_else(|| format!("node {} sent no host usage", node.name))?,
        })
    }
}

impl Node {
    /// The configured nodes, or none when `~/.vm/nodes` does not exist.
    pub(crate) fn load() -> Result<Vec<Self>, String> {
        let path = Self::config_path();
        let source = match fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
        };
        let permissions = fs::metadata(&path)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?
            .permissions()
            .mode();
        if permissions & 0o077 != 0 {
            return Err(format!(
                "{} holds tokens and must only be readable by you; run: chmod 600 {}",
                path.display(),
                path.display()
            ));
        }

        let mut nodes: Vec<Self> = Vec::new();
        for (index, line) in source.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (name, address, token, ca) = match fields.as_slice() {
                [name, address, token] => (*name, *address, *token, None),
                [name, address, token, ca] => (*name, *address, *token, Some(*ca)),
                _ => {
                    return Err(format!(
                        "{}:{}: expected NAME ADDRESS TOKEN [CA]",
                        path.display(),
                        index + 1
                    ));
                }
            };
            instance::validate_name(name, "node")?;
            if name == "local" {
                return Err(format!("{}:{}: \"local\" is reserved", path.display(), index + 1));
            }
            if nodes.iter().any(|node| node.name == name) {
                return Err(format!("{}:{}: node {name} appears twice", path.display(), index + 1));
            }
            let tls = match ca {
                Some(ca) => Some(tls_config(&expand_home(ca))?),
                None => None,
            };
            nodes.push(Self {
                name: name.to_owned(),
                address: address.to_owned(),
                token: token.to_owned(),
                tls,
            });
        }
        nodes.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(nodes)
    }

    pub(crate) fn find(name: &str) -> Result<Self, String> {
        let nodes = Self::load()?;
        nodes.iter().find(|node| node.name == name).cloned().ok_or_else(|| {
            let configured: Vec<&str> = nodes.iter().map(|node| node.name.as_str()).collect();
            match configured.is_empty() {
                true => format!("no node named {name}; add it to {}", Self::config_path().display()),
                false => format!("no node named {name}; configured nodes: {}", configured.join(", ")),
            }
        })
    }

    pub(crate) fn config_path() -> PathBuf {
        instance::home().join("nodes")
    }

    pub(crate) fn uses_tls(&self) -> bool {
        self.tls.is_some()
    }

    /// Connects, through TLS when the node has a CA.
    fn connect(&self) -> Result<Wire, String> {
        let (host, port) = split_address(&self.address)?;
        let addresses: Vec<SocketAddr> = (host, port)
            .to_socket_addrs()
            .map_err(|error| format!("node {} ({}): {error}", self.name, self.address))?
            .collect();
        let mut last_error = None;
        for address in addresses {
            match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
                Ok(stream) => {
                    stream
                        .set_nodelay(true)
                        .map_err(|error| format!("node {}: {error}", self.name))?;
                    keepalive(&stream);
                    let wire = match &self.tls {
                        Some(config) => Wire::Unix(tls_wrap(stream, config.clone(), host)?),
                        None => Wire::Tcp(stream),
                    };
                    wire
                        .set_read_timeout(Some(REPLY_TIMEOUT))
                        .map_err(|error| format!("node {}: {error}", self.name))?;
                    wire
                        .set_write_timeout(Some(REPLY_TIMEOUT))
                        .map_err(|error| format!("node {}: {error}", self.name))?;
                    return Ok(wire);
                }
                Err(error) => last_error = Some(error),
            }
        }
        Err(format!(
            "node {} ({}): {}",
            self.name,
            self.address,
            last_error.map_or_else(|| "no addresses resolved".to_owned(), |error| error.to_string())
        ))
    }
}

/// Sends one serve request and returns its reply.
pub(crate) fn rpc(node: &Node, request: &Request) -> Result<Reply, String> {
    let mut wire = node.connect()?;
    let timeout = match request {
        Request::Run { .. } => Duration::from_secs(300),
        Request::Save { .. } => Duration::from_secs(3600),
        Request::Ls => REPLY_TIMEOUT,
        _ => Duration::from_secs(300),
    };
    wire.set_read_timeout(Some(timeout))
        .map_err(|error| format!("node {}: {error}", node.name))?;
    wire.set_write_timeout(Some(REPLY_TIMEOUT))
        .map_err(|error| format!("node {}: {error}", node.name))?;

    let mut value = serde_json::to_value(request).map_err(|error| error.to_string())?;
    value["token"] = json!(&node.token);
    protocol::write_json(&mut wire, &value).map_err(|error| format!("node {}: {error}", node.name))?;

    let mut reader = BufReader::new(wire);
    let reply: Reply =
        protocol::read_json(&mut reader).map_err(|error| format!("node {}: {error}", node.name))?;
    match reply.error.clone() {
        Some(error) => Err(error),
        None => Ok(reply),
    }
}

/// Sends a control request to a local or remote VM and reads its reply.
pub(crate) fn request(target: &Target, request: &ControlRequest) -> Result<Control, String> {
    match target {
        Target::Local(instance) => {
            let mut wire = Wire::Unix(instance.connect()?);
            let writer = wire
                .try_clone()
                .map_err(|error| format!("VM {}: {error}", instance.name))?;
            protocol::write_json(&mut wire, request)
                .map_err(|error| format!("VM {}: {error}", instance.name))?;
            let mut reader = BufReader::new(wire);
            let reply = if matches!(request, ControlRequest::Open { .. }) {
                ControlReply::default()
            } else {
                let reply: ControlReply = protocol::read_json(&mut reader)
                    .map_err(|error| format!("VM {}: {error}", instance.name))?;
                if let Some(error) = &reply.error {
                    return Err(error.clone());
                }
                reply
            };
            Ok(Control { reader, writer, reply })
        }
        Target::Remote { node, name } => {
            let mut wire = node.connect()?;
            let timeout = match request {
                ControlRequest::Wait => Duration::from_secs(300),
                // The control injects one event every 2 ms before replying.
                ControlRequest::Input { events } => Duration::from_secs(10 + events.len() as u64 / 500),
                _ => REPLY_TIMEOUT,
            };
            wire.set_read_timeout(Some(timeout))
                .map_err(|error| format!("node {}: {error}", node.name))?;
            wire.set_write_timeout(Some(Duration::from_secs(60)))
                .map_err(|error| format!("node {}: {error}", node.name))?;
            let writer = wire
                .try_clone()
                .map_err(|error| format!("node {}: {error}", node.name))?;
            let outer = json!({
                "token": &node.token,
                "op": "attach",
                "name": name,
                "request": request,
            });
            protocol::write_json(&mut wire, &outer)
                .map_err(|error| format!("node {}: {error}", node.name))?;
            let mut reader = BufReader::new(wire);
            let reply: Reply =
                protocol::read_json(&mut reader).map_err(|error| format!("node {}: {error}", node.name))?;
            if let Some(error) = reply.error {
                return Err(error);
            }
            let reply = if matches!(request, ControlRequest::Open { .. }) {
                ControlReply::default()
            } else {
                let reply: ControlReply = protocol::read_json(&mut reader)
                    .map_err(|error| format!("node {}: VM {name}: {error}", node.name))?;
                if let Some(error) = &reply.error {
                    return Err(error.clone());
                }
                reply
            };
            let control = Control { reader, writer, reply };
            // Requests that keep using the connection must not time out while
            // they stream. The others end with their reply, and clearing the
            // timeout then fails on macOS once the peer has closed.
            if matches!(
                request,
                ControlRequest::Stream { .. }
                    | ControlRequest::InputStream
                    | ControlRequest::Open { .. }
                    | ControlRequest::Screenshot
            ) {
                control
                    .set_timeout(None)
                    .map_err(|error| format!("node {}: {error}", node.name))?;
            }
            Ok(control)
        }
    }
}

fn split_address(address: &str) -> Result<(&str, u16), String> {
    let (host, port) = match address.strip_prefix('[') {
        Some(rest) => rest
            .split_once("]:")
            .ok_or_else(|| format!("invalid node address {address:?}; expected HOST:PORT"))?,
        None => address
            .rsplit_once(':')
            .ok_or_else(|| format!("invalid node address {address:?}; expected HOST:PORT"))?,
    };
    let port = port
        .parse::<u16>()
        .map_err(|_| format!("invalid port in node address {address:?}"))?;
    if host.is_empty() {
        return Err(format!("invalid node address {address:?}; the host is empty"));
    }
    Ok((host, port))
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => PathBuf::from(env::var_os("HOME").expect("HOME is not set")).join(rest),
        None => PathBuf::from(path),
    }
}

fn keepalive(stream: &TcpStream) {
    let _ = protocol::set_socket_option(stream, libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1);
    #[cfg(target_os = "macos")]
    let _ = protocol::set_socket_option(stream, libc::IPPROTO_TCP, libc::TCP_KEEPALIVE, 30);
    #[cfg(target_os = "linux")]
    {
        let _ = protocol::set_socket_option(stream, libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, 30);
        let _ = protocol::set_socket_option(stream, libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, 10);
        let _ = protocol::set_socket_option(stream, libc::IPPROTO_TCP, libc::TCP_KEEPCNT, 3);
    }
}

fn tls_config(ca: &Path) -> Result<Arc<ClientConfig>, String> {
    let mut roots = RootCertStore::empty();
    let certificates =
        CertificateDer::pem_file_iter(ca).map_err(|error| format!("failed to read {}: {error}", ca.display()))?;
    for certificate in certificates {
        let certificate = certificate.map_err(|error| format!("failed to read {}: {error}", ca.display()))?;
        roots
            .add(certificate)
            .map_err(|error| format!("invalid certificate in {}: {error}", ca.display()))?;
    }
    if roots.is_empty() {
        return Err(format!("{} contains no certificates", ca.display()));
    }
    Ok(Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

/// The TLS connection is driven by two threads around a socket pair, so the
/// rest of the client gets a cloneable, blocking stream like a local one.
fn tls_wrap(stream: TcpStream, config: Arc<ClientConfig>, host: &str) -> Result<UnixStream, String> {
    let name = match host.parse::<IpAddr>() {
        Ok(address) => ServerName::IpAddress(address.into()),
        Err(_) => ServerName::try_from(host.to_owned())
            .map_err(|_| format!("invalid TLS server name {host:?}"))?,
    };
    let mut stream = stream;
    let mut connection =
        ClientConnection::new(config, name).map_err(|error| format!("TLS: {error}"))?;
    stream
        .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .map_err(|error| format!("TLS handshake with {host}: {error}"))?;
    stream
        .set_write_timeout(Some(HANDSHAKE_TIMEOUT))
        .map_err(|error| format!("TLS handshake with {host}: {error}"))?;
    while connection.is_handshaking() {
        connection
            .complete_io(&mut stream)
            .map_err(|error| format!("TLS handshake with {host}: {error}"))?;
    }
    stream
        .set_read_timeout(None)
        .map_err(|error| format!("TLS: {error}"))?;
    stream
        .set_write_timeout(None)
        .map_err(|error| format!("TLS: {error}"))?;

    let session = Arc::new(Mutex::new(connection));
    let (plain, inner) = UnixStream::pair().map_err(|error| format!("TLS: {error}"))?;
    let mut inner_writer = inner.try_clone().map_err(|error| format!("TLS: {error}"))?;
    let mut read_end = stream.try_clone().map_err(|error| format!("TLS: {error}"))?;
    let mut read_write_end = stream.try_clone().map_err(|error| format!("TLS: {error}"))?;
    let mut write_end = stream;

    let read_session = session.clone();
    thread::Builder::new()
        .name("vm-tls-read".into())
        .spawn(move || {
            let mut buffer = vec![0; TLS_CHUNK];
            let mut plaintext = Vec::new();
            loop {
                let count = match read_end.read(&mut buffer) {
                    Ok(0) | Err(_) => return,
                    Ok(count) => count,
                };
                let mut session = read_session.lock().expect("the TLS session is poisoned");
                let mut received = &buffer[..count];
                while !received.is_empty() {
                    if session.read_tls(&mut received).is_err() {
                        return;
                    }
                    let state = match session.process_new_packets() {
                        Ok(state) => state,
                        Err(_) => return,
                    };
                    let start = plaintext.len();
                    plaintext.resize(start + state.plaintext_bytes_to_read(), 0);
                    if session.reader().read_exact(&mut plaintext[start..]).is_err() {
                        return;
                    }
                }
                while session.wants_write() {
                    if session.write_tls(&mut read_write_end).is_err() {
                        return;
                    }
                }
                drop(session);
                if inner_writer.write_all(&plaintext).is_err() {
                    return;
                }
                plaintext.clear();
            }
        })
        .map_err(|error| format!("TLS: {error}"))?;

    let write_session = session;
    thread::Builder::new()
        .name("vm-tls-write".into())
        .spawn(move || {
            let mut inner = inner;
            let mut buffer = vec![0; TLS_CHUNK];
            loop {
                let count = match inner.read(&mut buffer) {
                    Ok(0) => 0,
                    Ok(count) => count,
                    Err(_) => return,
                };
                let mut session = write_session.lock().expect("the TLS session is poisoned");
                if count == 0 {
                    session.send_close_notify();
                } else if session.writer().write_all(&buffer[..count]).is_err() {
                    return;
                }
                while session.wants_write() {
                    if session.write_tls(&mut write_end).is_err() {
                        return;
                    }
                }
                if count == 0 {
                    return;
                }
            }
        })
        .map_err(|error| format!("TLS: {error}"))?;

    Ok(plain)
}
