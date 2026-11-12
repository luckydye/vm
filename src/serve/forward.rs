//! Forwards of guest ports to loopback ports of the host, so a browser treats
//! the guest's web servers as `localhost`, a secure context. They last until
//! the server exits, the VM stops or the client removes them.

use super::attach;
use crate::protocol::{self, Frame, Request, Session, kind};
use std::{
    collections::BTreeMap,
    io::{self, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

/// Pause after a failed accept, so a lasting error does not spin the loop.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

struct Forward {
    local_port: u16,
    stopped: Arc<AtomicBool>,
}

static FORWARDS: Mutex<BTreeMap<(String, u16), Forward>> = Mutex::new(BTreeMap::new());

/// Listens on the host's loopback, on `port` itself when it is free, and
/// returns the port it listens on.
pub fn start(name: &str, port: u16) -> Result<u16, String> {
    let mut forwards = FORWARDS.lock().unwrap();
    if let Some(forward) = forwards.get(&(name.to_owned(), port)) {
        return Ok(forward.local_port);
    }
    let listener = TcpListener::bind(("127.0.0.1", port))
        .or_else(|_| TcpListener::bind(("127.0.0.1", 0)))
        .map_err(|error| format!("failed to listen for port {port}: {error}"))?;
    let local_port = listener.local_addr().map_err(|error| error.to_string())?.port();
    let stopped = Arc::new(AtomicBool::new(false));
    let (vm, flag) = (name.to_owned(), stopped.clone());
    thread::Builder::new()
        .name(format!("varmint-forward-{port}"))
        .spawn(move || accept(listener, &vm, port, &flag))
        .map_err(|error| format!("failed to start the forward: {error}"))?;
    forwards.insert((name.to_owned(), port), Forward { local_port, stopped });
    Ok(local_port)
}

pub fn stop(name: &str, port: u16) {
    let forward = FORWARDS.lock().unwrap().remove(&(name.to_owned(), port));
    forward.inspect(close);
}

/// Stops the forwards of VMs that are not in `running`.
pub fn retain(running: &[&str]) {
    let gone: Vec<Forward> = {
        let mut forwards = FORWARDS.lock().unwrap();
        let keys: Vec<_> = forwards.keys().filter(|(name, _)| !running.contains(&name.as_str())).cloned().collect();
        keys.iter().filter_map(|key| forwards.remove(key)).collect()
    };
    gone.iter().for_each(close);
}

/// The host port of each forwarded guest port of VM `name`.
pub fn ports(name: &str) -> BTreeMap<u16, u16> {
    FORWARDS
        .lock()
        .unwrap()
        .iter()
        .filter(|((vm, _), _)| vm.as_str() == name)
        .map(|((_, port), forward)| (*port, forward.local_port))
        .collect()
}

/// Ends the accept loop, which is blocked until the next connection.
fn close(forward: &Forward) {
    forward.stopped.store(true, Ordering::Release);
    let _ = TcpStream::connect(("127.0.0.1", forward.local_port));
}

fn accept(listener: TcpListener, name: &str, port: u16, stopped: &AtomicBool) {
    for connection in listener.incoming() {
        if stopped.load(Ordering::Acquire) {
            return;
        }
        match connection {
            Ok(connection) => {
                let name = name.to_owned();
                thread::spawn(move || {
                    let _ = bridge(connection, &name, port);
                });
            }
            Err(_) => thread::sleep(ACCEPT_RETRY_DELAY),
        }
    }
}

/// Passes bytes between a client and a TCP session to the guest port until either side ends.
fn bridge(client: TcpStream, name: &str, port: u16) -> io::Result<()> {
    let control = attach(name, &Request::Open { session: Session::Tcp { port } }).map_err(io::Error::other)?;
    let mut to_guest = control.try_clone()?;
    let mut from_guest = control;
    let mut from_client = client.try_clone()?;
    let mut to_client = client;
    thread::scope(|scope| {
        scope.spawn(move || {
            while let Ok(Some(frame)) = protocol::read_frame(&mut from_guest) {
                match frame.kind {
                    kind::DATA if to_client.write_all(&frame.payload).is_err() => break,
                    kind::EXIT => break,
                    _ => {}
                }
            }
            let _ = to_client.shutdown(Shutdown::Both);
        });
        let mut buffer = vec![0; 64 * 1024];
        loop {
            let frame = match from_client.read(&mut buffer) {
                Ok(0) | Err(_) => Frame::new(0, kind::EOF, Vec::new()),
                Ok(n) => Frame::new(0, kind::DATA, buffer[..n].to_vec()),
            };
            let eof = frame.kind == kind::EOF;
            if protocol::write_frame(&mut to_guest, &frame).is_err() || eof {
                break;
            }
        }
        let _ = to_guest.shutdown(Shutdown::Write);
    });
    Ok(())
}
