#[cfg(target_os = "linux")]
#[path = "audio/libopus.rs"]
mod opus;
#[cfg(target_os = "macos")]
mod opus;
#[cfg(target_os = "linux")]
mod pulse;

pub use opus::OpusEncoder;

#[cfg(target_os = "macos")]
use coreaudio::audio_unit::{
    AudioUnit, Element, IOType, SampleFormat, Scope, StreamFormat,
    audio_format::LinearPcmFlags,
    render_callback::{self, data},
};
use rtrb::{Consumer, Producer, RingBuffer};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
#[cfg(target_os = "macos")]
use zerocopy::IntoBytes;

const MAX_PERIOD_BYTES: usize = 16 * 1024;
const RING_CAPACITY: usize = 8;

/// How much audio a listener may fall behind by, in periods, before it misses some.
const LISTENER_BACKLOG: usize = 64;

/// How often a VM without speakers consumes its audio, standing in for the
/// sound card's clock so the guest plays at the same pace.
const CLOCK_TICK: Duration = Duration::from_millis(10);

/// 48 kHz stereo signed 16-bit, as the guest plays it.
const BYTES_PER_SECOND: usize = 48_000 * 2 * 2;

struct Period {
    seq: u64,
    generation: u64,
    len: usize,
    data: [u8; MAX_PERIOD_BYTES],
}

pub enum BackendEvent {
    PeriodElapsed(u64),
}

pub struct Backend {
    #[cfg(target_os = "macos")]
    _unit: Option<AudioUnit>,
    completion: Arc<CompletionState>,
    completion_thread: Option<JoinHandle<()>>,
    clock_thread: Option<JoinHandle<()>>,
}

pub struct PeriodSink {
    producer: Producer<Period>,
    playback: Arc<PlaybackState>,
    listeners: Arc<Listeners>,
}

/// Receivers of the guest's audio as it is queued for playback, such as web UI streams.
#[derive(Default)]
pub struct Listeners(Mutex<Vec<SyncSender<Arc<[u8]>>>>);

struct CallbackState {
    consumer: Consumer<Period>,
    completion: Arc<CompletionState>,
    completion_thread: thread::Thread,
    playback: Arc<PlaybackState>,
    current: Option<(Period, usize)>,
}

struct PlaybackState {
    running: AtomicBool,
    generation: AtomicU64,
}

struct CompletionState {
    latest_seq: AtomicU64,
    pending: AtomicBool,
    running: AtomicBool,
}

impl Backend {
    /// Plays on the host's default output with `speakers`, otherwise only to `listeners`.
    pub fn new<F>(speakers: bool, listeners: Arc<Listeners>, on_event: F) -> Result<(Backend, PeriodSink), String>
    where
        F: Fn(BackendEvent) + Send + 'static,
    {
        let (producer, consumer) = RingBuffer::new(RING_CAPACITY);
        let playback = Arc::new(PlaybackState {
            running: AtomicBool::new(false),
            generation: AtomicU64::new(0),
        });
        let completion = Arc::new(CompletionState {
            latest_seq: AtomicU64::new(0),
            pending: AtomicBool::new(false),
            running: AtomicBool::new(true),
        });

        let completion_state = Arc::clone(&completion);
        let completion_thread = thread::Builder::new()
            .name("audio-completion".into())
            .spawn(move || {
                while completion_state.running.load(Ordering::Acquire) {
                    thread::park();

                    while completion_state.pending.swap(false, Ordering::AcqRel) {
                        let seq = completion_state.latest_seq.load(Ordering::Acquire);
                        on_event(BackendEvent::PeriodElapsed(seq));
                    }
                }
            })
            .expect("failed to start audio completion thread");

        let state = CallbackState {
            consumer,
            completion: Arc::clone(&completion),
            completion_thread: completion_thread.thread().clone(),
            playback: Arc::clone(&playback),
            current: None,
        };

        #[cfg(target_os = "macos")]
        let (unit, clock_thread) = if speakers {
            let unit = play_on_speakers(state).map_err(|error| error.to_string())?;
            (Some(unit), None)
        } else {
            (None, Some(spawn_clock(Arc::clone(&completion), state)))
        };
        #[cfg(target_os = "linux")]
        let clock_thread = Some(spawn_clock(
            Arc::clone(&completion),
            state,
            speakers.then(pulse::Output::open).transpose()?,
        ));

        Ok((
            Backend {
                #[cfg(target_os = "macos")]
                _unit: unit,
                completion,
                completion_thread: Some(completion_thread),
                clock_thread,
            },
            PeriodSink {
                producer,
                playback,
                listeners,
            },
        ))
    }
}

#[cfg(target_os = "macos")]
fn play_on_speakers(mut state: CallbackState) -> Result<AudioUnit, coreaudio::Error> {
    let mut unit = AudioUnit::new(IOType::DefaultOutput)?;

    let stream_format = StreamFormat {
        sample_rate: 48000.0,
        sample_format: SampleFormat::I16,
        flags: LinearPcmFlags::IS_PACKED | LinearPcmFlags::IS_SIGNED_INTEGER,
        channels: 2,
    };

    unit.set_stream_format(stream_format, Scope::Input, Element::Output)?;

    unit.set_render_callback(move |args: render_callback::Args<data::Interleaved<i16>>| {
        state.render(args.data.buffer.as_mut_bytes());
        Ok(())
    })?;

    unit.start()?;
    Ok(unit)
}

/// Consumes the guest's audio at the pace of a timer, standing in for the sound card.
#[cfg(target_os = "macos")]
fn spawn_clock(clock_state: Arc<CompletionState>, mut state: CallbackState) -> JoinHandle<()> {
    thread::Builder::new()
        .name("audio-clock".into())
        .spawn(move || {
            let mut out = vec![0; BYTES_PER_SECOND * CLOCK_TICK.as_millis() as usize / 1000];
            let mut next = Instant::now();
            while clock_state.running.load(Ordering::Acquire) {
                state.render(&mut out);
                next += CLOCK_TICK;
                thread::sleep(next.saturating_duration_since(Instant::now()));
            }
        })
        .expect("failed to start audio clock thread")
}

/// Consumes the guest's audio at the pace of `output`, whose writes block
/// while it is full, or of a timer standing in for the sound card.
#[cfg(target_os = "linux")]
fn spawn_clock(
    clock_state: Arc<CompletionState>,
    mut state: CallbackState,
    output: Option<pulse::Output>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("audio-clock".into())
        .spawn(move || {
            let mut out = vec![0; BYTES_PER_SECOND * CLOCK_TICK.as_millis() as usize / 1000];
            let mut next = Instant::now();
            while clock_state.running.load(Ordering::Acquire) {
                state.render(&mut out);
                match &output {
                    Some(output) => output.write(&out).unwrap_or_else(|error| panic!("{error}")),
                    None => {
                        next += CLOCK_TICK;
                        thread::sleep(next.saturating_duration_since(Instant::now()));
                    }
                }
            }
        })
        .expect("failed to start audio clock thread")
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.completion.running.store(false, Ordering::Release);

        if let Some(thread) = self.clock_thread.take() {
            let _ = thread.join();
        }

        if let Some(thread) = self.completion_thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

impl CallbackState {
    fn render(&mut self, out: &mut [u8]) {
        let mut filled = 0;

        if !self.playback.running.load(Ordering::Acquire) {
            out.fill(0);
            return;
        }

        while filled < out.len() && self.playback.running.load(Ordering::Acquire) {
            let generation = self.playback.generation.load(Ordering::Acquire);

            if self
                .current
                .as_ref()
                .is_some_and(|(period, _)| period.generation != generation)
            {
                self.current = None;
            }

            if self.current.is_none() {
                loop {
                    match self.consumer.pop() {
                        Ok(period) if period.generation == generation => {
                            self.current = Some((period, 0));
                            break;
                        }
                        Ok(_) => continue,
                        Err(_) => break,
                    }
                }

                if self.current.is_none() {
                    break;
                }
            }

            let (period, off) = self.current.as_mut().unwrap();
            let n = (period.len - *off).min(out.len() - filled);
            out[filled..filled + n].copy_from_slice(&period.data[*off..*off + n]);
            filled += n;
            *off += n;

            if *off == period.len {
                let (done, _) = self.current.take().unwrap();
                if done.generation == self.playback.generation.load(Ordering::Acquire) {
                    self.completion.latest_seq.store(done.seq, Ordering::Release);
                    self.completion.pending.store(true, Ordering::Release);
                    self.completion_thread.unpark();
                }
            }
        }

        out[filled..].fill(0);
    }
}

impl PeriodSink {
    pub fn push(&mut self, seq: u64, bytes: &[u8]) -> bool {
        if bytes.len() > MAX_PERIOD_BYTES {
            return false;
        }
        let mut period = Period {
            seq,
            generation: self.playback.generation.load(Ordering::Acquire),
            len: bytes.len(),
            data: [0; MAX_PERIOD_BYTES],
        };
        period.data[..bytes.len()].copy_from_slice(bytes);
        if self.producer.push(period).is_err() {
            return false;
        }
        self.listeners.send(bytes);
        true
    }

    pub fn start(&self) {
        self.playback.running.store(true, Ordering::Release);
    }

    pub fn stop(&self) {
        self.playback.running.store(false, Ordering::Release);
    }

    pub fn reset(&self) {
        self.playback.running.store(false, Ordering::Release);
        self.playback.generation.fetch_add(1, Ordering::AcqRel);
    }
}

impl Listeners {
    /// The audio queued from now on, in chunks of 48 kHz stereo signed 16-bit little-endian PCM.
    pub fn subscribe(&self) -> Receiver<Arc<[u8]>> {
        let (sender, receiver) = mpsc::sync_channel(LISTENER_BACKLOG);
        self.0.lock().unwrap().push(sender);
        receiver
    }

    /// Drops listeners that left; one that fell behind misses this chunk.
    fn send(&self, bytes: &[u8]) {
        let mut listeners = self.0.lock().unwrap();
        if listeners.is_empty() {
            return;
        }
        let chunk: Arc<[u8]> = bytes.into();
        listeners.retain(|listener| {
            !matches!(
                listener.try_send(Arc::clone(&chunk)),
                Err(TrySendError::Disconnected(_))
            )
        });
    }
}
