//! Native voice engine: microphone → gate → Opus → WebRTC, and WebRTC →
//! per-slot jitter buffer → Opus decode → mix → speakers.
//!
//! Threads: a tokio task drives the str0m WebRTC session; one best-effort realtime
//! "pipeline" thread does all audio DSP and coding; device callbacks only
//! move samples through lock-free rings (see [`io`]).

mod denoise;
pub mod io;
pub mod jitter;
pub mod keys;
pub mod permission;
mod playout;
mod priority;
mod rtc;
pub mod sframe;

use std::{
    net::{IpAddr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use audiopus::{Application, Bitrate, Channels, SampleRate, coder::Encoder};
use gwar_proto::AUDIO_SLOTS;
use tokio::sync::mpsc;

pub use self::denoise::NoiseSuppression;
use self::{
    denoise::Denoiser,
    io::{AudioIo, AudioPorts, CaptureSample, OutputTiming},
    jitter::Clock,
    playout::{Slot, SpeakerBuffer},
};
use crate::Connection;

/// Native WebRTC packet duration. Set to 960 to restore 20 ms capture.
pub const FRAME: usize = 480; // 10 ms at 48 kHz
const TS_FRAME: usize = 960; // Direct TeamSpeak retains its 20 ms packet clock.

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputMode {
    /// Transmit while the input level exceeds `threshold_db` (dBFS).
    VoiceActivity { threshold_db: f32 },
    /// Transmit while [`VoiceHandle::set_ptt`] is held.
    PushToTalk,
}

/// Lock-free controls shared between the UI and the pipeline thread.
pub struct Controls {
    muted: AtomicBool,
    deafened: AtomicBool,
    ptt: AtomicBool,
    vad_threshold: AtomicU32,
    push_to_talk: AtomicBool,
    /// [`NoiseSuppression`], as `to_u8`.
    noise_suppression: AtomicU8,
    master: AtomicU32,
    slot_gain: [AtomicU32; AUDIO_SLOTS],
    /// Latest input level in dBFS (for the meter).
    input_level: AtomicU32,
    transmitting: AtomicBool,
    stop: AtomicBool,
}

fn f(v: &AtomicU32) -> f32 {
    f32::from_bits(v.load(Ordering::Relaxed))
}

fn set_f(v: &AtomicU32, x: f32) {
    v.store(x.to_bits(), Ordering::Relaxed);
}

impl Default for Controls {
    fn default() -> Self {
        Self {
            muted: AtomicBool::new(false),
            deafened: AtomicBool::new(false),
            ptt: AtomicBool::new(false),
            vad_threshold: AtomicU32::new((-50.0f32).to_bits()),
            push_to_talk: AtomicBool::new(false),
            noise_suppression: AtomicU8::new(NoiseSuppression::default().to_u8()),
            master: AtomicU32::new(1.0f32.to_bits()),
            slot_gain: std::array::from_fn(|_| AtomicU32::new(1.0f32.to_bits())),
            input_level: AtomicU32::new((-100.0f32).to_bits()),
            transmitting: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        }
    }
}

/// State of the WebRTC link to the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// Offer/answer done, ICE not connected yet.
    Connecting,
    Connected,
    /// The transport died (or the server closed it); start a new session.
    Lost,
}

/// Counters written by the transport/pipeline, read through [`VoiceHandle::stats`].
#[derive(Default)]
pub(crate) struct Counters {
    frames_sent: AtomicU64,
    frames_received: AtomicU64,
    unexpected_duration: AtomicU64,
    oversize_dropped: AtomicU64,
    malformed_dropped: AtomicU64,
    capture_position_dropped: AtomicU64,
    capture_stale_samples: AtomicU64,
    capture_stale_frames: AtomicU64,
    outgoing_full_dropped: AtomicU64,
    /// 0 connecting, 1 connected, 2 lost.
    link: AtomicU8,
    slots: [SlotCounters; AUDIO_SLOTS],
    speaker_samples: AtomicU64,
}

impl Counters {
    pub(crate) fn set_link(&self, state: LinkState) {
        let v = match state {
            LinkState::Connecting => 0,
            LinkState::Connected => 1,
            LinkState::Lost => 2,
        };
        // `Lost` is final.
        let _ = self.link.try_update(Ordering::Relaxed, Ordering::Relaxed, |old| (old != 2).then_some(v));
    }
    pub(crate) fn frame_received(&self) {
        self.frames_received.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct SlotCounters {
    generation: AtomicU64,
    packets: AtomicU64,
    samples: AtomicU64,
    concealments: AtomicU64,
    late_drops: AtomicU64,
    target_samples: AtomicU64,
    staged_samples: AtomicU64,
    shrink_samples: AtomicU64,
    trim_events: AtomicU64,
    concealments_after_trim: AtomicU64,
}

/// Per-slot receive queue snapshot; sample counts use the 48 kHz clock.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SlotStats {
    pub generation: u64,
    pub queued_packets: u64,
    pub queued_samples: u64,
    pub concealments: u64,
    pub late_drops: u64,
    pub target_samples: u64,
    pub staged_samples: u64,
    pub shrink_samples: u64,
    pub trim_events: u64,
    /// Concealments within one second of a trim, including stall recovery.
    pub concealments_after_trim: u64,
}

/// Snapshot of transport counters, for diagnostics and the connection indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceStats {
    /// Opus frames handed to the transport.
    pub frames_sent: u64,
    /// Opus frames received from the server (all slots).
    pub frames_received: u64,
    pub unexpected_duration: u64,
    pub oversize_dropped: u64,
    pub malformed_dropped: u64,
    pub capture_position_dropped: u64,
    /// PCM discarded by age, including partial frames invalidated by a stall.
    pub capture_stale_samples: u64,
    /// Encoded frames discarded by age before transport admission.
    pub capture_stale_frames: u64,
    pub capture_overflow_samples: u64,
    pub outgoing_full_dropped: u64,
    pub link: LinkState,
    pub slots: [SlotStats; AUDIO_SLOTS],
    pub speaker_samples: u64,
    pub callback_samples: usize,
    /// Slowly decaying callback maximum used to size the speaker queue.
    pub callback_max_samples: usize,
    /// Scheduling headroom beyond one held callback, in 48 kHz samples.
    pub headroom_samples: usize,
    /// Output callbacks missing samples while a receive stream was playing.
    pub underruns: u64,
    pub underrun_samples: u64,
}

/// Handle owned by the app; dropping it stops the engine.
pub struct VoiceHandle {
    controls: Arc<Controls>,
    counters: Arc<Counters>,
    keys: Option<keys::KeyChannel>,
    gate: Option<keys::RunGate>,
    crypto_events: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<sframe::CryptoStatus>>>,
    _ports_guard: Box<dyn Send>,
    output: Arc<OutputTiming>,
}

impl VoiceHandle {
    /// The key channel of an encrypted run; `None` in plaintext and TeamSpeak.
    pub fn keys(&self) -> Option<&keys::KeyChannel> {
        self.keys.as_ref()
    }
    /// `voice://crypto` statuses of an encrypted run; taken once.
    pub fn take_crypto_events(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<sframe::CryptoStatus>> {
        self.crypto_events.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
    pub fn set_muted(&self, muted: bool) {
        self.controls.muted.store(muted, Ordering::Relaxed);
    }
    pub fn set_deafened(&self, deafened: bool) {
        self.controls.deafened.store(deafened, Ordering::Relaxed);
    }
    pub fn set_ptt(&self, down: bool) {
        self.controls.ptt.store(down, Ordering::Relaxed);
    }
    pub fn set_input_mode(&self, mode: InputMode) {
        match mode {
            InputMode::VoiceActivity { threshold_db } => {
                set_f(&self.controls.vad_threshold, threshold_db);
                self.controls.push_to_talk.store(false, Ordering::Relaxed);
            }
            InputMode::PushToTalk => self.controls.push_to_talk.store(true, Ordering::Relaxed),
        }
    }
    /// Per-slot volume, 0.0–2.0. The app maps slots to users via `voice.slot`.
    pub fn set_slot_gain(&self, slot: usize, gain: f32) {
        if let Some(g) = self.controls.slot_gain.get(slot) {
            set_f(g, gain.clamp(0.0, 2.0));
        }
    }
    /// Background-noise suppression of the microphone (see [`NoiseSuppression`]).
    pub fn set_noise_suppression(&self, mode: NoiseSuppression) {
        self.controls.noise_suppression.store(mode.to_u8(), Ordering::Relaxed);
    }
    pub fn set_master_gain(&self, gain: f32) {
        set_f(&self.controls.master, gain.clamp(0.0, 2.0));
    }
    pub fn input_level_db(&self) -> f32 {
        f(&self.controls.input_level)
    }
    pub fn transmitting(&self) -> bool {
        self.controls.transmitting.load(Ordering::Relaxed)
    }
    pub fn stats(&self) -> VoiceStats {
        let c = &self.counters;
        VoiceStats {
            frames_sent: c.frames_sent.load(Ordering::Relaxed),
            frames_received: c.frames_received.load(Ordering::Relaxed),
            unexpected_duration: c.unexpected_duration.load(Ordering::Relaxed),
            oversize_dropped: c.oversize_dropped.load(Ordering::Relaxed),
            malformed_dropped: c.malformed_dropped.load(Ordering::Relaxed),
            capture_position_dropped: c.capture_position_dropped.load(Ordering::Relaxed),
            capture_stale_samples: c.capture_stale_samples.load(Ordering::Relaxed),
            capture_stale_frames: c.capture_stale_frames.load(Ordering::Relaxed),
            capture_overflow_samples: self.output.capture_overflow_samples.load(Ordering::Relaxed),
            outgoing_full_dropped: c.outgoing_full_dropped.load(Ordering::Relaxed),
            slots: std::array::from_fn(|i| {
                let s = &c.slots[i];
                SlotStats {
                    generation: s.generation.load(Ordering::Relaxed),
                    queued_packets: s.packets.load(Ordering::Relaxed),
                    queued_samples: s.samples.load(Ordering::Relaxed),
                    concealments: s.concealments.load(Ordering::Relaxed),
                    late_drops: s.late_drops.load(Ordering::Relaxed),
                    target_samples: s.target_samples.load(Ordering::Relaxed),
                    staged_samples: s.staged_samples.load(Ordering::Relaxed),
                    shrink_samples: s.shrink_samples.load(Ordering::Relaxed),
                    trim_events: s.trim_events.load(Ordering::Relaxed),
                    concealments_after_trim: s.concealments_after_trim.load(Ordering::Relaxed),
                }
            }),
            speaker_samples: c.speaker_samples.load(Ordering::Relaxed),
            callback_samples: self.output.callback_samples.load(Ordering::Relaxed),
            callback_max_samples: self.output.callback_max_samples.load(Ordering::Relaxed),
            headroom_samples: self.output.headroom_samples.load(Ordering::Relaxed),
            underruns: self.output.underruns.load(Ordering::Relaxed),
            underrun_samples: self.output.underrun_samples.load(Ordering::Relaxed),
            link: match c.link.load(Ordering::Relaxed) {
                0 => LinkState::Connecting,
                1 => LinkState::Connected,
                _ => LinkState::Lost,
            },
        }
    }
}

impl Drop for VoiceHandle {
    fn drop(&mut self) {
        // Retire the RTC run first: after this no control applies and no packet is sent or played.
        if let Some(gate) = &self.gate {
            gate.close();
        }
        self.controls.stop.store(true, Ordering::Relaxed);
        self.output.wake();
    }
}

/// An encoded microphone frame on its way to the network.
pub(crate) struct Outgoing {
    pub captured_at: Instant,
    pub capture_position: u64,
    pub opus: Vec<u8>,
    /// RFC 6464 level, negative dBov.
    pub level: i8,
    pub speech: bool,
}

impl Outgoing {
    fn fresh_at(&self, now: Instant, counters: &Counters) -> bool {
        if now.saturating_duration_since(self.captured_at) > io::MAX_CAPTURE_AGE {
            counters.capture_stale_frames.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Generation {
    Rtp(gwar_proto::rtp::SlotGeneration),
    Sequence(u64),
}

impl Generation {
    fn is_newer_than(self, old: Self) -> bool {
        match (self, old) {
            (Self::Rtp(new), Self::Rtp(old)) => new.is_newer_than(old),
            (Self::Sequence(new), Self::Sequence(old)) => new > old,
            _ => true,
        }
    }
}

/// A received frame for one slot.
pub(crate) struct Incoming {
    pub slot: usize,
    pub rtp_time: u64,
    /// Missing native metadata leaves the current ownership unchanged.
    pub generation: Option<Generation>,
    pub arrived: Instant,
    pub clock: Clock,
    pub opus: Vec<u8>,
}

/// Negotiates voice with the server and starts audio.
///
/// `local_ip` is the address the server should send media to; `None` uses the
/// interface that routes to the server.
pub async fn start(connection: &Connection, io: Box<dyn AudioIo>, server: SocketAddr) -> Result<VoiceHandle> {
    let connection = connection.clone();
    start_with(io, server.ip(), move |offer| async move { Ok(connection.voice_offer(offer).await?) }).await
}

/// Like [`start`], but the SDP offer/answer exchange goes through `signal`
/// (e.g. a WebSocket owned by the UI layer). `server` is the server's IP,
/// used to pick the local interface for media.
pub async fn start_with<F, Fut>(io: Box<dyn AudioIo>, server: IpAddr, signal: F) -> Result<VoiceHandle>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    start_session(io, server, signal, None).await
}

/// Like [`start_with`], for an encrypted call: every microphone packet is
/// SFrame-encrypted right before its RTP write and every received payload must
/// authenticate before playout. The run starts closed; keys arrive through
/// [`VoiceHandle::keys`] from the MLS controller.
pub async fn start_encrypted<F, Fut>(
    io: Box<dyn AudioIo>,
    server: IpAddr,
    signal: F,
    crypto: keys::EncryptedStart,
) -> Result<VoiceHandle>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    start_session(io, server, signal, Some(crypto)).await
}

async fn start_session<F, Fut>(
    io: Box<dyn AudioIo>,
    server: IpAddr,
    signal: F,
    crypto: Option<keys::EncryptedStart>,
) -> Result<VoiceHandle>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    let local_ip = route_ip(server).context("no route to server")?;
    let ports = tokio::task::spawn_blocking(move || io.open()).await??;
    let (out_tx, out_rx) = mpsc::channel::<Outgoing>(64);
    let (in_tx, in_rx) = std::sync::mpsc::sync_channel::<Incoming>(512);
    let counters = Arc::new(Counters::default());
    let gate = keys::RunGate::new();
    let (crypto, keys, crypto_events) = match crypto {
        Some(start) => {
            let (crypto, keys, events) = keys::channel(start, FRAME as u32, gate.clone());
            (Some(crypto), Some(keys), Some(events))
        }
        None => (None, None, None),
    };
    rtc::start(signal, local_ip, out_rx, in_tx, counters.clone(), ports.output.clone(), crypto, gate.clone()).await?;
    let controls = Arc::new(Controls::default());
    let AudioPorts { mic, speaker, output, _guard } = ports;
    let timing = output.clone();
    let pipeline_controls = controls.clone();
    let pipeline_counters = counters.clone();
    thread::Builder::new().name("gwar-voice-pipeline".into()).spawn(move || {
        if let Err(e) = pipeline::<FRAME>(pipeline_controls, pipeline_counters, mic, speaker, timing, out_tx, in_rx) {
            tracing::warn!("voice pipeline stopped: {e:#}");
        }
    })?;
    Ok(VoiceHandle {
        controls,
        counters,
        output,
        keys,
        gate: Some(gate),
        crypto_events: std::sync::Mutex::new(crypto_events),
        _ports_guard: _guard,
    })
}

/// Voice on a TeamSpeak server (see [`crate::teamspeak`]): frames travel
/// over the TS connection instead of WebRTC; DSP and mixing are shared.
pub async fn start_teamspeak(
    io: Box<dyn AudioIo>,
    mut audio_in: mpsc::Receiver<crate::teamspeak::TsAudio>,
    audio_out: mpsc::Sender<crate::teamspeak::TsMicrophone>,
) -> Result<VoiceHandle> {
    let ports = tokio::task::spawn_blocking(move || io.open()).await??;
    let (out_tx, mut out_rx) = mpsc::channel::<Outgoing>(64);
    let (in_tx, in_rx) = std::sync::mpsc::sync_channel::<Incoming>(512);
    let counters = Arc::new(Counters::default());
    counters.set_link(LinkState::Connected);
    let link = counters.clone();
    let wake = ports.output.clone();
    tokio::spawn(async move {
        // TS ends a talk spurt with an empty frame; send one after a short silence.
        let mut talking = false;
        loop {
            tokio::select! {
                frame = audio_in.recv() => {
                    let Some(frame) = frame else { break };
                    link.frame_received();
                    let _ = in_tx.try_send(Incoming { slot: frame.slot, generation: Some(Generation::Sequence(frame.generation)), rtp_time: frame.rtp_time, opus: frame.opus, arrived: Instant::now(), clock: Clock::Sequence });
                    wake.wake();
                }
                outgoing = async {
                    if talking {
                        tokio::time::timeout(Duration::from_millis(80), out_rx.recv()).await
                    } else {
                        Ok(out_rx.recv().await)
                    }
                } => match outgoing {
                    Ok(Some(frame)) => {
                        if !frame.fresh_at(Instant::now(), &link) { continue; }
                        talking = true;
                        match audio_out.try_send(crate::teamspeak::TsMicrophone { captured_at: frame.captured_at, opus: frame.opus }) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(_)) => { link.outgoing_full_dropped.fetch_add(1, Ordering::Relaxed); }
                            Err(mpsc::error::TrySendError::Closed(_)) => break,
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {
                        talking = false;
                        // The end marker is never age-dropped: losing it leaves TS showing speech.
                        if audio_out.send(crate::teamspeak::TsMicrophone { captured_at: Instant::now(), opus: Vec::new() }).await.is_err() { break; }
                    }
                },
            }
        }
        link.set_link(LinkState::Lost);
    });
    let controls = Arc::new(Controls::default());
    let AudioPorts { mic, speaker, output, _guard } = ports;
    let timing = output.clone();
    let pipeline_controls = controls.clone();
    let pipeline_counters = counters.clone();
    thread::Builder::new().name("gwar-voice-pipeline".into()).spawn(move || {
        if let Err(e) = pipeline::<TS_FRAME>(pipeline_controls, pipeline_counters, mic, speaker, timing, out_tx, in_rx)
        {
            tracing::warn!("voice pipeline stopped: {e:#}");
        }
    })?;
    Ok(VoiceHandle {
        controls,
        counters,
        output,
        keys: None,
        gate: None,
        crypto_events: std::sync::Mutex::new(None),
        _ports_guard: _guard,
    })
}

/// The local address the OS uses to reach `server` (no packets are sent).
fn route_ip(server: IpAddr) -> Option<IpAddr> {
    let bind: SocketAddr = if server.is_ipv4() { "0.0.0.0:0".parse().ok()? } else { "[::]:0".parse().ok()? };
    let socket = std::net::UdpSocket::bind(bind).ok()?;
    socket.connect((server, 9)).ok()?;
    Some(socket.local_addr().ok()?.ip())
}

fn level_db(frame: &[f32]) -> f32 {
    let rms = (frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32).sqrt();
    20.0 * rms.max(1e-5).log10()
}

const HANGOVER: Duration = Duration::from_millis(300);
/// Packets per pipeline pass; eight slots of 10 ms audio deliver about eight per pass.
const RECEIVE_BATCH: usize = 256;

fn voice_encoder() -> Result<Encoder> {
    let mut encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)?;
    encoder.set_bitrate(Bitrate::BitsPerSecond(48_000))?;
    encoder.set_inband_fec(true)?;
    encoder.set_packet_loss_perc(5)?;
    Ok(encoder)
}

fn validate_encoded(packet: &[u8], expected: u32, counters: &Counters) -> bool {
    if packet.len() > 1000 {
        counters.oversize_dropped.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    let Some(samples) = opus_samples(packet) else {
        counters.malformed_dropped.fetch_add(1, Ordering::Relaxed);
        return false;
    };
    if samples != expected {
        counters.unexpected_duration.fetch_add(1, Ordering::Relaxed);
    }
    true
}

struct CaptureFrame {
    samples: Vec<f32>,
    filled: usize,
    start: u64,
    captured_at: Instant,
}

impl CaptureFrame {
    fn new(size: usize) -> Self {
        Self { samples: vec![0.0; size], filled: 0, start: 0, captured_at: Instant::now() }
    }

    fn push_at(&mut self, sample: CaptureSample, now: Instant, counters: &Counters) -> Option<u64> {
        // Age is residence in the ring, not the device callback period: a
        // stalled consumer finds old samples here and the position gap resets
        // any partial frame, while large callbacks still complete their frames.
        if now.saturating_duration_since(sample.captured_at) > io::MAX_CAPTURE_AGE {
            // A stale backlog also invalidates the partial frame it interrupts.
            counters.capture_stale_samples.fetch_add(1 + self.filled as u64, Ordering::Relaxed);
            self.filled = 0;
            return None;
        }
        if sample.position != self.start + self.filled as u64 {
            self.filled = 0;
        }
        if self.filled == 0 {
            self.start = sample.position;
        }
        // A frame becomes sendable with its last sample; later queues measure from there.
        self.captured_at = sample.captured_at;
        self.samples[self.filled] = sample.value;
        self.filled += 1;
        if self.filled == self.samples.len() {
            self.filled = 0;
            Some(self.start)
        } else {
            None
        }
    }
}

fn pipeline<const CAPTURE: usize>(
    controls: Arc<Controls>,
    counters: Arc<Counters>,
    mut mic: rtrb::Consumer<CaptureSample>,
    mut speaker: rtrb::Producer<f32>,
    output: Arc<OutputTiming>,
    out: mpsc::Sender<Outgoing>,
    incoming: std::sync::mpsc::Receiver<Incoming>,
) -> Result<()> {
    let _priority = priority::Priority::promote();
    let encoder = voice_encoder()?;
    output.attach();
    let mut slots = (0..AUDIO_SLOTS).map(|_| Slot::new()).collect::<Result<Vec<_>>>()?;
    let mut capture = CaptureFrame::new(CAPTURE);
    let mut denoiser = Denoiser::new();
    let mut next_position = None;
    let mut mix = vec![0f32; FRAME];
    let mut packet = vec![0u8; 1275];
    let mut last_voice = Instant::now() - HANGOVER;
    let mut speaker_buffer = SpeakerBuffer::new(Instant::now());

    while !controls.stop.load(Ordering::Relaxed) {
        // ---- capture
        // Take only what is queued now: a producer faster than real time (an
        // always-ready device) must not keep a promoted thread from playback.
        for _ in 0..mic.slots() {
            let Ok(sample) = mic.pop() else { break };
            let Some(capture_position) = capture.push_at(sample, Instant::now(), &counters) else { continue };
            // The meter and the gate measure the microphone as it is, so its sensitivity setting
            // means what it did (a strong suppressor also takes very quiet speech down). The
            // encoder gets the cleaned audio. Suppression runs on every frame, sent or not, so its
            // state follows the audio.
            let db = level_db(&capture.samples);
            let mode = NoiseSuppression::from_u8(controls.noise_suppression.load(Ordering::Relaxed));
            denoiser.process(&mut capture.samples, mode, next_position == Some(capture_position));
            next_position = Some(capture_position + CAPTURE as u64);
            let frame = &capture.samples;
            set_f(&controls.input_level, db);
            let blocked = controls.muted.load(Ordering::Relaxed) || controls.deafened.load(Ordering::Relaxed);
            let wants = if controls.push_to_talk.load(Ordering::Relaxed) {
                controls.ptt.load(Ordering::Relaxed)
            } else {
                db >= f(&controls.vad_threshold)
            };
            if wants && !blocked {
                last_voice = Instant::now();
            }
            let send = !blocked && last_voice.elapsed() < HANGOVER;
            controls.transmitting.store(send, Ordering::Relaxed);
            if send {
                let n = encoder.encode_float(frame, &mut packet[..])?;
                if !validate_encoded(&packet[..n], CAPTURE as u32, &counters) {
                    continue;
                }
                let level = (-db.clamp(-127.0, 0.0)).round() as i8;
                // Tell the server's speech gate the truth about voice activity.
                if out
                    .try_send(Outgoing {
                        captured_at: capture.captured_at,
                        capture_position,
                        opus: packet[..n].to_vec(),
                        level: -level,
                        speech: true,
                    })
                    .is_ok()
                {
                    counters.frames_sent.fetch_add(1, Ordering::Relaxed);
                } else {
                    counters.outgoing_full_dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        // ---- receive
        for item in incoming.try_iter().take(RECEIVE_BATCH) {
            if let Some(slot) = slots.get_mut(item.slot) {
                slot.receive(item)?;
            }
        }

        // Decode only as needed; each slot retains the rest of its packet.
        speaker_buffer.update(
            Instant::now(),
            output.callback_samples.load(Ordering::Relaxed),
            output.callback_peak.swap(0, Ordering::Relaxed),
            output.underruns.load(Ordering::Relaxed),
        );
        output.callback_max_samples.store(speaker_buffer.callback, Ordering::Relaxed);
        output.headroom_samples.store(speaker_buffer.headroom, Ordering::Relaxed);
        let target = speaker_buffer.target(speaker.buffer().capacity());
        let level = speaker.buffer().capacity() - speaker.slots();
        let mut needed = target.saturating_sub(level);
        let deafened = controls.deafened.load(Ordering::Relaxed);
        while needed > 0 {
            let take = needed.min(FRAME);
            mix[..take].fill(0.0);
            let now = Instant::now();
            for (index, slot) in slots.iter_mut().enumerate() {
                slot.mix_refill(&mut mix[..take], f(&controls.slot_gain[index]), now, needed)?;
            }
            let master = if deafened { 0.0 } else { f(&controls.master) };
            output.playing.store(slots.iter().any(Slot::playing), Ordering::Relaxed);
            for s in &mix[..take] {
                let _ = speaker.push((s * master).clamp(-1.0, 1.0));
            }
            needed -= take;
        }
        counters.speaker_samples.store((speaker.buffer().capacity() - speaker.slots()) as u64, Ordering::Relaxed);
        for (slot, stats) in slots.iter().zip(&counters.slots) {
            stats.generation.store(slot.generation(), Ordering::Relaxed);
            let buffer = &slot.buffer;
            let (packets, samples) = buffer.depth();
            stats.packets.store(packets as u64, Ordering::Relaxed);
            stats.samples.store(samples, Ordering::Relaxed);
            stats.concealments.store(buffer.concealments, Ordering::Relaxed);
            stats.late_drops.store(buffer.late_drops, Ordering::Relaxed);
            stats.target_samples.store(buffer.target_samples(), Ordering::Relaxed);
            stats.staged_samples.store(slot.staged_samples() as u64, Ordering::Relaxed);
            stats.shrink_samples.store(buffer.shrink_samples, Ordering::Relaxed);
            stats.trim_events.store(buffer.trim_events, Ordering::Relaxed);
            stats.concealments_after_trim.store(buffer.concealments_after_trim, Ordering::Relaxed);
        }
        // Capture, receive and output all unpark us. The timeout also handles device failure.
        thread::park_timeout(Duration::from_millis(20));
    }
    Ok(())
}

pub use gwar_proto::opus::packet_samples as opus_samples;

#[cfg(test)]
mod codec_tests {
    use super::*;
    use audiopus::{Bandwidth, MutSignals, coder::Decoder, packet::Packet};

    #[test]
    fn encoded_sender_bounds_count_unexpected_duration_without_dropping_it() {
        let counters = Counters::default();
        assert!(validate_encoded(&[0xf0, 1], 480, &counters));
        assert!(validate_encoded(&[0xf8, 1], 480, &counters));
        assert_eq!(counters.unexpected_duration.load(Ordering::Relaxed), 1);
        assert!(!validate_encoded(&[0xff, 0], 480, &counters));
        assert!(!validate_encoded(&vec![0xf0; 1001], 480, &counters));
        assert!(validate_encoded(&vec![0xf0; 1000], 480, &counters));
        for packet in [&[0xf2][..], &[0xf1, 1], &[0xf3, 0x82], &[0xf3, 0x41, 255]] {
            assert!(!validate_encoded(packet, 480, &counters));
        }
        for packet in [&[0xf1, 1, 2][..], &[0xf2, 1, 1, 2], &[0xf3, 2, 1, 2]] {
            assert!(validate_encoded(packet, 960, &counters));
        }
        assert_eq!(counters.malformed_dropped.load(Ordering::Relaxed), 5);
        assert_eq!(counters.oversize_dropped.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn stalled_capture_and_encoded_consumers_drop_old_audio_without_a_late_burst() {
        let now = Instant::now();
        for size in [FRAME, TS_FRAME] {
            let counters = Counters::default();
            let (mut tx, mut rx) = rtrb::RingBuffer::new(size * 4);
            let mut capture = CaptureFrame::new(size);
            // Retain a partial frame, then stall while the mic fills its ring.
            for position in 0..size / 2 {
                assert!(
                    capture
                        .push_at(
                            CaptureSample { position: position as u64, captured_at: now, value: 0.5 },
                            now,
                            &counters
                        )
                        .is_none()
                );
            }
            for position in size / 2..size * 3 {
                tx.push(CaptureSample { position: position as u64, captured_at: now, value: 0.5 }).unwrap();
            }
            let resumed = now + Duration::from_millis(200);
            for _ in 0..rx.slots() {
                assert!(capture.push_at(rx.pop().unwrap(), resumed, &counters).is_none());
            }
            // The ring backlog and the partial frame it interrupted.
            assert_eq!(counters.capture_stale_samples.load(Ordering::Relaxed), (size * 3) as u64);
            let mut positions = Vec::new();
            for position in size * 3..size * 4 {
                positions.extend(capture.push_at(
                    CaptureSample { position: position as u64, captured_at: resumed, value: 0.5 },
                    resumed,
                    &counters,
                ));
            }
            assert_eq!(positions, [(size * 3) as u64]);

            let (tx, mut rx) = mpsc::channel(4);
            for (position, captured_at) in [(0, now), (size, now), (size * 3, resumed)] {
                tx.try_send(Outgoing {
                    captured_at,
                    capture_position: position as u64,
                    opus: vec![1],
                    level: -10,
                    speech: true,
                })
                .unwrap();
            }
            let mut sent = Vec::new();
            while let Ok(frame) = rx.try_recv() {
                if frame.fresh_at(resumed, &counters) {
                    sent.push(frame.capture_position);
                }
            }
            assert_eq!(sent, [(size * 3) as u64]);
            assert_eq!(counters.capture_stale_frames.load(Ordering::Relaxed), 2);
            // Encode can itself cross the deadline after PCM passed admission.
            let frame = Outgoing { captured_at: resumed, capture_position: 0, opus: vec![1], level: -10, speech: true };
            assert!(frame.fresh_at(resumed + io::MAX_CAPTURE_AGE, &counters));
            assert!(!frame.fresh_at(resumed + io::MAX_CAPTURE_AGE + Duration::from_nanos(1), &counters));
            assert_eq!(counters.capture_stale_frames.load(Ordering::Relaxed), 3);
        }
    }

    #[test]
    fn device_callbacks_longer_than_the_age_limit_still_complete_frames() {
        // Some devices deliver 100 ms per callback: a frame split across two
        // callbacks is not stale, and its age starts when it becomes complete.
        let counters = Counters::default();
        let mut capture = CaptureFrame::new(FRAME);
        let first = Instant::now();
        let second = first + Duration::from_millis(100);
        let split = FRAME as u64 * 5 / 4;
        let mut positions = Vec::new();
        for position in 0..FRAME as u64 * 2 {
            let at = if position < split { first } else { second };
            positions.extend(capture.push_at(CaptureSample { position, captured_at: at, value: 0.1 }, at, &counters));
        }
        assert_eq!(positions, [0, FRAME as u64]);
        assert_eq!(capture.captured_at, second);
        assert_eq!(counters.capture_stale_samples.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn capture_frames_keep_positions_across_batches_and_ring_overflow() {
        let (mut tx, mut rx) = rtrb::RingBuffer::new(4);
        let mut frame = CaptureFrame::new(3);
        for position in 0..7 {
            let _ = tx.push(CaptureSample { captured_at: std::time::Instant::now(), position, value: position as f32 });
        }
        let mut positions = Vec::new();
        while let Ok(sample) = rx.pop() {
            positions.extend(frame.push_at(sample, Instant::now(), &Counters::default()));
        }
        assert_eq!(positions, vec![0]);
        for position in 7..10 {
            tx.push(CaptureSample { captured_at: std::time::Instant::now(), position, value: position as f32 })
                .unwrap();
        }
        while let Ok(sample) = rx.pop() {
            positions.extend(frame.push_at(sample, Instant::now(), &Counters::default()));
        }
        assert_eq!(positions, vec![0, 7]);
        assert_eq!(frame.samples, vec![7.0, 8.0, 9.0]);
        for position in 10..19 {
            positions.extend(frame.push_at(
                CaptureSample { captured_at: Instant::now(), position, value: 0.0 },
                Instant::now(),
                &Counters::default(),
            ));
        }
        assert_eq!(positions, vec![0, 7, 10, 13, 16]);
    }

    #[test]
    fn capture_clock_survives_gates_and_outgoing_queue_drops() {
        for gate in 0..3 {
            let controls = Arc::new(Controls::default());
            controls.muted.store(gate == 0, Ordering::Relaxed);
            controls.push_to_talk.store(gate == 1, Ordering::Relaxed);
            let (mut tx, mic) = rtrb::RingBuffer::new(FRAME * 16);
            let (speaker, _rx) = rtrb::RingBuffer::new(FRAME * 4);
            let timing = Arc::new(OutputTiming::default());
            let (out, mut outgoing) = mpsc::channel(1);
            let (_incoming, input) = std::sync::mpsc::sync_channel(1);
            let c = controls.clone();
            let t = timing.clone();
            let worker = thread::spawn(move || pipeline::<FRAME>(c, Arc::default(), mic, speaker, t, out, input));
            let mut feed = |start: usize, count: usize, value: f32| {
                for position in start..start + count {
                    tx.push(CaptureSample { captured_at: std::time::Instant::now(), position: position as u64, value })
                        .unwrap();
                }
                timing.wake();
                let deadline = Instant::now() + Duration::from_secs(2);
                while tx.slots() != tx.buffer().capacity() {
                    assert!(Instant::now() < deadline, "pipeline did not drain capture");
                    thread::sleep(Duration::from_millis(1));
                }
                // Let the final consumed sample pass through the gate/encoder.
                thread::sleep(Duration::from_millis(10));
            };
            feed(0, FRAME * 10, if gate == 2 { 0.0 } else { 0.3 });
            assert!(outgoing.try_recv().is_err());
            controls.muted.store(false, Ordering::Relaxed);
            controls.ptt.store(true, Ordering::Relaxed);
            // Only the first of three frames fits the outgoing queue.
            feed(FRAME * 10, FRAME * 3, 0.3);
            assert_eq!(outgoing.try_recv().unwrap().capture_position, (FRAME * 10) as u64);
            feed(FRAME * 13, FRAME, 0.3);
            assert_eq!(outgoing.try_recv().unwrap().capture_position, (FRAME * 13) as u64);
            controls.stop.store(true, Ordering::Relaxed);
            timing.wake();
            worker.join().unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn direct_teamspeak_capture_remains_twenty_ms_without_a_server() {
        struct Capture;
        impl AudioIo for Capture {
            fn open(self: Box<Self>) -> Result<AudioPorts> {
                let (mut tx, mic) = rtrb::RingBuffer::new(1920);
                let (speaker, rx) = rtrb::RingBuffer::new(1920);
                for i in 0..960 {
                    tx.push(CaptureSample {
                        captured_at: std::time::Instant::now(),
                        position: i,
                        value: (i as f32 * 0.04).sin() * 0.3,
                    })
                    .unwrap();
                }
                Ok(AudioPorts { mic, speaker, output: Arc::new(OutputTiming::default()), _guard: Box::new((tx, rx)) })
            }
        }
        let (_incoming, input) = mpsc::channel(1);
        let (output, mut outgoing) = mpsc::channel(4);
        let _handle = start_teamspeak(Box::new(Capture), input, output).await.unwrap();
        let packet = tokio::time::timeout(Duration::from_secs(2), outgoing.recv()).await.unwrap().unwrap();
        assert_eq!(opus_samples(&packet.opus), Some(960));
    }

    #[test]
    fn ten_ms_silk_and_hybrid_recover_lbrr_instead_of_plc() {
        for bandwidth in [Bandwidth::Wideband, Bandwidth::Fullband] {
            for fec_enabled in [false, true] {
                let mut encoder = voice_encoder().unwrap();
                // Label this synthetic voiced fixture so music detection does not
                // choose CELT. Production retains Opus's automatic classification.
                encoder.set_signal(audiopus::Signal::Voice).unwrap();
                encoder.set_bandwidth(bandwidth).unwrap();
                encoder.set_inband_fec(fec_enabled).unwrap();
                let mut packets = Vec::new();
                for frame in 0..80 {
                    let pcm: Vec<_> = (0..480)
                        .map(|i| {
                            let t = (frame * 480 + i) as f32 / 48_000.0;
                            let pitch = std::f32::consts::TAU * (130.0 * t + 0.2 * (3.0 * t).sin());
                            (pitch.sin() + 0.4 * (2.0 * pitch).sin() + 0.2 * (3.0 * pitch).sin()) * 0.2
                        })
                        .collect();
                    let mut data = vec![0; 1275];
                    let n = encoder.encode_float(&pcm, &mut data).unwrap();
                    data.truncate(n);
                    assert_eq!(opus_samples(&data), Some(480));
                    packets.push(data);
                }
                let mut recovered = 0;
                for lost in [10, 20, 40, 60] {
                    let mut fec = Decoder::new(SampleRate::Hz48000, Channels::Mono).unwrap();
                    let mut plc = Decoder::new(SampleRate::Hz48000, Channels::Mono).unwrap();
                    let mut a = [0.0; 480];
                    let mut b = [0.0; 480];
                    for packet in &packets[..lost] {
                        for (decoder, pcm) in [(&mut fec, &mut a), (&mut plc, &mut b)] {
                            decoder
                                .decode_float(
                                    Some(Packet::try_from(packet.as_slice()).unwrap()),
                                    MutSignals::try_from(&mut pcm[..]).unwrap(),
                                    false,
                                )
                                .unwrap();
                        }
                    }
                    let next = &packets[lost + 1];
                    assert_eq!(
                        fec.decode_float(
                            Some(Packet::try_from(next.as_slice()).unwrap()),
                            MutSignals::try_from(&mut a[..]).unwrap(),
                            true
                        )
                        .unwrap(),
                        480
                    );
                    assert_eq!(plc.decode_float(None, MutSignals::try_from(&mut b[..]).unwrap(), false).unwrap(), 480);
                    if a != b {
                        recovered += 1;
                    }
                    if fec_enabled {
                        let config = next[0] >> 3;
                        assert!(
                            match bandwidth {
                                Bandwidth::Wideband => config <= 11,
                                _ => (12..=15).contains(&config),
                            },
                            "{bandwidth:?}: TOC={:x}",
                            next[0]
                        );
                    }
                }
                eprintln!("10 ms {bandwidth:?}, FEC={fec_enabled}: {recovered}/4 recoveries differ from PLC");
                if fec_enabled {
                    assert!(recovered > 0);
                } else {
                    assert_eq!(recovered, 0);
                }
            }
        }
    }
}

#[cfg(test)]
mod handle_tests {
    use super::*;

    #[test]
    fn dropping_the_handle_closes_the_run_gate_before_returning() {
        let gate = keys::RunGate::new();
        let handle = VoiceHandle {
            controls: Arc::new(Controls::default()),
            counters: Arc::new(Counters::default()),
            output: Arc::new(OutputTiming::default()),
            keys: None,
            gate: Some(gate.clone()),
            crypto_events: std::sync::Mutex::new(None),
            _ports_guard: Box::new(()),
        };
        assert!(gate.enter().is_some());
        drop(handle);
        assert!(gate.enter().is_none(), "no control, packet write or playout after retirement");
    }
}
