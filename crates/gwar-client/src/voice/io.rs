//! Audio device I/O. Everything inside the engine is 48 kHz mono `f32`;
//! device formats are adapted here.

use std::{
    sync::{
        Arc, Mutex, MutexGuard, OnceLock, TryLockError,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc as std_mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rtrb::{Consumer, Producer, RingBuffer};

pub use super::follow::Side;
use super::{
    follow::{Follow, Offer, Opened, Plan, Route},
    permission::{self, MicPermission, MicPermissionDenied},
    watch,
};

pub const RATE: u32 = 48_000;

/// Maximum local capture residence, shared by the PCM and encoded queues.
/// Allows normal 10/20 ms batching without replaying a stalled consumer's backlog.
pub const MAX_CAPTURE_AGE: Duration = Duration::from_millis(50);

/// One resampled microphone sample; position advances even when the ring is full.
#[derive(Debug, Clone, Copy)]
pub struct CaptureSample {
    pub position: u64,
    /// Monotonic time when this sample entered the capture callback.
    pub captured_at: Instant,
    pub value: f32,
}

/// Ring ends handed to the engine. Dropping `_guard` stops the devices.
pub struct AudioPorts {
    pub mic: Consumer<CaptureSample>,
    pub speaker: Producer<f32>,
    pub _guard: Box<dyn Send>,
    /// Shared with the output callback; report each callback to wake the pipeline.
    pub output: Arc<OutputTiming>,
}

/// Callback measurements in 48 kHz mono samples. Custom devices report once per callback.
#[derive(Default)]
pub struct OutputTiming {
    pipeline: OnceLock<thread::Thread>,
    pub(crate) callback_samples: AtomicUsize,
    pub(crate) callback_peak: AtomicUsize,
    pub(crate) callback_max_samples: AtomicUsize,
    pub(crate) headroom_samples: AtomicUsize,
    pub(crate) playing: AtomicBool,
    pub(crate) underruns: AtomicU64,
    pub(crate) capture_overflow_samples: AtomicU64,
    pub(crate) underrun_samples: AtomicU64,
}

impl OutputTiming {
    pub(crate) fn attach(&self) {
        let _ = self.pipeline.set(thread::current());
    }

    pub(crate) fn wake(&self) {
        if let Some(pipeline) = self.pipeline.get() {
            pipeline.unpark();
        }
    }

    /// `missing` excludes intentionally silent samples supplied by the pipeline.
    pub fn report(&self, samples: usize, missing: usize) {
        self.callback_samples.store(samples, Ordering::Relaxed);
        // Keep every peak until the pipeline consumes it, even across multiple callbacks.
        self.callback_peak.fetch_max(samples, Ordering::Relaxed);
        if missing > 0 && self.playing.load(Ordering::Relaxed) {
            self.underruns.fetch_add(1, Ordering::Relaxed);
            self.underrun_samples.fetch_add(missing as u64, Ordering::Relaxed);
        }
        self.wake();
    }
}

pub trait AudioIo: Send {
    fn open(self: Box<Self>) -> Result<AudioPorts>;
}

/// Lists device names for the settings UI.
pub fn devices() -> (Vec<String>, Vec<String>) {
    let host = cpal::default_host();
    (names(host.input_devices()), names(host.output_devices()))
}

/// A problem with one of the audio devices, or a change the user should hear about, reported
/// while the engine runs. The engine keeps going without a broken side (e.g. you can still
/// listen when there is no microphone) and reopens it as soon as a device works again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceIssue {
    /// The microphone could not be opened or stopped working.
    Input(String),
    /// The operating system denies microphone access (privacy settings). Not retryable
    /// until the user changes the setting.
    InputDenied(String),
    /// The speakers could not be opened or stopped working.
    Output(String),
    /// Audio moved to this device by itself: the chosen device disappeared or came back.
    Switched(Side, String),
    /// The side works again after a reported problem.
    Recovered(Side),
}

/// Receives [`DeviceIssue`]s from any thread.
pub type IssueSink = Arc<dyn Fn(DeviceIssue) + Send + Sync>;

/// Real devices via cpal. `None` follows the system default.
///
/// Which device each side is on, as devices come and go, is decided by [`super::follow`]. A
/// side that cannot be opened is reported through `on_issue` and stays silent until a device
/// works again; open itself fails only when the device thread cannot start.
pub struct DeviceIo {
    pub input: Option<String>,
    pub output: Option<String>,
    pub on_issue: Option<IssueSink>,
    events: std_mpsc::Sender<Event>,
    inbox: std_mpsc::Receiver<Event>,
}

impl DeviceIo {
    pub fn new(input: Option<String>, output: Option<String>) -> Self {
        let (events, inbox) = std_mpsc::channel();
        Self { input, output, on_issue: None, events, inbox }
    }

    pub fn with_issue_sink(mut self, sink: IssueSink) -> Self {
        self.on_issue = Some(sink);
        self
    }

    /// Moves the devices once open to other choices, without restarting the engine.
    pub fn control(&self) -> DeviceControl {
        DeviceControl(self.events.clone())
    }
}

/// Changes the devices of a [`DeviceIo`]; does nothing once it stopped.
#[derive(Clone)]
pub struct DeviceControl(std_mpsc::Sender<Event>);

impl DeviceControl {
    /// `None` follows the system default. Only a side whose choice changed is reopened.
    pub fn set_devices(&self, input: Option<String>, output: Option<String>) {
        let _ = self.0.send(Event::Want { input, output });
    }
}

/// What the device thread reacts to.
pub(crate) enum Event {
    Stop,
    Want {
        input: Option<String>,
        output: Option<String>,
    },
    /// The operating system reported a default-device or device-list change.
    Changed,
    /// Reported by the stream that `stream` numbers (a reopened side ignores its old streams).
    Trouble {
        side: Side,
        stream: u64,
        trouble: Trouble,
    },
}

/// What an open stream reports.
#[derive(Debug)]
pub(crate) enum Trouble {
    /// It stopped working: the device is gone, the stream was invalidated, ...
    Lost(String),
    Denied(String),
    /// The operating system moved it to the new default device by itself.
    Rerouted,
}

pub(crate) type TroubleFn = Arc<dyn Fn(Trouble) + Send + Sync>;

struct StopOnDrop(std_mpsc::Sender<Event>);
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send(Event::Stop);
    }
}

/// Converts between a device's native rate/channels and 48 kHz mono using
/// linear interpolation. Adequate for speech; a polyphase resampler can
/// replace it without touching the engine.
struct Resampler {
    step: f64,
    position: f64,
    last: f32,
}

impl Resampler {
    fn new(from: u32, to: u32) -> Self {
        Self { step: f64::from(from) / f64::from(to), position: 0.0, last: 0.0 }
    }

    fn process(&mut self, input: &[f32], mut emit: impl FnMut(f32)) {
        if (self.step - 1.0).abs() < f64::EPSILON {
            input.iter().for_each(|s| emit(*s));
            return;
        }
        for &sample in input {
            while self.position < 1.0 {
                emit(self.last + (sample - self.last) * self.position as f32);
                self.position += self.step;
            }
            self.position -= 1.0;
            self.last = sample;
        }
    }
}

/// Opens streams on real (or, in tests, fake) devices; a stream stops when dropped.
///
/// A new stream stays silent until activated, so it can open beside the one it replaces: the
/// old stream keeps playing until the new one works, and keeps playing when it does not.
pub(crate) trait Backend {
    type Stream;
    fn offer(&self, side: Side) -> Offer;
    /// Opens stream number `id` of `side`, inactive.
    fn open(&mut self, side: Side, route: &Route, id: u64, trouble: TroubleFn) -> Result<Self::Stream>;
    /// Makes stream `id` the one that feeds the engine (capture) or plays it (output).
    fn activate(&mut self, side: Side, id: u64);
    /// The operating system moves an open default stream to a new default by itself.
    fn reroutes(&self, side: Side) -> bool;
}

/// Related notifications (a headset appears, then becomes the default) settle before acting.
const SETTLE: Duration = Duration::from_millis(150);
/// How often a side that waits for a device looks for it without a notification.
const POLL: Duration = Duration::from_secs(1);

const SIDES: [Side; 2] = [Side::Input, Side::Output];

fn lane_index(side: Side) -> usize {
    match side {
        Side::Input => 0,
        Side::Output => 1,
    }
}

fn failure(side: Side, denied: bool, message: String) -> DeviceIssue {
    match side {
        Side::Input if denied => DeviceIssue::InputDenied(message),
        Side::Input => DeviceIssue::Input(message),
        Side::Output => DeviceIssue::Output(message),
    }
}

/// Why a side moves: the user's own choice closes the old stream first (the same device may
/// not open twice); a move by itself keeps the old stream until the new one works.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Move {
    User,
    Auto,
}

struct Lane<S> {
    side: Side,
    follow: Follow,
    stream: Option<S>,
    /// The open stream's number, so trouble from a closed one is ignored.
    generation: u64,
    /// The last number given to a stream.
    issued: u64,
}

/// Keeps each side on the device it belongs on. Runs on the device thread.
pub(crate) struct Router<B: Backend> {
    backend: B,
    lanes: [Lane<B::Stream>; 2],
    events: std_mpsc::Sender<Event>,
    sink: Option<IssueSink>,
    settle_at: Option<Instant>,
    poll_at: Option<Instant>,
}

impl<B: Backend> Router<B> {
    pub(crate) fn new(
        backend: B,
        input: Option<String>,
        output: Option<String>,
        events: std_mpsc::Sender<Event>,
        sink: Option<IssueSink>,
    ) -> Self {
        let mut wanted = [input, output];
        let lanes = [0, 1].map(|i| Lane {
            side: SIDES[i],
            follow: Follow::new(wanted[i].take(), backend.reroutes(SIDES[i])),
            stream: None,
            generation: 0,
            issued: 0,
        });
        Self { backend, lanes, events, sink, settle_at: None, poll_at: None }
    }

    fn report(&self, issue: DeviceIssue) {
        if let Some(sink) = &self.sink {
            sink(issue);
        }
    }

    /// Opens both sides; problems are reported before this returns.
    pub(crate) fn start(&mut self, now: Instant) {
        for index in 0..2 {
            self.reconcile(index, now, Move::User);
        }
        self.schedule(now);
    }

    /// Puts one side on the device it belongs on now, or reports why it cannot be.
    fn reconcile(&mut self, index: usize, now: Instant, why: Move) {
        let side = self.lanes[index].side;
        let offer = self.backend.offer(side);
        let plan = self.lanes[index].follow.plan(&offer, now);
        match plan {
            Plan::Keep => {}
            Plan::Missing => {
                if self.lanes[index].follow.failed(&offer, now, false) {
                    let message = match side {
                        Side::Input => "no microphone found",
                        Side::Output => "no output device found",
                    };
                    self.report(failure(side, false, message.into()));
                }
            }
            Plan::Open(route) => {
                let lane = &mut self.lanes[index];
                if why == Move::User {
                    lane.stream = None;
                    lane.follow.closed();
                }
                lane.issued += 1;
                let (events, id) = (self.events.clone(), lane.issued);
                let trouble: TroubleFn = Arc::new(move |trouble| {
                    let _ = events.send(Event::Trouble { side, stream: id, trouble });
                });
                match self.backend.open(side, &route, id, trouble) {
                    Ok(opened) => {
                        tracing::info!("audio {side:?} opened on {route:?}");
                        self.backend.activate(side, id);
                        // The old stream closes once the new one plays.
                        lane.stream = Some(opened);
                        lane.generation = id;
                        let Opened { switched, recovered } = lane.follow.opened(route, &offer, now);
                        if recovered {
                            self.report(DeviceIssue::Recovered(side));
                        }
                        if let Some(name) = switched {
                            self.report(DeviceIssue::Switched(side, name));
                        }
                    }
                    Err(e) => {
                        tracing::warn!("audio {side:?} unavailable on {route:?}: {e:#}");
                        let denied = side == Side::Input && permission::is_denied(&e);
                        if let (Route::Named(name), false) = (&route, denied) {
                            // The default stands in: a stream still running stays, else it opens now.
                            lane.follow.bench(name, now);
                            self.reconcile(index, now, why);
                        } else if why == Move::Auto && lane.stream.is_some() && !denied {
                            // The stream it would replace still plays: keep it, try again later.
                            lane.follow.stayed(&offer, now);
                        } else {
                            lane.stream = None;
                            if lane.follow.failed(&offer, now, denied) {
                                self.report(failure(side, denied, format!("{e:#}")));
                            }
                        }
                    }
                }
            }
        }
    }

    /// Handles one event; false once the devices should close.
    pub(crate) fn handle(&mut self, event: Event, now: Instant) -> bool {
        match event {
            Event::Stop => return false,
            Event::Want { input, output } => {
                for (index, wanted) in [input, output].into_iter().enumerate() {
                    if self.lanes[index].follow.want(wanted) {
                        self.reconcile(index, now, Move::User);
                    }
                }
            }
            Event::Changed => {
                self.settle_at.get_or_insert(now + SETTLE);
            }
            Event::Trouble { side, stream, trouble } => {
                let index = lane_index(side);
                let lane = &self.lanes[index];
                if lane.generation != stream || lane.stream.is_none() {
                    return true;
                }
                match trouble {
                    // Only the default's name changes.
                    Trouble::Rerouted => self.reconcile(index, now, Move::Auto),
                    Trouble::Lost(message) => {
                        tracing::warn!("audio {side:?} stream lost: {message}");
                        let offer = self.backend.offer(side);
                        let lane = &mut self.lanes[index];
                        lane.stream = None;
                        if lane.follow.lost(&offer, now) {
                            self.report(failure(side, false, message));
                        }
                        self.reconcile(index, now, Move::Auto);
                    }
                    Trouble::Denied(message) => {
                        let lane = &mut self.lanes[index];
                        lane.stream = None;
                        if lane.follow.denied() {
                            self.report(failure(side, true, message));
                        }
                    }
                }
            }
        }
        self.schedule(now);
        true
    }

    /// When to act without an event.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        match (self.settle_at, self.poll_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    pub(crate) fn tick(&mut self, now: Instant) {
        if self.settle_at.is_some_and(|at| now >= at) {
            self.settle_at = None;
            for index in 0..2 {
                self.reconcile(index, now, Move::Auto);
            }
        }
        if self.poll_at.is_some_and(|at| now >= at) {
            self.poll_at = None;
            for index in 0..2 {
                if !self.lanes[index].follow.waiting() {
                    continue;
                }
                if self.lanes[index].follow.settle(now) {
                    self.report(DeviceIssue::Recovered(self.lanes[index].side));
                }
                self.reconcile(index, now, Move::Auto);
            }
        }
        self.schedule(now);
    }

    fn schedule(&mut self, now: Instant) {
        if !self.lanes.iter().any(|lane| lane.follow.waiting()) {
            self.poll_at = None;
        } else if self.poll_at.is_none() {
            self.poll_at = Some(now + POLL);
        }
    }

    /// Serves events until [`Event::Stop`]; the streams close when the router drops.
    pub(crate) fn run(&mut self, inbox: &std_mpsc::Receiver<Event>) {
        loop {
            // Queued events first: a Stop wins over a poll that is due, so leaving a call
            // never opens a device.
            match inbox.try_recv() {
                Ok(event) => {
                    if !self.handle(event, Instant::now()) {
                        return;
                    }
                    continue;
                }
                Err(std_mpsc::TryRecvError::Disconnected) => return,
                Err(std_mpsc::TryRecvError::Empty) => {}
            }
            let now = Instant::now();
            let event = match self.deadline() {
                Some(at) if at <= now => {
                    self.tick(now);
                    continue;
                }
                Some(at) => match inbox.recv_timeout(at - now) {
                    Ok(event) => event,
                    // Due now: the loop checks the mailbox once more, then ticks.
                    Err(std_mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std_mpsc::RecvTimeoutError::Disconnected) => return,
                },
                None => match inbox.recv() {
                    Ok(event) => event,
                    Err(_) => return,
                },
            };
            if !self.handle(event, Instant::now()) {
                return;
            }
        }
    }
}

/// The microphone ring's producer end, shared by whichever input stream is open.
struct MicFeed {
    tx: Producer<CaptureSample>,
    position: u64,
    /// When the last block arrived; a reopened device continues the clock from there.
    last: Option<Instant>,
}

/// Locks state shared with the audio callbacks without blocking one: only a stream being
/// replaced can hold it, and its callback then skips a block.
fn try_lock<T>(mutex: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match mutex.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

fn device_name(device: &cpal::Device) -> Option<String> {
    device.description().ok().map(|d| d.name().to_owned())
}

fn names<E>(devices: Result<impl Iterator<Item = cpal::Device>, E>) -> Vec<String> {
    devices.map(|it| it.filter_map(|d| device_name(&d)).collect()).unwrap_or_default()
}

fn find<E>(devices: Result<impl Iterator<Item = cpal::Device>, E>, name: &str) -> Option<cpal::Device> {
    devices.ok()?.find(|d| d.description().is_ok_and(|desc| desc.name() == name))
}

/// Routes a running stream's errors: glitches are ignored, the rest are the router's.
fn on_error(trouble: TroubleFn) -> impl FnMut(cpal::Error) + Send + 'static {
    move |e| match e.kind() {
        cpal::ErrorKind::Xrun | cpal::ErrorKind::RealtimeDenied => tracing::debug!("audio stream: {e}"),
        cpal::ErrorKind::DeviceChanged => trouble(Trouble::Rerouted),
        cpal::ErrorKind::PermissionDenied => trouble(Trouble::Denied(e.to_string())),
        _ => trouble(Trouble::Lost(e.to_string())),
    }
}

/// Which stream of a side feeds the engine; the others stay silent.
type Active = Arc<AtomicU64>;

struct Cpal {
    host: cpal::Host,
    mic: Arc<Mutex<MicFeed>>,
    speaker: Arc<Mutex<Consumer<f32>>>,
    timing: Arc<OutputTiming>,
    active: [Active; 2],
    /// The last offers read, standing in when enumeration fails.
    offers: [std::cell::RefCell<Offer>; 2],
}

/// Runs a cpal call that may panic (wasapi's `description()` expects its property store),
/// so a misbehaving driver cannot take the device thread down.
fn guarded<T>(what: &str, call: impl FnOnce() -> T) -> Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(call)).map_err(|panic| {
        let message = panic
            .downcast_ref::<&str>()
            .map(|m| (*m).to_owned())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        anyhow!("{what} panicked: {message}")
    })
}

impl Backend for Cpal {
    type Stream = cpal::Stream;

    fn offer(&self, side: Side) -> Offer {
        let read = || match side {
            Side::Input => Offer {
                default: self.host.default_input_device().as_ref().and_then(device_name),
                names: names(self.host.input_devices()),
            },
            Side::Output => Offer {
                default: self.host.default_output_device().as_ref().and_then(device_name),
                names: names(self.host.output_devices()),
            },
        };
        let last = &self.offers[lane_index(side)];
        match guarded("audio device enumeration", read) {
            Ok(offer) => *last.borrow_mut() = offer,
            Err(e) => tracing::warn!("{e:#}"),
        }
        last.borrow().clone()
    }

    fn open(&mut self, side: Side, route: &Route, id: u64, trouble: TroubleFn) -> Result<cpal::Stream> {
        let active = self.active[lane_index(side)].clone();
        guarded("opening an audio device", || match side {
            Side::Input => open_input(&self.host, route, self.mic.clone(), self.timing.clone(), (active, id), trouble),
            Side::Output => {
                open_output(&self.host, route, self.speaker.clone(), self.timing.clone(), (active, id), trouble)
            }
        })?
    }

    fn activate(&mut self, side: Side, id: u64) {
        self.active[lane_index(side)].store(id, Ordering::Release);
    }

    fn reroutes(&self, side: Side) -> bool {
        // cpal opens the macOS default output as a default-output AudioUnit, which follows the default.
        cfg!(target_os = "macos") && side == Side::Output
    }
}

fn open_input(
    host: &cpal::Host,
    route: &Route,
    feed: Arc<Mutex<MicFeed>>,
    timing: Arc<OutputTiming>,
    (active, id): (Active, u64),
    trouble: TroubleFn,
) -> Result<cpal::Stream> {
    if permission::ensure_microphone_access() == MicPermission::Denied {
        return Err(MicPermissionDenied.into());
    }
    let device = match route {
        Route::Named(name) => {
            find(host.input_devices(), name).with_context(|| format!("microphone {name} not found"))?
        }
        Route::Default(_) => host.default_input_device().context("no microphone found")?,
    };
    let config = device.default_input_config()?;
    let channels = usize::from(config.channels());
    let device_rate = u64::from(config.sample_rate());
    let mut to_engine = Resampler::new(config.sample_rate(), RATE);
    let mut mono = Vec::new();
    let mut fresh = true;
    let stream = device.build_input_stream(
        config.config(),
        move |data: &[f32], _| {
            if active.load(Ordering::Acquire) != id {
                return;
            }
            let Some(mut feed) = try_lock(&feed) else { return };
            let feed = &mut *feed;
            mono.clear();
            mono.extend(data.chunks(channels).map(|frame| frame.iter().sum::<f32>() / frame.len() as f32));
            let captured_at = Instant::now();
            if std::mem::take(&mut fresh)
                && let Some(last) = feed.last
            {
                // A reopened device continues the capture clock across the gap, so the sent
                // timestamps keep pace with time (and stay increasing).
                let gap = (captured_at.saturating_duration_since(last).as_secs_f64() * f64::from(RATE)) as u64;
                feed.position += gap.saturating_sub(mono.len() as u64 * u64::from(RATE) / device_rate);
            }
            to_engine.process(&mono, |s| {
                if feed.tx.push(CaptureSample { position: feed.position, captured_at, value: s }).is_err() {
                    timing.capture_overflow_samples.fetch_add(1, Ordering::Relaxed);
                }
                feed.position += 1;
            });
            feed.last = Some(captured_at);
            timing.wake();
        },
        on_error(trouble),
        None,
    )?;
    stream.play()?;
    Ok(stream)
}

fn open_output(
    host: &cpal::Host,
    route: &Route,
    speaker: Arc<Mutex<Consumer<f32>>>,
    timing: Arc<OutputTiming>,
    (active, id): (Active, u64),
    trouble: TroubleFn,
) -> Result<cpal::Stream> {
    let device = match route {
        Route::Named(name) => {
            find(host.output_devices(), name).with_context(|| format!("output device {name} not found"))?
        }
        Route::Default(_) => host.default_output_device().context("no output device found")?,
    };
    let config = device.default_output_config()?;
    let channels = usize::from(config.channels());
    let device_rate = config.sample_rate() as usize;
    let mut to_device = Resampler::new(RATE, config.sample_rate());
    let mut pending: Vec<f32> = Vec::new();
    let stream = device.build_output_stream(
        config.config(),
        move |data: &mut [f32], _| {
            let frames = data.len() / channels;
            // A stream opened beside the playing one stays silent until it takes over.
            if active.load(Ordering::Acquire) != id {
                data.fill(0.0);
                return;
            }
            if let Some(mut speaker_rx) = try_lock(&speaker) {
                while pending.len() < frames {
                    let Ok(sample) = speaker_rx.pop() else { break };
                    to_device.process(&[sample], |s| pending.push(s));
                }
            }
            let take = pending.len().min(frames);
            for (i, frame) in data.chunks_mut(channels).enumerate() {
                frame.fill(if i < take { pending[i] } else { 0.0 });
            }
            pending.drain(..take);
            timing.report(
                (frames * RATE as usize).div_ceil(device_rate),
                ((frames - take) * RATE as usize).div_ceil(device_rate),
            );
        },
        on_error(trouble),
        None,
    )?;
    stream.play()?;
    Ok(stream)
}

impl AudioIo for DeviceIo {
    fn open(self: Box<Self>) -> Result<AudioPorts> {
        let DeviceIo { input, output: speakers, on_issue, events, inbox } = *self;
        let (mic_tx, mic) = RingBuffer::<CaptureSample>::new(RATE as usize); // 1 s
        let (speaker, speaker_rx) = RingBuffer::<f32>::new(RATE as usize / 2);
        let output = Arc::new(OutputTiming::default());
        let timing = output.clone();
        let stop = StopOnDrop(events.clone());
        let (ready_tx, ready_rx) = std_mpsc::channel::<()>();
        // cpal streams are not `Send` on every platform; they live on this thread, and the
        // CoreAudio listeners are added and removed on it.
        thread::Builder::new().name("gwar-audio-devices".into()).spawn(move || {
            let backend = Cpal {
                host: cpal::default_host(),
                mic: Arc::new(Mutex::new(MicFeed { tx: mic_tx, position: 0, last: None })),
                speaker: Arc::new(Mutex::new(speaker_rx)),
                timing,
                active: Default::default(),
                offers: Default::default(),
            };
            let changed = events.clone();
            let _watch = watch::start(move || {
                let _ = changed.send(Event::Changed);
            });
            let sink = on_issue.clone();
            let mut router = Router::new(backend, input, speakers, events, on_issue);
            let served = guarded("the audio device thread", || {
                router.start(Instant::now());
                let _ = ready_tx.send(());
                router.run(&inbox);
            });
            if let (Err(e), Some(sink)) = (served, sink) {
                // Without this thread the devices are gone; say so instead of falling silent.
                tracing::error!("{e:#}");
                sink(DeviceIssue::Input(format!("{e:#}")));
                sink(DeviceIssue::Output(format!("{e:#}")));
            }
        })?;
        ready_rx.recv().map_err(|_| anyhow!("audio thread died"))?;
        Ok(AudioPorts { mic, speaker, output, _guard: Box::new(stop) })
    }
}

#[cfg(test)]
mod tests {
    use super::{OutputTiming, Resampler};
    use std::sync::atomic::Ordering;

    #[test]
    fn underruns_exclude_idle_output_and_count_callbacks_and_samples_separately() {
        let timing = OutputTiming::default();
        timing.report(480, 480);
        assert_eq!(timing.underruns.load(Ordering::Relaxed), 0);
        timing.playing.store(true, Ordering::Relaxed);
        timing.report(480, 0);
        timing.report(480, 120);
        timing.report(960, 960);
        assert_eq!(timing.callback_samples.load(Ordering::Relaxed), 960);
        assert_eq!(timing.underruns.load(Ordering::Relaxed), 2);
        assert_eq!(timing.underrun_samples.load(Ordering::Relaxed), 1080);
        timing.playing.store(false, Ordering::Relaxed);
        timing.report(480, 480);
        assert_eq!(timing.underruns.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn callback_peak_survives_smaller_callbacks_until_consumed() {
        let timing = OutputTiming::default();
        for size in [480, 960, 240, 481, 480] {
            timing.report(size, 0);
        }
        assert_eq!(timing.callback_samples.load(Ordering::Relaxed), 480);
        assert_eq!(timing.callback_peak.swap(0, Ordering::Relaxed), 960);
        timing.report(240, 0);
        assert_eq!(timing.callback_peak.swap(0, Ordering::Relaxed), 240);
    }

    #[test]
    fn resampler_keeps_duration() {
        let mut up = Resampler::new(44_100, 48_000);
        let mut out = 0;
        up.process(&vec![0.5; 44_100], |_| out += 1);
        assert!((47_990..=48_010).contains(&out), "{out}");
        let mut down = Resampler::new(48_000, 16_000);
        let mut out = 0;
        down.process(&vec![0.5; 48_000], |_| out += 1);
        assert!((15_990..=16_010).contains(&out), "{out}");
    }
}

#[cfg(test)]
mod router_tests {
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        time::{Duration, Instant},
    };

    use anyhow::{Result, bail};

    use super::{Backend, DeviceIssue, Event, POLL, Router, SETTLE, Side, Trouble, TroubleFn, guarded, lane_index};
    use crate::voice::follow::{Offer, Route};

    /// Devices as the test describes them; what was opened, and how many streams are open.
    #[derive(Default)]
    struct Fake {
        offers: [Offer; 2],
        busy: Vec<String>,
        opened: Vec<(Side, Route)>,
        open_streams: Arc<AtomicUsize>,
        /// The stream each side plays, by number.
        active: [u64; 2],
        rerouting_output: bool,
    }

    struct Stream(Arc<AtomicUsize>);
    impl Drop for Stream {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::Relaxed);
        }
    }

    impl Backend for Fake {
        type Stream = Stream;
        fn offer(&self, side: Side) -> Offer {
            self.offers[lane_index(side)].clone()
        }
        fn open(&mut self, side: Side, route: &Route, _id: u64, _trouble: TroubleFn) -> Result<Stream> {
            let (Route::Named(name) | Route::Default(Some(name))) = route else { bail!("nothing to open") };
            if self.busy.contains(name) {
                bail!("{name} is busy");
            }
            self.opened.push((side, route.clone()));
            self.open_streams.fetch_add(1, Ordering::Relaxed);
            Ok(Stream(self.open_streams.clone()))
        }
        fn activate(&mut self, side: Side, id: u64) {
            self.active[lane_index(side)] = id;
        }
        fn reroutes(&self, side: Side) -> bool {
            self.rerouting_output && side == Side::Output
        }
    }

    fn offer(default: Option<&str>, names: &[&str]) -> Offer {
        Offer { default: default.map(str::to_owned), names: names.iter().map(|n| (*n).to_owned()).collect() }
    }

    fn laptop() -> Fake {
        Fake {
            offers: [offer(Some("MacBook mic"), &["MacBook mic"]), offer(Some("Speakers"), &["Speakers"])],
            ..Fake::default()
        }
    }

    fn default(name: &str) -> Route {
        Route::Default(Some(name.into()))
    }

    struct Setup {
        router: Router<Fake>,
        issues: Arc<Mutex<Vec<DeviceIssue>>>,
        _inbox: mpsc::Receiver<Event>,
    }

    impl Setup {
        fn new(fake: Fake, input: Option<&str>, output: Option<&str>) -> Self {
            let issues = Arc::new(Mutex::new(Vec::new()));
            let sink = issues.clone();
            let (events, inbox) = mpsc::channel();
            let router = Router::new(
                fake,
                input.map(str::to_owned),
                output.map(str::to_owned),
                events,
                Some(Arc::new(move |issue| sink.lock().unwrap().push(issue))),
            );
            Self { router, issues, _inbox: inbox }
        }
        fn fake(&mut self) -> &mut Fake {
            &mut self.router.backend
        }
        fn opened(&mut self) -> Vec<(Side, Route)> {
            std::mem::take(&mut self.fake().opened)
        }
        fn issues(&self) -> Vec<DeviceIssue> {
            std::mem::take(&mut self.issues.lock().unwrap())
        }
        fn lost(&mut self, side: Side, now: Instant) {
            let stream = self.router.lanes[lane_index(side)].generation;
            assert!(self.router.handle(Event::Trouble { side, stream, trouble: Trouble::Lost("gone".into()) }, now));
        }
    }

    #[test]
    fn the_default_microphone_follows_a_new_system_default_once_changes_settle() {
        let now = Instant::now();
        let mut s = Setup::new(laptop(), None, None);
        s.router.start(now);
        assert_eq!(s.opened(), [(Side::Input, default("MacBook mic")), (Side::Output, default("Speakers"))]);
        assert_eq!(s.router.deadline(), None, "nothing to poll for");

        s.fake().offers[0] = offer(Some("AirPods"), &["MacBook mic", "AirPods"]);
        s.router.handle(Event::Changed, now);
        s.router.handle(Event::Changed, now + Duration::from_millis(50));
        assert!(s.opened().is_empty(), "waits for related notifications");
        assert_eq!(s.router.deadline(), Some(now + SETTLE));
        s.router.tick(now + SETTLE);
        assert_eq!(s.opened(), [(Side::Input, default("AirPods"))], "only the side that changed");
        assert_eq!(s.fake().open_streams.load(Ordering::Relaxed), 2, "the old stream closed");
        assert!(s.issues().is_empty(), "following the default is quiet");
    }

    #[test]
    fn a_default_stream_invalidated_by_a_default_change_reopens_on_the_new_default() {
        // Windows: cpal reports the change as a stream error on the default stream.
        let now = Instant::now();
        let mut s = Setup::new(laptop(), None, None);
        s.router.start(now);
        s.opened();
        s.fake().offers[1] = offer(Some("Headphones"), &["Speakers", "Headphones"]);
        s.lost(Side::Output, now + Duration::from_secs(10));
        assert_eq!(s.opened(), [(Side::Output, default("Headphones"))]);
        assert!(s.issues().is_empty());
    }

    #[test]
    fn a_rerouting_default_output_is_left_alone() {
        let now = Instant::now();
        let mut s = Setup::new(Fake { rerouting_output: true, ..laptop() }, None, None);
        s.router.start(now);
        s.opened();
        s.fake().offers[1] = offer(Some("AirPods"), &["Speakers", "AirPods"]);
        s.router.handle(Event::Changed, now);
        s.router.tick(now + SETTLE);
        let stream = s.router.lanes[1].generation;
        s.router.handle(Event::Trouble { side: Side::Output, stream, trouble: Trouble::Rerouted }, now + SETTLE);
        assert!(s.opened().is_empty());
        assert!(s.issues().is_empty());
    }

    #[test]
    fn an_unplugged_headset_falls_back_to_the_default_and_comes_back() {
        let now = Instant::now();
        let mut fake = laptop();
        fake.offers[0] = offer(Some("MacBook mic"), &["MacBook mic", "Headset"]);
        let mut s = Setup::new(fake, Some("Headset"), None);
        s.router.start(now);
        assert_eq!(s.opened()[0], (Side::Input, Route::Named("Headset".into())));

        let later = now + Duration::from_secs(30);
        s.fake().offers[0] = offer(Some("MacBook mic"), &["MacBook mic"]);
        s.lost(Side::Input, later);
        assert_eq!(s.opened(), [(Side::Input, default("MacBook mic"))]);
        assert_eq!(s.issues(), [DeviceIssue::Switched(Side::Input, "MacBook mic".into())], "a notice, not an error");
        assert_eq!(s.router.deadline(), Some(later + POLL), "looks for the headset");

        s.router.tick(later + POLL);
        assert!(s.opened().is_empty());
        s.fake().offers[0] = offer(Some("MacBook mic"), &["MacBook mic", "Headset"]);
        s.router.tick(later + POLL * 2);
        assert_eq!(s.opened(), [(Side::Input, Route::Named("Headset".into()))]);
        assert_eq!(s.issues(), [DeviceIssue::Switched(Side::Input, "Headset".into())]);
        assert_eq!(s.router.deadline(), None);
        assert_eq!(s.fake().open_streams.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn trouble_from_a_replaced_stream_is_ignored() {
        let now = Instant::now();
        let mut s = Setup::new(laptop(), None, None);
        s.fake().offers[0] = offer(Some("MacBook mic"), &["MacBook mic", "USB mic"]);
        s.router.start(now);
        s.opened();
        let old = s.router.lanes[0].generation;
        s.router.handle(Event::Want { input: Some("USB mic".into()), output: None }, now);
        assert_eq!(s.opened(), [(Side::Input, Route::Named("USB mic".into()))]);
        s.router.handle(
            Event::Trouble { side: Side::Input, stream: old, trouble: Trouble::Lost("late".into()) },
            now + Duration::from_secs(10),
        );
        assert!(s.opened().is_empty());
        assert!(s.issues().is_empty());
    }

    #[test]
    fn choosing_a_device_reopens_only_that_side_without_a_notice() {
        let now = Instant::now();
        let mut fake = laptop();
        fake.offers[1] = offer(Some("Speakers"), &["Speakers", "Headphones"]);
        let mut s = Setup::new(fake, None, None);
        s.router.start(now);
        s.opened();
        s.router.handle(Event::Want { input: None, output: Some("Headphones".into()) }, now);
        assert_eq!(s.opened(), [(Side::Output, Route::Named("Headphones".into()))]);
        s.router.handle(Event::Want { input: None, output: Some("Headphones".into()) }, now);
        assert!(s.opened().is_empty(), "an unchanged choice keeps the stream");
        assert!(s.issues().is_empty());
    }

    #[test]
    fn a_missing_microphone_is_reported_once_and_picked_up_when_plugged_in() {
        let now = Instant::now();
        let mut fake = laptop();
        fake.offers[0] = Offer::default();
        let mut s = Setup::new(fake, None, None);
        s.router.start(now);
        assert_eq!(s.opened(), [(Side::Output, default("Speakers"))], "listening still works");
        assert_eq!(s.issues(), [DeviceIssue::Input("no microphone found".into())]);
        s.router.tick(now + POLL);
        s.router.tick(now + POLL * 3);
        assert!(s.issues().is_empty(), "told once");

        s.fake().offers[0] = offer(Some("USB mic"), &["USB mic"]);
        s.router.handle(Event::Changed, now + POLL * 3);
        s.router.tick(now + POLL * 3 + SETTLE);
        assert_eq!(s.opened(), [(Side::Input, default("USB mic"))]);
        assert_eq!(s.issues(), [DeviceIssue::Recovered(Side::Input)]);
    }

    #[test]
    fn a_busy_device_is_reported_and_retried() {
        let now = Instant::now();
        let mut fake = laptop();
        fake.busy = vec!["MacBook mic".into()];
        let mut s = Setup::new(fake, None, None);
        s.router.start(now);
        assert_eq!(s.issues(), [DeviceIssue::Input("MacBook mic is busy".into())]);
        s.fake().busy.clear();
        let deadline = s.router.deadline().expect("retries");
        s.router.tick(deadline);
        assert_eq!(s.opened(), [(Side::Output, default("Speakers")), (Side::Input, default("MacBook mic"))]);
        assert_eq!(s.issues(), [DeviceIssue::Recovered(Side::Input)]);
    }

    #[test]
    fn denied_microphone_access_while_running_is_reported_and_not_retried() {
        let now = Instant::now();
        let mut s = Setup::new(laptop(), None, None);
        s.router.start(now);
        s.opened();
        let stream = s.router.lanes[0].generation;
        s.router.handle(Event::Trouble { side: Side::Input, stream, trouble: Trouble::Denied("denied".into()) }, now);
        assert_eq!(s.issues(), [DeviceIssue::InputDenied("denied".into())]);
        assert_eq!(s.router.deadline(), None);
        assert_eq!(s.fake().open_streams.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn stop_ends_the_device_thread_and_closes_its_streams() {
        let mut s = Setup::new(laptop(), None, None);
        s.router.start(Instant::now());
        let (events, inbox) = mpsc::channel();
        events.send(Event::Changed).unwrap();
        events.send(Event::Stop).unwrap();
        s.router.run(&inbox);
        let open = s.fake().open_streams.clone();
        drop(s);
        assert_eq!(open.load(Ordering::Relaxed), 0);
    }

    fn headset_offer(with_headset: bool) -> Offer {
        let names: &[&str] = if with_headset { &["MacBook mic", "Headset"] } else { &["MacBook mic"] };
        offer(Some("MacBook mic"), names)
    }

    #[test]
    fn a_returning_headset_that_does_not_open_yet_leaves_the_fallback_playing() {
        let now = Instant::now();
        let mut fake = laptop();
        fake.offers[0] = headset_offer(false);
        let mut s = Setup::new(fake, Some("Headset"), None);
        s.router.start(now);
        assert_eq!(s.opened()[0], (Side::Input, default("MacBook mic")));
        let fallback = s.fake().active[0];

        // Plugged back in, but still negotiating its Bluetooth profile.
        s.fake().offers[0] = headset_offer(true);
        s.fake().busy = vec!["Headset".into()];
        s.router.tick(now + POLL);
        assert!(s.opened().is_empty());
        assert_eq!(s.fake().active[0], fallback, "the fallback still plays");
        assert_eq!(s.fake().open_streams.load(Ordering::Relaxed), 2);
        assert!(s.issues().is_empty(), "not an error");

        s.fake().busy.clear();
        let retry = s.router.deadline().expect("tries again");
        s.router.tick(retry);
        s.router.tick(retry + POLL);
        assert_eq!(s.opened(), [(Side::Input, Route::Named("Headset".into()))]);
        assert_ne!(s.fake().active[0], fallback);
        assert_eq!(s.fake().open_streams.load(Ordering::Relaxed), 2, "the fallback closed after the switch");
        assert_eq!(s.issues(), [DeviceIssue::Switched(Side::Input, "Headset".into())]);
    }

    #[test]
    fn a_chosen_headset_that_does_not_open_lets_the_default_stand_in() {
        let now = Instant::now();
        let mut fake = laptop();
        fake.offers[0] = headset_offer(true);
        fake.busy = vec!["Headset".into()];
        let mut s = Setup::new(fake, None, None);
        s.router.start(now);
        s.opened();
        s.router.handle(Event::Want { input: Some("Headset".into()), output: None }, now);
        assert_eq!(s.opened(), [(Side::Input, default("MacBook mic"))]);
        assert_eq!(s.issues(), [DeviceIssue::Switched(Side::Input, "MacBook mic".into())], "a notice, not a dead mic");
        assert_eq!(s.fake().open_streams.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_new_default_that_does_not_open_leaves_the_working_stream() {
        let now = Instant::now();
        let mut s = Setup::new(laptop(), None, None);
        s.router.start(now);
        s.opened();
        let playing = s.fake().active[0];
        s.fake().offers[0] = offer(Some("USB mic"), &["MacBook mic", "USB mic"]);
        s.fake().busy = vec!["USB mic".into()];
        s.router.handle(Event::Changed, now);
        s.router.tick(now + SETTLE);
        assert_eq!(s.fake().active[0], playing);
        assert_eq!(s.fake().open_streams.load(Ordering::Relaxed), 2);
        assert!(s.issues().is_empty());
        s.fake().busy.clear();
        let retry = s.router.deadline().expect("tries again");
        s.router.tick(retry);
        s.router.tick(retry + POLL);
        assert_eq!(s.opened(), [(Side::Input, default("USB mic"))]);
        assert!(s.issues().is_empty());
    }

    #[test]
    fn a_default_change_right_after_opening_is_not_reported_as_a_broken_device() {
        // Windows: the default stream is invalidated by the change, here 300 ms after it opened.
        let now = Instant::now();
        let mut s = Setup::new(laptop(), None, None);
        s.router.start(now);
        s.opened();
        s.fake().offers[1] = offer(Some("Headphones"), &["Speakers", "Headphones"]);
        s.lost(Side::Output, now + Duration::from_millis(300));
        assert_eq!(s.opened(), [(Side::Output, default("Headphones"))], "reopened at once");
        assert!(s.issues().is_empty());
    }

    #[test]
    fn a_stop_queued_behind_a_due_poll_wins() {
        let start = Instant::now() - POLL * 3;
        let mut fake = laptop();
        fake.offers[0] = Offer::default();
        let mut s = Setup::new(fake, None, None);
        s.router.start(start);
        s.opened();
        assert!(s.router.deadline().is_some_and(|at| at < Instant::now()), "a poll is due");
        s.fake().offers[0] = offer(Some("USB mic"), &["USB mic"]);
        let (events, inbox) = mpsc::channel();
        events.send(Event::Stop).unwrap();
        s.router.run(&inbox);
        assert!(s.opened().is_empty(), "left the call without opening the microphone");
    }

    #[test]
    fn a_panicking_driver_call_becomes_an_error() {
        let error = guarded("enumeration", || -> u32 { panic!("could not open property store") }).unwrap_err();
        assert!(format!("{error:#}").contains("could not open property store"), "{error:#}");
        assert_eq!(guarded("enumeration", || 7).unwrap(), 7);
    }
}
