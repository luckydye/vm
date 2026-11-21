//! A machine's screen, sound and clipboard, as the web UI has them.
//!
//! The screen and sound come as the control socket's H.264 stream with Opus
//! beside it, locally or through a node's `attach`, and play on this host.
//! Input goes as JSON lines on an input stream, and text copied in the guest
//! lands on the host clipboard. A thread writes the input, so a slow node
//! never stalls the window, and pastes in line with the keys around them.

use crate::{
    audio::{self, Backend, OpusDecoder, PeriodSink},
    cli,
    display::{Decoded, H264Decoder},
    node::{self, Target},
    protocol::{Codec, InputEvent, Request, stream_kind, write_json},
};
use gpui::{AppContext, AsyncApp, Bounds, Context, Pixels, Point, RenderImage, Size, Task, WeakEntity, point, size};
use image::{Frame, RgbaImage};
use std::{
    io::{self, Read},
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

const FPS: u32 = 30;

/// How long the pane keeps a size before the guest is asked to take it, so
/// dragging the window does not resize the guest on every step.
const RESIZE_DELAY: Duration = Duration::from_millis(300);

/// What the input thread does, in the order it is asked.
enum Command {
    Event(InputEvent),
    /// Puts the host clipboard into the guest's before the events after it.
    Paste,
}

pub struct Display {
    /// The machine, as the vm CLI names it.
    pub name: String,
    /// The latest frame, `None` until the first arrives.
    pub frame: Option<Arc<RenderImage>>,
    /// The guest's screen size in pixels.
    pub screen: Size<u32>,
    /// Why the stream ended.
    pub error: Option<String>,
    input: Option<mpsc::Sender<Command>>,
    /// The pane size last asked of the guest, in device pixels.
    requested: Option<Size<u32>>,
    /// Plays the sound while the display is open.
    _speakers: Option<Backend>,
    _stream: Option<Task<()>>,
    resize: Option<Task<()>>,
}

/// The stream and what turns it into pictures and sound, moved to a
/// background thread for each blocking read and back.
struct Media {
    reader: io::BufReader<node::Wire>,
    video: H264Decoder,
    audio: OpusDecoder,
    sink: PeriodSink,
    periods: u64,
}

enum Message {
    Video(Decoded),
    /// Sound, which went to the speakers, or a frame the decoder kept.
    Nothing,
}

impl Display {
    pub fn new(name: String, cx: &mut Context<Self>) -> Self {
        let mut display = Self {
            name: name.clone(),
            frame: None,
            screen: size(0, 0),
            error: None,
            input: None,
            requested: None,
            _speakers: None,
            _stream: None,
            resize: None,
        };
        let (speakers, sink) = match Backend::new(true, Arc::new(audio::Listeners::default()), |_| {}) {
            Ok(opened) => opened,
            Err(error) => {
                display.error = Some(format!("failed to open the speakers: {error}"));
                return display;
            }
        };
        display._speakers = Some(speakers);
        display._stream = Some(cx.spawn(async move |this, cx| {
            let error = stream(name, sink, this.clone(), cx).await;
            this.update(cx, |this, cx| {
                this.error = Some(error);
                cx.notify();
            })
            .ok();
        }));
        display
    }

    pub fn send(&self, event: InputEvent) {
        self.command(Command::Event(event));
    }

    /// Puts the host clipboard into the guest's, before the input that follows.
    pub fn paste(&self) {
        self.command(Command::Paste);
    }

    fn command(&self, command: Command) {
        if let Some(input) = &self.input {
            // The stream's own error tells why, should the input thread have stopped.
            let _ = input.send(command);
        }
    }

    /// Asks the guest to take the shape of a pane of `pane` device pixels,
    /// once it has kept that size for a moment.
    pub fn fit_to(&mut self, pane: Size<u32>, cx: &mut Context<Self>) {
        if self.requested == Some(pane) || pane.width == 0 || pane.height == 0 {
            return;
        }
        self.requested = Some(pane);
        let name = self.name.clone();
        self.resize = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(RESIZE_DELAY).await;
            let request = Request::Resize { width: pane.width, height: pane.height };
            // A failed resize keeps the old shape; the stream reports a stopped VM.
            let _ = cx
                .background_spawn(async move { Target::resolve(&name).and_then(|target| node::request(&target, &request)) })
                .await;
        }));
    }
}

/// Where a frame of `screen` pixels is drawn in `bounds`: as large as fits,
/// centered, keeping its aspect ratio.
pub fn fit(bounds: Bounds<Pixels>, screen: Size<u32>) -> Bounds<Pixels> {
    if screen.width == 0 || screen.height == 0 {
        return bounds;
    }
    let scale = (bounds.size.width / screen.width as f32).min(bounds.size.height / screen.height as f32);
    let drawn = size(scale * screen.width as f32, scale * screen.height as f32);
    let origin = point(
        bounds.origin.x + (bounds.size.width - drawn.width) / 2.,
        bounds.origin.y + (bounds.size.height - drawn.height) / 2.,
    );
    Bounds::new(origin, drawn)
}

/// The guest pixel under `position`, for a frame of `screen` drawn at `drawn`.
pub fn guest_position(position: Point<Pixels>, drawn: Bounds<Pixels>, screen: Size<u32>) -> InputEvent {
    let x = ((position.x - drawn.origin.x) / drawn.size.width).clamp(0., 1.);
    let y = ((position.y - drawn.origin.y) / drawn.size.height).clamp(0., 1.);
    InputEvent::Move {
        x: (x * (screen.width - 1) as f32).round() as u32,
        y: (y * (screen.height - 1) as f32).round() as u32,
    }
}

/// Shows frames and plays sound until the stream ends, and returns why it did.
async fn stream(name: String, sink: PeriodSink, this: WeakEntity<Display>, cx: &mut AsyncApp) -> String {
    let request = Request::Stream { fps: FPS, codec: Codec::H264, audio: true };
    let opened = cx
        .background_spawn(async move {
            let target = Target::resolve(&name)?;
            let reader = node::request(&target, &request)?.reader;
            Ok::<_, String>((reader, open_input(&target)?, OpusDecoder::new()?))
        })
        .await;
    let (reader, input, audio) = match opened {
        Ok(opened) => opened,
        Err(error) => return error,
    };
    if this.update(cx, |this, _| this.input = Some(input)).is_err() {
        return "the display closed".to_owned();
    }
    sink.start();
    let mut media = Media { reader, video: H264Decoder::new(), audio, sink, periods: 0 };
    loop {
        let (message, returned) = cx
            .background_spawn(async move {
                let message = read_message(&mut media);
                (message, media)
            })
            .await;
        media = returned;
        let decoded = match message {
            Ok(Message::Video(decoded)) => decoded,
            Ok(Message::Nothing) => continue,
            Err(error) => return error,
        };
        let screen = size(decoded.width, decoded.height);
        let buffer = RgbaImage::from_raw(decoded.width, decoded.height, decoded.bgra)
            .expect("a decoded frame holds width times height pixels");
        let frame = Arc::new(RenderImage::new([Frame::new(buffer)]));
        let updated = this.update(cx, |this, cx| {
            this.screen = screen;
            if let Some(old) = this.frame.replace(frame) {
                cx.drop_image(old, None);
            }
            cx.notify();
        });
        if updated.is_err() {
            return "the display closed".to_owned();
        }
    }
}

/// Reads one message of the stream: a frame to show, or sound for the speakers.
fn read_message(media: &mut Media) -> Result<Message, String> {
    let ended = |error: io::Error| format!("the screen stream ended: {error}");
    let mut kind = [0u8];
    media.reader.read_exact(&mut kind).map_err(ended)?;
    match kind[0] {
        stream_kind::VIDEO => {
            // Width and height, whether it is a keyframe, then the length.
            let mut header = [0u8; 13];
            media.reader.read_exact(&mut header).map_err(ended)?;
            let length = u32::from_le_bytes(header[9..13].try_into().unwrap()) as usize;
            let mut access_unit = vec![0; length];
            media.reader.read_exact(&mut access_unit).map_err(ended)?;
            Ok(media.video.decode(&access_unit)?.map_or(Message::Nothing, Message::Video))
        }
        stream_kind::AUDIO => {
            let mut length = [0u8; 4];
            media.reader.read_exact(&mut length).map_err(ended)?;
            let mut packet = vec![0; u32::from_le_bytes(length) as usize];
            media.reader.read_exact(&mut packet).map_err(ended)?;
            let pcm = media.audio.decode(&packet)?;
            media.periods += 1;
            // A full queue drops the sound that would come too late anyway.
            media.sink.push(media.periods, &pcm);
            Ok(Message::Nothing)
        }
        other => Err(format!("unexpected message kind {other} in the screen stream")),
    }
}

/// Opens the machine's input stream and clipboard, with a thread doing every
/// command sent in order.
fn open_input(target: &Target) -> Result<mpsc::Sender<Command>, String> {
    let mut writer = node::request(target, &Request::InputStream)?.writer;
    let mut paste = cli::clipboard_link(target)?;
    let (sender, receiver) = mpsc::channel::<Command>();
    thread::spawn(move || {
        for command in receiver {
            let written = match command {
                Command::Event(event) => write_json(&mut writer, &event),
                Command::Paste => paste(),
            };
            if written.is_err() {
                return;
            }
        }
    });
    Ok(sender)
}
