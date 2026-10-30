#[cfg(target_os = "macos")]
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSPasteboard, NSPasteboardTypePNG, NSPasteboardTypeString,
    NSPasteboardTypeTIFF,
};
#[cfg(target_os = "macos")]
use objc2_foundation::{NSDictionary, NSString};
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    sync::mpsc::{Receiver, Sender, TryRecvError},
    thread,
    time::Duration,
};

pub struct Sink {
    tx: Sender<Vec<u8>>,
}

impl Sink {
    pub fn push(&self, bytes: Vec<u8>) {
        let _ = self.tx.send(bytes);
    }
}

pub fn start(on_host_change: impl FnMut(Vec<u8>) + Send + 'static) -> Sink {
    let (tx, rx) = std::sync::mpsc::channel();
    thread::Builder::new()
        .name("varmint-clipboard".into())
        .spawn(move || run(rx, on_host_change))
        .unwrap_or_else(|error| panic!("failed to spawn clipboard thread: {error}"));
    Sink { tx }
}

const POLL_INTERVAL: Duration = Duration::from_millis(250);

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

/// Puts `text` on the host clipboard from a command that exits right after.
#[cfg(target_os = "macos")]
pub fn copy_text(text: &[u8]) {
    objc2::rc::autoreleasepool(|_| Pasteboard::general().write_text(text));
}

/// The host clipboard image as PNG.
#[cfg(target_os = "macos")]
pub fn read_png() -> Option<Vec<u8>> {
    objc2::rc::autoreleasepool(|_| Pasteboard::general().read_png())
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

    fn change_count(&self) -> isize {
        self.inner.changeCount()
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

/// The argument that makes `varmint` serve the text on its stdin as the
/// clipboard, since X11 and Wayland clipboards live only as long as the
/// program offering them.
#[cfg(target_os = "linux")]
pub const SERVE_TEXT_COMMAND: &str = "__serve-clipboard-text";

/// Puts `text` on the host clipboard from a command that exits right after:
/// a detached `varmint` keeps offering it until another program takes over.
#[cfg(target_os = "linux")]
pub fn copy_text(text: &[u8]) {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    let mut server = Command::new(std::env::current_exe().expect("cannot find the varmint executable"))
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

/// The host clipboard image as PNG.
#[cfg(target_os = "linux")]
pub fn read_png() -> Option<Vec<u8>> {
    Pasteboard::general().read_png()
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

    /// Changes with the clipboard text, which X11 and Wayland do not count.
    fn change_count(&self) -> isize {
        self.read_text().map_or(0, |text| hash_bytes(&text) as isize)
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

struct Deframer {
    buf: Vec<u8>,
}

impl Deframer {
    fn new() -> Deframer {
        Deframer { buf: Vec::new() }
    }

    fn push(&mut self, bytes: &[u8], mut on_frame: impl FnMut(Vec<u8>)) {
        self.buf.extend_from_slice(bytes);

        loop {
            if self.buf.len() < FRAME_LEN_PREFIX {
                return;
            }
            let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;

            if len > MAX_FRAME {
                self.buf.remove(0);
                continue;
            }

            let total = FRAME_LEN_PREFIX + len;
            if self.buf.len() < total {
                return;
            }

            let payload = self.buf[FRAME_LEN_PREFIX..total].to_vec();
            self.buf.drain(..total);
            on_frame(payload);
        }
    }
}

const FRAME_LEN_PREFIX: usize = 4;

const MAX_FRAME: usize = 16 * 1024 * 1024;

fn run(out_rx: Receiver<Vec<u8>>, mut on_input: impl FnMut(Vec<u8>)) {
    let pb = Pasteboard::general();

    let mut last_change = pb.change_count();
    let mut last_hash = pb.read_text().map(|b| hash_bytes(&b));
    let mut deframer = Deframer::new();

    loop {
        loop {
            match out_rx.try_recv() {
                Ok(bytes) => {
                    let mut to_write: Option<Vec<u8>> = None;
                    deframer.push(&bytes, |frame| to_write = Some(frame));
                    if let Some(frame) = to_write {
                        let h = hash_bytes(&frame);
                        if Some(h) != last_hash {
                            pb.write_text(&frame);
                            last_hash = Some(h);
                            last_change = pb.change_count();
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        let change = pb.change_count();
        if change != last_change {
            last_change = change;
            if let Some(bytes) = pb.read_text() {
                let h = hash_bytes(&bytes);
                if Some(h) != last_hash {
                    last_hash = Some(h);
                    on_input(bytes);
                }
            }
        }

        std::thread::sleep(POLL_INTERVAL);
    }
}
