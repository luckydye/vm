//! The host clipboard, and the guest clipboard that varmint-clipboard-agent
//! serves on the `clipboard` console port. Text copied in the guest goes to the
//! window and to `vm run` and `vm screen` clients, which put it on their host
//! clipboard. The host clipboard goes into the guest only when Ctrl+V pastes it.

use crate::{
    devices::RuntimeEvent,
    protocol::{Frame, MAX_PAYLOAD, kind},
};
#[cfg(target_os = "macos")]
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSPasteboard, NSPasteboardTypePNG, NSPasteboardTypeString,
    NSPasteboardTypeTIFF,
};
#[cfg(target_os = "macos")]
use objc2_foundation::{NSDictionary, NSString};
use std::{
    sync::{
        Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, Sender, channel},
    },
    time::Duration,
};

/// What a paste puts into the guest clipboard; empty clears it.
pub enum Content {
    Text(Vec<u8>),
    Png(Vec<u8>),
}

impl Content {
    /// A paste a client sent in a [`kind::CLIPBOARD_TEXT`] or [`kind::CLIPBOARD_IMAGE`] frame.
    pub fn from_frame(frame: Frame) -> Option<Content> {
        match frame.kind {
            kind::CLIPBOARD_TEXT => Some(Content::Text(frame.payload)),
            kind::CLIPBOARD_IMAGE => Some(Content::Png(frame.payload)),
            _ => None,
        }
    }

    pub fn into_frame(self, stream: u32) -> Frame {
        match self {
            Content::Text(text) => Frame::new(stream, kind::CLIPBOARD_TEXT, text),
            Content::Png(png) => Frame::new(stream, kind::CLIPBOARD_IMAGE, png),
        }
    }
}

/// The host clipboard for a paste: its image, otherwise its text.
pub fn read() -> Content {
    let read = || {
        let pasteboard = Pasteboard::general();
        match pasteboard.read_png() {
            Some(png) => Content::Png(png),
            None => Content::Text(pasteboard.read_text().unwrap_or_default()),
        }
    };
    #[cfg(target_os = "macos")]
    return objc2::rc::autoreleasepool(|_| read());
    #[cfg(target_os = "linux")]
    read()
}

/// Puts `text` on the host clipboard from a command that exits right after.
#[cfg(target_os = "macos")]
pub fn copy_text(text: &[u8]) {
    objc2::rc::autoreleasepool(|_| Pasteboard::general().write_text(text));
}

#[cfg(target_os = "macos")]
pub struct Pasteboard {
    inner: objc2::rc::Retained<NSPasteboard>,
}

#[cfg(target_os = "macos")]
impl Pasteboard {
    pub fn general() -> Pasteboard {
        let inner = NSPasteboard::generalPasteboard();
        Pasteboard { inner }
    }

    fn read_text(&self) -> Option<Vec<u8>> {
        let s = unsafe { self.inner.stringForType(NSPasteboardTypeString) }?;
        Some(s.to_string().into_bytes())
    }

    /// The clipboard image as PNG; many apps only offer TIFF.
    pub fn read_png(&self) -> Option<Vec<u8>> {
        if let Some(png) = self.inner.dataForType(unsafe { NSPasteboardTypePNG }) {
            return Some(png.to_vec());
        }
        let tiff = self.inner.dataForType(unsafe { NSPasteboardTypeTIFF })?;
        let image = NSBitmapImageRep::imageRepWithData(&tiff).expect("the pasteboard TIFF is not a bitmap");
        let png = unsafe { image.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }
            .expect("converting the pasteboard TIFF to PNG failed");
        Some(png.to_vec())
    }

    pub fn write_text(&self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes);
        let ns = NSString::from_str(&text);
        self.inner.clearContents();
        unsafe {
            self.inner.setString_forType(&ns, NSPasteboardTypeString);
        }
    }
}

/// The argument that makes `vm` serve the text on its stdin as the
/// clipboard, since X11 and Wayland clipboards live only as long as the
/// program offering them.
#[cfg(target_os = "linux")]
pub const SERVE_TEXT_COMMAND: &str = "__serve-clipboard-text";

/// Puts `text` on the host clipboard from a command that exits right after:
/// a detached `vm` keeps offering it until another program takes over.
#[cfg(target_os = "linux")]
pub fn copy_text(text: &[u8]) {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    let mut server = Command::new(std::env::current_exe().expect("cannot find the vm executable"))
        .arg(SERVE_TEXT_COMMAND)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to start the clipboard server");
    server
        .stdin
        .take()
        .unwrap()
        .write_all(text)
        .expect("failed to hand the clipboard server its text");
}

/// Serves the text on stdin as the clipboard until another program takes it.
#[cfg(target_os = "linux")]
pub fn serve_text() {
    use arboard::SetExtLinux;
    use std::io::Read;
    let mut text = Vec::new();
    std::io::stdin()
        .read_to_end(&mut text)
        .expect("failed to read the clipboard text");
    unsafe { libc::setsid() };
    arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.set().wait().text(String::from_utf8_lossy(&text)))
        .expect("failed to serve the clipboard");
}

#[cfg(target_os = "linux")]
pub struct Pasteboard {
    inner: std::cell::RefCell<arboard::Clipboard>,
}

#[cfg(target_os = "linux")]
impl Pasteboard {
    pub fn general() -> Pasteboard {
        let inner = arboard::Clipboard::new().unwrap_or_else(|error| panic!("cannot open the clipboard: {error}"));
        Pasteboard { inner: inner.into() }
    }

    fn read_text(&self) -> Option<Vec<u8>> {
        self.inner.borrow_mut().get_text().ok().map(String::into_bytes)
    }

    pub fn read_png(&self) -> Option<Vec<u8>> {
        let image = self.inner.borrow_mut().get_image().ok()?;
        let mut png = Vec::new();
        let mut encoder = png::Encoder::new(&mut png, image.width as u32, image.height as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .and_then(|mut writer| writer.write_image_data(&image.bytes))
            .expect("encoding the clipboard image as PNG failed");
        Some(png)
    }

    pub fn write_text(&self, bytes: &[u8]) {
        self.inner
            .borrow_mut()
            .set_text(String::from_utf8_lossy(bytes))
            .unwrap_or_else(|error| panic!("cannot write the clipboard: {error}"));
    }
}

/// How long a paste waits for varmint-clipboard-agent, which runs on the guest desktop.
const PASTE_TIMEOUT: Duration = Duration::from_secs(5);

/// Frames on the clipboard port are `[u32 len][u8 type][u32 paste][payload]`,
/// little endian, where `len` counts the payload.
const HEADER_LEN: usize = 9;

/// The frame types on the clipboard port.
mod port {
    /// Text copied in the guest, or pasted into it.
    pub const TEXT: u8 = 0;
    /// A PNG pasted into the guest.
    pub const PNG: u8 = 1;
    /// The guest clipboard holds paste number `paste`.
    pub const PASTED: u8 = 2;
}

/// The guest clipboard, served by varmint-clipboard-agent on the `clipboard` console port.
pub struct Guest {
    to_guest: Sender<RuntimeEvent>,
    /// Bytes from the port that do not form a whole frame yet.
    received: Mutex<Vec<u8>>,
    /// Who gets the text copied in the guest, by subscription number.
    subscribers: Mutex<Vec<(u64, Sender<Vec<u8>>)>>,
    next_subscriber: AtomicU64,
    /// The number of the last paste; held while that paste waits, so pastes take turns.
    sent: Mutex<u32>,
    /// The number of the last paste the guest clipboard took.
    pasted: Mutex<u32>,
    pasted_changed: Condvar,
}

impl Guest {
    pub fn new(to_guest: Sender<RuntimeEvent>) -> Guest {
        Guest {
            to_guest,
            received: Mutex::new(Vec::new()),
            subscribers: Mutex::new(Vec::new()),
            next_subscriber: AtomicU64::new(0),
            sent: Mutex::new(0),
            pasted: Mutex::new(0),
            pasted_changed: Condvar::new(),
        }
    }

    /// The text copied in the guest from now on, until [`Guest::unsubscribe`].
    pub fn subscribe(&self) -> (u64, Receiver<Vec<u8>>) {
        let id = self.next_subscriber.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = channel();
        self.subscribers.lock().unwrap().push((id, tx));
        (id, rx)
    }

    /// Ends subscription `id`, which ends its receiver.
    pub fn unsubscribe(&self, id: u64) {
        self.subscribers.lock().unwrap().retain(|(subscriber, _)| *subscriber != id);
    }

    /// Puts `content` into the guest clipboard and returns once it is there,
    /// so a Ctrl+V sent afterwards pastes it.
    pub fn paste(&self, content: Content) -> Result<(), String> {
        let mut sent = self.sent.lock().unwrap();
        *sent = sent.wrapping_add(1);
        let (kind, payload) = match content {
            Content::Text(text) => (port::TEXT, text),
            Content::Png(png) => (port::PNG, png),
        };
        let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.push(kind);
        frame.extend_from_slice(&sent.to_le_bytes());
        frame.extend_from_slice(&payload);
        self.to_guest
            .send(RuntimeEvent::Clipboard(frame))
            .expect("runtime event pump exited");

        let pasted = self.pasted.lock().unwrap();
        let (_pasted, wait) = self
            .pasted_changed
            .wait_timeout_while(pasted, PASTE_TIMEOUT, |pasted| *pasted != *sent)
            .unwrap();
        if wait.timed_out() {
            return Err("the guest clipboard did not take the paste; it needs the guest desktop".into());
        }
        Ok(())
    }

    /// Takes bytes the guest wrote to the clipboard port.
    pub fn receive(&self, bytes: &[u8]) {
        let mut received = self.received.lock().unwrap();
        received.extend_from_slice(bytes);
        while received.len() >= HEADER_LEN {
            let len = u32::from_le_bytes(received[0..4].try_into().unwrap()) as usize;
            assert!(len <= MAX_PAYLOAD, "clipboard frame of {len} bytes exceeds the protocol limit");
            if received.len() < HEADER_LEN + len {
                return;
            }
            let kind = received[4];
            let paste = u32::from_le_bytes(received[5..9].try_into().unwrap());
            let payload: Vec<u8> = received.drain(..HEADER_LEN + len).skip(HEADER_LEN).collect();
            match kind {
                port::TEXT => self
                    .subscribers
                    .lock()
                    .unwrap()
                    .retain(|(_, subscriber)| subscriber.send(payload.clone()).is_ok()),
                port::PASTED => {
                    *self.pasted.lock().unwrap() = paste;
                    self.pasted_changed.notify_all();
                }
                other => panic!("the guest clipboard agent sent frame type {other}"),
            }
        }
    }
}
