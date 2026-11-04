//! Usage telemetry in `~/.varmint/telemetry.sqlite`, for finding programs that
//! run too often or get installed repeatedly and are worth preinstalling.
//!
//! Every VM process appends to the same database (WAL mode). A dedicated
//! thread owns the connection; other threads send it events.

use crate::{config::Hardware, instance};
use rusqlite::{Connection, params};
use serde::Deserialize;
use std::{
    sync::{
        OnceLock,
        mpsc::{Sender, SyncSender, channel, sync_channel},
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS vms (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    persist INTEGER NOT NULL,
    window INTEGER NOT NULL,
    cpus INTEGER NOT NULL,
    memory_mib INTEGER NOT NULL,
    mounts INTEGER NOT NULL,
    started_at REAL NOT NULL,
    boot_ms INTEGER,
    stopped_at REAL,
    stop_reason TEXT
);
-- Every process that exited in the guest, from kernel process accounting.
CREATE TABLE IF NOT EXISTS processes (
    vm_id INTEGER NOT NULL REFERENCES vms(id),
    ended_at REAL NOT NULL,
    command TEXT NOT NULL,
    elapsed_ms REAL NOT NULL,
    cpu_ms REAL NOT NULL,
    exit_code INTEGER NOT NULL,
    uid INTEGER NOT NULL
);
-- Command lines: typed in interactive guest shells, or run with varmint exec.
CREATE TABLE IF NOT EXISTS commands (
    vm_id INTEGER NOT NULL REFERENCES vms(id),
    source TEXT NOT NULL,
    started_at REAL NOT NULL,
    duration_ms REAL NOT NULL,
    exit_code INTEGER NOT NULL,
    command TEXT NOT NULL
);
-- Packages and tools installed in the guest (apt and mise).
CREATE TABLE IF NOT EXISTS packages (
    vm_id INTEGER NOT NULL REFERENCES vms(id),
    installed_at REAL NOT NULL,
    manager TEXT NOT NULL,
    name TEXT NOT NULL,
    version TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS processes_command ON processes(command);
CREATE INDEX IF NOT EXISTS commands_command ON commands(command);
CREATE INDEX IF NOT EXISTS packages_name ON packages(name);
";

/// A batch of observations the guest agent sends.
#[derive(Deserialize)]
pub struct GuestReport {
    processes: Vec<Process>,
    commands: Vec<Command>,
    packages: Vec<Package>,
}

#[derive(Deserialize)]
struct Process {
    ended_at: f64,
    command: String,
    elapsed_ms: f64,
    cpu_ms: f64,
    exit_code: i32,
    uid: u32,
}

#[derive(Deserialize)]
struct Command {
    started_at: f64,
    duration_ms: f64,
    exit_code: i32,
    command: String,
}

#[derive(Deserialize)]
struct Package {
    installed_at: f64,
    manager: String,
    name: String,
    version: String,
}

pub enum Event {
    Booted {
        boot_ms: u64,
    },
    Guest(GuestReport),
    /// A command run from the host with `varmint exec`.
    HostCommand {
        source: &'static str,
        started_at: f64,
        duration_ms: f64,
        exit_code: i32,
        command: String,
    },
    Stopped {
        reason: &'static str,
        flushed: SyncSender<()>,
    },
}

static EVENTS: OnceLock<Sender<Event>> = OnceLock::new();

pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("host clock is before the unix epoch")
        .as_secs_f64()
}

/// Opens the database, registers this VM run and starts the writer thread.
pub fn start(name: &str, hardware: &Hardware, mounts: usize, window: bool) {
    let path = instance::home().join("telemetry.sqlite");
    let connection =
        Connection::open(&path).unwrap_or_else(|error| panic!("failed to open {}: {error}", path.display()));
    connection
        .busy_timeout(std::time::Duration::from_secs(10))
        .expect("failed to set the telemetry busy timeout");
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .expect("failed to enable WAL for telemetry");
    connection.execute_batch(SCHEMA).expect("failed to create the telemetry schema");
    connection
        .execute(
            "INSERT INTO vms (name, persist, window, cpus, memory_mib, mounts, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                name,
                hardware.persist,
                window,
                hardware.cpus as i64,
                hardware.memory_mib as i64,
                mounts as i64,
                now()
            ],
        )
        .expect("failed to register the VM in telemetry");
    let vm_id = connection.last_insert_rowid();

    let (tx, rx) = channel();
    EVENTS.set(tx).expect("telemetry started twice");
    thread::Builder::new()
        .name("varmint-telemetry".into())
        .spawn(move || {
            let mut connection = connection;
            while let Ok(event) = rx.recv() {
                if let Some(flushed) = write(&mut connection, vm_id, event) {
                    let _ = flushed.send(());
                    return;
                }
            }
        })
        .expect("failed to spawn the telemetry thread");
}

pub fn record(event: Event) {
    let events = EVENTS.get().expect("telemetry is not started");
    events.send(event).expect("telemetry thread exited");
}

/// Records why the VM stopped and waits until everything is written.
pub fn stop(reason: &'static str) {
    let Some(events) = EVENTS.get() else {
        return;
    };
    let (flushed, done) = sync_channel(1);
    if events.send(Event::Stopped { reason, flushed }).is_ok() {
        let _ = done.recv();
    }
}

/// Writes one event; returns the flush acknowledgement for `Stopped`.
fn write(connection: &mut Connection, vm_id: i64, event: Event) -> Option<SyncSender<()>> {
    let result = match event {
        Event::Booted { boot_ms } => connection
            .execute("UPDATE vms SET boot_ms = ?1 WHERE id = ?2", params![boot_ms as i64, vm_id])
            .map(drop),
        Event::Guest(report) => write_report(connection, vm_id, &report),
        Event::HostCommand {
            source,
            started_at,
            duration_ms,
            exit_code,
            command,
        } => connection
            .execute(
                "INSERT INTO commands (vm_id, source, started_at, duration_ms, exit_code, command)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![vm_id, source, started_at, duration_ms, exit_code, command],
            )
            .map(drop),
        Event::Stopped { reason, flushed } => {
            connection
                .execute(
                    "UPDATE vms SET stopped_at = ?1, stop_reason = ?2 WHERE id = ?3",
                    params![now(), reason, vm_id],
                )
                .expect("failed to record the VM stop in telemetry");
            return Some(flushed);
        }
    };
    result.expect("failed to write telemetry");
    None
}

fn write_report(connection: &mut Connection, vm_id: i64, report: &GuestReport) -> rusqlite::Result<()> {
    let transaction = connection.transaction()?;
    {
        let mut insert = transaction.prepare_cached(
            "INSERT INTO processes (vm_id, ended_at, command, elapsed_ms, cpu_ms, exit_code, uid)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for process in &report.processes {
            insert.execute(params![
                vm_id,
                process.ended_at,
                process.command,
                process.elapsed_ms,
                process.cpu_ms,
                process.exit_code,
                process.uid
            ])?;
        }

        let mut insert = transaction.prepare_cached(
            "INSERT INTO commands (vm_id, source, started_at, duration_ms, exit_code, command)
             VALUES (?1, 'shell', ?2, ?3, ?4, ?5)",
        )?;
        for command in &report.commands {
            insert.execute(params![
                vm_id,
                command.started_at,
                command.duration_ms,
                command.exit_code,
                command.command
            ])?;
        }

        let mut insert = transaction.prepare_cached(
            "INSERT INTO packages (vm_id, installed_at, manager, name, version) VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for package in &report.packages {
            insert.execute(params![
                vm_id,
                package.installed_at,
                package.manager,
                package.name,
                package.version
            ])?;
        }
    }
    transaction.commit()
}
