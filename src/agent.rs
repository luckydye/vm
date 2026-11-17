//! Host side of the guest agent link on the `agent` virtio-console port.
//! Routes frames between guest streams and control socket clients.

use crate::{
    devices::RuntimeEvent,
    protocol::{Deframer, Frame, GuestMount, GuestSeeds, RegistryTokenRequest, Session, Setup, SetupDone, kind},
    registry,
    store::{self, Grant},
    telemetry,
};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU32, Ordering},
        mpsc::{Receiver, Sender, channel},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub struct Agent {
    to_guest: Sender<RuntimeEvent>,
    streams: Mutex<HashMap<u32, Sender<Frame>>>,
    next_stream: AtomicU32,
    started: Instant,
    boot: Mutex<Option<Result<Duration, String>>>,
    booted: Condvar,
    authorized_keys: String,
    mounts: Vec<GuestMount>,
    environment: BTreeMap<String, String>,
    seeds: Option<GuestSeeds>,
    registries: Vec<String>,
    /// The VM's name and the buckets it may use, for store requests.
    name: String,
    grants: Vec<Grant>,
    /// Where the body of each store request in progress goes, by stream.
    store_requests: Mutex<HashMap<u32, Sender<Vec<u8>>>>,
}

/// Receives raw bytes the guest wrote to the agent port.
pub struct Sink {
    tx: Sender<Vec<u8>>,
}

impl Sink {
    pub fn push(&self, bytes: Vec<u8>) {
        self.tx.send(bytes).expect("agent reader thread exited");
    }
}

pub fn start(
    to_guest: Sender<RuntimeEvent>,
    authorized_keys: String,
    mounts: Vec<GuestMount>,
    environment: BTreeMap<String, String>,
    seeds: Option<GuestSeeds>,
    registries: Vec<String>,
    name: String,
    grants: Vec<Grant>,
) -> (Arc<Agent>, Sink) {
    let agent = Arc::new(Agent {
        to_guest,
        streams: Mutex::new(HashMap::new()),
        next_stream: AtomicU32::new(1),
        started: Instant::now(),
        boot: Mutex::new(None),
        booted: Condvar::new(),
        authorized_keys,
        mounts,
        environment,
        seeds,
        registries,
        name,
        grants,
        store_requests: Mutex::new(HashMap::new()),
    });

    let (tx, rx) = channel::<Vec<u8>>();
    let reader = agent.clone();
    thread::Builder::new()
        .name("varmint-agent".into())
        .spawn(move || {
            let mut deframer = Deframer::new();
            while let Ok(bytes) = rx.recv() {
                deframer.push(&bytes, |frame| reader.receive(frame));
            }
        })
        .unwrap_or_else(|error| panic!("failed to spawn agent thread: {error}"));

    (agent, Sink { tx })
}

impl Agent {
    pub fn send(&self, frame: &Frame) {
        self.to_guest
            .send(RuntimeEvent::AgentRx(frame.encode()))
            .expect("runtime event pump exited");
    }

    pub fn open(&self, session: &Session) -> (u32, Receiver<Frame>) {
        let stream = self.next_stream.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = channel();
        self.streams.lock().unwrap().insert(stream, tx);
        self.send(&Frame::new(stream, kind::OPEN, serde_json::to_vec(session).unwrap()));
        (stream, rx)
    }

    pub fn close(&self, stream: u32) {
        if self.streams.lock().unwrap().remove(&stream).is_some() {
            self.send(&Frame::new(stream, kind::CLOSE, Vec::new()));
        }
    }

    /// Blocks until the guest agent has finished setup and returns the boot time.
    pub fn wait_booted(&self) -> Result<Duration, String> {
        let boot = self
            .booted
            .wait_while(self.boot.lock().unwrap(), |boot| boot.is_none())
            .unwrap();
        boot.clone().unwrap()
    }

    fn receive(self: &Arc<Self>, frame: Frame) {
        match frame.kind {
            kind::HELLO => return self.hello(),
            kind::SETUP_DONE => return self.setup_done(&frame.payload),
            kind::TELEMETRY => {
                match serde_json::from_slice::<telemetry::GuestReport>(&frame.payload) {
                    Ok(report) => telemetry::record(telemetry::Event::Guest(report)),
                    Err(error) => eprintln!("agent: dropping malformed telemetry report: {error}"),
                }
                return;
            }
            kind::REGISTRY_TOKEN => return self.registry_token(&frame.payload),
            kind::STORE => return self.store_request(frame.stream, frame.payload),
            kind::STORE_DATA => {
                let mut requests = self.store_requests.lock().unwrap();
                if frame.payload.is_empty() {
                    // Dropping the sender ends the body.
                    requests.remove(&frame.stream);
                } else if let Some(body) = requests.get(&frame.stream) {
                    // A request answered before its body arrived no longer reads it.
                    let _ = body.send(frame.payload);
                }
                return;
            }
            _ => {}
        }

        let mut streams = self.streams.lock().unwrap();
        let finished = frame.kind == kind::EXIT;
        let stream = frame.stream;
        if let Some(client) = streams.get(&stream) {
            // A disconnected client is cleaned up by its own connection thread.
            let _ = client.send(frame);
        }
        if finished {
            streams.remove(&stream);
        }
    }

    fn hello(&self) {
        // A restarted agent has lost every stream it served before, and
        // dropping the store bodies ends requests whose body never finished.
        self.streams.lock().unwrap().clear();
        self.store_requests.lock().unwrap().clear();

        let time_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("host clock is before the unix epoch")
            .as_millis() as u64;
        let setup = Setup {
            time_ms,
            authorized_keys: self.authorized_keys.clone(),
            mounts: self.mounts.clone(),
            environment: self.environment.clone(),
            seeds: self.seeds.clone(),
            registries: self.registries.clone(),
        };
        self.send(&Frame::new(0, kind::SETUP, serde_json::to_vec(&setup).unwrap()));
    }

    /// Answers on another thread, since the token takes network round trips.
    fn registry_token(self: &Arc<Self>, payload: &[u8]) {
        let request = match serde_json::from_slice::<RegistryTokenRequest>(payload) {
            Ok(request) => request,
            Err(error) => return eprintln!("agent: dropping malformed registry token request: {error}"),
        };
        let agent = self.clone();
        thread::Builder::new()
            .name("varmint-registry-token".into())
            .spawn(move || {
                let token = registry::token(&agent.registries, &request);
                agent.send(&Frame::new(0, kind::REGISTRY_TOKEN, serde_json::to_vec(&token).unwrap()));
            })
            .unwrap_or_else(|error| panic!("failed to spawn registry token thread: {error}"));
    }

    /// Answers on another thread, which reads the body as it arrives and
    /// sends the reply's head, body and an empty frame on the request's stream.
    fn store_request(self: &Arc<Self>, stream: u32, payload: Vec<u8>) {
        let (body, frames) = channel();
        self.store_requests.lock().unwrap().insert(stream, body);
        let agent = self.clone();
        thread::Builder::new()
            .name("varmint-store".into())
            .spawn(move || {
                let caller = store::Caller::Vm {
                    name: &agent.name,
                    grants: &agent.grants,
                };
                let response = store::handle_frame(&caller, &payload, &mut store::Chunks::new(frames));
                agent.send(&Frame::new(stream, kind::STORE, serde_json::to_vec(&response.head).unwrap()));
                // A file that fails to read cuts the body short of its Content-Length, which HTTP clients report.
                let _ = response.send_body(|chunk| agent.send(&Frame::new(stream, kind::STORE_DATA, chunk)));
                agent.send(&Frame::new(stream, kind::STORE_DATA, Vec::new()));
            })
            .unwrap_or_else(|error| panic!("failed to spawn store thread: {error}"));
    }

    fn setup_done(&self, payload: &[u8]) {
        let result = match serde_json::from_slice::<SetupDone>(payload) {
            Ok(done) if done.errors.is_empty() => Ok(self.started.elapsed()),
            Ok(done) => Err(done.errors.join("; ")),
            Err(error) => Err(format!("malformed setup reply from the guest agent: {error}")),
        };
        let mut boot = self.boot.lock().unwrap();
        if boot.is_none() {
            if let Ok(boot_time) = &result {
                telemetry::record(telemetry::Event::Booted {
                    boot_ms: boot_time.as_millis() as u64,
                });
            }
            *boot = Some(result);
            self.booted.notify_all();
        }
    }
}
