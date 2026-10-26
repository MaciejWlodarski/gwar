//! Synthetic mouth-to-ear latency through two native clients and a Gwar server.
//! Uses virtual audio devices; no microphone or speaker device is opened.
//!   cargo run --release -p gwar-client --example latency -- output.csv [marker-count]
//! Marker count defaults to 100. Writes output.csv and output.csv.json.
//! Set both LATENCY_URL (ws(s)://host:port/ws) and LATENCY_SERVER (IP:media-port)
//! to use an existing server; otherwise starts a fresh loopback-only server.
//! LATENCY_NET adds a local-only seeded UDP relay; see docs/latency.md.
//! LATENCY_MIN_MS defaults to 5; below-floor matches fail the run.
//! LATENCY_EXTRA_DELAY_MS adds a sample delay line to the fake output device.
//! LATENCY_SFRAME=1 measures an encrypted call (local server only): a real voice
//! MLS group, exporter keys installed through the engine's key channel, SFrame.
//! LATENCY_SCREEN adds screen shares and viewers beside the call; see docs/latency.md.
//! RUST_LOG logs to stderr, for example `RUST_LOG=gwar_server::media::screen=debug`.
//! Exits with an error if more than 5% of markers are unmatched.
#[path = "latency/mls.rs"]
mod mls;
#[path = "latency/netem.rs"]
mod netem;
#[path = "latency/screen.rs"]
mod screen;

use anyhow::Context;
use gwar_client::{
    ConnectOptions, Identity, connect,
    voice::{
        self,
        io::{AudioIo, AudioPorts, CaptureSample, OutputTiming},
    },
};
use gwar_proto::{ClientSoftware, Platform};
use rtrb::RingBuffer;
use serde::Serialize;
use std::{
    io::Write,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const RATE: usize = 48_000;
const BLOCK: usize = 480;
const PERIOD: usize = 12_000; // 250 ms
// scripts/latency/run_local.py budgets its client deadline for this maximum.
const MAX_WARMUP_MS: usize = 30_000;
/// Marker start after connection: 2 s, or `LATENCY_WARMUP_MS` (whole ms, at most
/// 30 s). A gate uses a longer warmup so the receiver's startup trims
/// finish before the markers.
fn warmup_samples(value: Option<&str>) -> anyhow::Result<usize> {
    let Some(value) = value else { return Ok(RATE * 2) };
    let ms = value.parse::<usize>().ok().filter(|ms| *ms <= MAX_WARMUP_MS);
    let ms = ms.with_context(|| format!("LATENCY_WARMUP_MS must be whole milliseconds from 0 to {MAX_WARMUP_MS}"))?;
    Ok(ms * (RATE / 1000))
}
const MARKER: usize = 960; // 20 ms
// At least 200 Hz apart, and 100 Hz away from the 400 Hz carrier's harmonics.
const TONES: [f64; 8] = [900.0, 1100.0, 1300.0, 1500.0, 1700.0, 1900.0, 2300.0, 2700.0];
const MATCH_WINDOW_MS: f64 = TONES.len() as f64 * PERIOD as f64 * 1000.0 / RATE as f64;
const RMS_WINDOW: usize = 240; // 5 ms
const IDENTITY_WINDOW: usize = 480; // 10 ms after the detected onset
const MIN_IDENTITY_CONFIDENCE: f64 = 0.8;
const MIN_TONE_ENERGY_FRACTION: f64 = 0.6;

#[derive(Clone)]
struct Settings {
    minimum_ms: f64,
    extra_delay_ms: usize,
    /// Receiver callbacks cycle through 10/20/5 ms (`LATENCY_CALLBACK_JITTER=1`).
    callback_jitter: bool,
    /// Arm markers after both transports connect; keep carrier/impairments running.
    wait_connected: bool,
    warmup_samples: usize,
    /// Screen shares and viewers beside the call (`LATENCY_SCREEN`).
    screen: Option<screen::Spec>,
    /// Encrypted call with SFrame (`LATENCY_SFRAME=1`).
    sframe: bool,
}
impl Settings {
    fn from_env() -> anyhow::Result<Self> {
        let minimum_ms = std::env::var("LATENCY_MIN_MS")
            .ok()
            .map(|value| value.parse::<f64>())
            .transpose()
            .context("LATENCY_MIN_MS must be a number")?
            .unwrap_or(5.0);
        anyhow::ensure!(
            minimum_ms.is_finite() && minimum_ms > 0.0 && minimum_ms < MATCH_WINDOW_MS,
            "LATENCY_MIN_MS must be positive and below {MATCH_WINDOW_MS}"
        );
        let extra_delay_ms = std::env::var("LATENCY_EXTRA_DELAY_MS")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()
            .context("LATENCY_EXTRA_DELAY_MS must be a nonnegative integer")?
            .unwrap_or(0);
        anyhow::ensure!(extra_delay_ms < MATCH_WINDOW_MS as usize, "extra delay must be below {MATCH_WINDOW_MS} ms");
        let callback_jitter = match std::env::var("LATENCY_CALLBACK_JITTER") {
            Ok(value) if value == "1" => true,
            Ok(value) if value == "0" => false,
            Err(std::env::VarError::NotPresent) => false,
            _ => anyhow::bail!("LATENCY_CALLBACK_JITTER must be 0 or 1"),
        };
        let wait_connected = match std::env::var("LATENCY_WAIT_CONNECTED") {
            Ok(value) if value == "1" => true,
            Ok(value) if value == "0" => false,
            Err(std::env::VarError::NotPresent) => false,
            _ => anyhow::bail!("LATENCY_WAIT_CONNECTED must be 0 or 1"),
        };
        let warmup = match std::env::var("LATENCY_WARMUP_MS") {
            Ok(value) => Some(value),
            Err(std::env::VarError::NotPresent) => None,
            Err(std::env::VarError::NotUnicode(_)) => anyhow::bail!("LATENCY_WARMUP_MS must be valid UTF-8"),
        };
        let warmup_samples = warmup_samples(warmup.as_deref())?;
        let sframe = match std::env::var("LATENCY_SFRAME") {
            Ok(value) if value == "1" => true,
            Ok(value) if value == "0" => false,
            Err(std::env::VarError::NotPresent) => false,
            _ => anyhow::bail!("LATENCY_SFRAME must be 0 or 1"),
        };
        let screen = std::env::var("LATENCY_SCREEN").ok().map(|source| screen::Spec::parse(&source)).transpose()?;
        anyhow::ensure!(!(sframe && screen.is_some()), "LATENCY_SCREEN measures an unencrypted call");
        Ok(Self { minimum_ms, extra_delay_ms, callback_jitter, wait_connected, warmup_samples, screen, sframe })
    }
}

struct DelayLine {
    samples: Vec<f32>,
    position: usize,
}
impl DelayLine {
    fn new(samples: usize) -> Self {
        Self { samples: vec![0.0; samples], position: 0 }
    }

    fn process(&mut self, value: f32) -> f32 {
        if self.samples.is_empty() {
            return value;
        }
        let output = std::mem::replace(&mut self.samples[self.position], value);
        self.position = (self.position + 1) % self.samples.len();
        output
    }
}

#[derive(Clone, Copy, Serialize)]
struct Marker {
    capture_s: f64,
    identity: usize,
}
#[derive(Serialize)]
struct Onset {
    playout_s: f64,
    sample: usize,
    identity: Option<usize>,
    identity_confidence: f64,
    tone_energy_fraction: f64,
}
struct Match {
    marker: usize,
    onset: usize,
    latency_ms: f64,
}
#[derive(Serialize)]
struct RejectedOnset {
    marker: usize,
    onset: usize,
    latency_ms: f64,
    reason: &'static str,
}
struct Matching {
    matched: Vec<Match>,
    lost: Vec<usize>,
    rejected: Vec<RejectedOnset>,
    unused: Vec<usize>,
}

fn match_markers(markers: &[Marker], onsets: &[Onset], minimum_ms: f64, window_ms: f64) -> Matching {
    let mut used = vec![false; onsets.len()];
    let mut result = Matching { matched: Vec::new(), lost: Vec::new(), rejected: Vec::new(), unused: Vec::new() };
    for (marker, capture) in markers.iter().enumerate() {
        let found = onsets.iter().enumerate().find(|&(index, onset)| {
            !used[index]
                && onset.identity == Some(capture.identity)
                && (0.0..window_ms).contains(&((onset.playout_s - capture.capture_s) * 1000.0))
        });
        if let Some((onset, detected)) = found {
            // Consume even a rejected onset, so it can never measure another marker.
            used[onset] = true;
            let latency_ms = (detected.playout_s - capture.capture_s) * 1000.0;
            if latency_ms < minimum_ms {
                result.rejected.push(RejectedOnset { marker, onset, latency_ms, reason: "below_minimum_latency" });
                result.lost.push(marker);
            } else {
                result.matched.push(Match { marker, onset, latency_ms });
            }
        } else {
            result.lost.push(marker);
        }
    }
    result.unused = used.iter().enumerate().filter_map(|(i, &used)| (!used).then_some(i)).collect();
    result
}

fn identify_tone(samples: &[(f64, f32)]) -> (Option<usize>, f64, f64) {
    if samples.len() != IDENTITY_WINDOW {
        return (None, 0.0, 0.0);
    }
    // Hann-weighted Goertzel bins reject leakage from the carrier and nearby tones.
    let weights: Vec<_> = (0..samples.len())
        .map(|i| 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / (samples.len() - 1) as f64).cos())
        .collect();
    let energy: f64 = samples.iter().zip(&weights).map(|((_, value), weight)| f64::from(*value).powi(2) * weight).sum();
    let powers: Vec<_> = TONES
        .iter()
        .map(|frequency| {
            let coefficient = 2.0 * (std::f64::consts::TAU * frequency / RATE as f64).cos();
            let (mut previous, mut before_previous) = (0.0, 0.0);
            for ((_, value), weight) in samples.iter().zip(&weights) {
                let current = f64::from(*value) * weight + coefficient * previous - before_previous;
                before_previous = previous;
                previous = current;
            }
            (previous.powi(2) + before_previous.powi(2) - coefficient * previous * before_previous).max(0.0)
        })
        .collect();
    let identity = (0..TONES.len()).max_by(|&a, &b| powers[a].total_cmp(&powers[b])).unwrap();
    let total: f64 = powers.iter().sum();
    let confidence = if total > 0.0 { powers[identity] / total } else { 0.0 };
    let weighted_energy = energy * weights.iter().sum::<f64>();
    let fraction = if weighted_energy > 0.0 { 2.0 * powers[identity] / weighted_energy } else { 0.0 };
    let identified = confidence >= MIN_IDENTITY_CONFIDENCE && fraction >= MIN_TONE_ENERGY_FRACTION;
    (identified.then_some(identity), confidence, fraction)
}

fn detect_onsets(samples: &[(f64, f32)]) -> Vec<Onset> {
    let mut square = 0.0f64;
    let mut high = false;
    let mut onsets = Vec::new();
    for i in 0..samples.len() {
        square += f64::from(samples[i].1).powi(2);
        if i >= RMS_WINDOW {
            square -= f64::from(samples[i - RMS_WINDOW].1).powi(2);
        }
        let rms = (square.max(0.0) / RMS_WINDOW as f64).sqrt();
        if !high && rms > 0.18 {
            let end = (i + IDENTITY_WINDOW).min(samples.len());
            let (identity, identity_confidence, tone_energy_fraction) = identify_tone(&samples[i..end]);
            onsets.push(Onset {
                playout_s: samples[i].0,
                sample: i,
                identity,
                identity_confidence,
                tone_energy_fraction,
            });
            high = true;
        }
        if high && rms < 0.11 {
            high = false;
        }
    }
    onsets
}

/// The longest wait for both voice transports before markers are armed (`LATENCY_WAIT_CONNECTED`).
const CONNECT_WAIT: Duration = Duration::from_secs(30);
/// At most 250 s of markers: the playout record (16 bytes a sample) stays below about 250 MB.
const MAX_MARKERS: usize = 1000;

/// A fixed-capacity buffer for the device callbacks: allocated and written once before the
/// devices start, so a callback never grows or first touches it. Past its capacity it counts
/// what it could not keep. Grown in the callback, the playout record reallocated at each power
/// of two; the move at 2^20 samples (21.8 s) coincided with a single underrun and a lasting
/// 15 ms latency step in about one gate run in six (docs/latency.md). A reservation alone is not
/// enough: with transparent huge pages, the first write to each 2 MB of it (every 2.7 s)
/// stalled the callback for up to 17 ms.
struct Record<T> {
    items: Vec<T>,
    dropped: u64,
}
impl<T: Copy> Record<T> {
    fn new(capacity: usize, fill: T) -> Self {
        let mut items = Vec::with_capacity(capacity);
        items.resize(items.capacity(), fill);
        items.clear();
        Self { items, dropped: 0 }
    }

    /// Keeps `item` if there is room; never reallocates.
    fn push(&mut self, item: T) -> bool {
        if self.items.len() < self.items.capacity() {
            self.items.push(item);
            true
        } else {
            self.dropped += 1;
            false
        }
    }

    fn capacity(&self) -> usize {
        self.items.capacity()
    }

    /// Capacity, use and overflow, for the run's JSON.
    fn report(&self) -> serde_json::Value {
        serde_json::json!({
            "capacity_samples": self.capacity(),
            "recorded_samples": self.len(),
            "overflow_samples": self.dropped,
        })
    }
}
impl<T> std::ops::Deref for Record<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.items
    }
}
impl<T: Serialize> Serialize for Record<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.items.serialize(serializer)
    }
}

/// Room for every receiver playout sample of a run: from the device start through voice
/// signaling, the connection wait, warmup, markers and matching window, until the devices are
/// stopped. Anything beyond is counted as overflow and fails the run.
fn playout_record(run_for: Duration, wait_connected: bool) -> Record<(f64, f32)> {
    let wait = if wait_connected { CONNECT_WAIT } else { Duration::ZERO };
    let samples = ((run_for + wait + Duration::from_secs(5)).as_secs_f64() * RATE as f64) as usize;
    Record::new(samples, (0.0, 0.0))
}

struct Measurements {
    captures: Record<Marker>,
    samples: Record<(f64, f32)>,
    underflows: usize,
    device: DeviceTiming,
}
impl Measurements {
    fn sender(markers: usize) -> Self {
        Self::new(markers, Record::new(0, (0.0, 0.0)))
    }

    fn receiver(samples: Record<(f64, f32)>) -> Self {
        Self::new(0, samples)
    }

    fn new(markers: usize, samples: Record<(f64, f32)>) -> Self {
        let captures = Record::new(markers, Marker { capture_s: 0.0, identity: 0 });
        Self { captures, samples, underflows: 0, device: DeviceTiming::new() }
    }
}

/// Fake device callbacks that started late or ran long. A late callback is followed by
/// back-to-back catch-up callbacks, which can drain the speaker ring faster than real time.
const LATE_CALLBACK: Duration = Duration::from_millis(5);
const LONG_CALLBACK: Duration = Duration::from_millis(2);
const MAX_SLOW_CALLBACKS: usize = 32;
#[derive(Serialize)]
struct DeviceTiming {
    callbacks: u64,
    max_lateness_ms: f64,
    max_busy_ms: f64,
    late_over_5ms: u64,
    busy_over_2ms: u64,
    /// The first slow callbacks, on the measurement clock; `slow_dropped` counts the rest.
    slow: Record<SlowCallback>,
    slow_dropped: u64,
    /// Why the callbacks ended (`stopped` or `mic_ring_full`); null while running.
    termination: Option<&'static str>,
}
#[derive(Clone, Copy, Serialize)]
struct SlowCallback {
    at_s: f64,
    lateness_ms: f64,
    busy_ms: f64,
    /// Playout samples recorded before this callback (receiver only).
    recorded_samples: usize,
}
impl DeviceTiming {
    fn new() -> Self {
        let empty = SlowCallback { at_s: 0.0, lateness_ms: 0.0, busy_ms: 0.0, recorded_samples: 0 };
        Self {
            callbacks: 0,
            max_lateness_ms: 0.0,
            max_busy_ms: 0.0,
            late_over_5ms: 0,
            busy_over_2ms: 0,
            slow: Record::new(MAX_SLOW_CALLBACKS, empty),
            slow_dropped: 0,
            termination: None,
        }
    }

    /// Constant time and allocation-free, so `busy` may end just before it.
    fn record(&mut self, at_s: f64, lateness: Duration, busy: Duration, recorded_samples: usize) {
        self.callbacks += 1;
        let (lateness_ms, busy_ms) = (lateness.as_secs_f64() * 1000.0, busy.as_secs_f64() * 1000.0);
        self.max_lateness_ms = self.max_lateness_ms.max(lateness_ms);
        self.max_busy_ms = self.max_busy_ms.max(busy_ms);
        let (late, long) = (lateness > LATE_CALLBACK, busy > LONG_CALLBACK);
        self.late_over_5ms += u64::from(late);
        self.busy_over_2ms += u64::from(long);
        if (late || long) && !self.slow.push(SlowCallback { at_s, lateness_ms, busy_ms, recorded_samples }) {
            self.slow_dropped += 1;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Termination {
    Stopped,
    MicRingFull,
}
impl Termination {
    fn name(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::MicRingFull => "mic_ring_full",
        }
    }
}

/// Stops a fake device from outside and waits until its last callback has stored its
/// measurements.
#[derive(Clone, Default)]
struct DeviceControl {
    stop: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
}
/// How long a stopping device may take to finish its callback; one is at most a few ms.
const STOP_WAIT: Duration = Duration::from_secs(2);
/// How long a dropped device's thread may take to finish before it is left detached, so a
/// stuck device cannot keep a failing run from returning.
const DROP_WAIT: Duration = Duration::from_secs(1);
impl DeviceControl {
    fn finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }
}

/// Signals every device first, then waits for all of them together, at most `STOP_WAIT`.
async fn stop_devices(controls: &[DeviceControl]) -> anyhow::Result<()> {
    for control in controls {
        control.stop.store(true, Ordering::Relaxed);
    }
    tokio::time::timeout(STOP_WAIT, async {
        while !controls.iter().all(DeviceControl::finished) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .with_context(|| format!("a fake audio device did not stop within {} s", STOP_WAIT.as_secs()))
}

struct FakeIo {
    markers_ready: Arc<AtomicBool>,
    data: Arc<Mutex<Measurements>>,
    tone: bool,
    /// The measurement clock's zero.
    epoch: Instant,
    /// When both devices' first callbacks run.
    start: Instant,
    control: DeviceControl,
    count: usize,
    delay_samples: usize,
    callback_jitter: bool,
    warmup_samples: usize,
}
struct Guard {
    control: DeviceControl,
    thread: Option<thread::JoinHandle<()>>,
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.control.stop.store(true, Ordering::Relaxed);
        // Wait for the last callback, but never unboundedly: a thread still running after
        // `DROP_WAIT` is stuck and is left detached.
        let deadline = Instant::now() + DROP_WAIT;
        while !self.control.finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        if let Some(thread) = self.thread.take()
            && self.control.finished()
        {
            let _ = thread.join();
        }
    }
}

/// A ring whose pages have all been written once, so the device's first pass through it does
/// not fault them in.
fn prefaulted_ring<T: Copy>(capacity: usize, fill: T) -> (rtrb::Producer<T>, rtrb::Consumer<T>) {
    let (mut producer, mut consumer) = RingBuffer::new(capacity);
    while producer.push(fill).is_ok() {}
    while consumer.pop().is_ok() {}
    (producer, consumer)
}

/// One fake device's callback state. Everything a callback writes is allocated before the
/// first one runs.
struct Callbacks {
    io: Box<FakeIo>,
    mic: rtrb::Producer<CaptureSample>,
    speaker: rtrb::Consumer<f32>,
    timing: Arc<OutputTiming>,
    /// Delays decoded audio, including silence and carrier, before playout. Capture/playout
    /// timestamps always remain the actual callback times.
    delay: DelayLine,
    position: usize,
    callback: usize,
    marker_start: Option<usize>,
    stats: DeviceTiming,
}
impl Callbacks {
    fn new(
        io: Box<FakeIo>,
        mic: rtrb::Producer<CaptureSample>,
        speaker: rtrb::Consumer<f32>,
        timing: Arc<OutputTiming>,
    ) -> Self {
        let delay = DelayLine::new(io.delay_samples);
        // The first lock may allocate (a lazily boxed pthread mutex on macOS): take it here.
        drop(io.data.lock().unwrap());
        Self {
            io,
            mic,
            speaker,
            timing,
            delay,
            position: 0,
            callback: 0,
            marker_start: None,
            stats: DeviceTiming::new(),
        }
    }

    fn due(&self) -> Instant {
        self.io.start + Duration::from_secs_f64(self.position as f64 / RATE as f64)
    }

    /// One callback, run at `now`.
    fn run(&mut self, now: Instant) -> Result<(), Termination> {
        let io = &*self.io;
        // Only the receiver varies its block sizes when callback jitter is enabled.
        let block = if io.callback_jitter { [480, 960, 240][self.callback % 3] } else { BLOCK };
        let lateness = now.saturating_duration_since(self.due());
        if self.marker_start.is_none() && io.markers_ready.load(Ordering::Acquire) {
            self.marker_start = Some(self.position + io.warmup_samples);
        }
        let warmup = self.marker_start.unwrap_or(usize::MAX);
        let mut missing = 0;
        let mut data = io.data.lock().unwrap();
        let recorded = data.samples.len();
        for i in 0..block {
            let at = self.position + i;
            let in_marker = at >= warmup && (at - warmup) / PERIOD < io.count && (at - warmup) % PERIOD < MARKER;
            if io.tone && at >= warmup && (at - warmup).is_multiple_of(PERIOD) && data.captures.len() < io.count {
                data.captures.push(Marker {
                    capture_s: (now - io.epoch).as_secs_f64() + i as f64 / RATE as f64,
                    identity: (at - warmup) / PERIOD % TONES.len(),
                });
            }
            let frequency = if in_marker { TONES[(at - warmup) / PERIOD % TONES.len()] } else { 400.0 };
            let amplitude = if in_marker { 0.6 } else { 0.08 };
            let value = if io.tone {
                (at as f64 * frequency * std::f64::consts::TAU / RATE as f64).sin() as f32 * amplitude
            } else {
                0.0
            };
            if self.mic.push(CaptureSample { captured_at: Instant::now(), position: at as u64, value }).is_err() {
                return Err(Termination::MicRingFull);
            }
            let value = self.speaker.pop().unwrap_or_else(|_| {
                data.underflows += 1;
                missing += 1;
                0.0
            });
            if !io.tone {
                let playout_s = (now - io.epoch).as_secs_f64() + i as f64 / RATE as f64;
                data.samples.push((playout_s, self.delay.process(value)));
            }
        }
        drop(data);
        self.timing.report(block, missing);
        self.stats.record((now - io.epoch).as_secs_f64(), lateness, now.elapsed(), recorded);
        self.callback += 1;
        self.position += block;
        Ok(())
    }

    /// Every exit stores the measurements and why the callbacks ended.
    fn finish(mut self, termination: Termination) {
        self.stats.termination = Some(termination.name());
        self.io.data.lock().unwrap().device = self.stats;
        self.io.control.finished.store(true, Ordering::Release);
    }

    fn run_until_stopped(mut self) {
        // Both devices start on the same phase.
        if let Some(wait) = self.io.start.checked_duration_since(Instant::now()) {
            thread::sleep(wait);
        }
        let stop = self.io.control.stop.clone();
        let termination = loop {
            if stop.load(Ordering::Relaxed) {
                break Termination::Stopped;
            }
            if let Err(termination) = self.run(Instant::now()) {
                break termination;
            }
            if let Some(wait) = self.due().checked_duration_since(Instant::now()) {
                thread::sleep(wait);
            }
        };
        self.finish(termination);
    }
}

impl AudioIo for FakeIo {
    fn open(self: Box<Self>) -> anyhow::Result<AudioPorts> {
        let blank = CaptureSample { captured_at: Instant::now(), position: 0, value: 0.0 };
        let (mic_tx, mic) = prefaulted_ring(RATE, blank);
        let (speaker, speaker_rx) = prefaulted_ring(RATE / 2, 0.0);
        let output = Arc::new(OutputTiming::default());
        let control = self.control.clone();
        let callbacks = Callbacks::new(self, mic_tx, speaker_rx, output.clone());
        let thread =
            thread::Builder::new().name("latency-audio".into()).spawn(move || callbacks.run_until_stopped())?;
        Ok(AudioPorts { mic, speaker, output, _guard: Box::new(Guard { control, thread: Some(thread) }) })
    }
}
/// Routes the voice answer's ICE candidates through the selected relay role (or direct socket).
fn voice_answer(sdp: &str, server: std::net::SocketAddr) -> String {
    sdp.lines()
        .map(|line| {
            if line.starts_with("a=candidate:") {
                let mut fields: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
                if fields.len() >= 6 {
                    fields[4] = server.ip().to_string();
                    fields[5] = server.port().to_string();
                    return fields.join(" ");
                }
            }
            line.to_owned()
        })
        .collect::<Vec<_>>()
        .join("\r\n")
        + "\r\n"
}

async fn voice_start(
    connection: &gwar_client::Connection,
    io: Box<dyn AudioIo>,
    server: std::net::SocketAddr,
) -> anyhow::Result<voice::VoiceHandle> {
    let connection = connection.clone();
    voice::start_with(io, server.ip(), move |offer| async move {
        Ok(voice_answer(&connection.voice_offer(offer).await?, server))
    })
    .await
}

/// Starts an encrypted native run; the offer names the call's voice group.
async fn encrypted_start(
    client: &gwar_client::Connected,
    state: &serde_json::Value,
    io: Box<FakeIo>,
    server: std::net::SocketAddr,
) -> anyhow::Result<(voice::VoiceHandle, serde_json::Value)> {
    const RUN: u64 = 1;
    let binding = serde_json::json!({
        "server_key": client.connection.server_key, "session": client.welcome.session, "channel": state["channel"],
        "call": state["call"], "participation": state["participation"], "run": RUN, "group": state["group"],
    });
    let start = voice::keys::EncryptedStart {
        binding: voice::sframe::Binding::parse(&binding).context("binding")?,
        resume_after: None,
    };
    let connection = client.connection.clone();
    let offer = state.clone();
    let signal = move |sdp: String| async move {
        let ssrc: u32 = sdp
            .lines()
            .find_map(|l| l.strip_prefix("a=ssrc:").and_then(|s| s.split_whitespace().next()?.parse().ok()))
            .context("offer without microphone SSRC")?;
        let request: gwar_proto::Request = serde_json::from_value(serde_json::json!({ "op": "voice.offer", "d": {
            "channel": offer["channel"], "call": offer["call"], "participation": offer["participation"],
            "run": RUN, "group": offer["group"], "ssrc": ssrc, "sdp": sdp } }))?;
        let answer = connection.request(request).await?;
        Ok(voice_answer(answer["sdp"].as_str().context("answer without sdp")?, server))
    };
    Ok((voice::start_encrypted(io, server.ip(), signal, start).await?, binding))
}

fn options(url: &str, name: &str) -> ConnectOptions {
    ConnectOptions {
        url: url.into(),
        nickname: name.into(),
        invite: None,
        certificate: None,
        expected_server_key: None,
        trust_on_first_use: true,
        insecure_lan: false,
        server_password: None,
        identity: Identity::generate(),
        software: ClientSoftware { name: "Gwar latency".into(), version: "1".into(), platform: Platform::Desktop },
    }
}
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    // Logs only on request (`RUST_LOG`): the run's own output is its report.
    if let Ok(filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
    }
    let mut args = std::env::args().skip(1);
    let output = args.next().context("usage: latency output.csv [marker-count]")?;
    let count: usize = args.next().map(|s| s.parse()).transpose().context("invalid marker count")?.unwrap_or(100);
    anyhow::ensure!(args.next().is_none(), "usage: latency output.csv [marker-count]");
    anyhow::ensure!(count > 0, "marker count must be positive");
    anyhow::ensure!(count <= MAX_MARKERS, "marker count must be at most {MAX_MARKERS}");
    let remote = match (std::env::var("LATENCY_URL"), std::env::var("LATENCY_SERVER")) {
        (Ok(url), Ok(addr)) => Some((url, addr.parse().context("LATENCY_SERVER must be IP:port")?)),
        (Err(std::env::VarError::NotPresent), Err(std::env::VarError::NotPresent)) => None,
        _ => anyhow::bail!("set both LATENCY_URL and LATENCY_SERVER, or neither"),
    };
    let net = std::env::var("LATENCY_NET").ok().map(|source| netem::Spec::parse(&source)).transpose()?;
    anyhow::ensure!(remote.is_none() || net.is_none(), "LATENCY_NET is supported only with the fresh local server");
    let settings = Settings::from_env()?;
    let mut tasks = ServerTasks { tasks: Vec::new(), data_dir: None };
    let result = tokio::select! {
        result = measure(&output, count, remote, &mut tasks, settings, net) => result,
        signal = shutdown_signal() => {
            signal?;
            Err(anyhow::anyhow!("measurement interrupted"))
        }
    };
    tasks.shutdown().await;
    result
}

async fn shutdown_signal() -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}

struct ServerTasks {
    tasks: Vec<tokio::task::JoinHandle<()>>,
    data_dir: Option<DataDir>,
}
impl ServerTasks {
    async fn shutdown(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }
        self.data_dir.take();
    }
}
impl Drop for ServerTasks {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

struct DataDir(PathBuf);
impl Drop for DataDir {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            eprintln!("could not remove temporary data directory {}: {error}", self.0.display());
        }
    }
}

async fn measure(
    output: &str,
    count: usize,
    remote: Option<(String, std::net::SocketAddr)>,
    tasks: &mut ServerTasks,
    settings: Settings,
    net: Option<netem::Spec>,
) -> anyhow::Result<()> {
    if let Some((url, addr)) = remote {
        anyhow::ensure!(!settings.sframe, "LATENCY_SFRAME needs the fresh local server");
        return run(output, count, &url, addr, settings, None, None).await;
    }
    // Keep transient server files beside the output, and never reuse state.
    let data_path = PathBuf::from(format!("{output}.data-{}", std::process::id()));
    std::fs::create_dir(&data_path).context("create temporary server data directory")?;
    let data = DataDir(data_path);
    let any = "127.0.0.1:0".parse().unwrap();
    let relay = if net.is_some() { Some(netem::Relay::bind().await?) } else { None };
    let advertise = relay.as_ref().map(|relay| relay.address()).transpose()?.unwrap_or(any);
    let server = gwar_server::start(gwar_server::Config {
        database: Some(data.0.join("server.sqlite3")),
        http_bind: any,
        media_bind: any,
        media_advertise: advertise,
        // Two voice clients and up to 14 sharers and viewers (LATENCY_SCREEN).
        max_clients: 32,
        encrypted_server_chat: false,
        server_password: None,
        web_root: None,
        ice_servers: vec![],
        teamspeak: None,
        public_url: None,
        data_dir: Some(data.0.clone()),
        public_ip: None,
        insecure_any_host: true,
        public_endpoints: Vec::new(),
        client_ip_header: None,
        upload_limit: 10 * 1024 * 1024,
        upload_quota: 0,
        storage_limit: 0,
        files_dir: Some(data.0.join("files")),
        web_origins: Vec::new(),
        tls: None,
        redirect_http: None,
        connect_url: None,
    })
    .await
    .context("start local latency server")?;

    assert!(server.http.ip().is_loopback() && server.media.ip().is_loopback());
    println!("local server http={} media={} media_bind={}", server.http, server.media, server.media_bind);
    let url = format!("ws://{}/ws", server.http);
    tasks.tasks = server.tasks;
    tasks.data_dir = Some(data);
    let net = relay.zip(net).map(|(relay, spec)| (relay, spec, server.media_bind));
    let admin = server.admin_token.clone().map(|token| (server.http, token));
    let result = run(output, count, &url, server.media, settings, net, admin).await;
    drop(server.core);
    drop(server.plane);
    tasks.shutdown().await;
    result
}

async fn run(
    output: &str,
    count: usize,
    url: &str,
    server_addr: std::net::SocketAddr,
    settings: Settings,
    mut net: Option<(netem::Relay, netem::Spec, std::net::SocketAddr)>,
    admin: Option<(std::net::SocketAddr, String)>,
) -> anyhow::Result<()> {
    let url = url.to_string();
    let (mut sender, mut receiver) = (mls::Device::new(), mls::Device::new());
    let with = |options: ConnectOptions, device: &mls::Device| {
        if !settings.sframe {
            return options;
        }
        ConnectOptions { identity: device.identity.clone(), certificate: Some(device.certificate.clone()), ..options }
    };
    let a = connect(with(options(&url, "latency sender"), &sender)).await?;
    let mut b = connect(with(options(&url, "latency receiver"), &receiver)).await?;
    let calls = if settings.sframe {
        let (http, token) = admin.context("LATENCY_SFRAME needs the local server's admin token")?;
        Some(mls::encrypted_call(http, &token, (&a, &mut sender), (&b, &mut receiver)).await?)
    } else {
        for client in [&a, &b] {
            client.connection.join(client.welcome.server.default_channel, None).await?;
        }
        None
    };
    let epoch = Instant::now();
    let media_addr = if let Some((relay, spec, server)) = &mut net {
        relay.start(*server, spec.clone(), epoch)?;
        println!("netem relay={} spec={}", relay.address()?, spec.source);
        relay.address()?
    } else {
        server_addr
    };
    // The screen load runs from before voice starts until the measurement ends.
    let load = match &settings.screen {
        Some(spec) => {
            let (relay, direct) = match &net {
                Some((relay, _, server)) => (Some(relay.address()?), *server),
                None => (None, server_addr),
            };
            let channel = a.welcome.server.default_channel;
            let load = screen::Load::start(&url, spec.clone(), channel, media_addr, relay, direct).await?;
            println!("screen load {}", spec.source);
            Some(load)
        }
        None => None,
    };
    let markers_ready = Arc::new(AtomicBool::new(!settings.wait_connected));
    // Markers, then their matching window, after the warmup.
    let run_for = Duration::from_secs_f64(
        1.0 + settings.warmup_samples as f64 / RATE as f64
            + count as f64 * PERIOD as f64 / RATE as f64
            + MATCH_WINDOW_MS / 1000.0,
    );
    let sent = Arc::new(Mutex::new(Measurements::sender(count)));
    let heard = Arc::new(Mutex::new(Measurements::receiver(playout_record(run_for, settings.wait_connected))));
    let controls = [DeviceControl::default(), DeviceControl::default()];
    // After the screen load, whose start can take seconds, and the playout record: a device
    // that starts late runs ahead to catch up and overflows its ring.
    let start = Instant::now() + Duration::from_millis(200);
    let io = |data: &Arc<Mutex<Measurements>>, tone: bool, delay_samples: usize, callback_jitter: bool| {
        Box::new(FakeIo {
            markers_ready: markers_ready.clone(),
            data: data.clone(),
            tone,
            epoch,
            start,
            control: controls[usize::from(!tone)].clone(),
            count,
            delay_samples,
            callback_jitter,
            warmup_samples: settings.warmup_samples,
        })
    };
    let voice_addresses = match &net {
        Some((relay, _, _)) => [relay.voice_address(0)?, relay.voice_address(1)?],
        None => [media_addr; 2],
    };
    let receiver_io = io(&heard, false, settings.extra_delay_ms * RATE / 1000, settings.callback_jitter);
    let (va, vb) = match &calls {
        None => (
            voice_start(&a.connection, io(&sent, true, 0, false), voice_addresses[0]).await?,
            voice_start(&b.connection, receiver_io, voice_addresses[1]).await?,
        ),
        Some((a_state, b_state)) => {
            let (va, a_binding) = encrypted_start(&a, a_state, io(&sent, true, 0, false), voice_addresses[0]).await?;
            let (vb, b_binding) = encrypted_start(&b, b_state, receiver_io, voice_addresses[1]).await?;
            mls::activate(va.keys().context("sender keys")?, &sender, &a_binding).await?;
            mls::activate(vb.keys().context("receiver keys")?, &receiver, &b_binding).await?;
            (va, vb)
        }
    };
    let connected_s = if settings.wait_connected {
        tokio::time::timeout(CONNECT_WAIT, async {
            loop {
                let links = [va.stats().link, vb.stats().link];
                anyhow::ensure!(!links.contains(&voice::LinkState::Lost), "voice lost while waiting to arm markers");
                if links.iter().all(|link| *link == voice::LinkState::Connected) {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("voice did not connect within 30 seconds")??;
        let at = epoch.elapsed().as_secs_f64();
        markers_ready.store(true, Ordering::Release);
        Some(at)
    } else {
        None
    };
    // The receiver's playout state every 100 ms, to see where a latency step sits.
    let mut timeline = String::from(
        "t_s,queued_samples,staged_samples,target_samples,shrink_samples,concealments,late_drops,trim_events,speaker_samples,callback_samples,headroom_samples,underruns\n",
    );
    let deadline = tokio::time::Instant::now() + run_for;
    while tokio::time::Instant::now() < deadline {
        let s = vb.stats();
        if let Some(slot) = s.slots.iter().find(|slot| slot.generation > 0) {
            timeline.push_str(&format!(
                "{:.3},{},{},{},{},{},{},{},{},{},{},{}\n",
                epoch.elapsed().as_secs_f64(),
                slot.queued_samples,
                slot.staged_samples,
                slot.target_samples,
                slot.shrink_samples,
                slot.concealments,
                slot.late_drops,
                slot.trim_events,
                s.speaker_samples,
                s.callback_samples,
                s.headroom_samples,
                s.underruns,
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Before any blocking write: recording ends here, within the playout record's budget.
    stop_devices(&controls).await?;
    std::fs::write(format!("{output}.timeline.csv"), timeline)?;
    let receiver_stats = vb.stats();
    let crypto = vb.keys().map(|keys| keys.counters());
    println!("sender={:?} receiver={receiver_stats:?}", va.stats());
    let mut voice_events = Vec::new();
    while let Ok(event) = b.events.try_recv() {
        match event {
            gwar_proto::Event::VoiceSlot { slot, client, .. } => voice_events.push(
                serde_json::json!({"type":"slot","slot":slot,"speaker_is_sender":client==Some(a.welcome.session)}),
            ),
            gwar_proto::Event::VoiceTalking { client, talking, .. } => voice_events.push(
                serde_json::json!({"type":"talking","speaker_is_sender":client==a.welcome.session,"talking":talking}),
            ),
            _ => {}
        }
    }
    drop((va, vb));
    let screen = match load {
        Some(mut load) => Some(load.report().await?),
        None => None,
    };
    let network = if let Some((relay, _, _)) = &mut net { Some(relay.finish().await?) } else { None };
    let sent = sent.lock().unwrap();
    let heard = heard.lock().unwrap();
    let onsets = detect_onsets(&heard.samples);
    anyhow::ensure!(sent.captures.len() == count, "only {} captures", sent.captures.len());
    let matching = match_markers(&sent.captures, &onsets, settings.minimum_ms, MATCH_WINDOW_MS);
    let mut csv = std::fs::File::create(output)?;
    writeln!(csv, "marker,identity,capture_s,playout_s,latency_ms,playout_sample")?;
    let mut latencies = Vec::new();
    for found in &matching.matched {
        let Marker { capture_s, identity } = sent.captures[found.marker];
        let onset = &onsets[found.onset];
        writeln!(
            csv,
            "{},{identity},{capture_s:.9},{:.9},{:.6},{}",
            found.marker, onset.playout_s, found.latency_ms, onset.sample
        )?;
        latencies.push(found.latency_ms);
    }
    let lost = &matching.lost;
    let ambiguous: Vec<_> =
        onsets.iter().enumerate().filter_map(|(i, onset)| onset.identity.is_none().then_some(i)).collect();
    let marker_count = count;
    let count = latencies.len();
    latencies.sort_by(f64::total_cmp);
    let percentile = |q: f64| latencies.get(((q * count as f64).ceil() as usize).saturating_sub(1)).copied();
    println!(
        "n={count} median={:.3} p95={:.3} p99={:.3} ms underflow_samples={} ambiguous_onsets={} rejected_onsets={}",
        percentile(0.5).unwrap_or(f64::NAN),
        percentile(0.95).unwrap_or(f64::NAN),
        percentile(0.99).unwrap_or(f64::NAN),
        heard.underflows,
        ambiguous.len(),
        matching.rejected.len()
    );
    let slots = receiver_stats
        .slots
        .iter()
        .enumerate()
        .map(|(slot, stats)| {
            serde_json::json!({
                "slot": slot,
                "target_samples": stats.target_samples,
                "queued_packets": stats.queued_packets,
                "queued_samples": stats.queued_samples,
                "staged_samples": stats.staged_samples,
                "concealments": stats.concealments,
                "late_drops": stats.late_drops,
                "overflow_drops": stats.overflow_drops,
                "shrink_samples": stats.shrink_samples,
                "trim_events": stats.trim_events,
                "concealments_after_trim": stats.concealments_after_trim,
            })
        })
        .collect::<Vec<_>>();
    let mut metadata = serde_json::json!({
        "network": network,
        "wait_connected": settings.wait_connected,
        "connected_s": connected_s,
        "voice_events": voice_events,
        "markers": marker_count,
        "n": count,
        "lost_markers": lost,
        "onsets": onsets.len(),
        "detected_onsets": onsets,
        "ambiguous_onsets": ambiguous,
        "rejected_onsets": matching.rejected,
        "unmatched_onsets": matching.unused,
        "marker_captures": sent.captures,
        "marker_identity_count": TONES.len(),
        "marker_frequencies_hz": TONES,
        "matching_window_ms": MATCH_WINDOW_MS,
        "minimum_latency_ms": settings.minimum_ms,
        "extra_delay_ms": settings.extra_delay_ms,
        "extra_delay_samples": settings.extra_delay_ms * RATE / 1000,
        "identity_window_samples": IDENTITY_WINDOW,
        "minimum_identity_confidence": MIN_IDENTITY_CONFIDENCE,
        "minimum_tone_energy_fraction": MIN_TONE_ENERGY_FRACTION,
        "median_ms": percentile(0.5),
        "p95_ms": percentile(0.95),
        "p99_ms": percentile(0.99),
        "receiver_underflow_samples": heard.underflows,
        "receiver_playout": {
            "speaker_samples": receiver_stats.speaker_samples,
            "callback_samples": receiver_stats.callback_samples,
            "callback_max_samples": receiver_stats.callback_max_samples,
            "headroom_samples": receiver_stats.headroom_samples,
            "underruns": receiver_stats.underruns,
            "underrun_samples": receiver_stats.underrun_samples,
            "slots": slots,
        },
        "receiver_frames_sent": receiver_stats.frames_sent,
        "receiver_frames_received": receiver_stats.frames_received,
        "url": url,
        "server": server_addr.to_string(),
        "sample_rate": RATE,
        "callback_samples": BLOCK,
        "receiver_callback_pattern": if settings.callback_jitter { vec![480, 960, 240] } else { vec![BLOCK] },
        "rms_window_samples": RMS_WINDOW,
        "rms_on_threshold": 0.18,
        "rms_off_threshold": 0.11,
        "marker_samples": MARKER,
        "marker_interval_samples": PERIOD,
        "warmup_samples": settings.warmup_samples,
    });
    metadata["receiver_device"] = serde_json::to_value(&heard.device)?;
    metadata["receiver_record"] = heard.samples.report();
    metadata["sender_device"] = serde_json::to_value(&sent.device)?;
    metadata["sframe"] = serde_json::json!(settings.sframe);
    metadata["screen"] = serde_json::json!(screen);
    metadata["receiver_sframe"] = serde_json::to_value(crypto)?;
    std::fs::write(format!("{output}.json"), serde_json::to_string_pretty(&metadata)?)?;
    anyhow::ensure!(
        heard.samples.dropped == 0,
        "the playout record overflowed by {} samples (capacity {}); see {output}.json",
        heard.samples.dropped,
        heard.samples.capacity()
    );
    anyhow::ensure!(
        matching.rejected.is_empty(),
        "rejected {} onsets below {:.3} ms; see {output}.json",
        matching.rejected.len(),
        settings.minimum_ms
    );
    anyhow::ensure!(
        lost.len() <= marker_count / 20,
        "lost {} of {marker_count} markers ({:.1}%); at least 95% must match; see {output}.json",
        lost.len(),
        lost.len() as f64 * 100.0 / marker_count as f64
    );
    anyhow::ensure!(
        voice_events.iter().any(|e| e["type"] == "slot" && e["speaker_is_sender"] == true),
        "no sender slot; see {output}.json"
    );
    anyhow::ensure!(
        voice_events.iter().any(|e| e["type"] == "talking" && e["speaker_is_sender"] == true && e["talking"] == true),
        "no sender talking indicator; see {output}.json"
    );
    Ok(())
}

/// Counts the allocations made on one thread while it runs a closure.
#[cfg(test)]
mod counting {
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        cell::Cell,
    };

    thread_local! {
        static COUNTING: Cell<bool> = const { Cell::new(false) };
        static COUNT: Cell<u64> = const { Cell::new(0) };
    }

    struct Counting;
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            note();
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            note();
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            note();
            unsafe { System.realloc(ptr, layout, new_size) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: Counting = Counting;

    fn note() {
        let _ = COUNTING.try_with(|on| {
            if on.get() {
                COUNT.with(|count| count.set(count.get() + 1));
            }
        });
    }

    /// Allocations and reallocations made on this thread while `run` runs.
    pub fn allocations(run: impl FnOnce()) -> u64 {
        COUNT.with(|count| count.set(0));
        COUNTING.with(|on| on.set(true));
        run();
        COUNTING.with(|on| on.set(false));
        COUNT.with(Cell::get)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic(count: usize, latency_ms: f64) -> (Vec<Marker>, Vec<Onset>) {
        let markers: Vec<_> = (0..count)
            .map(|i| Marker { capture_s: 2.0 + i as f64 * PERIOD as f64 / RATE as f64, identity: i % TONES.len() })
            .collect();
        let onsets = markers
            .iter()
            .enumerate()
            .map(|(i, marker)| Onset {
                playout_s: marker.capture_s + latency_ms / 1000.0,
                sample: i,
                identity: Some(marker.identity),
                identity_confidence: 1.0,
                tone_energy_fraction: 1.0,
            })
            .collect();
        (markers, onsets)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn voice_relay_uses_the_same_role_seeds_with_screen_load() {
        async fn paired(with_screen: bool) -> Vec<netem::FlowSeed> {
            let any = "127.0.0.1:0".parse().unwrap();
            let mut relay = netem::Relay::bind().await.unwrap();
            let advertise = relay.address().unwrap();
            let server = gwar_server::start(gwar_server::Config {
                database: None,
                http_bind: any,
                media_bind: any,
                media_advertise: advertise,
                max_clients: 32,
                encrypted_server_chat: false,
                server_password: None,
                web_root: None,
                ice_servers: vec![],
                teamspeak: None,
                public_url: None,
                data_dir: None,
                public_ip: None,
                insecure_any_host: true,
                public_endpoints: Vec::new(),
                client_ip_header: None,
                upload_limit: 10 * 1024 * 1024,
                upload_quota: 0,
                storage_limit: 0,
                files_dir: None,
                web_origins: Vec::new(),
                tls: None,
                redirect_http: None,
                connect_url: None,
            })
            .await
            .expect("start local latency server");

            let mut tasks = ServerTasks { tasks: server.tasks, data_dir: None };
            let url = format!("ws://{}/ws", server.http);
            let a = connect(options(&url, "voice sender")).await.unwrap();
            let b = connect(options(&url, "voice receiver")).await.unwrap();
            for client in [&a, &b] {
                client.connection.join(client.welcome.server.default_channel, None).await.unwrap();
            }
            let epoch = Instant::now();
            relay
                .start(server.media_bind, netem::Spec::parse("seed=17;up:jitter=1ms;down:jitter=1ms").unwrap(), epoch)
                .unwrap();
            let mut load = if with_screen {
                Some(
                    screen::Load::start(
                        &url,
                        screen::Spec::parse("shares=1,viewers=1,bitrate=100kbit,fps=5,keyframe=1000,via=all").unwrap(),
                        a.welcome.server.default_channel,
                        advertise,
                        Some(advertise),
                        server.media_bind,
                    )
                    .await
                    .unwrap(),
                )
            } else {
                None
            };
            if let Some(load) = &mut load {
                tokio::time::timeout(Duration::from_secs(3), async {
                    while load.report().await.unwrap()["received_bytes"].as_u64().unwrap() == 0 {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("screen transport carries media before voice starts");
            }
            let io = || {
                Box::new(FakeIo {
                    markers_ready: Arc::new(AtomicBool::new(false)),
                    data: Arc::new(Mutex::new(Measurements::receiver(Record::new(RATE * 20, (0.0, 0.0))))),
                    tone: false,
                    epoch,
                    start: Instant::now() + Duration::from_millis(50),
                    control: DeviceControl::default(),
                    count: 1,
                    delay_samples: 0,
                    callback_jitter: false,
                    warmup_samples: 0,
                })
            };
            let va = voice_start(&a.connection, io(), relay.voice_address(0).unwrap()).await.unwrap();
            let vb = voice_start(&b.connection, io(), relay.voice_address(1).unwrap()).await.unwrap();
            tokio::time::timeout(Duration::from_secs(10), async {
                while [va.stats().link, vb.stats().link].iter().any(|s| *s != voice::LinkState::Connected) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("actual voice transports connect through the relay");
            drop(va);
            drop(vb);
            drop(load);
            let report = relay.finish().await.unwrap();
            assert_eq!(report.flows, if with_screen { 4 } else { 2 });
            if with_screen {
                assert!(report.flow_seeds[..2].iter().all(|f| f.voice_role.is_none()), "screen flows arrived first");
            }
            let mut seeds: Vec<_> = report.flow_seeds.into_iter().filter(|f| f.voice_role.is_some()).collect();
            seeds.sort_by_key(|f| f.voice_role);
            assert_eq!(seeds.iter().map(|f| f.voice_role).collect::<Vec<_>>(), vec![Some(0), Some(1)]);
            tasks.shutdown().await;
            seeds
        }
        let baseline = paired(false).await;
        let loaded = paired(true).await;
        assert_eq!(baseline, loaded);
        assert_ne!(baseline[0].up, baseline[1].up);
    }

    #[test]
    fn warmup_defaults_to_two_seconds_and_rejects_bad_values() {
        assert_eq!(warmup_samples(None).unwrap(), 96_000);
        assert_eq!(warmup_samples(Some("0")).unwrap(), 0);
        assert_eq!(warmup_samples(Some("15000")).unwrap(), 720_000);
        assert_eq!(warmup_samples(Some("30000")).unwrap(), 1_440_000);
        for bad in ["", "-1", "1.5", "15s", "30001", "144115188075855872"] {
            assert!(warmup_samples(Some(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn latencies_beyond_one_period_do_not_alias() {
        for latency in [350.0, 600.0, 1999.0] {
            let (markers, onsets) = synthetic(100, latency);
            let result = match_markers(&markers, &onsets, 5.0, MATCH_WINDOW_MS);
            assert_eq!(result.matched.len(), 100);
            assert!(result.lost.is_empty());
            for (i, found) in result.matched.iter().enumerate() {
                assert_eq!((found.marker, found.onset), (i, i));
                assert!((found.latency_ms - latency).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn lost_marker_does_not_shift_later_matches() {
        let (markers, mut onsets) = synthetic(100, 600.0);
        onsets.remove(3);
        let result = match_markers(&markers, &onsets, 5.0, MATCH_WINDOW_MS);
        assert_eq!(result.lost, [3]);
        assert_eq!(result.matched.len(), 99);
        for found in result.matched {
            assert_eq!(onsets[found.onset].sample, found.marker);
            assert!((found.latency_ms - 600.0).abs() < 1e-9);
        }
    }

    #[test]
    fn duplicate_and_ambiguous_onsets_are_not_reused() {
        let (markers, mut onsets) = synthetic(100, 350.0);
        onsets[3].identity = None;
        let duplicate = Onset { sample: 100, ..onsets[4] };
        onsets.insert(5, duplicate);
        let result = match_markers(&markers, &onsets, 5.0, MATCH_WINDOW_MS);
        assert_eq!(result.lost, [3]);
        assert_eq!(result.matched.len(), 99);
        assert_eq!(result.unused, [3, 5]);
        let used: std::collections::HashSet<_> = result.matched.iter().map(|found| found.onset).collect();
        assert_eq!(used.len(), result.matched.len());
        for found in result.matched {
            assert_eq!(onsets[found.onset].sample, found.marker);
        }
    }

    #[test]
    fn below_floor_is_rejected_and_consumed() {
        let (mut markers, onsets) = synthetic(1, 1.0);
        markers.push(markers[0]);
        let result = match_markers(&markers, &onsets, 5.0, MATCH_WINDOW_MS);
        assert!(result.matched.is_empty());
        assert_eq!(result.lost, [0, 1]);
        assert_eq!(result.rejected.len(), 1);
        assert_eq!(result.rejected[0].reason, "below_minimum_latency");
        assert!(result.unused.is_empty());
    }

    #[test]
    fn matching_window_is_exclusive() {
        let (markers, onsets) = synthetic(1, MATCH_WINDOW_MS);
        let result = match_markers(&markers, &onsets, 5.0, MATCH_WINDOW_MS);
        assert!(result.matched.is_empty());
        assert_eq!(result.lost, [0]);
    }

    #[test]
    fn mixed_tones_and_carrier_have_no_identity() {
        for frequencies in [[TONES[2], TONES[3]], [400.0, 400.0]] {
            let samples: Vec<_> = (0..IDENTITY_WINDOW)
                .map(|i| {
                    let time = i as f64 / RATE as f64;
                    let value = frequencies.iter().map(|f| (time * f * std::f64::consts::TAU).sin()).sum::<f64>();
                    (time, value as f32 * 0.3)
                })
                .collect();
            assert_eq!(identify_tone(&samples).0, None);
        }
    }

    #[test]
    fn playout_record_holds_the_longest_run_without_growing() {
        let run_for = Duration::from_secs(43); // 15 s warmup, 100 markers
        for (wait_connected, seconds) in [(false, 48), (true, 78)] {
            let record = playout_record(run_for, wait_connected);
            assert!(record.is_empty());
            assert!(record.capacity() >= seconds * RATE, "{}", record.capacity());
        }
    }

    #[test]
    fn a_full_record_counts_overflow_and_never_grows() {
        let mut record = Record::new(3, 0u8);
        let capacity = record.capacity();
        let kept: Vec<_> = (0..5).map(|i| record.push(i)).collect();
        assert_eq!(&kept[..capacity], vec![true; capacity]);
        assert!(kept[capacity..].iter().all(|kept| !kept));
        assert_eq!((record.capacity(), record.len(), record.dropped), (capacity, capacity, (5 - capacity) as u64));
        assert_eq!(
            record.report(),
            serde_json::json!({"capacity_samples": capacity, "recorded_samples": capacity,
                "overflow_samples": 5 - capacity})
        );
    }

    #[test]
    fn device_timing_keeps_the_first_slow_callbacks_and_counts_all() {
        let mut timing = DeviceTiming::new();
        let ms = Duration::from_millis;
        timing.record(1.0, ms(1), Duration::from_micros(30), 0);
        assert!(timing.slow.is_empty());
        for i in 0..40 {
            timing.record(2.0 + f64::from(i), ms(6), ms(3), 1 << 20);
        }
        timing.record(50.0, ms(1), ms(17), 1 << 21);
        assert_eq!((timing.callbacks, timing.late_over_5ms, timing.busy_over_2ms), (42, 40, 41));
        assert_eq!((timing.slow.len(), timing.slow_dropped), (MAX_SLOW_CALLBACKS, 41 - MAX_SLOW_CALLBACKS as u64));
        assert_eq!(timing.slow[0].recorded_samples, 1 << 20);
        assert!((timing.max_busy_ms - 17.0).abs() < 1e-9 && (timing.max_lateness_ms - 6.0).abs() < 1e-9);
    }

    /// A fake device's callbacks with its own measurements, started now, markers armed.
    fn device(
        tone: bool,
        markers: usize,
        samples: usize,
        mic_ring: usize,
    ) -> (Callbacks, rtrb::Consumer<CaptureSample>, rtrb::Producer<f32>, Arc<Mutex<Measurements>>, DeviceControl) {
        let now = Instant::now();
        let data = Arc::new(Mutex::new(Measurements::new(markers, Record::new(samples, (0.0, 0.0)))));
        let control = DeviceControl::default();
        let io = Box::new(FakeIo {
            markers_ready: Arc::new(AtomicBool::new(true)),
            data: data.clone(),
            tone,
            epoch: now,
            start: now,
            control: control.clone(),
            count: markers,
            delay_samples: 0,
            callback_jitter: false,
            warmup_samples: 0,
        });
        let (mic_tx, mic) = prefaulted_ring(mic_ring, CaptureSample { captured_at: now, position: 0, value: 0.0 });
        let (speaker, speaker_rx) = prefaulted_ring(RATE / 2, 0.0);
        (Callbacks::new(io, mic_tx, speaker_rx, Arc::new(OutputTiming::default())), mic, speaker, data, control)
    }

    #[test]
    fn callbacks_never_allocate_even_when_slow_or_full() {
        // The receiver's record holds ten callbacks; forty run, every one late.
        let (mut receiver, mut mic, mut speaker, heard, _) = device(false, 0, 10 * BLOCK, RATE);
        let (mut sender, mut sender_mic, _, sent, _) = device(true, 2, 0, RATE);
        let capacity = heard.lock().unwrap().samples.capacity();
        assert_eq!(counting::allocations(|| drop(std::hint::black_box(Vec::<u8>::with_capacity(8)))), 1);
        let allocations = counting::allocations(|| {
            for _ in 0..40 {
                for _ in 0..BLOCK / 2 {
                    let _ = speaker.push(0.25);
                }
                receiver.run(receiver.due() + Duration::from_millis(6)).unwrap();
                sender.run(sender.due()).unwrap();
                while mic.pop().is_ok() {}
                while sender_mic.pop().is_ok() {}
            }
        });
        assert_eq!(allocations, 0);
        let heard = heard.lock().unwrap();
        assert_eq!(heard.samples.capacity(), capacity, "the record never grows");
        assert_eq!(heard.samples.dropped, (40 * BLOCK - capacity) as u64);
        assert_eq!(heard.underflows, 40 * BLOCK / 2);
        assert_eq!((receiver.stats.callbacks, receiver.stats.late_over_5ms), (40, 40));
        assert_eq!(receiver.stats.slow_dropped, 40 - MAX_SLOW_CALLBACKS as u64);
        assert_eq!(sent.lock().unwrap().captures.len(), 2); // markers at 0 and 12 000 samples
    }

    /// Runs `callbacks` on a thread of its own, owned by a `Guard` as an opened device's is.
    fn spawn_device(callbacks: Callbacks, control: &DeviceControl) -> Guard {
        Guard { control: control.clone(), thread: Some(thread::spawn(move || callbacks.run_until_stopped())) }
    }

    #[tokio::test]
    async fn a_device_stuck_forever_cannot_hang_the_run() {
        // Its measurement lock is taken, never to be released, before its thread exists: the
        // first callback blocks before it touches a ring, whatever the scheduling.
        let (stuck, _stuck_mic, _stuck_speaker, stuck_data, stuck_control) = device(false, 0, RATE, RATE);
        std::mem::forget(stuck_data.lock().unwrap());
        let stuck = spawn_device(stuck, &stuck_control);
        // The healthy device's mic ring is drained until it finishes, so only a stop can end it.
        let (healthy, mut healthy_mic, _healthy_speaker, healthy_data, healthy_control) = device(false, 0, RATE, RATE);
        let drained = healthy_control.clone();
        let drainer = thread::spawn(move || {
            while !drained.finished() {
                while healthy_mic.pop().is_ok() {}
                thread::sleep(Duration::from_millis(1));
            }
        });
        let healthy = spawn_device(healthy, &healthy_control);
        let started = Instant::now();
        let error = stop_devices(&[stuck_control.clone(), healthy_control.clone()]).await.unwrap_err();
        assert!(error.to_string().contains("did not stop"), "{error}");
        // Both were signaled before either was awaited: the healthy one stopped normally.
        assert!(healthy_control.finished() && !stuck_control.finished());
        assert_eq!(healthy_data.lock().unwrap().device.termination, Some("stopped"));
        drainer.join().unwrap();
        // Dropping the devices, as an error return does, waits for the stuck thread only so long.
        let (dropped, done) = std::sync::mpsc::channel();
        thread::spawn(move || {
            drop((stuck, healthy));
            let _ = dropped.send(());
        });
        assert!(done.recv_timeout(DROP_WAIT + Duration::from_secs(1)).is_ok(), "dropping a stuck device hung");
        assert!(started.elapsed() < STOP_WAIT + DROP_WAIT + Duration::from_secs(1), "{:?}", started.elapsed());
    }

    #[test]
    fn an_early_exit_still_stores_the_measurements() {
        // A mic ring that nobody drains is full during the third callback.
        let (receiver, _mic, _speaker, heard, control) = device(false, 0, RATE, 2 * BLOCK + 40);
        receiver.run_until_stopped();
        assert!(control.finished.load(Ordering::Acquire));
        let heard = heard.lock().unwrap();
        assert_eq!(heard.device.termination, Some("mic_ring_full"));
        assert_eq!(heard.device.callbacks, 2);
        assert_eq!(heard.samples.len(), 2 * BLOCK + 40);
    }

    #[test]
    fn delay_line_holds_the_audio_samples() {
        let mut delay = DelayLine::new(3);
        let output: Vec<_> = [1.0, 2.0, 3.0, 0.0, 0.0, 0.0].into_iter().map(|value| delay.process(value)).collect();
        assert_eq!(output, [0.0, 0.0, 0.0, 1.0, 2.0, 3.0]);
        assert_eq!(DelayLine::new(0).process(0.5), 0.5);
    }
}
