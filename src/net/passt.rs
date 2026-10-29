//! Networking through passt, which runs as our child on one end of a stream
//! socket pair and exits with us. Frames travel with a 4-byte big-endian
//! length in front, as for QEMU's stream netdev.

use std::{
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    process::{Child, Command, Stdio},
    sync::Mutex,
    thread,
};

/// The guest has no offloads, so frames fit a standard Ethernet MTU.
const MTU: usize = 1500;
const MAX_FRAME: usize = MTU + 14;

pub struct Backend {
    stream: Mutex<UnixStream>,
    mac: [u8; 6],
    process: Child,
}

pub fn start(on_frame: impl Fn(Vec<u8>) + Send + 'static) -> io::Result<Backend> {
    let (ours, theirs) = UnixStream::pair()?;
    let fd = theirs.as_raw_fd();
    unsafe {
        if libc::fcntl(fd, libc::F_SETFD, 0) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let mut command = Command::new("passt");
    command.args([
        "--foreground",
        "--quiet",
        "--mtu",
        &MTU.to_string(),
        "--fd",
        &fd.to_string(),
    ]);
    // As root passt drops to nobody, and AppArmor keeps it from looking that
    // user up by name, so give it the IDs.
    if unsafe { libc::geteuid() } == 0 {
        command.args(["--runas", "65534:65534"]);
    }
    let process = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot start passt, which networking needs: {error}"),
            )
        })?;
    drop(theirs);

    let mut reader = ours.try_clone()?;
    thread::Builder::new().name("varmint-passt-rx".into()).spawn(move || {
        let mut length = [0; 4];
        loop {
            reader
                .read_exact(&mut length)
                .unwrap_or_else(|error| panic!("passt stopped: {error}"));
            let mut frame = vec![0; u32::from_be_bytes(length) as usize];
            reader
                .read_exact(&mut frame)
                .unwrap_or_else(|error| panic!("passt stopped: {error}"));
            if frame.len() >= 14 {
                on_frame(frame);
            }
        }
    })?;

    Ok(Backend {
        stream: Mutex::new(ours),
        mac: random_mac()?,
        process,
    })
}

/// A random, locally administered unicast address.
fn random_mac() -> io::Result<[u8; 6]> {
    let mut mac = [0; 6];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut mac)?;
    mac[0] = (mac[0] & 0xfe) | 0x02;
    Ok(mac)
}

impl Backend {
    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    pub fn write(&self, frame: &[u8]) {
        if frame.len() > MAX_FRAME {
            eprintln!("passt tx error, dropping frame: frame exceeds the MTU");
            return;
        }
        let mut message = Vec::with_capacity(4 + frame.len());
        message.extend_from_slice(&(frame.len() as u32).to_be_bytes());
        message.extend_from_slice(frame);
        self.stream
            .lock()
            .unwrap()
            .write_all(&message)
            .unwrap_or_else(|error| panic!("passt stopped: {error}"));
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}
