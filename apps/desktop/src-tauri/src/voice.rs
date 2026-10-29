//! Native voice: Tauri commands around `gwar_client::voice`.
//!
//! The web UI owns the WebSocket and signalling. For the SDP exchange this
//! module emits `voice://offer {owner, run, sdp}`; the UI sends it as `voice.offer`
//! and hands the reply back with `voice_answer`.
//!
//! Ownership: `voice_bootstrap` retires the previous web-view and returns a
//! native owner generation. Every mutating voice/microphone command requires
//! it; a retired engine must never bootstrap again. See `../VOICE-IPC.md`.
//!
//! Events emitted to the UI:
//! - `voice://offer`   `{owner, run, sdp}`            (see above)
//! - `voice://state`   `{owner, run, state}`          `connected` | `failed`
//! - `voice://level`   `{owner, db, transmitting}`    ~20 Hz while the window is visible
//! - `voice://issue`   `{owner, side, kind, message}` device problems (`input`/`output`)
//!   with `kind` = `no_device` | `permission_denied` (input only) | `failed`
//! - `voice://device`  `{owner, side, kind, device}` the devices sorted themselves out:
//!   `switched` (audio moved to `device` by itself) | `recovered` (a reported problem is over)
//! - `voice://test-level {owner, db}`                 microphone test in settings
//! - `voice://crypto`  `{owner, run, binding, state, reason}` encrypted-run status
//!   (`securing` | `failed`); never an installation acknowledgement
//!
//! Encrypted runs: `voice_start` with `mode: "encrypted"` binds the run before
//! negotiation; `voice_set_keys` (main window only, never a remote origin)
//! returns after the RTC task applied the control.

use std::{
    future::Future,
    net::IpAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use gwar_client::voice::{
    InputMode, LinkState, NoiseSuppression, VoiceHandle,
    io::{AudioIo, AudioPorts, DeviceControl, DeviceIo, DeviceIssue, IssueSink, Side},
    keys::EncryptedStart,
    permission,
    sframe::{Binding, ControlReply, Counters, CryptoStatus},
    start_encrypted, start_teamspeak, start_with,
};
use gwar_proto::AUDIO_SLOTS;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State, WebviewWindow};
use tokio::sync::{oneshot, watch};

use crate::{
    fake_audio::{self, FakeIo, Heard, NullIo},
    ts::{Ts, VoiceLink, VoiceSinkLease},
};

const ANSWER_TIMEOUT: Duration = Duration::from_secs(20);
/// Default voice-activity threshold (dBFS) until the UI sets one.
pub const DEFAULT_THRESHOLD_DB: f32 = -50.0;

/// Everything the UI configured, kept here so it survives engine restarts
/// (new session, device change) and is applied when voice (re)starts.
struct Prefs {
    input: Option<String>,
    output: Option<String>,
    mode: InputMode,
    muted: bool,
    deafened: bool,
    ptt: bool,
    echo: bool,
    gain: bool,
    noise: NoiseSuppression,
    master: f32,
    slot_gain: [f32; AUDIO_SLOTS],
}

impl Prefs {
    fn apply(&self, handle: &VoiceHandle) {
        handle.set_input_mode(self.mode);
        handle.set_muted(self.muted);
        handle.set_deafened(self.deafened);
        handle.set_ptt(self.ptt);
        handle.set_capture_processing(self.echo, self.gain);
        handle.set_noise_suppression(self.noise);
        handle.set_master_gain(self.master);
        for (slot, gain) in self.slot_gain.iter().enumerate() {
            handle.set_slot_gain(slot, *gain);
        }
    }
}

struct Inner {
    owner: u64,
    startup: Option<Arc<NativeRun>>,
    mic_test: Option<Arc<NativeRun>>,
    prefs: Prefs,
    handle: Option<VoiceHandle>,
    /// Moves the running engine's devices (`voice_set_devices`) without renegotiating.
    devices: Option<DeviceControl>,
    pending_answer: Option<(u64, oneshot::Sender<Result<String, String>>)>,
    cancelled_through: u64,
    ts_sink: Option<VoiceSinkLease>,
}

pub struct Voice {
    inner: Mutex<Inner>,
    /// Id of the active `voice_start`; zero after retirement.
    run: AtomicU64,
    /// What the fake speaker heard (only with `GWAR_FAKE_AUDIO`).
    pub heard: Arc<Heard>,
}

impl Default for Voice {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner {
                owner: 0,
                startup: None,
                mic_test: None,
                prefs: Prefs {
                    input: None,
                    output: None,
                    mode: InputMode::VoiceActivity { threshold_db: DEFAULT_THRESHOLD_DB },
                    muted: false,
                    deafened: false,
                    ptt: false,
                    // Until the settings say otherwise (they default to all on, at full strength).
                    echo: true,
                    gain: true,
                    noise: NoiseSuppression::FULL,
                    master: 1.0,
                    slot_gain: [1.0; AUDIO_SLOTS],
                },
                handle: None,
                devices: None,
                pending_answer: None,
                cancelled_through: 0,
                ts_sink: None,
            }),
            run: AtomicU64::new(0),
            heard: Arc::default(),
        }
    }
}

impl Voice {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Push-to-talk state from a global shortcut.
    pub fn set_ptt(&self, down: bool) {
        let mut inner = self.lock();
        inner.prefs.ptt = down;
        if let Some(h) = &inner.handle {
            h.set_ptt(down);
        }
    }

    /// Diagnostics for the debug command and log lines.
    pub fn snapshot(&self) -> Snapshot {
        let inner = self.lock();
        let stats = inner.handle.as_ref().map(VoiceHandle::stats);
        Snapshot {
            running: inner.handle.is_some(),
            link: stats.map(|s| format!("{:?}", s.link)),
            frames_sent: stats.map_or(0, |s| s.frames_sent),
            frames_received: stats.map_or(0, |s| s.frames_received),
            unexpected_duration: stats.map_or(0, |s| s.unexpected_duration),
            oversize_dropped: stats.map_or(0, |s| s.oversize_dropped),
            malformed_dropped: stats.map_or(0, |s| s.malformed_dropped),
            capture_position_dropped: stats.map_or(0, |s| s.capture_position_dropped),
            input_db: inner.handle.as_ref().map_or(-100.0, VoiceHandle::input_level_db),
            transmitting: inner.handle.as_ref().is_some_and(VoiceHandle::transmitting),
            fake_audio: fake_audio::enabled(),
            speaker_rms: self.heard.rms(),
            speaker_peak_rms: self.heard.peak_rms(),
            speaker_loud_samples: self.heard.loud_samples(),
        }
    }
}

#[derive(Serialize)]
pub struct Snapshot {
    running: bool,
    link: Option<String>,
    frames_sent: u64,
    frames_received: u64,
    unexpected_duration: u64,
    oversize_dropped: u64,
    malformed_dropped: u64,
    capture_position_dropped: u64,
    input_db: f32,
    transmitting: bool,
    fake_audio: bool,
    speaker_rms: f32,
    speaker_peak_rms: f32,
    speaker_loud_samples: u64,
}

/// A startup can outlive its async caller inside `spawn_blocking`. Keep the
/// device guard here so retirement closes capture even during SDP negotiation;
/// a blocking open that finishes after retirement drops its ports immediately.
struct NativeRun {
    cancelled: watch::Sender<bool>,
    audio: Mutex<Option<Box<dyn Send>>>,
}

impl NativeRun {
    fn new() -> Arc<Self> {
        Arc::new(Self { cancelled: watch::channel(false).0, audio: Mutex::new(None) })
    }

    fn check(&self) -> Result<(), String> {
        if *self.cancelled.borrow() { Err("superseded".into()) } else { Ok(()) }
    }

    fn retire(&self) {
        let mut audio = self.audio.lock().unwrap_or_else(|e| e.into_inner());
        self.cancelled.send_replace(true);
        *audio = None;
    }

    async fn until_retired<T>(&self, future: impl Future<Output = Result<T, String>>) -> Result<T, String> {
        let mut cancelled = self.cancelled.subscribe();
        self.check()?;
        tokio::select! {
            biased;
            _ = cancelled.changed() => Err("superseded".into()),
            result = future => {
                self.check()?;
                result
            }
        }
    }
}

struct RunAudioGuard(Arc<NativeRun>);
impl Drop for RunAudioGuard {
    fn drop(&mut self) {
        *self.0.audio.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

struct RunIo {
    run: Arc<NativeRun>,
    io: Box<dyn AudioIo>,
}
impl AudioIo for RunIo {
    fn open(self: Box<Self>) -> anyhow::Result<AudioPorts> {
        self.run.check().map_err(anyhow::Error::msg)?;
        let mut ports = self.io.open()?;
        let mut audio = self.run.audio.lock().unwrap_or_else(|e| e.into_inner());
        self.run.check().map_err(anyhow::Error::msg)?;
        *audio = Some(std::mem::replace(&mut ports._guard, Box::new(RunAudioGuard(self.run.clone()))));
        Ok(ports)
    }
}

/// DNS is injected so the retirement boundary can be tested without networking.
async fn start_resolved<T, F: Future<Output = Result<T, String>>>(
    startup: &NativeRun,
    dns: impl Future<Output = Result<IpAddr, String>>,
    start: impl FnOnce(IpAddr) -> F,
) -> Result<T, String> {
    let ip = dns.await?;
    startup.check()?;
    start(ip).await
}

// --------------------------------------------------------------------- events

#[derive(Clone, Serialize)]
struct OfferPayload {
    owner: u64,
    run: u64,
    sdp: String,
}

#[derive(Clone, Serialize)]
struct StatePayload {
    owner: u64,
    run: u64,
    state: &'static str,
}

#[derive(Clone, Serialize)]
struct CryptoPayload {
    owner: u64,
    run: u64,
    #[serde(flatten)]
    status: CryptoStatus,
}

#[derive(Clone, Serialize)]
struct LevelPayload {
    owner: u64,
    db: f32,
    transmitting: bool,
}

#[derive(Clone, Serialize)]
struct IssuePayload {
    owner: u64,
    side: &'static str,
    kind: &'static str,
    message: String,
}

#[derive(Clone, Serialize)]
struct DevicePayload {
    owner: u64,
    side: &'static str,
    kind: &'static str,
    device: Option<String>,
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Input => "input",
        Side::Output => "output",
    }
}

fn issue_sink(app: &AppHandle, owner: u64) -> IssueSink {
    let app = app.clone();
    Arc::new(move |issue| {
        let denied = matches!(issue, DeviceIssue::InputDenied(_));
        let (side, message) = match issue {
            DeviceIssue::Input(m) | DeviceIssue::InputDenied(m) => ("input", m),
            DeviceIssue::Output(m) => ("output", m),
            DeviceIssue::Switched(side, device) => {
                tracing::info!("audio {side:?} switched to {device}");
                let payload = DevicePayload { owner, side: side_name(side), kind: "switched", device: Some(device) };
                let _ = app.emit("voice://device", payload);
                return;
            }
            DeviceIssue::Recovered(side) => {
                let payload = DevicePayload { owner, side: side_name(side), kind: "recovered", device: None };
                let _ = app.emit("voice://device", payload);
                return;
            }
        };
        let kind = if denied {
            "permission_denied"
        } else if message.contains("no microphone found") || message.contains("no output device found") {
            "no_device"
        } else {
            "failed"
        };
        tracing::warn!("audio {side} problem: {message}");
        let _ = app.emit("voice://issue", IssuePayload { owner, side, kind, message });
    })
}

/// Reports a failure to open the real devices, then carries on without audio.
struct FallbackIo {
    primary: Box<dyn AudioIo>,
    sink: IssueSink,
}

impl AudioIo for FallbackIo {
    fn open(self: Box<Self>) -> anyhow::Result<gwar_client::voice::io::AudioPorts> {
        let FallbackIo { primary, sink } = *self;
        match primary.open() {
            Ok(ports) => Ok(ports),
            Err(e) => {
                tracing::warn!("no usable audio devices: {e:#}");
                if permission::is_denied(&e) {
                    sink(DeviceIssue::InputDenied(format!("{e:#}")));
                } else {
                    sink(DeviceIssue::Input(format!("no microphone found: {e:#}")));
                }
                sink(DeviceIssue::Output(format!("no output device found: {e:#}")));
                Box::new(NullIo).open()
            }
        }
    }
}

/// The devices to open, and a control that moves them while voice runs (real devices only).
fn make_io(
    voice: &Voice,
    input: Option<String>,
    output: Option<String>,
    sink: IssueSink,
) -> (Box<dyn AudioIo>, Option<DeviceControl>) {
    if cfg!(debug_assertions) && std::env::var("GWAR_FAKE_AUDIO").is_ok_and(|v| v == "fail") {
        // Exercises the "no devices at all" path: report problems, keep listening/chatting.
        struct Failing;
        impl AudioIo for Failing {
            fn open(self: Box<Self>) -> anyhow::Result<gwar_client::voice::io::AudioPorts> {
                anyhow::bail!("simulated: no audio hardware")
            }
        }
        return (Box::new(FallbackIo { primary: Box::new(Failing), sink }), None);
    }
    if fake_audio::enabled() {
        tracing::info!("using FAKE audio devices (tone={})", fake_audio::tone());
        return (Box::new(FakeIo { tone: fake_audio::tone(), heard: voice.heard.clone() }), None);
    }
    let devices = DeviceIo::new(input, output).with_issue_sink(sink.clone());
    let control = devices.control();
    (Box::new(FallbackIo { primary: Box::new(devices), sink }), Some(control))
}

async fn resolve(host: &str) -> Result<IpAddr, String> {
    if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
        return Ok(ip);
    }
    let addrs: Vec<_> =
        tokio::net::lookup_host((host, 0)).await.map_err(|e| format!("cannot resolve {host}: {e}"))?.collect();
    addrs.iter().find(|a| a.is_ipv4()).or(addrs.first()).map(|a| a.ip()).ok_or_else(|| format!("cannot resolve {host}"))
}

// -------------------------------------------------------------------- commands

// Capabilities scope plugin permissions, not these app-owned commands.
fn check_main_window(label: &str) -> Result<(), String> {
    if label == "main" { Ok(()) } else { Err("voice commands require the main window".into()) }
}

fn check_owner(inner: &Inner, owner: u64) -> Result<(), String> {
    if owner == 0 || inner.owner != owner { Err("superseded owner".into()) } else { Ok(()) }
}

fn clear_run(voice: &Voice, inner: &mut Inner) {
    if let Some(startup) = inner.startup.take() {
        startup.retire();
    }
    voice.run.store(0, Ordering::SeqCst);
    inner.handle = None;
    inner.devices = None;
    inner.pending_answer = None;
    inner.ts_sink = None;
}

fn bootstrap_owner(voice: &Voice) -> u64 {
    let mut inner = voice.lock();
    clear_run(voice, &mut inner);
    if let Some(test) = inner.mic_test.take() {
        test.retire();
    }
    inner.owner += 1;
    inner.cancelled_through = 0;
    // A held key belongs to the old view; never carry it into a new owner.
    inner.prefs.ptt = false;
    inner.owner
}

/// The engine calls this exactly once on construction, before other commands.
/// Owner generations are native-issued and independent of timestamp-seeded runs.
#[tauri::command]
pub fn voice_bootstrap(window: WebviewWindow, voice: State<'_, Voice>) -> Result<u64, String> {
    check_main_window(window.label())?;
    Ok(bootstrap_owner(&voice))
}

// Claim/retire under the same lock as ownership, answers and the media handle.
fn claim_run(voice: &Voice, owner: u64, run: u64) -> Result<Arc<NativeRun>, String> {
    let mut inner = voice.lock();
    check_owner(&inner, owner)?;
    if run <= voice.run.load(Ordering::SeqCst) || run <= inner.cancelled_through {
        return Err("superseded".into());
    }
    inner.cancelled_through = inner.cancelled_through.max(voice.run.load(Ordering::SeqCst));
    clear_run(voice, &mut inner);
    let startup = NativeRun::new();
    inner.startup = Some(startup.clone());
    voice.run.store(run, Ordering::SeqCst);
    Ok(startup)
}
fn retire_run(voice: &Voice, owner: u64, run: u64) -> Result<(), String> {
    let mut inner = voice.lock();
    check_owner(&inner, owner)?;
    // IPC handlers can be scheduled out of order: remember cancellation even
    // when the matching start has not claimed its run yet.
    inner.cancelled_through = inner.cancelled_through.max(run);
    if voice.run.load(Ordering::SeqCst) <= run {
        clear_run(voice, &mut inner);
    }
    Ok(())
}

fn link_teamspeak(
    voice: &Voice,
    owner: u64,
    run: u64,
    link: impl FnOnce() -> Option<(VoiceLink, VoiceSinkLease)>,
) -> Result<Option<VoiceLink>, String> {
    let mut inner = voice.lock();
    check_owner(&inner, owner)?;
    if voice.run.load(Ordering::SeqCst) != run {
        return Err("superseded".into());
    }
    // Claim, retirement and sink replacement share this lock. The lease clears
    // only its own run's sink when a newer run claims or this run stops.
    Ok(link().map(|(link, lease)| {
        inner.ts_sink = Some(lease);
        link
    }))
}

/// Starts (or restarts) voice against `server_host`. Resolves when the SDP
/// exchange is done and audio runs; ICE completion arrives as `voice://state`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartBinding {
    server_key: Option<String>,
    session: u32,
    channel: u32,
    call: Option<String>,
    participation: Option<String>,
    group: Option<String>,
}
impl StartBinding {
    fn valid(&self, direct: bool) -> bool {
        let _ = (self.session, self.channel);
        self.group.is_none()
            && if direct {
                self.server_key.is_none() && self.call.is_none() && self.participation.is_none()
            } else {
                self.server_key.as_deref().and_then(gwar_proto::decode_fixed::<32>).is_some()
                    && self.call.as_deref().and_then(gwar_proto::decode_fixed::<16>).is_some()
                    && self.participation.as_deref().and_then(gwar_proto::decode_fixed::<16>).is_some()
            }
    }

    /// The SFrame binding of an encrypted Gwar call; direct TeamSpeak has none.
    fn encrypted(&self, run: u64, direct: bool) -> Option<Binding> {
        if direct {
            return None;
        }
        Binding::parse(&serde_json::json!({
            "server_key": self.server_key, "session": self.session, "channel": self.channel, "call": self.call,
            "participation": self.participation, "run": run, "group": self.group,
        }))
    }
}

/// `resume_after` is a canonical decimal epoch string, or absent on a first run.
fn resume_floor(value: Option<&str>) -> Result<Option<u64>, String> {
    value
        .map(|text| {
            let canonical = !text.is_empty() && (text == "0" || !text.starts_with('0')) && text.len() <= 20;
            text.parse::<u64>().ok().filter(|_| canonical && text.bytes().all(|b| b.is_ascii_digit()))
        })
        .map(|epoch| epoch.ok_or_else(|| "invalid resume_after".to_owned()))
        .transpose()
}

// Tauri passes each IPC field as its own argument.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn voice_start(
    window: WebviewWindow,
    voice: State<'_, Voice>,
    owner: u64,
    run: u64,
    server_host: String,
    mode: String,
    binding: StartBinding,
    resume_after: Option<String>,
) -> Result<(), String> {
    check_main_window(window.label())?;
    let app = window.app_handle().clone();
    if run == 0 || run > gwar_proto::MAX_SAFE_INTEGER as u64 {
        return Err("invalid voice binding".into());
    }
    let direct = app.state::<Ts>().has_voice_connection();
    let crypto = match mode.as_str() {
        "plaintext" if binding.valid(direct) && resume_after.is_none() => None,
        // Direct TeamSpeak keeps its local binding and rejects encrypted mode.
        "encrypted" => Some(EncryptedStart {
            binding: binding.encrypted(run, direct).ok_or("invalid voice binding")?,
            resume_after: resume_floor(resume_after.as_deref())?,
        }),
        _ => return Err("invalid voice binding".into()),
    };
    let startup = claim_run(&voice, owner, run)?;
    let (input, output) = {
        let inner = voice.lock();
        (inner.prefs.input.clone(), inner.prefs.output.clone())
    };
    let (io, control) = make_io(&voice, input, output, issue_sink(&app, owner));
    let io: Box<dyn AudioIo> = Box::new(RunIo { run: startup.clone(), io });

    let handle = startup
        .until_retired(async {
            if let Some(link) = link_teamspeak(&voice, owner, run, || app.state::<Ts>().voice_link(run))? {
                tracing::info!("voice_start run={run} (TeamSpeak)");
                if crypto.is_some() {
                    return Err("TeamSpeak voice cannot be encrypted".into());
                }
                startup.check()?;
                start_teamspeak(io, link.audio_in, link.audio_out).await.map_err(|e| format!("{e:#}"))
            } else {
                start_resolved(&startup, resolve(&server_host), |ip| {
                    tracing::info!("voice_start run={run} host={server_host} ip={ip}");
                    start_vc(&app, io, ip, owner, run, crypto)
                })
                .await
            }
        })
        .await?;

    {
        let mut inner = voice.lock();
        check_owner(&inner, owner)?;
        if voice.run.load(Ordering::SeqCst) != run {
            return Err("superseded".into()); // `handle` drops here
        }
        inner.prefs.apply(&handle);
        if let Some(events) = handle.take_crypto_events() {
            tauri::async_runtime::spawn(forward_crypto(app.clone(), owner, run, events));
        }
        inner.handle = Some(handle);
        // A choice made while the devices opened applies now.
        if let Some(control) = &control {
            control.set_devices(inner.prefs.input.clone(), inner.prefs.output.clone());
        }
        inner.devices = control;
    }
    tauri::async_runtime::spawn(monitor(app, owner, run));
    Ok(())
}

/// Negotiates WebRTC with a Gwar server through the UI's WebSocket.
async fn start_vc(
    app: &AppHandle,
    io: Box<dyn AudioIo>,
    ip: IpAddr,
    owner: u64,
    run: u64,
    crypto: Option<EncryptedStart>,
) -> Result<VoiceHandle, String> {
    let signal_app = app.clone();
    let signal = move |sdp: String| async move {
        let (tx, rx) = oneshot::channel();
        {
            let voice = signal_app.state::<Voice>();
            let mut inner = voice.lock();
            check_owner(&inner, owner).map_err(anyhow::Error::msg)?;
            if voice.run.load(Ordering::SeqCst) != run {
                anyhow::bail!("superseded");
            }
            inner.pending_answer = Some((run, tx));
        }
        tracing::info!("offer ready ({} bytes), asking the UI to send voice.offer", sdp.len());
        signal_app.emit("voice://offer", OfferPayload { owner, run, sdp }).map_err(|e| anyhow::anyhow!("emit: {e}"))?;
        match tokio::time::timeout(ANSWER_TIMEOUT, rx).await {
            Err(_) => anyhow::bail!("timed out waiting for the server's answer"),
            Ok(Err(_)) => anyhow::bail!("negotiation cancelled"),
            Ok(Ok(Err(message))) => anyhow::bail!("{message}"),
            Ok(Ok(Ok(answer))) => {
                tracing::info!("answer received ({} bytes)", answer.len());
                Ok(answer)
            }
        }
    };
    match crypto {
        Some(crypto) => start_encrypted(io, ip, signal, crypto).await,
        None => start_with(io, ip, signal).await,
    }
    .map_err(|e| format!("{e:#}"))
}

/// Relays `voice://crypto` for one run until its engine ends.
async fn forward_crypto(
    app: AppHandle,
    owner: u64,
    run: u64,
    mut events: tokio::sync::mpsc::UnboundedReceiver<CryptoStatus>,
) {
    while let Some(status) = events.recv().await {
        let failed = status.state == gwar_client::voice::sframe::CryptoState::Failed;
        let _ = app.emit("voice://crypto", CryptoPayload { owner, run, status });
        if failed {
            // A failed run is retired at once: transport, pipeline and devices close.
            let _ = retire_run(&app.state::<Voice>(), owner, run);
            return;
        }
    }
}

/// The current encrypted run's key channel, without holding the lock across awaits.
fn run_keys(voice: &Voice, owner: u64, run: u64) -> Result<Option<gwar_client::voice::keys::KeyChannel>, String> {
    let inner = voice.lock();
    check_owner(&inner, owner)?;
    if voice.run.load(Ordering::SeqCst) != run {
        return Ok(None);
    }
    Ok(inner.handle.as_ref().and_then(|h| h.keys().cloned()))
}

/// Applies one key control to the current encrypted run (docs/e2e-build.md
/// "Installation and activation"). Returns after the RTC task applied it; a
/// retired or plaintext run answers `ignored`. High-risk: main window only.
#[tauri::command]
pub async fn voice_set_keys(
    window: WebviewWindow,
    voice: State<'_, Voice>,
    owner: u64,
    run: u64,
    body: serde_json::Value,
) -> Result<ControlReply, String> {
    // Wipe the body's exporter keys on every exit, including refusals.
    let mut body = gwar_client::voice::sframe::ScrubOnDrop(body);
    check_main_window(window.label())?;
    match run_keys(&voice, owner, run)? {
        Some(keys) => Ok(keys.control(std::mem::take(&mut body.0)).await),
        None => Ok(ControlReply::ignored(&body.0)),
    }
}

/// Aggregate SFrame counters of the current encrypted run (never keys).
#[tauri::command]
pub fn voice_crypto_counters(
    window: WebviewWindow,
    voice: State<'_, Voice>,
    owner: u64,
    run: u64,
) -> Result<Option<Counters>, String> {
    check_main_window(window.label())?;
    Ok(run_keys(&voice, owner, run)?.map(|keys| keys.counters()))
}

/// Delivers the server's answer (or a failure) for the offer of `run`.
#[tauri::command]
pub fn voice_answer(
    window: WebviewWindow,
    voice: State<'_, Voice>,
    owner: u64,
    run: u64,
    sdp: Option<String>,
    error: Option<String>,
) -> Result<(), String> {
    check_main_window(window.label())?;
    answer_run(&voice, owner, run, sdp, error)
}

fn answer_run(voice: &Voice, owner: u64, run: u64, sdp: Option<String>, error: Option<String>) -> Result<(), String> {
    let mut inner = voice.lock();
    check_owner(&inner, owner)?;
    if let Some((r, tx)) = inner.pending_answer.take() {
        if r == run {
            let _ = tx.send(match (sdp, error) {
                (Some(sdp), _) => Ok(sdp),
                (None, error) => Err(error.unwrap_or_else(|| "no answer".into())),
            });
        } else {
            inner.pending_answer = Some((r, tx));
        }
    }
    Ok(())
}

#[tauri::command]
pub fn voice_stop(window: WebviewWindow, voice: State<'_, Voice>, owner: u64, run: u64) -> Result<(), String> {
    check_main_window(window.label())?;
    retire_run(&voice, owner, run)
}

/// Polls the engine at ~20 Hz: meter level and link state changes.
async fn monitor(app: AppHandle, owner: u64, run: u64) {
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    let mut last_link = None;
    let mut ticks = 0u32;
    let log_stats = std::env::var("GWAR_VOICE_STATS").is_ok();
    loop {
        tick.tick().await;
        ticks += 1;
        let voice = app.state::<Voice>();
        if voice.run.load(Ordering::SeqCst) != run {
            return;
        }
        let (db, transmitting, stats) = {
            let inner = voice.lock();
            if check_owner(&inner, owner).is_err() {
                return;
            }
            let Some(h) = &inner.handle else { return };
            (h.input_level_db(), h.transmitting(), h.stats())
        };
        if last_link != Some(stats.link) {
            last_link = Some(stats.link);
            tracing::info!("voice link: {:?}", stats.link);
            let state = match stats.link {
                LinkState::Connecting => None,
                LinkState::Connected => Some("connected"),
                LinkState::Lost => Some("failed"),
            };
            if let Some(state) = state {
                let _ = app.emit("voice://state", StatePayload { owner, run, state });
            }
            if stats.link == LinkState::Lost {
                return;
            }
        }
        let visible = app.get_webview_window("main").is_some_and(|w| w.is_visible().unwrap_or(true));
        if visible {
            let _ = app.emit("voice://level", LevelPayload { owner, db, transmitting });
        }
        if log_stats && ticks.is_multiple_of(20) {
            let s = voice.snapshot();
            tracing::info!(
                "voice stats: link={:?} sent={} received={} input_db={:.1} tx={} speaker_rms={:.4} peak={:.4}",
                s.link,
                s.frames_sent,
                s.frames_received,
                s.input_db,
                s.transmitting,
                s.speaker_rms,
                s.speaker_peak_rms
            );
        }
    }
}

// ------------------------------------------------------------------- controls

#[tauri::command]
pub fn voice_set_muted(window: WebviewWindow, voice: State<'_, Voice>, owner: u64, muted: bool) -> Result<(), String> {
    check_main_window(window.label())?;
    let mut inner = voice.lock();
    check_owner(&inner, owner)?;
    inner.prefs.muted = muted;
    if let Some(h) = &inner.handle {
        h.set_muted(muted);
    }
    Ok(())
}

#[tauri::command]
pub fn voice_set_deafened(
    window: WebviewWindow,
    voice: State<'_, Voice>,
    owner: u64,
    deafened: bool,
) -> Result<(), String> {
    check_main_window(window.label())?;
    let mut inner = voice.lock();
    check_owner(&inner, owner)?;
    inner.prefs.deafened = deafened;
    if let Some(h) = &inner.handle {
        h.set_deafened(deafened);
    }
    Ok(())
}

#[tauri::command]
pub fn voice_set_ptt(window: WebviewWindow, voice: State<'_, Voice>, owner: u64, active: bool) -> Result<(), String> {
    check_main_window(window.label())?;
    let mut inner = voice.lock();
    check_owner(&inner, owner)?;
    inner.prefs.ptt = active;
    if let Some(h) = &inner.handle {
        h.set_ptt(active);
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Vad,
    Ptt,
}

#[tauri::command]
pub fn voice_set_input_mode(
    window: WebviewWindow,
    voice: State<'_, Voice>,
    owner: u64,
    mode: Mode,
    threshold_db: Option<f32>,
) -> Result<(), String> {
    check_main_window(window.label())?;
    let mut inner = voice.lock();
    check_owner(&inner, owner)?;
    inner.prefs.mode = match mode {
        Mode::Vad => InputMode::VoiceActivity { threshold_db: threshold_db.unwrap_or(DEFAULT_THRESHOLD_DB) },
        Mode::Ptt => InputMode::PushToTalk,
    };
    if let Some(h) = &inner.handle {
        h.set_input_mode(inner.prefs.mode);
    }
    Ok(())
}

/// The microphone's processing, as the settings choose it; `strength` is noise suppression's, in
/// percent (see [`NoiseSuppression`]).
#[tauri::command]
pub fn voice_set_capture_options(
    window: WebviewWindow,
    voice: State<'_, Voice>,
    owner: u64,
    echo_cancellation: bool,
    auto_gain_control: bool,
    noise_suppression: bool,
    strength: u8,
) -> Result<(), String> {
    check_main_window(window.label())?;
    let mut inner = voice.lock();
    check_owner(&inner, owner)?;
    inner.prefs.echo = echo_cancellation;
    inner.prefs.gain = auto_gain_control;
    inner.prefs.noise = if noise_suppression { NoiseSuppression::percent(strength) } else { NoiseSuppression::OFF };
    if let Some(h) = &inner.handle {
        h.set_capture_processing(inner.prefs.echo, inner.prefs.gain);
        h.set_noise_suppression(inner.prefs.noise);
    }
    Ok(())
}

#[tauri::command]
pub fn voice_set_master_gain(
    window: WebviewWindow,
    voice: State<'_, Voice>,
    owner: u64,
    gain: f32,
) -> Result<(), String> {
    check_main_window(window.label())?;
    let mut inner = voice.lock();
    check_owner(&inner, owner)?;
    inner.prefs.master = gain.clamp(0.0, 2.0);
    if let Some(h) = &inner.handle {
        h.set_master_gain(inner.prefs.master);
    }
    Ok(())
}

#[tauri::command]
pub fn voice_set_slot_gain(
    window: WebviewWindow,
    voice: State<'_, Voice>,
    owner: u64,
    slot: usize,
    gain: f32,
) -> Result<(), String> {
    check_main_window(window.label())?;
    let mut inner = voice.lock();
    check_owner(&inner, owner)?;
    if let Some(g) = inner.prefs.slot_gain.get_mut(slot) {
        *g = gain.clamp(0.0, 2.0);
    }
    if let Some(h) = &inner.handle {
        h.set_slot_gain(slot, gain);
    }
    Ok(())
}

/// Chooses devices by name (`null` = system default). Running voice moves to
/// them at once, reopening only the side that changed; no renegotiation.
#[tauri::command]
pub fn voice_set_devices(
    window: WebviewWindow,
    voice: State<'_, Voice>,
    owner: u64,
    input: Option<String>,
    output: Option<String>,
) -> Result<(), String> {
    check_main_window(window.label())?;
    let mut inner = voice.lock();
    check_owner(&inner, owner)?;
    if let Some(devices) = &inner.devices {
        devices.set_devices(input.clone(), output.clone());
    }
    inner.prefs.input = input;
    inner.prefs.output = output;
    Ok(())
}

// -------------------------------------------------------------------- devices

#[derive(Serialize)]
pub struct Devices {
    inputs: Vec<String>,
    outputs: Vec<String>,
}

fn dedupe(mut names: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    names.retain(|n| seen.insert(n.clone()));
    names
}

#[tauri::command]
pub async fn audio_devices(window: WebviewWindow) -> Result<Devices, String> {
    check_main_window(window.label())?;
    // Enumeration talks to the OS audio stack and can block for a moment.
    let (inputs, outputs) =
        tokio::task::spawn_blocking(gwar_client::voice::io::devices).await.map_err(|e| e.to_string())?;
    Ok(Devices { inputs: dedupe(inputs), outputs: dedupe(outputs) })
}

// ------------------------------------------------------------------ mic test

#[derive(Clone, Serialize)]
struct TestLevel {
    owner: u64,
    db: f32,
}

/// Opens the microphone on its own and reports `voice://test-level` until
/// `mic_test_stop`. Independent of the voice session.
#[tauri::command]
pub async fn mic_test_start(
    window: WebviewWindow,
    voice: State<'_, Voice>,
    owner: u64,
    input: Option<String>,
) -> Result<(), String> {
    check_main_window(window.label())?;
    let app = window.app_handle().clone();
    let test = {
        let mut inner = voice.lock();
        check_owner(&inner, owner)?;
        if let Some(test) = inner.mic_test.take() {
            test.retire();
        }
        let test = NativeRun::new();
        inner.mic_test = Some(test.clone());
        test
    };
    let io = Box::new(RunIo { run: test.clone(), io: make_io(&voice, input, None, issue_sink(&app, owner)).0 });
    let ports = test
        .until_retired(async {
            tokio::task::spawn_blocking(move || io.open())
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| format!("{e:#}"))
        })
        .await?;
    thread::Builder::new()
        .name("gwar-mic-test".into())
        .spawn(move || {
            let AudioPorts { mut mic, speaker: _speaker, _guard, .. } = ports;
            let mut acc = (0f32, 0usize);
            while test.check().is_ok() {
                while let Ok(s) = mic.pop() {
                    acc.0 += s.value * s.value;
                    acc.1 += 1;
                }
                if acc.1 > 0 {
                    let rms = (acc.0 / acc.1 as f32).sqrt();
                    let _ = app.emit("voice://test-level", TestLevel { owner, db: 20.0 * rms.max(1e-5).log10() });
                    acc = (0.0, 0);
                }
                thread::sleep(Duration::from_millis(50));
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn mic_test_stop(window: WebviewWindow, voice: State<'_, Voice>, owner: u64) -> Result<(), String> {
    check_main_window(window.label())?;
    let mut inner = voice.lock();
    check_owner(&inner, owner)?;
    if let Some(test) = inner.mic_test.take() {
        test.retire();
    }
    Ok(())
}

// ------------------------------------------------------------ privacy settings

/// Opens the operating system's microphone privacy page, so the user can allow
/// Gwar after refusing the prompt. Errors on platforms without such a page.
#[tauri::command]
pub fn open_mic_privacy_settings(window: WebviewWindow) -> Result<(), String> {
    check_main_window(window.label())?;
    let url = if cfg!(target_os = "macos") {
        "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
    } else if cfg!(target_os = "windows") {
        "ms-settings:privacy-microphone"
    } else {
        return Err("this system has no microphone privacy settings".into());
    };
    open::that(url).map_err(|e| format!("cannot open {url}: {e}"))
}

#[cfg(test)]
mod run_tests {
    use super::*;
    use std::{
        sync::atomic::{AtomicBool, AtomicUsize},
        task::Poll,
    };

    #[test]
    fn voice_commands_require_the_main_window_label() {
        assert_eq!(check_main_window("main"), Ok(()));
        for label in ["", "secondary", "Main", "main-popup"] {
            assert_eq!(check_main_window(label), Err("voice commands require the main window".into()));
        }
    }

    struct Closed(Arc<AtomicBool>);
    impl Drop for Closed {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    struct TrackedIo {
        opened: Arc<AtomicUsize>,
        closed: Arc<AtomicBool>,
        gate: Option<(oneshot::Sender<()>, std::sync::mpsc::Receiver<()>)>,
    }
    impl AudioIo for TrackedIo {
        fn open(self: Box<Self>) -> anyhow::Result<AudioPorts> {
            self.opened.fetch_add(1, Ordering::SeqCst);
            if let Some((entered, release)) = self.gate {
                let _ = entered.send(());
                release.recv_timeout(Duration::from_secs(2))?;
            }
            let mut ports = Box::new(NullIo).open()?;
            ports._guard = Box::new(Closed(self.closed));
            Ok(ports)
        }
    }

    fn tracked(run: &Arc<NativeRun>) -> (Box<RunIo>, Arc<AtomicUsize>, Arc<AtomicBool>) {
        let opened = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicBool::new(false));
        let io = Box::new(RunIo {
            run: run.clone(),
            io: Box::new(TrackedIo { opened: opened.clone(), closed: closed.clone(), gate: None }),
        });
        (io, opened, closed)
    }

    #[tokio::test]
    async fn retirement_cancels_deferred_dns_without_opening_media() {
        let voice = Voice::default();
        let owner = bootstrap_owner(&voice);
        let startup = claim_run(&voice, owner, 10).unwrap();
        let (dns, resolved) = oneshot::channel();
        let (io, opened, _) = tracked(&startup);
        let future = startup.until_retired(start_resolved(
            &startup,
            async { resolved.await.map_err(|e| e.to_string()) },
            |_| async {
                io.open().map_err(|e| e.to_string())?;
                Ok(())
            },
        ));
        tokio::pin!(future);
        assert!(std::future::poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await.is_pending());
        retire_run(&voice, owner, 10).unwrap();
        assert_eq!(tokio::time::timeout(Duration::from_secs(1), future).await.unwrap(), Err("superseded".into()));
        assert!(dns.is_closed(), "retirement must drop the pending DNS future");
        assert_eq!(opened.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn dns_completion_rechecks_ownership_before_starting_media() {
        let voice = Voice::default();
        let owner = bootstrap_owner(&voice);
        let startup = claim_run(&voice, owner, 10).unwrap();
        let started = AtomicBool::new(false);
        let result = start_resolved(
            &startup,
            async {
                retire_run(&voice, owner, 10).unwrap();
                Ok("127.0.0.1".parse().unwrap())
            },
            |_| async {
                started.store(true, Ordering::SeqCst);
                Ok(())
            },
        )
        .await;
        assert_eq!(result, Err("superseded".into()));
        assert!(!started.load(Ordering::SeqCst));
    }

    #[test]
    fn queued_device_open_is_rejected_after_retirement() {
        let startup = NativeRun::new();
        let (io, opened, _) = tracked(&startup);
        startup.retire();
        assert!(io.open().is_err());
        assert_eq!(opened.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn blocking_device_open_finishing_after_retirement_closes_capture() {
        let voice = Voice::default();
        let owner = bootstrap_owner(&voice);
        let startup = claim_run(&voice, owner, 10).unwrap();
        let opened = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicBool::new(false));
        let (entered, opening) = oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let io = Box::new(RunIo {
            run: startup,
            io: Box::new(TrackedIo { opened: opened.clone(), closed: closed.clone(), gate: Some((entered, wait)) }),
        });
        let opening_task = tokio::task::spawn_blocking(move || io.open());
        opening.await.unwrap();
        retire_run(&voice, owner, 10).unwrap();
        release.send(()).unwrap();
        assert!(opening_task.await.unwrap().is_err());
        assert_eq!(opened.load(Ordering::SeqCst), 1);
        assert!(closed.load(Ordering::SeqCst));
    }

    #[test]
    fn retirement_closes_capture_while_negotiation_still_holds_ports() {
        let voice = Voice::default();
        let owner = bootstrap_owner(&voice);
        let startup = claim_run(&voice, owner, 10).unwrap();
        let (io, _, closed) = tracked(&startup);
        let ports = io.open().unwrap();
        assert!(!closed.load(Ordering::SeqCst));
        retire_run(&voice, owner, 10).unwrap();
        assert!(closed.load(Ordering::SeqCst));
        drop(ports);
    }

    #[tokio::test]
    async fn bootstrap_retires_previous_views_audio_and_teamspeak_transmit() {
        let voice = Voice::default();
        let old_owner = bootstrap_owner(&voice);
        // Deliberately use a timestamp-sized run; the next view starts at zero.
        let old_run = 1_800_000_000_000_123;
        let startup = claim_run(&voice, old_owner, old_run).unwrap();
        let (io, _, closed) = tracked(&startup);
        let sink = Arc::new(crate::ts::VoiceSink::default());
        let (audio_out, mut outgoing) = tokio::sync::mpsc::channel(1);
        let link = link_teamspeak(&voice, old_owner, old_run, || Some(sink.link(old_run, audio_out))).unwrap().unwrap();
        let handle = start_teamspeak(io, link.audio_in, link.audio_out).await.unwrap();
        let (tx, mut answer) = oneshot::channel();
        {
            let mut inner = voice.lock();
            inner.handle = Some(handle);
            inner.pending_answer = Some((old_run, tx));
            inner.prefs.ptt = true;
        }
        assert!(voice.snapshot().running);
        let owner = bootstrap_owner(&voice);
        assert!(closed.load(Ordering::SeqCst));
        assert!(startup.check().is_err());
        assert!(!voice.snapshot().running);
        assert_eq!(voice.run.load(Ordering::SeqCst), 0);
        assert!(voice.lock().ts_sink.is_none());
        assert_eq!(tokio::time::timeout(Duration::from_secs(1), outgoing.recv()).await.unwrap(), None);
        assert!(!voice.lock().prefs.ptt);
        assert!(matches!(answer.try_recv(), Err(oneshot::error::TryRecvError::Closed)));
        retire_run(&voice, owner, 0).unwrap();
        // Owner generation is independent of wall-clock ordering and run values.
        claim_run(&voice, owner, 1).unwrap();
        let (tx, mut answer) = oneshot::channel();
        voice.lock().pending_answer = Some((1, tx));
        assert!(claim_run(&voice, old_owner, old_run + 1).is_err());
        assert!(retire_run(&voice, old_owner, u64::MAX).is_err());
        assert!(answer_run(&voice, old_owner, 1, Some("stale".into()), None).is_err());
        assert!(link_teamspeak(&voice, old_owner, 1, || panic!("stale link")).is_err());
        assert_eq!(voice.run.load(Ordering::SeqCst), 1);
        assert!(matches!(answer.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
        answer_run(&voice, owner, 1, Some("current".into()), None).unwrap();
        assert_eq!(answer.try_recv().unwrap(), Ok("current".into()));
    }

    #[test]
    fn bootstrap_also_retires_pending_microphone_tests() {
        let voice = Voice::default();
        bootstrap_owner(&voice);
        let test = NativeRun::new();
        voice.lock().mic_test = Some(test.clone());
        let (io, _, closed) = tracked(&test);
        let ports = io.open().unwrap();
        bootstrap_owner(&voice);
        assert!(test.check().is_err());
        assert!(closed.load(Ordering::SeqCst));
        assert!(voice.lock().mic_test.is_none());
        drop(ports);
    }

    fn b64(bytes: &[u8]) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    fn start_binding(group: Option<&str>) -> StartBinding {
        let b = |n: usize, v: u8| b64(&vec![v; n]);
        StartBinding {
            server_key: Some(b(32, 1)),
            session: 7,
            channel: 3,
            call: Some(b(16, 2)),
            participation: Some(b(16, 3)),
            group: group.map(str::to_owned),
        }
    }

    #[test]
    fn encrypted_runs_need_a_full_binding_and_never_use_direct_teamspeak() {
        let group = b64(&[4; 32]);
        let binding = start_binding(Some(&group)).encrypted(9, false).expect("complete binding");
        assert_eq!((binding.run, binding.session, binding.channel), (9, 7, 3));
        assert!(start_binding(Some(&group)).encrypted(9, true).is_none(), "direct TeamSpeak");
        assert!(start_binding(None).encrypted(9, false).is_none(), "no group");
        assert!(start_binding(Some("short")).encrypted(9, false).is_none(), "bad group");
        // Plaintext keeps refusing a group.
        assert!(!start_binding(Some(&group)).valid(false));
        assert!(start_binding(None).valid(false));
    }

    #[test]
    fn resume_floor_accepts_only_canonical_epochs() {
        assert_eq!(resume_floor(None), Ok(None));
        assert_eq!(resume_floor(Some("0")), Ok(Some(0)));
        assert_eq!(resume_floor(Some("18446744073709551615")), Ok(Some(u64::MAX)));
        for bad in ["", "01", "-1", "1.0", "18446744073709551616", " 1"] {
            assert!(resume_floor(Some(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn key_controls_reach_only_the_current_runs_engine() {
        let voice = Voice::default();
        let owner = bootstrap_owner(&voice);
        claim_run(&voice, owner, 5).unwrap();
        // Negotiating, plaintext or retired: no key channel, so a control is ignored.
        assert!(run_keys(&voice, owner, 5).unwrap().is_none());
        assert!(run_keys(&voice, owner, 4).unwrap().is_none());
        assert!(run_keys(&voice, owner + 1, 5).is_err(), "superseded owner");
        let body = serde_json::json!({ "serial": "3", "epoch": "2" });
        let reply = ControlReply::ignored(&body);
        assert_eq!(
            serde_json::to_value(reply).unwrap(),
            serde_json::json!({ "serial": "3", "epoch": "2", "result": "ignored" })
        );
    }

    #[test]
    fn stop_before_start_cancels_delayed_runs_and_retires_older_media() {
        let voice = Voice::default();
        let owner = bootstrap_owner(&voice);
        retire_run(&voice, owner, 10).unwrap();
        assert!(claim_run(&voice, owner, 10).is_err());
        claim_run(&voice, owner, 11).unwrap();
        let (tx, mut rx) = oneshot::channel();
        voice.lock().pending_answer = Some((11, tx));
        retire_run(&voice, owner, 12).unwrap();
        assert!(claim_run(&voice, owner, 12).is_err());
        assert_eq!(voice.run.load(Ordering::SeqCst), 0);
        assert!(matches!(rx.try_recv(), Err(oneshot::error::TryRecvError::Closed)));
        claim_run(&voice, owner, 13).unwrap();
        let (tx, mut rx) = oneshot::channel();
        voice.lock().pending_answer = Some((13, tx));
        retire_run(&voice, owner, 12).unwrap();
        assert_eq!(voice.run.load(Ordering::SeqCst), 13);
        assert!(matches!(rx.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
    }

    #[test]
    fn obsolete_start_cannot_replace_the_current_teamspeak_sink() {
        let voice = Voice::default();
        let owner = bootstrap_owner(&voice);
        let sink = Arc::new(crate::ts::VoiceSink::default());
        let (audio_out, _audio_rx) = tokio::sync::mpsc::channel(1);
        claim_run(&voice, owner, 10).unwrap();
        // A pauses before linking; B claims and installs its sink first.
        claim_run(&voice, owner, 12).unwrap();
        let mut current =
            link_teamspeak(&voice, owner, 12, || Some(sink.link(12, audio_out.clone()))).unwrap().unwrap();
        assert!(link_teamspeak(&voice, owner, 10, || Some(sink.link(10, audio_out))).is_err());
        assert!(matches!(current.audio_in.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Empty)));
        // Stopping A preserves B's receiver; stopping B releases its sink.
        retire_run(&voice, owner, 10).unwrap();
        assert!(matches!(current.audio_in.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Empty)));
        retire_run(&voice, owner, 12).unwrap();
        assert!(matches!(current.audio_in.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)));
    }

    #[test]
    fn old_stop_and_start_preserve_the_current_pending_answer() {
        let voice = Voice::default();
        let owner = bootstrap_owner(&voice);
        claim_run(&voice, owner, 10).unwrap();
        claim_run(&voice, owner, 12).unwrap();
        let (tx, mut rx) = oneshot::channel();
        voice.lock().pending_answer = Some((12, tx));
        retire_run(&voice, owner, 10).unwrap();
        assert!(claim_run(&voice, owner, 11).is_err());
        assert_eq!(voice.run.load(Ordering::SeqCst), 12);
        assert!(matches!(rx.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
        retire_run(&voice, owner, 12).unwrap();
        assert!(matches!(rx.try_recv(), Err(oneshot::error::TryRecvError::Closed)));
        assert!(claim_run(&voice, owner, 12).is_err());
        claim_run(&voice, owner, 14).unwrap();
    }
}
