//! Wire formats shared by the CLI, the VMM control socket and the guest agent.
//!
//! The control socket starts with one JSON request line. `open` requests then
//! switch the connection to frames, the same framing used on the guest agent
//! port: `[u32 stream][u8 kind][u32 len][payload]`, little endian.

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{self, BufRead, Read, Write},
    os::fd::AsRawFd,
};

/// Bytes a screen stream may queue in each socket on its way to the client.
/// Kept small so a slow client lowers the frame rate instead of falling
/// seconds behind.
pub const STREAM_QUEUE_BYTES: libc::c_int = 8 * 1024;

/// Sets the integer socket option `name` at `level`.
pub fn set_socket_option(socket: &impl AsRawFd, level: libc::c_int, name: libc::c_int, value: libc::c_int) -> io::Result<()> {
    let result = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            level,
            name,
            (&raw const value).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub mod kind {
    pub const OPEN: u8 = 1;
    pub const DATA: u8 = 2;
    pub const STDERR: u8 = 3;
    pub const EOF: u8 = 4;
    pub const EXIT: u8 = 5;
    pub const RESIZE: u8 = 6;
    pub const HELLO: u8 = 7;
    pub const SETUP: u8 = 8;
    pub const CLOSE: u8 = 9;
    pub const SETUP_DONE: u8 = 10;
    pub const TELEMETRY: u8 = 11;
    /// The host clipboard image as PNG for the guest clipboard, empty to clear it.
    pub const CLIPBOARD_IMAGE: u8 = 12;
    /// A [`RegistryTokenRequest`] from the guest, answered with a [`RegistryToken`].
    pub const REGISTRY_TOKEN: u8 = 13;
}

/// The kinds of messages in an H.264 [`Request::Stream`].
pub mod stream_kind {
    pub const VIDEO: u8 = 0;
    pub const AUDIO: u8 = 1;
}

const HEADER_LEN: usize = 9;
pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

pub struct Frame {
    pub stream: u32,
    pub kind: u8,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(stream: u32, kind: u8, payload: Vec<u8>) -> Self {
        Self { stream, kind, payload }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HEADER_LEN + self.payload.len());
        bytes.extend_from_slice(&self.stream.to_le_bytes());
        bytes.push(self.kind);
        bytes.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&self.payload);
        bytes
    }
}

pub struct Deframer {
    buf: Vec<u8>,
}

impl Deframer {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn push(&mut self, bytes: &[u8], mut on_frame: impl FnMut(Frame)) {
        self.buf.extend_from_slice(bytes);

        while self.buf.len() >= HEADER_LEN {
            let len = u32::from_le_bytes(self.buf[5..9].try_into().unwrap()) as usize;
            assert!(len <= MAX_PAYLOAD, "agent frame of {len} bytes exceeds the protocol limit");
            if self.buf.len() < HEADER_LEN + len {
                return;
            }

            let frame = Frame {
                stream: u32::from_le_bytes(self.buf[0..4].try_into().unwrap()),
                kind: self.buf[4],
                payload: self.buf[HEADER_LEN..HEADER_LEN + len].to_vec(),
            };
            self.buf.drain(..HEADER_LEN + len);
            on_frame(frame);
        }
    }
}

pub fn read_frame(reader: &mut impl Read) -> io::Result<Option<Frame>> {
    let mut header = [0u8; HEADER_LEN];
    match reader.read_exact(&mut header) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }

    let len = u32::from_le_bytes(header[5..9].try_into().unwrap()) as usize;
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame exceeds the protocol limit"));
    }
    let mut payload = vec![0; len];
    reader.read_exact(&mut payload)?;

    Ok(Some(Frame {
        stream: u32::from_le_bytes(header[0..4].try_into().unwrap()),
        kind: header[4],
        payload,
    }))
}

pub fn write_frame(writer: &mut impl Write, frame: &Frame) -> io::Result<()> {
    writer.write_all(&frame.encode())?;
    writer.flush()
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Request {
    Wait,
    Screenshot,
    /// A frame whenever the screen changes, at most `fps` per second, each
    /// starting with little-endian u32 width and height. See [`Codec`].
    /// `audio` adds the guest's sound to H.264 streams.
    Stream {
        fps: u32,
        codec: Codec,
        #[serde(default)]
        audio: bool,
    },
    Input { events: Vec<InputEvent> },
    /// Input events as JSON lines until the client disconnects, for interactive use.
    InputStream,
    /// Resizes the guest display to the aspect ratio of `width` x `height`,
    /// scaled down to at most the VM's configured pixel count.
    Resize { width: u32, height: u32 },
    Open { session: Session },
    /// What the VM process takes from the host, in [`Reply::memory_bytes`]
    /// and [`Reply::cpu_ms`].
    Usage,
    Stop,
}

/// How a [`Request::Stream`] sends frames after their size.
#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    /// Packed RGB rows, for local clients that draw pixels themselves.
    Rgb,
    /// Messages that each start with a u8 [`stream_kind`]. Video follows with
    /// the size, a u8 that is 1 for keyframes, a little-endian u32 length and
    /// an H.264 access unit in Annex B form, the first one a keyframe with its
    /// parameter sets. A still screen is encoded once more after a moment,
    /// sharpening it, and then sends nothing. Audio follows with a
    /// little-endian u32 length and an Opus packet of 20 ms, 48 kHz stereo.
    H264,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum InputEvent {
    Move { x: u32, y: u32 },
    Button { button: u16, down: bool },
    Key { code: u16, down: bool },
    Scroll { value: i32, horizontal: bool },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Session {
    Pty {
        argv: Vec<String>,
        cols: u16,
        rows: u16,
        /// TERM and the variables that tell programs which terminal emulator they run in.
        terminal: BTreeMap<String, String>,
    },
    Exec {
        argv: Vec<String>,
    },
    Tcp {
        port: u16,
    },
}

#[derive(Serialize, Deserialize)]
pub struct Setup {
    pub time_ms: u64,
    pub authorized_keys: String,
    pub mounts: Vec<GuestMount>,
    pub environment: BTreeMap<String, String>,
    pub seeds: Option<GuestSeeds>,
    /// Registry hosts the guest mirrors on its loopback, see [`kind::REGISTRY_TOKEN`].
    pub registries: Vec<String>,
}

/// Where the staged seeds are mounted and where each item goes.
#[derive(Serialize, Deserialize, Clone)]
pub struct GuestSeeds {
    pub mount: String,
    pub items: Vec<SeedItem>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SeedItem {
    /// Name inside the seed mount.
    pub source: String,
    pub target: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct GuestMount {
    pub tag: String,
    pub path: String,
    pub readonly: bool,
}

#[derive(Serialize, Deserialize)]
pub struct RegistryTokenRequest {
    pub id: u64,
    pub registry: String,
    pub repository: String,
}

/// A bearer token to pull `repository`, or why there is none.
#[derive(Serialize, Deserialize, Default)]
pub struct RegistryToken {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The agent's answer to [`Setup`]; `errors` is empty on success.
#[derive(Serialize, Deserialize)]
pub struct SetupDone {
    pub errors: Vec<String>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Reply {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boot_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub png_len: Option<usize>,
    /// The physical footprint of the VM process, the memory macOS counts for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    /// The CPU time the VM process used since it started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_ms: Option<u64>,
}

pub fn write_json(writer: &mut impl Write, value: &impl Serialize) -> io::Result<()> {
    let mut line = serde_json::to_vec(value).map_err(io::Error::other)?;
    line.push(b'\n');
    writer.write_all(&line)?;
    writer.flush()
}

pub fn read_json<T: for<'de> Deserialize<'de>>(reader: &mut impl BufRead) -> io::Result<T> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed"));
    }
    serde_json::from_str(&line).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}
