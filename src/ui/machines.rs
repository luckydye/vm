//! The machines of this host and of every node in `~/.vm/nodes`, polled one
//! listing at a time, and the actions on them. Actions run the vm CLI, like
//! `vm serve` does, so they behave exactly like the commands.

use crate::{
    cli,
    node::{Node, Status},
    serve::{Host, Vm},
};
use gpui::{AppContext, AsyncApp, Context, Task, WeakEntity};
use std::{collections::BTreeSet, env, process::{Command, Stdio}, time::Duration};

/// How long the listing waits before asking again.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// A machine on this host or a node.
#[derive(Clone)]
pub struct Machine {
    /// The name the vm CLI takes: `NAME` here, `NODE/NAME` on a node.
    pub name: String,
    /// The node it runs on, `None` for this host.
    pub node: Option<String>,
    pub vm: Vm,
}

impl Machine {
    /// The name without the node.
    pub fn short_name(&self) -> &str {
        &self.vm.name
    }
}

/// What a host offers, for the overview.
pub struct HostUsage {
    /// `None` for this host.
    pub node: Option<String>,
    pub host: Host,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Tone {
    Done,
    Error,
}

pub struct Notice {
    pub text: String,
    pub tone: Tone,
}

/// The size presets of a new machine.
#[derive(Clone, Copy, PartialEq)]
pub struct Preset {
    pub label: &'static str,
    pub cpus: usize,
    pub memory_mib: u64,
}

pub const PRESETS: [Preset; 3] = [
    Preset { label: "Small", cpus: 2, memory_mib: 2048 },
    Preset { label: "Standard", cpus: 4, memory_mib: 6144 },
    Preset { label: "Large", cpus: 8, memory_mib: 16384 },
];

/// A machine to create.
pub struct NewMachine {
    /// Random when empty.
    pub name: String,
    /// `None` for this host.
    pub node: Option<String>,
    pub preset: Preset,
    pub persist: bool,
}

pub struct Machines {
    /// Sorted by name, this host's first.
    pub machines: Vec<Machine>,
    pub hosts: Vec<HostUsage>,
    /// The nodes of `~/.vm/nodes`, for placing new machines.
    pub nodes: Vec<String>,
    /// Whether the first listing arrived.
    pub loaded: bool,
    /// Why the last listing, or a node in it, failed.
    pub errors: Vec<String>,
    /// Machines with an action under way, and `""` while one is created.
    pub pending: BTreeSet<String>,
    pub notice: Option<Notice>,
    _poll: Task<()>,
}

struct Listing {
    machines: Vec<Machine>,
    hosts: Vec<HostUsage>,
    nodes: Vec<String>,
    errors: Vec<String>,
}

impl Machines {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            machines: Vec::new(),
            hosts: Vec::new(),
            nodes: Vec::new(),
            loaded: false,
            errors: Vec::new(),
            pending: BTreeSet::new(),
            notice: None,
            _poll: cx.spawn(async move |this, cx| poll(this, cx).await),
        }
    }

    pub fn find(&self, name: &str) -> Option<&Machine> {
        self.machines.iter().find(|machine| machine.name == name)
    }

    pub fn running_count(&self) -> usize {
        self.machines.iter().filter(|machine| machine.vm.running).count()
    }

    pub fn start(&mut self, name: &str, cx: &mut Context<Self>) {
        let machine = self.find(name).expect("starting a machine that is not listed");
        let mut args = vec!["run".to_owned(), machine.short_name().to_owned()];
        if let Some(node) = &machine.node {
            args.extend(["--node".to_owned(), node.clone()]);
        }
        self.act(name, args, format!("Started {name}"), cx);
    }

    pub fn stop(&mut self, name: &str, cx: &mut Context<Self>) {
        let args = vec!["stop".to_owned(), name.to_owned()];
        self.act(name, args, format!("Stopped {name}"), cx);
    }

    pub fn delete(&mut self, name: &str, cx: &mut Context<Self>) {
        let args = vec!["stop".to_owned(), "--delete".to_owned(), name.to_owned()];
        self.act(name, args, format!("Deleted {name}"), cx);
    }

    pub fn create(&mut self, new: NewMachine, cx: &mut Context<Self>) {
        let mut args = vec!["run".to_owned()];
        if !new.name.is_empty() {
            args.push(new.name);
        }
        if let Some(node) = new.node {
            args.extend(["--node".to_owned(), node]);
        }
        if new.persist {
            args.push("--persist".to_owned());
        }
        args.extend([
            "--cpus".to_owned(),
            new.preset.cpus.to_string(),
            "--memory".to_owned(),
            new.preset.memory_mib.to_string(),
        ]);
        self.act("", args, "Created {}".to_owned(), cx);
    }

    pub fn dismiss_notice(&mut self, cx: &mut Context<Self>) {
        self.notice = None;
        cx.notify();
    }

    /// Runs `vm ARGS`, then tells how it went and lists the machines again.
    /// `{}` in `done` is replaced by what the command printed.
    fn act(&mut self, key: &str, args: Vec<String>, done: String, cx: &mut Context<Self>) {
        let key = key.to_owned();
        self.pending.insert(key.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { vm(&args) }).await;
            let listing = cx.background_spawn(async { list() }).await;
            this.update(cx, |this, cx| {
                this.pending.remove(&key);
                this.notice = Some(match result {
                    Ok(output) => Notice { text: done.replace("{}", output.trim()), tone: Tone::Done },
                    Err(error) => Notice { text: error, tone: Tone::Error },
                });
                this.apply(listing);
                cx.notify();
            })
            .expect("the machines outlive their actions");
        })
        .detach();
    }

    fn apply(&mut self, listing: Listing) {
        self.machines = listing.machines;
        self.hosts = listing.hosts;
        self.nodes = listing.nodes;
        self.errors = listing.errors;
        self.loaded = true;
    }
}

/// Lists the machines until the model goes away, one listing at a time, so
/// slow nodes never pile up requests.
async fn poll(this: WeakEntity<Machines>, cx: &mut AsyncApp) {
    loop {
        let listing = cx.background_spawn(async { list() }).await;
        let alive = this.update(cx, |this, cx| {
            this.apply(listing);
            cx.notify();
        });
        if alive.is_err() {
            return;
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

fn list() -> Listing {
    let mut listing = Listing { machines: Vec::new(), hosts: Vec::new(), nodes: Vec::new(), errors: Vec::new() };
    let mut statuses = Vec::new();
    match Status::local() {
        Ok(status) => statuses.push(status),
        Err(error) => listing.errors.push(error),
    }
    match Node::load() {
        Ok(nodes) => {
            listing.nodes = nodes.iter().map(|node| node.name.clone()).collect();
            for (name, result) in cli::remote_statuses(nodes) {
                match result {
                    Ok(status) => statuses.push(status),
                    Err(error) => listing.errors.push(format!("node {name}: {error}")),
                }
            }
        }
        Err(error) => listing.errors.push(error),
    }
    for status in statuses {
        let node = status.node.as_ref().map(|node| node.name.clone());
        let mut machines: Vec<Machine> = status
            .vms
            .into_iter()
            .map(|vm| Machine {
                name: match &node {
                    Some(node) => format!("{node}/{}", vm.name),
                    None => vm.name.clone(),
                },
                node: node.clone(),
                vm,
            })
            .collect();
        machines.sort_by(|a, b| a.name.cmp(&b.name));
        listing.machines.extend(machines);
        listing.hosts.push(HostUsage { node, host: status.host });
    }
    listing
}

/// Runs this vm binary with `args` and returns what it printed, or its error.
fn vm(args: &[String]) -> Result<String, String> {
    let exe = env::current_exe().map_err(|error| format!("failed to find the vm binary: {error}"))?;
    let output = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("failed to run vm: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    match output.status.success() {
        true => Ok(stdout),
        false => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = stderr.trim().trim_start_matches("vm: ");
            Err(match message.is_empty() {
                true => format!("vm {} failed with {}", args.join(" "), output.status),
                false => message.to_owned(),
            })
        }
    }
}
