//! The shared object store in `~/.varmint/store`: buckets of versioned
//! objects that VMs reach over their agent link, as an S3-like HTTP API the
//! guest agent serves on its loopback.
//!
//! Every put and delete adds a version; deletes add a delete marker, so a VM
//! never erases history. `varmint store gc` drops old versions. Each VM may
//! only use the buckets it was started with (`--store BUCKET[:ro]`), and
//! every request, allowed or not, goes into the log.
//!
//! `store.sqlite` holds versions and the log, `blobs/` one file per version,
//! named by a random id, which no row names until the file is complete. Every
//! VM process and the CLI open the database themselves (WAL mode).

use crate::{instance, telemetry};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Read},
    path::PathBuf,
    sync::mpsc::Receiver,
    time::Duration,
};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS versions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    bucket TEXT NOT NULL,
    key TEXT NOT NULL,
    -- The file in blobs/, NULL for a delete marker.
    blob TEXT,
    size INTEGER NOT NULL,
    content_type TEXT,
    created_at REAL NOT NULL,
    created_by TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS versions_key ON versions(bucket, key, id);
-- Every request, and every version gc removed.
CREATE TABLE IF NOT EXISTS log (
    at REAL NOT NULL,
    caller TEXT NOT NULL,
    method TEXT NOT NULL,
    bucket TEXT NOT NULL,
    key TEXT NOT NULL,
    version INTEGER,
    status INTEGER NOT NULL,
    error TEXT
);
CREATE INDEX IF NOT EXISTS log_bucket ON log(bucket, at);
";

/// How long a request waits for another process's write to finish.
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

/// Bytes per agent frame of an object's body.
pub const CHUNK: usize = 1024 * 1024;

const MAX_KEY_BYTES: usize = 1024;

/// A bucket a VM may use: `BUCKET[:ro]`.
#[derive(Debug, Clone)]
pub struct Grant {
    pub bucket: String,
    pub readonly: bool,
}

impl Grant {
    pub fn parse(spec: &str) -> Result<Self, String> {
        let (bucket, readonly) = match spec.strip_suffix(":ro") {
            Some(bucket) => (bucket, true),
            None => (spec, false),
        };
        validate_bucket(bucket)?;
        Ok(Self {
            bucket: bucket.to_owned(),
            readonly,
        })
    }

    pub fn spec(&self) -> String {
        let suffix = if self.readonly { ":ro" } else { "" };
        format!("{}{suffix}", self.bucket)
    }
}

/// S3's bucket names: lowercase letters, digits, dots and dashes.
fn validate_bucket(bucket: &str) -> Result<(), String> {
    let valid = (1..=63).contains(&bucket.len())
        && bucket.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && bucket.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || ".-".contains(c));
    if !valid {
        return Err(format!(
            "invalid bucket name {bucket:?}; use up to 63 lowercase letters, digits, dots and dashes"
        ));
    }
    Ok(())
}

pub enum Caller<'a> {
    /// `varmint store` on the host, which may use every bucket.
    Host,
    Vm { name: &'a str, grants: &'a [Grant] },
}

impl Caller<'_> {
    fn name(&self) -> &str {
        match self {
            Caller::Host => "host",
            Caller::Vm { name, .. } => name,
        }
    }
}

/// One HTTP request, as the guest agent relays it. `bucket` and `key` are
/// the decoded path; `length` is the body's Content-Length, `None` to read
/// the body to its end.
#[derive(Serialize, Deserialize)]
pub struct Request {
    pub method: String,
    pub bucket: String,
    pub key: String,
    #[serde(default)]
    pub query: BTreeMap<String, String>,
    pub length: Option<u64>,
    pub content_type: Option<String>,
}

/// The status line and headers the guest agent answers with.
#[derive(Serialize)]
pub struct Head {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

pub enum Body {
    Empty,
    Json(Vec<u8>),
    Blob(File),
}

pub struct Response {
    pub head: Head,
    pub body: Body,
}

impl Response {
    fn json(status: u16, value: Value) -> Self {
        let mut bytes = serde_json::to_vec_pretty(&value).unwrap();
        bytes.push(b'\n');
        Self {
            head: Head {
                status,
                headers: vec![
                    ("Content-Type".into(), "application/json".into()),
                    ("Content-Length".into(), bytes.len().to_string()),
                ],
            },
            body: Body::Json(bytes),
        }
    }

    fn version(mut self, version: i64) -> Self {
        self.head.headers.push(("x-amz-version-id".into(), version.to_string()));
        self.head.headers.push(("ETag".into(), format!("\"{version}\"")));
        self
    }

    /// Calls `send` with the body in chunks of at most [`CHUNK`] bytes.
    pub fn send_body(self, mut send: impl FnMut(Vec<u8>)) -> io::Result<()> {
        match self.body {
            Body::Empty => {}
            Body::Json(bytes) => send(bytes),
            Body::Blob(mut file) => loop {
                let mut chunk = vec![0; CHUNK];
                let read = file.read(&mut chunk)?;
                if read == 0 {
                    break;
                }
                chunk.truncate(read);
                send(chunk);
            },
        }
        Ok(())
    }
}

/// A failed request: its status, S3's error code and what went wrong.
struct Failure {
    status: u16,
    code: &'static str,
    message: String,
}

fn failure(status: u16, code: &'static str, message: impl Into<String>) -> Failure {
    Failure {
        status,
        code,
        message: message.into(),
    }
}

impl From<rusqlite::Error> for Failure {
    fn from(error: rusqlite::Error) -> Self {
        failure(500, "InternalError", format!("store database: {error}"))
    }
}

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        failure(500, "InternalError", format!("store files: {error}"))
    }
}

/// A request body that arrives in agent frames; it ends with an empty one or
/// when the guest agent restarts.
pub struct Chunks {
    frames: Receiver<Vec<u8>>,
    chunk: Vec<u8>,
    offset: usize,
}

impl Chunks {
    pub fn new(frames: Receiver<Vec<u8>>) -> Self {
        Self {
            frames,
            chunk: Vec::new(),
            offset: 0,
        }
    }
}

impl Read for Chunks {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.offset == self.chunk.len() {
            self.chunk = self.frames.recv().unwrap_or_default();
            self.offset = 0;
        }
        let count = buffer.len().min(self.chunk.len() - self.offset);
        buffer[..count].copy_from_slice(&self.chunk[self.offset..self.offset + count]);
        self.offset += count;
        Ok(count)
    }
}

fn dir() -> PathBuf {
    instance::home().join("store")
}

fn blobs() -> PathBuf {
    dir().join("blobs")
}

fn open() -> Result<Connection, Failure> {
    fs::create_dir_all(blobs())?;
    let connection = Connection::open(dir().join("store.sqlite"))?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.execute_batch(SCHEMA)?;
    Ok(connection)
}

/// Answers `payload`, a JSON [`Request`] from the guest agent, reading the
/// object of a put from `body`.
pub fn handle_frame(caller: &Caller, payload: &[u8], body: &mut impl Read) -> Response {
    match serde_json::from_slice::<Request>(payload) {
        Ok(request) => handle(caller, &request, body),
        Err(error) => Response::json(400, json!({"code": "InvalidRequest", "message": error.to_string()})),
    }
}

/// Answers `request` and logs it.
pub fn handle(caller: &Caller, request: &Request, body: &mut impl Read) -> Response {
    let connection = match open() {
        Ok(connection) => connection,
        Err(error) => return Response::json(error.status, json!({"code": error.code, "message": error.message})),
    };
    let (response, version, error) = match route(&connection, caller, request, body) {
        Ok((response, version)) => (response, version, None),
        Err(error) => {
            let response = Response::json(error.status, json!({"code": error.code, "message": error.message}));
            (response, None, Some(error.message))
        }
    };
    let logged = connection.execute(
        "INSERT INTO log (at, caller, method, bucket, key, version, status, error) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        params![telemetry::now(), caller.name(), request.method, request.bucket, request.key, version, response.head.status, error],
    );
    match logged {
        Ok(_) => response,
        // A request that cannot be logged must not have happened unnoticed.
        Err(error) => Response::json(500, json!({"code": "InternalError", "message": format!("store log: {error}")})),
    }
}

/// The response and the version it read or wrote.
fn route(
    connection: &Connection,
    caller: &Caller,
    request: &Request,
    body: &mut impl Read,
) -> Result<(Response, Option<i64>), Failure> {
    let method = request.method.as_str();
    if request.bucket.is_empty() {
        if method != "GET" {
            return Err(failure(405, "MethodNotAllowed", format!("{method} / is not supported")));
        }
        return Ok((buckets(connection, caller)?, None));
    }
    let write = matches!(method, "PUT" | "DELETE");
    authorize(caller, &request.bucket, write)?;
    if request.key.is_empty() {
        if method != "GET" {
            return Err(failure(405, "MethodNotAllowed", format!("{method} on a bucket is not supported")));
        }
        return Ok((list(connection, request)?, None));
    }
    if request.key.len() > MAX_KEY_BYTES || request.key.contains('\0') {
        return Err(failure(400, "InvalidKey", "keys are at most 1024 bytes and contain no NUL"));
    }
    match method {
        "GET" | "HEAD" => get(connection, request),
        "PUT" => put(connection, caller, request, body),
        "DELETE" => delete(connection, caller, request),
        _ => Err(failure(405, "MethodNotAllowed", format!("{method} is not supported"))),
    }
}

fn authorize(caller: &Caller, bucket: &str, write: bool) -> Result<(), Failure> {
    validate_bucket(bucket).map_err(|message| failure(400, "InvalidBucketName", message))?;
    let Caller::Vm { grants, .. } = caller else {
        return Ok(());
    };
    match grants.iter().find(|grant| grant.bucket == bucket) {
        None => Err(failure(
            403,
            "AccessDenied",
            format!("bucket {bucket} is not granted to this VM; start it with --store {bucket}"),
        )),
        Some(grant) if write && grant.readonly => {
            Err(failure(403, "AccessDenied", format!("bucket {bucket} is read-only for this VM")))
        }
        Some(_) => Ok(()),
    }
}

fn buckets(connection: &Connection, caller: &Caller) -> Result<Response, Failure> {
    let buckets: Vec<Value> = match caller {
        Caller::Host => connection
            .prepare("SELECT DISTINCT bucket FROM versions ORDER BY bucket")?
            .query_map([], |row| row.get::<_, String>(0))?
            .map(|bucket| Ok(json!({"name": bucket?, "readonly": false})))
            .collect::<Result<_, rusqlite::Error>>()?,
        Caller::Vm { grants, .. } => grants
            .iter()
            .map(|grant| json!({"name": grant.bucket, "readonly": grant.readonly}))
            .collect(),
    };
    Ok(Response::json(200, json!({"buckets": buckets})))
}

const VERSION_COLUMNS: &str = "key, id, blob IS NULL, size, content_type, \
     strftime('%Y-%m-%dT%H:%M:%fZ', created_at, 'unixepoch'), created_by";

fn version_json(row: &rusqlite::Row) -> rusqlite::Result<Value> {
    Ok(json!({
        "key": row.get::<_, String>(0)?,
        "version": row.get::<_, i64>(1)?,
        "deleted": row.get::<_, bool>(2)?,
        "size": row.get::<_, i64>(3)?,
        "content_type": row.get::<_, Option<String>>(4)?,
        "created_at": row.get::<_, String>(5)?,
        "created_by": row.get::<_, String>(6)?,
    }))
}

/// The newest version of each key under `prefix`, without deleted keys; with
/// `versions` in the query every version, newest first.
fn list(connection: &Connection, request: &Request) -> Result<Response, Failure> {
    let prefix = request.query.get("prefix").map(String::as_str).unwrap_or_default();
    let filter = if request.query.contains_key("versions") {
        ""
    } else {
        "AND blob IS NOT NULL AND id = (SELECT MAX(id) FROM versions AS newer WHERE newer.bucket = versions.bucket AND newer.key = versions.key)"
    };
    let objects: Vec<Value> = connection
        .prepare(&format!(
            "SELECT {VERSION_COLUMNS} FROM versions WHERE bucket = ?1 AND substr(key, 1, length(?2)) = ?2 {filter} ORDER BY key, id DESC"
        ))?
        .query_map(params![request.bucket, prefix], version_json)?
        .collect::<Result<_, _>>()?;
    Ok(Response::json(200, json!({"objects": objects})))
}

fn get(connection: &Connection, request: &Request) -> Result<(Response, Option<i64>), Failure> {
    let version = match request.query.get("versionId") {
        Some(version) => Some(
            version
                .parse::<i64>()
                .map_err(|_| failure(400, "InvalidArgument", format!("invalid versionId {version:?}")))?,
        ),
        None => None,
    };
    let row = connection
        .query_row(
            "SELECT id, blob, size, content_type FROM versions WHERE bucket = ?1 AND key = ?2 AND (?3 IS NULL OR id = ?3) ORDER BY id DESC LIMIT 1",
            params![request.bucket, request.key, version],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, i64>(2)?, row.get::<_, Option<String>>(3)?)),
        )
        .optional()?;
    let missing = || match version {
        Some(version) => failure(404, "NoSuchVersion", format!("{}/{} has no version {version}", request.bucket, request.key)),
        None => failure(404, "NoSuchKey", format!("{}/{} does not exist", request.bucket, request.key)),
    };
    let (id, blob, size, content_type) = row.ok_or_else(missing)?;
    let Some(blob) = blob else {
        let response = Response::json(404, json!({"code": "NoSuchKey", "message": format!("{}/{} is deleted", request.bucket, request.key)}));
        let mut response = response.version(id);
        response.head.headers.push(("x-amz-delete-marker".into(), "true".into()));
        return Ok((response, Some(id)));
    };
    // gc may have removed the version since it was read.
    let file = match File::open(blobs().join(blob)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(missing()),
        Err(error) => return Err(error.into()),
    };
    let mut headers = vec![("Content-Length".to_owned(), size.to_string())];
    if let Some(content_type) = content_type {
        headers.push(("Content-Type".into(), content_type));
    }
    let body = if request.method == "HEAD" { Body::Empty } else { Body::Blob(file) };
    Ok((Response { head: Head { status: 200, headers }, body }.version(id), Some(id)))
}

fn put(
    connection: &Connection,
    caller: &Caller,
    request: &Request,
    body: &mut impl Read,
) -> Result<(Response, Option<i64>), Failure> {
    let blob = uuid::Uuid::new_v4().to_string();
    let path = blobs().join(&blob);
    let size = io::copy(body, &mut File::create(&path)?).and_then(|size| {
        File::open(&path)?.sync_all()?;
        Ok(size)
    });
    let size = match size {
        Ok(size) if request.length.is_none_or(|length| length == size) => size,
        Ok(size) => {
            fs::remove_file(&path)?;
            let expected = request.length.unwrap();
            return Err(failure(400, "IncompleteBody", format!("received {size} of {expected} bytes")));
        }
        Err(error) => {
            fs::remove_file(&path)?;
            return Err(error.into());
        }
    };
    let added = connection.execute(
        "INSERT INTO versions (bucket, key, blob, size, content_type, created_at, created_by) VALUES (?, ?, ?, ?, ?, ?, ?)",
        params![request.bucket, request.key, blob, size as i64, request.content_type, telemetry::now(), caller.name()],
    );
    let id = match added {
        Ok(_) => connection.last_insert_rowid(),
        Err(error) => {
            fs::remove_file(&path)?;
            return Err(error.into());
        }
    };
    let response = Response::json(200, json!({"key": request.key, "version": id, "size": size}));
    Ok((response.version(id), Some(id)))
}

/// Adds a delete marker; earlier versions stay until gc removes them.
fn delete(connection: &Connection, caller: &Caller, request: &Request) -> Result<(Response, Option<i64>), Failure> {
    connection.execute(
        "INSERT INTO versions (bucket, key, blob, size, created_at, created_by) VALUES (?, ?, NULL, 0, ?, ?)",
        params![request.bucket, request.key, telemetry::now(), caller.name()],
    )?;
    let id = connection.last_insert_rowid();
    let mut response = Response::json(200, json!({"key": request.key, "version": id, "deleted": true})).version(id);
    response.head.headers.push(("x-amz-delete-marker".into(), "true".into()));
    Ok((response, Some(id)))
}

/// The log, oldest first, of one bucket or all of them: time, caller,
/// method, status, bucket/key, version and error, separated by tabs.
pub fn log(bucket: Option<&str>) -> Result<Vec<String>, String> {
    let connection = open().map_err(|error| error.message)?;
    connection
        .prepare(
            "SELECT strftime('%Y-%m-%dT%H:%M:%fZ', at, 'unixepoch'), caller, method, status, bucket, key, version, error \
             FROM log WHERE ?1 IS NULL OR bucket = ?1 ORDER BY at",
        )
        .and_then(|mut statement| {
            statement
                .query_map([bucket], |row| {
                    Ok(format!(
                        "{}\t{}\t{}\t{}\t{}/{}\t{}\t{}",
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<i64>>(6)?.map(|version| version.to_string()).unwrap_or_default(),
                        row.get::<_, Option<String>>(7)?.unwrap_or_default(),
                    ))
                })?
                .collect()
        })
        .map_err(|error| format!("store log: {error}"))
}

/// Removes all but the newest `keep` versions of every key and logs each
/// removal; returns how many versions and bytes went.
pub fn gc(keep: u32) -> Result<(i64, i64), String> {
    assert!(keep >= 1, "gc keeps at least the newest version of every key");
    let mut connection = open().map_err(|error| error.message)?;
    let collect = |transaction: &Transaction| -> rusqlite::Result<(Vec<String>, i64, i64)> {
        let old = "SELECT id FROM (SELECT id, ROW_NUMBER() OVER (PARTITION BY bucket, key ORDER BY id DESC) AS newer FROM versions) WHERE newer > ?1";
        let (count, bytes) = transaction.query_row(
            &format!("SELECT COUNT(*), COALESCE(SUM(size), 0) FROM versions WHERE id IN ({old})"),
            [keep],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )?;
        let blobs = transaction
            .prepare(&format!("SELECT blob FROM versions WHERE blob IS NOT NULL AND id IN ({old})"))?
            .query_map([keep], |row| row.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        transaction.execute(
            &format!(
                "INSERT INTO log (at, caller, method, bucket, key, version, status) \
                 SELECT ?2, 'host', 'GC', bucket, key, id, 200 FROM versions WHERE id IN ({old})"
            ),
            params![keep, telemetry::now()],
        )?;
        transaction.execute(&format!("DELETE FROM versions WHERE id IN ({old})"), [keep])?;
        Ok((blobs, count, bytes))
    };
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| format!("store gc: {error}"))?;
    let (removed, count, bytes) = collect(&transaction).map_err(|error| format!("store gc: {error}"))?;
    transaction.commit().map_err(|error| format!("store gc: {error}"))?;
    // Readers that found a version before the commit get NoSuchVersion when
    // its file is gone, or keep reading the file they already opened.
    for blob in removed {
        fs::remove_file(blobs().join(&blob)).map_err(|error| format!("failed to remove blob {blob}: {error}"))?;
    }
    Ok((count, bytes))
}
