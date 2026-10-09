//! Native voice: Tauri commands around `vc_client::voice`.
//!
//! The web UI owns the WebSocket and signalling. For the SDP exchange this
//! module emits `voice://offer {run, sdp}`; the UI sends it as `voice.offer`
//! and hands the reply back with `voice_answer`.
//!
//! Events emitted to the UI:
//! - `voice://offer`   `{run, sdp}`            (see above)
//! - `voice://state`   `{run, state}`          `connected` | `failed`
//! - `voice://level`   `{db, transmitting}`    ~20 Hz while the window is visible
//! - `voice://issue`   `{side, kind, message}` device problems (`input`/`output`)
//!   with `kind` = `no_device` | `permission_denied` (input only) | `failed`
//! - `voice://test-level {db}`                 microphone test in settings

use std::{
    net::IpAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::oneshot;
use vc_client::voice::{
    InputMode, LinkState, VoiceHandle,
    io::{AudioIo, DeviceIo, DeviceIssue, IssueSink},
    permission, start_teamspeak, start_with,
};
use vc_proto::AUDIO_SLOTS;

use crate::{
    fake_audio::{self, FakeIo, Heard, NullIo},
    ts::Ts,
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
    master: f32,
    slot_gain: [f32; AUDIO_SLOTS],
}

impl Prefs {
    fn apply(&self, handle: &VoiceHandle) {
        handle.set_input_mode(self.mode);
        handle.set_muted(self.muted);
        handle.set_deafened(self.deafened);
        handle.set_ptt(self.ptt);
        handle.set_master_gain(self.master);
        for (slot, gain) in self.slot_gain.iter().enumerate() {
            handle.set_slot_gain(slot, *gain);
        }
    }
}

struct Inner {
    prefs: Prefs,
    handle: Option<VoiceHandle>,
    pending_answer: Option<(u64, oneshot::Sender<Result<String, String>>)>,
}

pub struct Voice {
    inner: Mutex<Inner>,
    /// Id of the most recent `voice_start`; older runs abandon themselves.
    run: AtomicU64,
    mic_test: Mutex<Option<Arc<AtomicBool>>>,
    /// What the fake speaker heard (only with `VC_FAKE_AUDIO`).
    pub heard: Arc<Heard>,
}

impl Default for Voice {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner {
                prefs: Prefs {
                    input: None,
                    output: None,
                    mode: InputMode::VoiceActivity { threshold_db: DEFAULT_THRESHOLD_DB },
                    muted: false,
                    deafened: false,
                    ptt: false,
                    master: 1.0,
                    slot_gain: [1.0; AUDIO_SLOTS],
                },
                handle: None,
                pending_answer: None,
            }),
            run: AtomicU64::new(0),
            mic_test: Mutex::new(None),
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
    input_db: f32,
    transmitting: bool,
    fake_audio: bool,
    speaker_rms: f32,
    speaker_peak_rms: f32,
    speaker_loud_samples: u64,
}

// --------------------------------------------------------------------- events

#[derive(Clone, Serialize)]
struct OfferPayload {
    run: u64,
    sdp: String,
}

#[derive(Clone, Serialize)]
struct StatePayload {
    run: u64,
    state: &'static str,
}

#[derive(Clone, Serialize)]
struct LevelPayload {
    db: f32,
    transmitting: bool,
}

#[derive(Clone, Serialize)]
struct IssuePayload {
    side: &'static str,
    kind: &'static str,
    message: String,
}

fn issue_sink(app: &AppHandle) -> IssueSink {
    let app = app.clone();
    Arc::new(move |issue| {
        let denied = matches!(issue, DeviceIssue::InputDenied(_));
        let (side, message) = match issue {
            DeviceIssue::Input(m) | DeviceIssue::InputDenied(m) => ("input", m),
            DeviceIssue::Output(m) => ("output", m),
        };
        let kind = if denied {
            "permission_denied"
        } else if message.contains("no microphone found") || message.contains("no output device found") {
            "no_device"
        } else {
            "failed"
        };
        tracing::warn!("audio {side} problem: {message}");
        let _ = app.emit("voice://issue", IssuePayload { side, kind, message });
    })
}

/// Reports a failure to open the real devices, then carries on without audio.
struct FallbackIo {
    primary: Box<dyn AudioIo>,
    sink: IssueSink,
}

impl AudioIo for FallbackIo {
    fn open(self: Box<Self>) -> anyhow::Result<vc_client::voice::io::AudioPorts> {
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

fn make_io(voice: &Voice, input: Option<String>, output: Option<String>, sink: IssueSink) -> Box<dyn AudioIo> {
    if cfg!(debug_assertions) && std::env::var("VC_FAKE_AUDIO").is_ok_and(|v| v == "fail") {
        // Exercises the "no devices at all" path: report problems, keep listening/chatting.
        struct Failing;
        impl AudioIo for Failing {
            fn open(self: Box<Self>) -> anyhow::Result<vc_client::voice::io::AudioPorts> {
                anyhow::bail!("simulated: no audio hardware")
            }
        }
        return Box::new(FallbackIo { primary: Box::new(Failing), sink });
    }
    if fake_audio::enabled() {
        tracing::info!("using FAKE audio devices (tone={})", fake_audio::tone());
        return Box::new(FakeIo { tone: fake_audio::tone(), heard: voice.heard.clone() });
    }
    Box::new(FallbackIo { primary: Box::new(DeviceIo::new(input, output).with_issue_sink(sink.clone())), sink })
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

fn stop_inner(voice: &Voice) {
    voice.run.fetch_add(1, Ordering::SeqCst);
    let mut inner = voice.lock();
    inner.handle = None; // drops the engine, which stops audio
    inner.pending_answer = None; // wakes a signal() that is waiting
}

/// Starts (or restarts) voice against `server_host`. Resolves when the SDP
/// exchange is done and audio runs; ICE completion arrives as `voice://state`.
#[tauri::command]
pub async fn voice_start(app: AppHandle, voice: State<'_, Voice>, run: u64, server_host: String) -> Result<(), String> {
    stop_inner(&voice);
    voice.run.store(run, Ordering::SeqCst);
    let (input, output) = {
        let inner = voice.lock();
        (inner.prefs.input.clone(), inner.prefs.output.clone())
    };
    let io = make_io(&voice, input, output, issue_sink(&app));

    let handle = if let Some(link) = app.state::<Ts>().voice_link() {
        // TeamSpeak server: frames travel over the TS connection, no SDP exchange.
        tracing::info!("voice_start run={run} (TeamSpeak)");
        start_teamspeak(io, link.audio_in, link.audio_out).await.map_err(|e| format!("{e:#}"))?
    } else {
        let ip = resolve(&server_host).await?;
        tracing::info!("voice_start run={run} host={server_host} ip={ip}");
        start_vc(&app, io, ip, run).await?
    };

    {
        let mut inner = voice.lock();
        if voice.run.load(Ordering::SeqCst) != run {
            return Err("superseded".into()); // `handle` drops here
        }
        inner.prefs.apply(&handle);
        inner.handle = Some(handle);
    }
    tauri::async_runtime::spawn(monitor(app, run));
    Ok(())
}

/// Negotiates WebRTC with a vc server through the UI's WebSocket.
async fn start_vc(app: &AppHandle, io: Box<dyn AudioIo>, ip: IpAddr, run: u64) -> Result<VoiceHandle, String> {
    let signal_app = app.clone();
    start_with(io, ip, move |sdp| async move {
        let (tx, rx) = oneshot::channel();
        {
            let voice = signal_app.state::<Voice>();
            let mut inner = voice.lock();
            if voice.run.load(Ordering::SeqCst) != run {
                anyhow::bail!("superseded");
            }
            inner.pending_answer = Some((run, tx));
        }
        tracing::info!("offer ready ({} bytes), asking the UI to send voice.offer", sdp.len());
        signal_app.emit("voice://offer", OfferPayload { run, sdp }).map_err(|e| anyhow::anyhow!("emit: {e}"))?;
        match tokio::time::timeout(ANSWER_TIMEOUT, rx).await {
            Err(_) => anyhow::bail!("timed out waiting for the server's answer"),
            Ok(Err(_)) => anyhow::bail!("negotiation cancelled"),
            Ok(Ok(Err(message))) => anyhow::bail!("{message}"),
            Ok(Ok(Ok(answer))) => {
                tracing::info!("answer received ({} bytes)", answer.len());
                Ok(answer)
            }
        }
    })
    .await
    .map_err(|e| format!("{e:#}"))
}

/// Delivers the server's answer (or a failure) for the offer of `run`.
#[tauri::command]
pub fn voice_answer(voice: State<'_, Voice>, run: u64, sdp: Option<String>, error: Option<String>) {
    let mut inner = voice.lock();
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
}

#[tauri::command]
pub fn voice_stop(voice: State<'_, Voice>) {
    tracing::info!("voice_stop");
    stop_inner(&voice);
}

/// Polls the engine at ~20 Hz: meter level and link state changes.
async fn monitor(app: AppHandle, run: u64) {
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    let mut last_link = None;
    let mut ticks = 0u32;
    let log_stats = std::env::var("VC_VOICE_STATS").is_ok();
    loop {
        tick.tick().await;
        ticks += 1;
        let voice = app.state::<Voice>();
        if voice.run.load(Ordering::SeqCst) != run {
            return;
        }
        let (db, transmitting, stats) = {
            let inner = voice.lock();
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
                let _ = app.emit("voice://state", StatePayload { run, state });
            }
            if stats.link == LinkState::Lost {
                return;
            }
        }
        let visible = app.get_webview_window("main").is_some_and(|w| w.is_visible().unwrap_or(true));
        if visible {
            let _ = app.emit("voice://level", LevelPayload { db, transmitting });
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
pub fn voice_set_muted(voice: State<'_, Voice>, muted: bool) {
    let mut inner = voice.lock();
    inner.prefs.muted = muted;
    if let Some(h) = &inner.handle {
        h.set_muted(muted);
    }
}

#[tauri::command]
pub fn voice_set_deafened(voice: State<'_, Voice>, deafened: bool) {
    let mut inner = voice.lock();
    inner.prefs.deafened = deafened;
    if let Some(h) = &inner.handle {
        h.set_deafened(deafened);
    }
}

#[tauri::command]
pub fn voice_set_ptt(voice: State<'_, Voice>, active: bool) {
    voice.set_ptt(active);
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Vad,
    Ptt,
}

#[tauri::command]
pub fn voice_set_input_mode(voice: State<'_, Voice>, mode: Mode, threshold_db: Option<f32>) {
    let mut inner = voice.lock();
    inner.prefs.mode = match mode {
        Mode::Vad => InputMode::VoiceActivity { threshold_db: threshold_db.unwrap_or(DEFAULT_THRESHOLD_DB) },
        Mode::Ptt => InputMode::PushToTalk,
    };
    if let Some(h) = &inner.handle {
        h.set_input_mode(inner.prefs.mode);
    }
}

#[tauri::command]
pub fn voice_set_master_gain(voice: State<'_, Voice>, gain: f32) {
    let mut inner = voice.lock();
    inner.prefs.master = gain.clamp(0.0, 2.0);
    if let Some(h) = &inner.handle {
        h.set_master_gain(inner.prefs.master);
    }
}

#[tauri::command]
pub fn voice_set_slot_gain(voice: State<'_, Voice>, slot: usize, gain: f32) {
    let mut inner = voice.lock();
    if let Some(g) = inner.prefs.slot_gain.get_mut(slot) {
        *g = gain.clamp(0.0, 2.0);
    }
    if let Some(h) = &inner.handle {
        h.set_slot_gain(slot, gain);
    }
}

/// Chooses devices by name (`null` = system default). Takes effect the next
/// time voice starts; the UI restarts voice when it changes while connected.
#[tauri::command]
pub fn voice_set_devices(voice: State<'_, Voice>, input: Option<String>, output: Option<String>) {
    let mut inner = voice.lock();
    inner.prefs.input = input;
    inner.prefs.output = output;
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
pub async fn audio_devices() -> Result<Devices, String> {
    // Enumeration talks to the OS audio stack and can block for a moment.
    let (inputs, outputs) =
        tokio::task::spawn_blocking(vc_client::voice::io::devices).await.map_err(|e| e.to_string())?;
    Ok(Devices { inputs: dedupe(inputs), outputs: dedupe(outputs) })
}

// ------------------------------------------------------------------ mic test

#[derive(Clone, Serialize)]
struct TestLevel {
    db: f32,
}

/// Opens the microphone on its own and reports `voice://test-level` until
/// `mic_test_stop`. Independent of the voice session.
#[tauri::command]
pub async fn mic_test_start(app: AppHandle, voice: State<'_, Voice>, input: Option<String>) -> Result<(), String> {
    mic_test_stop(voice.clone());
    let sink = issue_sink(&app);
    let io = make_io(&voice, input, None, sink.clone());
    let ports = tokio::task::spawn_blocking(move || io.open())
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))?;
    let stop = Arc::new(AtomicBool::new(false));
    *voice.mic_test.lock().unwrap_or_else(|e| e.into_inner()) = Some(stop.clone());
    thread::Builder::new()
        .name("vc-mic-test".into())
        .spawn(move || {
            let vc_client::voice::io::AudioPorts { mut mic, speaker: _speaker, _guard } = ports;
            let mut acc = (0f32, 0usize);
            while !stop.load(Ordering::Relaxed) {
                while let Ok(s) = mic.pop() {
                    acc.0 += s * s;
                    acc.1 += 1;
                }
                if acc.1 > 0 {
                    let rms = (acc.0 / acc.1 as f32).sqrt();
                    let _ = app.emit("voice://test-level", TestLevel { db: 20.0 * rms.max(1e-5).log10() });
                    acc = (0.0, 0);
                }
                thread::sleep(Duration::from_millis(50));
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn mic_test_stop(voice: State<'_, Voice>) {
    if let Some(stop) = voice.mic_test.lock().unwrap_or_else(|e| e.into_inner()).take() {
        stop.store(true, Ordering::Relaxed);
    }
}

// ------------------------------------------------------------ privacy settings

/// Opens the operating system's microphone privacy page, so the user can allow
/// Gwar after refusing the prompt. Errors on platforms without such a page.
#[tauri::command]
pub fn open_mic_privacy_settings() -> Result<(), String> {
    let url = if cfg!(target_os = "macos") {
        "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
    } else if cfg!(target_os = "windows") {
        "ms-settings:privacy-microphone"
    } else {
        return Err("this system has no microphone privacy settings".into());
    };
    open::that(url).map_err(|e| format!("cannot open {url}: {e}"))
}
