//! Suspends an idle VM: once no control client has been connected for the idle
//! timeout, the vCPUs stop until the next client connects. The VM process keeps
//! the guest's memory and devices, so resuming is instant; the guest's clock
//! jumps ahead by the time it was suspended.

use std::{
    sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// Minutes without control clients before `vm run` suspends a VM.
pub const DEFAULT_IDLE_MINUTES: u64 = 15;

/// Makes one vCPU leave guest code soon, also if it is about to enter it.
pub type Kick = Box<dyn Fn() + Send + Sync>;

pub struct Suspend {
    suspended: AtomicBool,
    state: Mutex<State>,
    changed: Condvar,
}

struct State {
    clients: usize,
    idle_since: Instant,
    kicks: Vec<Kick>,
}

/// A connected control client, which keeps the VM awake until it is dropped.
pub struct Client<'a>(&'a Suspend);

impl Suspend {
    pub fn new() -> Self {
        Self {
            suspended: AtomicBool::new(false),
            state: Mutex::new(State {
                clients: 0,
                idle_since: Instant::now(),
                kicks: Vec::new(),
            }),
            changed: Condvar::new(),
        }
    }

    pub fn is_suspended(&self) -> bool {
        self.suspended.load(Ordering::SeqCst)
    }

    pub fn add_vcpu(&self, kick: Kick) {
        self.state.lock().unwrap().kicks.push(kick);
    }

    /// Blocks a vCPU about to enter guest code while the VM is suspended.
    pub fn wait_awake(&self) {
        if !self.is_suspended() {
            return;
        }
        let state = self.state.lock().unwrap();
        drop(self.changed.wait_while(state, |_| self.is_suspended()).unwrap());
    }

    /// Resumes the VM and keeps it awake until the returned client is dropped.
    pub fn client(&self) -> Client<'_> {
        let mut state = self.state.lock().unwrap();
        state.clients += 1;
        self.suspended.store(false, Ordering::SeqCst);
        self.changed.notify_all();
        Client(self)
    }

    /// Suspends the VM each time it had no client for `timeout`.
    pub fn watch(&self, timeout: Duration) -> ! {
        let mut state = self.state.lock().unwrap();
        loop {
            if state.clients > 0 || self.is_suspended() {
                state = self.changed.wait(state).unwrap();
                continue;
            }
            let idle = state.idle_since.elapsed();
            if idle < timeout {
                state = self.changed.wait_timeout(state, timeout - idle).unwrap().0;
                continue;
            }
            self.suspended.store(true, Ordering::SeqCst);
            for kick in &state.kicks {
                kick();
            }
        }
    }
}

impl Drop for Client<'_> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.clients -= 1;
        state.idle_since = Instant::now();
        self.0.changed.notify_all();
    }
}
