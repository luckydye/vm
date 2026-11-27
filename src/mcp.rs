//! `vm mcp`: a Model Context Protocol server on stdin and stdout, so agents
//! can manage VMs. Each tool runs this vm binary as a child process, so tools
//! behave exactly like the commands and a failing command cannot take the
//! server down. Speaks the stateless 2026-07-28 revision and, for clients
//! that open with `initialize`, the handshake-based revisions before it.

use crate::{
    cli::remote_statuses,
    node::{Node, Status, Target},
    screen::base64,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    env,
    io::{self, BufRead, Read, Write},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
};

const MODERN_VERSIONS: [&str; 1] = ["2026-07-28"];
const LEGACY_VERSIONS: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];
const VERSION_META: &str = "io.modelcontextprotocol/protocolVersion";

const INSTRUCTIONS: &str = "\
Manages quick, disposable Linux VMs (Debian with a Wayland desktop). Start one \
with run, which returns its name, then use exec to run shell commands in it as \
the user varmint, with passwordless sudo. VMs are ephemeral unless created with \
persist and are deleted when they stop; stop the VMs you started once you are \
done. Remote VMs are named NODE/NAME. For graphical work, take a screenshot and \
use its pixel coordinates with click and move.";

/// What a tool call answers: content blocks for the model, plus structured data.
struct Outcome {
    content: Vec<Value>,
    structured: Option<Value>,
    error: bool,
}

impl Outcome {
    fn text(text: impl Into<String>) -> Self {
        Self { content: vec![json!({"type": "text", "text": text.into()})], structured: None, error: false }
    }

    fn error(text: impl Into<String>) -> Self {
        Self { error: true, ..Self::text(text) }
    }

    fn json(value: Value) -> Self {
        Self { structured: Some(value.clone()), ..Self::text(value.to_string()) }
    }
}

/// Children of tool calls still running, by request id, so a cancellation can
/// kill them. A child leaves the map before it is reaped, so a kill never
/// reaches a reused pid.
type Running = Arc<Mutex<HashMap<String, Child>>>;

pub fn serve() {
    let output = Arc::new(Mutex::new(io::stdout()));
    let running: Running = Arc::default();
    for line in io::stdin().lock().lines() {
        let line = line.unwrap_or_else(|error| panic!("failed to read stdin: {error}"));
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(error) => {
                send(&output, &failure(Value::Null, -32700, &format!("parse error: {error}"), None));
                continue;
            }
        };
        let method = message["method"].as_str().unwrap_or_default().to_owned();
        let Some(id) = message.get("id").cloned() else {
            if method == "notifications/cancelled" {
                cancel(&running, &message["params"]["requestId"]);
            }
            continue;
        };
        let params = message.get("params").cloned().unwrap_or(json!({}));
        if let Some(version) = params["_meta"][VERSION_META].as_str() {
            if !MODERN_VERSIONS.contains(&version) {
                let data = json!({"supported": supported_versions(), "requested": version});
                send(&output, &failure(id, -32022, "Unsupported protocol version", Some(data)));
                continue;
            }
        }
        match method.as_str() {
            "tools/call" => {
                let (output, running) = (output.clone(), running.clone());
                thread::spawn(move || {
                    // A cancelled call gets no answer.
                    if let Some(outcome) = call(&params, &running, &id.to_string()) {
                        send(&output, &success(id, call_result(outcome)));
                    }
                });
            }
            "initialize" => send(&output, &success(id, initialize(&params))),
            "server/discover" => send(&output, &success(id, discover())),
            "ping" => send(&output, &success(id, json!({}))),
            "tools/list" => send(&output, &success(id, json!({"resultType": "complete", "tools": tools()}))),
            _ => send(&output, &failure(id, -32601, &format!("method not found: {method}"), None)),
        }
    }
}

fn supported_versions() -> Vec<&'static str> {
    MODERN_VERSIONS.iter().chain(&LEGACY_VERSIONS).copied().collect()
}

fn server_info() -> Value {
    json!({"name": "vm", "version": env!("CARGO_PKG_VERSION")})
}

/// The legacy handshake: the client's version when it is one we speak, else the newest legacy one.
fn initialize(params: &Value) -> Value {
    let requested = params["protocolVersion"].as_str().unwrap_or_default();
    let version = LEGACY_VERSIONS.iter().find(|known| **known == requested).unwrap_or(&LEGACY_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {}},
        "serverInfo": server_info(),
        "instructions": INSTRUCTIONS,
    })
}

fn discover() -> Value {
    json!({
        "resultType": "complete",
        "supportedVersions": supported_versions(),
        "capabilities": {"tools": {}},
        "_meta": {"io.modelcontextprotocol/serverInfo": server_info()},
        "instructions": INSTRUCTIONS,
    })
}

fn success(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn failure(id: Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut error = json!({"code": code, "message": message});
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({"jsonrpc": "2.0", "id": id, "error": error})
}

fn send(output: &Mutex<io::Stdout>, message: &Value) {
    let mut output = output.lock().unwrap();
    writeln!(output, "{message}").and_then(|()| output.flush()).unwrap_or_else(|error| panic!("failed to write stdout: {error}"));
}

fn call_result(outcome: Outcome) -> Value {
    let mut result = json!({"resultType": "complete", "content": outcome.content, "isError": outcome.error});
    if let Some(structured) = outcome.structured {
        result["structuredContent"] = structured;
    }
    result
}

fn cancel(running: &Running, id: &Value) {
    if let Some(mut child) = running.lock().unwrap().remove(&id.to_string()) {
        let _ = child.kill();
        thread::spawn(move || child.wait());
    }
}

fn tools() -> Value {
    let name = json!({"type": "string", "description": "VM name, or NODE/NAME for a VM on a node"});
    json!([
        {
            "name": "list",
            "title": "List VMs",
            "description": "Lists the VMs of this machine and of every configured node with their state \
                            (running, suspended or stopped), whether they persist, and their hardware.",
            "inputSchema": {"type": "object", "additionalProperties": false},
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "run",
            "title": "Start a VM",
            "description": "Creates and boots a VM, or starts a stopped persistent one, and returns its name \
                            once the guest is ready, about a second later. Without persist it is ephemeral and \
                            deleted when stopped.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "name": {"type": "string", "description": "VM name or NODE/NAME; a random name when left out"},
                    "persist": {"type": "boolean", "description": "Keep the VM and its disk after it stops"},
                    "base": {"type": "string", "description": "Image made with save to create the VM from"},
                    "node": {"type": "string", "description": "Node from ~/.vm/nodes to run on"},
                    "any": {"type": "boolean", "description": "Pick the least loaded node with room"},
                    "cpus": {"type": "integer", "minimum": 1},
                    "memory_mib": {"type": "integer", "minimum": 1},
                    "disk_gib": {"type": "integer", "minimum": 1},
                    "size": {"type": "string", "pattern": "^[0-9]+x[0-9]+$", "description": "Screen size as WxH"},
                    "mounts": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Host directories to share, as HOST:GUEST[:ro]; relative HOST paths are \
                                        under the server's working directory, relative GUEST paths under \
                                        /home/varmint",
                    },
                    "seeds": {"type": "array", "items": {"type": "string"}, "description": "Host files to copy in at start, as HOST:GUEST"},
                    "stores": {"type": "array", "items": {"type": "string"}, "description": "Shared store buckets, as BUCKET[:ro]"},
                    "env": {"type": "array", "items": {"type": "string"}, "description": "Names of host environment variables to copy into the VM"},
                    "idle_suspend": {"type": "integer", "minimum": 0, "description": "Minutes without clients before the VM suspends, 0 for never"},
                    "wait_network": {"type": "boolean", "description": "Return only once the guest has network and DNS (default true)"},
                },
            },
        },
        {
            "name": "exec",
            "title": "Run a command in a VM",
            "description": "Runs a bash command in a running or stopped persistent VM as the user varmint, \
                            waits for it to exit and returns its exit code, stdout and stderr.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["name", "command"],
                "properties": {
                    "name": name,
                    "command": {"type": "string", "description": "Run with bash -lc"},
                    "stdin": {"type": "string", "description": "Text for the command's stdin"},
                },
            },
            "outputSchema": {
                "type": "object",
                "required": ["exit_code", "stdout", "stderr"],
                "properties": {
                    "exit_code": {"type": "integer"},
                    "stdout": {"type": "string"},
                    "stderr": {"type": "string"},
                },
            },
        },
        {
            "name": "stop",
            "title": "Stop VMs",
            "description": "Stops VMs. Ephemeral VMs are deleted when they stop; with delete, persistent VMs \
                            and their disks are deleted too.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["names"],
                "properties": {
                    "names": {"type": "array", "items": name, "minItems": 1},
                    "delete": {"type": "boolean", "description": "Delete the VMs and their disks"},
                },
            },
            "annotations": {"destructiveHint": true},
        },
        {
            "name": "save",
            "title": "Save a VM as an image",
            "description": "Stops a running persistent VM and saves its disk as an image that run can create VMs from with base.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["name", "image"],
                "properties": {"name": name, "image": {"type": "string"}},
            },
        },
        {
            "name": "screenshot",
            "title": "Screenshot a VM",
            "description": "Returns the VM's screen as a PNG. Its pixel positions are the coordinates click and move take.",
            "inputSchema": {"type": "object", "additionalProperties": false, "required": ["name"], "properties": {"name": name}},
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "click",
            "title": "Click",
            "description": "Clicks a mouse button at screenshot pixel x y, or where the pointer is without them.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["name"],
                "properties": {
                    "name": name,
                    "x": {"type": "integer", "minimum": 0},
                    "y": {"type": "integer", "minimum": 0},
                    "button": {"enum": ["left", "right", "middle"], "description": "Default left"},
                    "double": {"type": "boolean"},
                },
            },
        },
        {
            "name": "move",
            "title": "Move the pointer",
            "description": "Moves the mouse pointer to screenshot pixel x y.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["name", "x", "y"],
                "properties": {"name": name, "x": {"type": "integer", "minimum": 0}, "y": {"type": "integer", "minimum": 0}},
            },
        },
        {
            "name": "type",
            "title": "Type text",
            "description": "Types text with a US keyboard layout: printable ASCII, newlines and tabs.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["name", "text"],
                "properties": {"name": name, "text": {"type": "string"}},
            },
        },
        {
            "name": "key",
            "title": "Press keys",
            "description": "Presses each combo in turn, such as enter or ctrl+alt+t. Keys are ctrl, shift, alt, \
                            super, enter, esc, tab, space, backspace, delete, insert, home, end, pageup, \
                            pagedown, up, down, left, right, f1 to f12, or a single character.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["name", "combos"],
                "properties": {"name": name, "combos": {"type": "array", "items": {"type": "string"}, "minItems": 1}},
            },
        },
        {
            "name": "scroll",
            "title": "Scroll",
            "description": "Scrolls the mouse wheel by amount steps at the pointer; positive scrolls up, or right when horizontal.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["name", "amount"],
                "properties": {"name": name, "amount": {"type": "integer"}, "horizontal": {"type": "boolean"}},
            },
        },
    ])
}

/// Runs a tool; `None` when the call was cancelled.
fn call(params: &Value, running: &Running, id: &str) -> Option<Outcome> {
    let tool = params["name"].as_str().unwrap_or_default();
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
    match tool {
        "list" => Some(list()),
        "screenshot" => {
            let result = argument(&arguments, "name")
                .and_then(|name| vm(&["screenshot".into(), name, "-".into()], None, running, id));
            result.map(|output| output.map(screenshot)).unwrap_or_else(|error| Some(Outcome::error(error)))
        }
        "exec" => {
            let result = (|| {
                let name = argument(&arguments, "name")?;
                // `vm run` would create a VM that does not exist.
                Target::resolve(&name)?;
                let argv = ["run", &name, "--", "bash", "-lc", &argument(&arguments, "command")?].map(str::to_owned);
                vm(&argv, arguments["stdin"].as_str(), running, id)
            })();
            result.map(|output| output.map(exec)).unwrap_or_else(|error| Some(Outcome::error(error)))
        }
        _ => match command(tool, &arguments) {
            Ok(argv) => match vm(&argv, None, running, id) {
                Ok(output) => output.map(Output::outcome),
                Err(error) => Some(Outcome::error(error)),
            },
            Err(error) => Some(Outcome::error(error)),
        },
    }
}

/// The vm command line of a tool that only reports what the command printed.
fn command(tool: &str, arguments: &Value) -> Result<Vec<String>, String> {
    let mut argv = vec![tool.to_owned()];
    match tool {
        "run" => {
            if let Some(name) = arguments["name"].as_str() {
                argv.push(name.to_owned());
            }
            for (key, flag) in [("persist", "--persist"), ("any", "--any")] {
                if arguments[key].as_bool() == Some(true) {
                    argv.push(flag.into());
                }
            }
            if arguments["wait_network"].as_bool() != Some(false) {
                argv.push("--wait-network".into());
            }
            for (key, flag) in [("base", "--base"), ("node", "--node"), ("size", "--size")] {
                if let Some(value) = arguments[key].as_str() {
                    argv.extend([flag.into(), value.to_owned()]);
                }
            }
            for (key, flag) in [("cpus", "--cpus"), ("memory_mib", "--memory"), ("disk_gib", "--disk"), ("idle_suspend", "--idle-suspend")] {
                if let Some(value) = arguments[key].as_u64() {
                    argv.extend([flag.into(), value.to_string()]);
                }
            }
            for (key, flag) in [("mounts", "--mount"), ("seeds", "--seed"), ("stores", "--store"), ("env", "--env")] {
                for value in strings(arguments, key)? {
                    argv.extend([flag.into(), value]);
                }
            }
        }
        "stop" => {
            argv.extend(strings(arguments, "names")?);
            if arguments["delete"].as_bool() == Some(true) {
                argv.push("--delete".into());
            }
        }
        "save" => argv.extend([argument(arguments, "name")?, argument(arguments, "image")?]),
        "move" => argv.extend([argument(arguments, "name")?, coordinate(arguments, "x")?, coordinate(arguments, "y")?]),
        "click" => {
            argv.push(argument(arguments, "name")?);
            match (arguments.get("x"), arguments.get("y")) {
                (None, None) => {}
                (Some(_), Some(_)) => argv.extend([coordinate(arguments, "x")?, coordinate(arguments, "y")?]),
                _ => return Err("give both x and y, or neither".into()),
            }
            match arguments["button"].as_str() {
                None | Some("left") => {}
                Some("right") => argv.push("--right".into()),
                Some("middle") => argv.push("--middle".into()),
                Some(other) => return Err(format!("unknown button {other:?}")),
            }
            if arguments["double"].as_bool() == Some(true) {
                argv.push("--double".into());
            }
        }
        "type" => argv.extend([argument(arguments, "name")?, argument(arguments, "text")?]),
        "key" => {
            argv.push(argument(arguments, "name")?);
            argv.extend(strings(arguments, "combos")?);
        }
        "scroll" => {
            let amount = arguments["amount"].as_i64().ok_or("amount must be an integer")?;
            argv.extend([argument(arguments, "name")?, amount.to_string()]);
            if arguments["horizontal"].as_bool() == Some(true) {
                argv.push("--horizontal".into());
            }
        }
        _ => return Err(format!("unknown tool {tool:?}")),
    }
    Ok(argv)
}

fn argument(arguments: &Value, key: &str) -> Result<String, String> {
    arguments[key].as_str().map(str::to_owned).ok_or_else(|| format!("{key} must be a string"))
}

fn coordinate(arguments: &Value, key: &str) -> Result<String, String> {
    arguments[key].as_u64().map(|value| value.to_string()).ok_or_else(|| format!("{key} must be a pixel coordinate"))
}

fn strings(arguments: &Value, key: &str) -> Result<Vec<String>, String> {
    match &arguments[key] {
        Value::Null => Ok(Vec::new()),
        Value::Array(values) => values
            .iter()
            .map(|value| value.as_str().map(str::to_owned).ok_or_else(|| format!("{key} must hold strings")))
            .collect(),
        _ => Err(format!("{key} must be an array of strings")),
    }
}

struct Output {
    code: i32,
    stdout: Vec<u8>,
    stderr: String,
}

impl Output {
    fn outcome(self) -> Outcome {
        let text = [String::from_utf8_lossy(&self.stdout).trim(), self.stderr.trim()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        match self.code {
            0 => Outcome::text(if text.is_empty() { "done".to_owned() } else { text }),
            _ => Outcome::error(text),
        }
    }
}

/// Runs this vm binary with `argv` and collects its output; `None` when the call was cancelled.
fn vm(argv: &[String], stdin: Option<&str>, running: &Running, id: &str) -> Result<Option<Output>, String> {
    let executable = env::current_exe().and_then(|path| path.canonicalize()).map_err(|error| error.to_string())?;
    let mut child = Command::new(executable)
        .args(argv)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to run vm: {error}"))?;
    let input = child.stdin.take().map(|mut pipe| {
        let text = stdin.expect("a piped stdin has text").to_owned();
        // A command that exits without reading its input closes the pipe, which is fine.
        thread::spawn(move || drop(pipe.write_all(text.as_bytes())))
    });
    let mut stdout_pipe = child.stdout.take().expect("stdout is piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr is piped");
    running.lock().unwrap().insert(id.to_owned(), child);

    let stderr_reader = thread::spawn(move || {
        let mut stderr = String::new();
        let _ = stderr_pipe.read_to_string(&mut stderr);
        stderr
    });
    let mut stdout = Vec::new();
    stdout_pipe.read_to_end(&mut stdout).map_err(|error| error.to_string())?;
    let stderr = stderr_reader.join().expect("the stderr reader panicked");
    if let Some(input) = input {
        input.join().expect("the stdin writer panicked");
    }

    // Reaped only once out of the map, so a cancellation never kills a reused pid.
    let Some(mut child) = running.lock().unwrap().remove(id) else {
        return Ok(None);
    };
    let status = child.wait().map_err(|error| error.to_string())?;
    let code = status.code().unwrap_or(-1);
    Ok(Some(Output { code, stdout, stderr }))
}

fn exec(output: Output) -> Outcome {
    Outcome::json(json!({
        "exit_code": output.code,
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": output.stderr,
    }))
}

fn screenshot(output: Output) -> Outcome {
    if output.code != 0 {
        return output.outcome();
    }
    let png = output.stdout;
    // The IHDR chunk follows the 8-byte signature: length, type, then width and height.
    let width = u32::from_be_bytes(png[16..20].try_into().expect("a PNG has an IHDR chunk"));
    let height = u32::from_be_bytes(png[20..24].try_into().expect("a PNG has an IHDR chunk"));
    Outcome {
        content: vec![
            json!({"type": "image", "data": base64(&png), "mimeType": "image/png"}),
            json!({"type": "text", "text": format!("{width}x{height} pixels")}),
        ],
        structured: None,
        error: false,
    }
}

fn list() -> Outcome {
    let local = match Status::local() {
        Ok(status) => status,
        Err(error) => return Outcome::error(error),
    };
    let nodes = match Node::load() {
        Ok(nodes) => nodes,
        Err(error) => return Outcome::error(error),
    };
    let mut statuses = vec![local];
    let mut errors = Vec::new();
    for (node, result) in remote_statuses(nodes) {
        match result {
            Ok(status) => statuses.push(status),
            Err(error) => errors.push(json!({"node": node, "error": error})),
        }
    }
    let mut vms = Vec::new();
    for status in &statuses {
        for vm in &status.vms {
            let name = match &status.node {
                Some(node) => format!("{}/{}", node.name, vm.name),
                None => vm.name.clone(),
            };
            let state = match vm.suspended {
                Some(true) => "suspended",
                Some(false) => "running",
                None => "stopped",
            };
            let hardware = &vm.hardware;
            vms.push(json!({
                "name": name,
                "state": state,
                "persist": hardware.persist,
                "cpus": hardware.cpus,
                "memory_mib": hardware.memory_mib,
                "disk_gib": hardware.disk_gib,
                "size": format!("{}x{}", hardware.width, hardware.height),
                "mounts": vm.mounts.iter().map(|mount| mount.spec()).collect::<Vec<_>>(),
            }));
        }
    }
    vms.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Outcome::json(json!({"vms": vms, "node_errors": errors}))
}
