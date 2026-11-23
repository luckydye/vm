//! The ports a running machine listens on, asked of its guest, and their
//! forwards to this computer's loopback, so a browser treats a guest server as
//! `localhost`.

use crate::{
    node::{self, Target},
    serve::{self, Network, Request},
};
use gpui::{AppContext, AsyncApp, Context, Task, WeakEntity};
use std::{collections::BTreeSet, time::Duration};

/// How long the guest's ports are kept before it is asked again.
const POLL_INTERVAL: Duration = Duration::from_secs(10);

pub struct Ports {
    /// The machine to ask, `None` while none is shown or it is stopped.
    machine: Option<String>,
    pub network: Option<Network>,
    /// Why the last ask failed, such as a guest that is still booting.
    pub error: Option<String>,
    /// Ports whose forward is on its way.
    pub pending: BTreeSet<u16>,
    _poll: Option<Task<()>>,
}

impl Ports {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self { machine: None, network: None, error: None, pending: BTreeSet::new(), _poll: None }
    }

    /// Asks `machine` for its ports every few seconds while it is set, and
    /// forgets the ports of the machine set before.
    pub fn follow(&mut self, machine: Option<String>, cx: &mut Context<Self>) {
        if self.machine == machine {
            return;
        }
        self.machine = machine.clone();
        self.network = None;
        self.error = None;
        self.pending.clear();
        self._poll = machine.map(|name| {
            cx.spawn(async move |this, cx| ask(name, this, cx).await)
        });
        cx.notify();
    }

    /// Whether the machine runs on this computer, whose ports alone can be
    /// forwarded here.
    pub fn local(&self) -> bool {
        self.machine.as_deref().is_some_and(|name| !name.contains('/'))
    }

    /// Forwards `port` to this computer's loopback, or stops its forward.
    pub fn toggle(&mut self, port: u16, cx: &mut Context<Self>) {
        let Some(machine) = self.machine.clone() else {
            return;
        };
        assert!(!machine.contains('/'), "port {port} of {machine} cannot be forwarded here");
        let Some(port_state) = self.port(port) else {
            return;
        };
        let forward = port_state.forwarded.is_none();
        if !self.pending.insert(port) {
            return;
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            let name = machine.clone();
            let result = cx
                .background_spawn(async move {
                    match forward {
                        true => serve::forward::start(&name, port).map(Some),
                        false => {
                            serve::forward::stop(&name, port);
                            Ok(None)
                        }
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                if this.machine.as_deref() != Some(machine.as_str()) {
                    return;
                }
                this.pending.remove(&port);
                match result {
                    Ok(forwarded) => {
                        if let Some(port_state) = this.port_mut(port) {
                            port_state.forwarded = forwarded;
                        }
                    }
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn port(&self, port: u16) -> Option<&serve::Port> {
        self.network.as_ref()?.ports.iter().find(|entry| entry.port == port)
    }

    fn port_mut(&mut self, port: u16) -> Option<&mut serve::Port> {
        self.network.as_mut()?.ports.iter_mut().find(|entry| entry.port == port)
    }
}

/// Asks for the ports of `name` until the machine is followed no more, one ask
/// at a time, so a slow guest never piles up requests.
async fn ask(name: String, this: WeakEntity<Ports>, cx: &mut AsyncApp) {
    loop {
        let asked = name.clone();
        let network = cx.background_spawn(async move { network(&asked) }).await;
        let alive = this.update(cx, |this, cx| {
            if this.machine.as_deref() != Some(name.as_str()) {
                return;
            }
            match network {
                Ok(network) => {
                    this.network = Some(network);
                    this.error = None;
                }
                // A guest that is still booting has no ports to show yet, so the next ask retries.
                Err(error) => this.error = Some(error),
            }
            cx.notify();
        });
        if alive.is_err() {
            return;
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// The addresses and listening ports of `name`, locally or from its node.
fn network(name: &str) -> Result<Network, String> {
    match Target::resolve(name)? {
        Target::Local(instance) => serve::network(&instance.name),
        Target::Remote { node, name } => node::rpc(&node, &Request::Network { name: name.clone() })?
            .network
            .ok_or_else(|| format!("node {} sent no ports of {name}", node.name)),
    }
}
