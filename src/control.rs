//! Per-VM control socket served by the VMM process.

use crate::{
    agent::Agent,
    audio,
    devices::{RuntimeEvent, RuntimeInputEvent},
    display::{DisplayBuffer, H264Encoder},
    protocol::{self, Codec, Frame, InputEvent, Reply, Request, Session, kind, stream_kind},
    telemetry, vmm,
};
use std::{
    io::{self, BufReader, Write},
    mem,
    net::Shutdown,
    os::{
        fd::AsRawFd,
        unix::net::{UnixListener, UnixStream},
    },
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{RecvTimeoutError, Sender},
    },
    thread,
    time::{Duration, Instant},
};

/// How long the screen stays still before its frame is encoded once more, which
/// sharpens what the rate control blurred while it changed.
const SETTLE_DELAY: Duration = Duration::from_millis(300);

/// Pause between injected events. Without it a long batch overflows the
/// guest's evdev buffers and the guest drops the whole burst.
const INPUT_EVENT_INTERVAL: Duration = Duration::from_millis(2);

/// How often the audio of a stream checks whether the stream ended, while the guest is silent.
const AUDIO_STOP_INTERVAL: Duration = Duration::from_millis(500);

pub struct Context<'a> {
    pub display: &'a Mutex<DisplayBuffer>,
    pub agent: &'a Agent,
    pub audio: &'a audio::Listeners,
    pub runtime_tx: Sender<RuntimeEvent>,
    /// The configured display size, which bounds the pixel count of resizes.
    pub display_size: (u32, u32),
}

pub fn serve(listener: UnixListener, context: &Context<'_>) -> ! {
    thread::scope(|scope| {
        for connection in listener.incoming() {
            let connection = connection.unwrap_or_else(|error| panic!("control socket accept failed: {error}"));
            scope.spawn(move || {
                if let Err(error) = handle(connection, context) {
                    eprintln!("control: client error: {error}");
                }
            });
        }
    });
    unreachable!("control listener stopped accepting connections")
}

fn handle(connection: UnixStream, context: &Context<'_>) -> io::Result<()> {
    let mut reader = BufReader::new(connection.try_clone()?);
    let mut writer = connection;

    let request = match protocol::read_json::<Request>(&mut reader) {
        Ok(request) => request,
        Err(error) => {
            reply_error(&mut writer, format!("unreadable request: {error}"))?;
            return Err(error);
        }
    };
    match request {
        Request::Wait => match context.agent.wait_booted() {
            Ok(boot_time) => protocol::write_json(
                &mut writer,
                &Reply {
                    boot_ms: Some(boot_time.as_millis() as u64),
                    ..Reply::default()
                },
            ),
            Err(error) => reply_error(&mut writer, format!("guest setup failed: {error}")),
        },
        Request::Screenshot => {
            let capture = context.display.lock().unwrap().capture();
            let png = capture
                .ok_or_else(|| "the guest has not configured a display yet".to_owned())
                .and_then(|capture| capture.encode_png());
            match png {
                Ok(png) => {
                    protocol::write_json(
                        &mut writer,
                        &Reply {
                            png_len: Some(png.len()),
                            ..Reply::default()
                        },
                    )?;
                    writer.write_all(&png)
                }
                Err(error) => reply_error(&mut writer, error),
            }
        }
        Request::Stream { fps, codec, audio } => match codec {
            Codec::Rgb if audio => reply_error(&mut writer, "audio needs the h264 codec".to_owned()),
            Codec::Rgb => {
                protocol::write_json(&mut writer, &Reply::default())?;
                stream(&mut writer, fps, context)
            }
            Codec::H264 => {
                protocol::write_json(&mut writer, &Reply::default())?;
                stream_h264(&writer, fps, audio, context)
            }
        },
        Request::InputStream => {
            protocol::write_json(&mut writer, &Reply::default())?;
            loop {
                match protocol::read_json::<InputEvent>(&mut reader) {
                    Ok(event) => inject(event, context),
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                    Err(error) => return Err(error),
                }
            }
        }
        Request::Input { events } => {
            for event in events {
                inject(event, context);
                thread::sleep(INPUT_EVENT_INTERVAL);
            }
            protocol::write_json(&mut writer, &Reply::default())
        }
        Request::Resize { width, height } => {
            if width == 0 || height == 0 {
                return reply_error(&mut writer, "the display size must not be zero".to_owned());
            }
            let (width, height) = fit_pixel_count((width, height), context.display_size);
            context
                .runtime_tx
                .send(RuntimeEvent::DisplayResized {
                    physical_width: width,
                    physical_height: height,
                    logical_width: width,
                    logical_height: height,
                    refresh_millihertz: 60_000,
                })
                .expect("runtime event pump exited");
            protocol::write_json(&mut writer, &Reply::default())
        }
        Request::Open { session } => relay(reader, writer, &session, context.agent),
        Request::Usage => protocol::write_json(
            &mut writer,
            &Reply {
                memory_bytes: Some(footprint()),
                cpu_ms: Some(cpu_ms()),
                ..Reply::default()
            },
        ),
        Request::Stop => {
            protocol::write_json(&mut writer, &Reply::default())?;
            vmm::shutdown("stopped")
        }
    }
}

#[cfg(target_os = "macos")]
fn footprint() -> u64 {
    let mut usage: libc::rusage_info_v2 = unsafe { mem::zeroed() };
    let result = unsafe { libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V2, (&raw mut usage).cast()) };
    assert_eq!(result, 0, "proc_pid_rusage failed: {}", io::Error::last_os_error());
    usage.ri_phys_footprint
}

/// The resident memory of the VM process, guest RAM included.
#[cfg(target_os = "linux")]
fn footprint() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").expect("failed to read /proc/self/statm");
    let resident_pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|pages| pages.parse().ok())
        .expect("malformed /proc/self/statm");
    resident_pages * unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64
}

fn cpu_ms() -> u64 {
    let mut usage: libc::rusage = unsafe { mem::zeroed() };
    let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    assert_eq!(result, 0, "getrusage failed: {}", io::Error::last_os_error());
    let ms = |time: libc::timeval| time.tv_sec as u64 * 1000 + time.tv_usec as u64 / 1000;
    ms(usage.ru_utime) + ms(usage.ru_stime)
}

/// Scales `size` down to at most as many pixels as `limit`, keeping its aspect ratio.
fn fit_pixel_count((width, height): (u32, u32), (limit_width, limit_height): (u32, u32)) -> (u32, u32) {
    let scale = (f64::from(limit_width) * f64::from(limit_height) / (f64::from(width) * f64::from(height)))
        .sqrt()
        .min(1.0);
    let scaled = |value: u32| ((f64::from(value) * scale) as u32).max(1);
    (scaled(width), scaled(height))
}

fn reply_error(writer: &mut UnixStream, error: String) -> io::Result<()> {
    protocol::write_json(
        writer,
        &Reply {
            error: Some(error),
            ..Reply::default()
        },
    )
}

fn inject(event: InputEvent, context: &Context<'_>) {
    let event = match event {
        InputEvent::Move { x, y } => {
            let (width, height) = context.display.lock().unwrap().size();
            RuntimeInputEvent::PointerMove {
                x,
                y,
                width: width as u32,
                height: height as u32,
            }
        }
        InputEvent::Button { button, down } => RuntimeInputEvent::PointerButton {
            button,
            pressed: down,
            relative: false,
        },
        InputEvent::Key { code, down } => RuntimeInputEvent::Key { code, pressed: down },
        InputEvent::Scroll { value, horizontal } => RuntimeInputEvent::Scroll {
            horizontal,
            value,
            relative: false,
        },
    };
    context
        .runtime_tx
        .send(RuntimeEvent::Input(event))
        .expect("runtime event pump exited");
}

/// Bridges a client connection to one guest agent stream until either side ends.
fn relay(mut reader: BufReader<UnixStream>, writer: UnixStream, session: &Session, agent: &Agent) -> io::Result<()> {
    let started_at = telemetry::now();
    let (stream, from_guest) = agent.open(session);

    thread::scope(|scope| {
        scope.spawn(move || {
            let mut writer = writer;
            for frame in from_guest {
                if frame.kind == kind::EXIT {
                    record_command(session, started_at, &frame.payload);
                }
                if protocol::write_frame(&mut writer, &frame).is_err() || frame.kind == kind::EXIT {
                    break;
                }
            }
            let _ = writer.shutdown(Shutdown::Both);
        });

        let result = loop {
            match protocol::read_frame(&mut reader) {
                Ok(Some(frame))
                    if matches!(frame.kind, kind::DATA | kind::EOF | kind::RESIZE)
                        || (frame.kind == kind::CLIPBOARD_IMAGE && matches!(session, Session::Pty { .. })) =>
                {
                    agent.send(&Frame::new(stream, frame.kind, frame.payload))
                }
                Ok(Some(frame)) => {
                    break Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("clients may not send frame kind {}", frame.kind),
                    ));
                }
                Ok(None) => break Ok(()),
                Err(error) => break Err(error),
            }
        };
        agent.close(stream);
        result
    })
}

/// Records commands run from the host; interactive guest shells report their own.
fn record_command(session: &Session, started_at: f64, exit_payload: &[u8]) {
    let (source, argv) = match session {
        Session::Exec { argv } => ("exec", argv),
        Session::Pty { argv, .. } if !argv.is_empty() => ("exec", argv),
        Session::Pty { .. } | Session::Tcp { .. } => return,
    };
    let Ok(exit_code) = <[u8; 4]>::try_from(exit_payload).map(i32::from_le_bytes) else {
        eprintln!("control: malformed exit frame from the guest agent");
        return;
    };
    telemetry::record(telemetry::Event::HostCommand {
        source,
        started_at,
        duration_ms: (telemetry::now() - started_at) * 1000.0,
        exit_code,
        command: argv.join(" "),
    });
}

/// Sends a frame whenever the guest presented a new one, until the client leaves.
fn stream(writer: &mut UnixStream, fps: u32, context: &Context<'_>) -> io::Result<()> {
    let interval = Duration::from_secs(1) / fps.clamp(1, 60);
    let mut last_sequence = None;
    while !hung_up(writer) {
        let started = Instant::now();
        let frame = {
            let display = context.display.lock().unwrap();
            let sequence = display.sequence();
            if last_sequence == Some(sequence) {
                None
            } else {
                last_sequence = Some(sequence);
                display.capture()
            }
        };
        if let Some(capture) = frame {
            let (width, height) = capture.size();
            let rgb = capture.rgb().map_err(io::Error::other)?;
            writer.write_all(&(width as u32).to_le_bytes())?;
            writer.write_all(&(height as u32).to_le_bytes())?;
            writer.write_all(&rgb)?;
        }
        thread::sleep(interval.saturating_sub(started.elapsed()));
    }
    Ok(())
}

/// Like `stream`, with each frame H.264 encoded in hardware, and with
/// `audio` the guest's sound as Opus beside it.
fn stream_h264(writer: &UnixStream, fps: u32, audio: bool, context: &Context<'_>) -> io::Result<()> {
    let writer = Mutex::new(writer);
    let ended = AtomicBool::new(false);
    thread::scope(|scope| {
        let sound = audio.then(|| scope.spawn(|| stream_opus(&writer, &ended, context)));
        let video = stream_video(&writer, fps, context);
        ended.store(true, Ordering::Release);
        video?;
        sound.map_or(Ok(()), |sound| sound.join().expect("audio stream panicked"))
    })
}

fn stream_video(writer: &Mutex<&UnixStream>, fps: u32, context: &Context<'_>) -> io::Result<()> {
    let fps = fps.clamp(1, 60);
    let interval = Duration::from_secs(1) / fps;
    let started = Instant::now();
    let mut last_sequence = None;
    let mut settle_at = None;
    let mut encoder: Option<H264Encoder> = None;
    while !hung_up(&writer.lock().unwrap()) {
        let tick = Instant::now();
        let (capture, changed) = {
            let display = context.display.lock().unwrap();
            let sequence = display.sequence();
            if last_sequence != Some(sequence) {
                last_sequence = Some(sequence);
                (display.capture(), true)
            } else if settle_at.is_some_and(|at| tick >= at) {
                (display.capture(), false)
            } else {
                (None, false)
            }
        };
        if let Some(capture) = capture {
            let size = capture.size();
            if encoder.as_ref().is_none_or(|encoder| encoder.size() != size) {
                encoder = Some(H264Encoder::new(size.0, size.1, fps).map_err(io::Error::other)?);
            }
            let encoder = encoder.as_mut().expect("encoder was just created");
            if let Some(frame) = encoder.encode(&capture, started.elapsed()).map_err(io::Error::other)? {
                let mut message = Vec::with_capacity(14 + frame.data.len());
                message.push(stream_kind::VIDEO);
                message.extend_from_slice(&(size.0 as u32).to_le_bytes());
                message.extend_from_slice(&(size.1 as u32).to_le_bytes());
                message.push(u8::from(frame.keyframe));
                message.extend_from_slice(&(frame.data.len() as u32).to_le_bytes());
                message.extend_from_slice(&frame.data);
                writer.lock().unwrap().write_all(&message)?;
            }
            settle_at = changed.then(|| tick + SETTLE_DELAY);
        }
        thread::sleep(interval.saturating_sub(tick.elapsed()));
    }
    Ok(())
}

/// Sends the guest's audio as Opus packets until the video of the stream `ended`.
fn stream_opus(writer: &Mutex<&UnixStream>, ended: &AtomicBool, context: &Context<'_>) -> io::Result<()> {
    let mut encoder = audio::OpusEncoder::new().map_err(io::Error::other)?;
    let chunks = context.audio.subscribe();
    while !ended.load(Ordering::Acquire) {
        let chunk = match chunks.recv_timeout(AUDIO_STOP_INTERVAL) {
            Ok(chunk) => chunk,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => unreachable!("audio listeners keep their senders"),
        };
        let mut messages = Vec::new();
        encoder
            .encode(&chunk, |packet| {
                messages.push(stream_kind::AUDIO);
                messages.extend_from_slice(&(packet.len() as u32).to_le_bytes());
                messages.extend_from_slice(packet);
            })
            .map_err(io::Error::other)?;
        writer.lock().unwrap().write_all(&messages)?;
    }
    Ok(())
}

/// Whether a stream client left. Writes only fail once the screen changes, so
/// without this a stream of a still screen would outlive its client, holding
/// its encoder. Stream clients send nothing, so anything readable is the end.
fn hung_up(stream: &UnixStream) -> bool {
    let mut poll = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    (unsafe { libc::poll(&mut poll, 1, 0) }) != 0
}
