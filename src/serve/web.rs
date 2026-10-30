//! The web UI of `varmint serve --ui`: HTTP on the serve port, told apart from
//! JSON requests by their first byte. The page and its files are compiled in.
//! `POST /api` takes a serve request as its body and answers with the reply;
//! a WebSocket on `/attach` takes an attach request as its first message and
//! then carries the control socket's bytes both ways in binary messages, the
//! first of them the serve reply line. The browser sends the token itself,
//! which the page takes from the `#token=` of the URL serve prints.

use super::{Client, Connection, Deadline, respond};
use crate::screen::base64;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{self, BufRead, BufReader, Cursor, Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    thread,
};

const FILES: &[(&str, &str, &[u8])] = &[
    ("/", "text/html; charset=utf-8", include_bytes!("../../ui/index.html")),
    ("/app.css", "text/css", include_bytes!("../../ui/app.css")),
    ("/ui.js", "text/javascript", include_bytes!("../../ui/ui.js")),
    ("/data.js", "text/javascript", include_bytes!("../../ui/data.js")),
    ("/logo.png", "image/png", include_bytes!("../../assets/logo.png")),
];

/// WebSocket messages carry stream data; a frame is never this big.
const MAX_MESSAGE_BYTES: u64 = 32 * 1024 * 1024;

const OPCODE_BINARY: u8 = 2;
const OPCODE_CLOSE: u8 = 8;

struct Request {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

pub fn handle<C: Connection>(
    client: &Client,
    mut reader: BufReader<io::Take<Deadline<C>>>,
    mut writer: C,
    token: &str,
) -> io::Result<()> {
    let request = read_request(&mut reader)?;
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/attach") => websocket(client, &request, reader, writer, token),
        ("POST", "/api") => {
            let body = serde_json::from_slice::<Value>(&request.body)
                .map_err(|error| format!("unreadable request: {error}"))
                .and_then(|body| match body["op"] == "attach" {
                    true => Err("attach requests need a WebSocket on /attach".to_owned()),
                    false => Ok(body),
                });
            let (reply, _) = respond(client, body, token);
            let reply = serde_json::to_vec(&reply).expect("a reply serializes");
            send(&mut writer, "200 OK", "application/json", &reply)
        }
        ("GET", path) => {
            // Every view of the page has its own path; all of them load the page.
            let path = path.split_once('?').map_or(path, |(path, _)| path);
            let path = if path.starts_with("/vm/") { "/" } else { path };
            match FILES.iter().find(|(file, ..)| *file == path) {
                Some((_, content_type, body)) => send(&mut writer, "200 OK", content_type, body),
                None => send(&mut writer, "404 Not Found", "text/plain", b"not found"),
            }
        }
        _ => send(&mut writer, "405 Method Not Allowed", "text/plain", b"method not allowed"),
    }
}

fn read_request(reader: &mut impl BufRead) -> io::Result<Request> {
    let invalid = |message: &str| io::Error::new(io::ErrorKind::InvalidData, message.to_owned());
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return Err(invalid("malformed HTTP request line"));
    };
    let (method, path) = (method.to_owned(), path.to_owned());
    let mut headers = BTreeMap::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(invalid("HTTP headers ended early"));
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').ok_or_else(|| invalid("malformed HTTP header"))?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
    }
    let length = headers.get("content-length").map_or(Ok(0), |length| length.parse::<usize>());
    let mut body = vec![0; length.map_err(|_| invalid("malformed Content-Length"))?];
    reader.read_exact(&mut body)?;
    Ok(Request {
        method,
        path,
        headers,
        body,
    })
}

fn send(writer: &mut impl Write, status: &str, content_type: &str, body: &[u8]) -> io::Result<()> {
    write!(
        writer,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    writer.write_all(body)
}

fn websocket<C: Connection>(
    client: &Client,
    request: &Request,
    mut reader: BufReader<io::Take<Deadline<C>>>,
    mut writer: C,
    token: &str,
) -> io::Result<()> {
    let Some(key) = request.headers.get("sec-websocket-key") else {
        return send(&mut writer, "400 Bad Request", "text/plain", b"expected a WebSocket upgrade");
    };
    let accept = base64(&sha1(format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes()));
    write!(
        writer,
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    )?;

    let (_, message) = read_message(&mut reader)?;
    let body = serde_json::from_slice::<Value>(&message)
        .map_err(|error| format!("unreadable request: {error}"))
        .and_then(|body| match body["op"] == "attach" {
            true => Ok(body),
            false => Err("only attach requests go over /attach".to_owned()),
        });
    let (reply, control) = respond(client, body, token);
    let mut line = serde_json::to_vec(&reply).expect("a reply serializes");
    line.push(b'\n');
    write_message(&mut writer, OPCODE_BINARY, &line)?;
    let Some(control) = control else {
        return write_message(&mut writer, OPCODE_CLOSE, &[]);
    };

    // The rest of the connection has no deadline or size limit; keep what the
    // reader already buffered.
    let buffered = Cursor::new(reader.buffer().to_vec());
    writer.set_read_timeout(None)?;
    let reader = BufReader::new(buffered.chain(writer.try_clone()?));
    bridge(reader, writer, control)
}

/// Passes messages from the browser to the control socket and the control
/// socket's bytes back as messages, until either side closes.
fn bridge(mut reader: impl Read, mut writer: impl Connection, mut control: UnixStream) -> io::Result<()> {
    let mut from_control = control.try_clone()?;
    thread::scope(|scope| {
        scope.spawn(move || {
            let mut buffer = vec![0; 64 * 1024];
            while let Ok(count @ 1..) = from_control.read(&mut buffer) {
                if write_message(&mut writer, OPCODE_BINARY, &buffer[..count]).is_err() {
                    break;
                }
            }
            let _ = write_message(&mut writer, OPCODE_CLOSE, &[]);
            let _ = writer.shutdown(Shutdown::Both);
        });
        let result = loop {
            match read_message(&mut reader) {
                Ok((OPCODE_CLOSE, _)) => break Ok(()),
                Ok((_, payload)) => {
                    if let Err(error) = control.write_all(&payload) {
                        break Err(error);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break Ok(()),
                Err(error) => break Err(error),
            }
        };
        let _ = control.shutdown(Shutdown::Both);
        result
    })
}

/// One frame from the browser, which masks every frame, as opcode and payload.
fn read_message(reader: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut head = [0; 2];
    reader.read_exact(&mut head)?;
    let length = match head[1] & 0x7f {
        126 => {
            let mut length = [0; 2];
            reader.read_exact(&mut length)?;
            u64::from(u16::from_be_bytes(length))
        }
        127 => {
            let mut length = [0; 8];
            reader.read_exact(&mut length)?;
            u64::from_be_bytes(length)
        }
        length => u64::from(length),
    };
    if length > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "WebSocket message too large"));
    }
    let mut mask = [0; 4];
    if head[1] & 0x80 != 0 {
        reader.read_exact(&mut mask)?;
    }
    let mut payload = vec![0; length as usize];
    reader.read_exact(&mut payload)?;
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[index % 4];
    }
    Ok((head[0] & 0x0f, payload))
}

fn write_message(writer: &mut impl Write, opcode: u8, payload: &[u8]) -> io::Result<()> {
    let mut frame = vec![0x80 | opcode];
    match payload.len() {
        length @ 0..126 => frame.push(length as u8),
        length @ 126..65536 => {
            frame.push(126);
            frame.extend_from_slice(&(length as u16).to_be_bytes());
        }
        length => {
            frame.push(127);
            frame.extend_from_slice(&(length as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(payload);
    writer.write_all(&frame)
}

/// SHA-1, which the WebSocket handshake requires.
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut state: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&(data.len() as u64 * 8).to_be_bytes());
    for block in message.chunks(64) {
        let mut words = [0u32; 80];
        for (word, bytes) in words.iter_mut().zip(block.chunks(4)) {
            *word = u32::from_be_bytes(bytes.try_into().unwrap());
        }
        for index in 16..80 {
            words[index] = (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = state;
        for (index, word) in words.into_iter().enumerate() {
            let (f, k) = match index {
                0..20 => ((b & c) | (!b & d), 0x5A827999),
                20..40 => (b ^ c ^ d, 0x6ED9EBA1),
                40..60 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let next = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(word);
            (e, d, c, b, a) = (d, c, b.rotate_left(30), a, next);
        }
        for (value, add) in state.iter_mut().zip([a, b, c, d, e]) {
            *value = value.wrapping_add(add);
        }
    }
    let mut digest = [0; 20];
    for (bytes, value) in digest.chunks_mut(4).zip(state) {
        bytes.copy_from_slice(&value.to_be_bytes());
    }
    digest
}
