//! TeamSpeak 3/6 servers ("our client on their server"), bridged to the UI.
//!
//! `vc_client::teamspeak` presents a TS server as `vc/1`: a `Welcome`, a stream
//! of events and request handling. This module forwards them to the web UI:
//!
//! - `ts_connect {session, address, nickname, password?}` -> `Welcome` JSON
//! - `ts_request {session, op, d}` -> reply value, or rejects with an `ErrorBody`
//! - `ts_disconnect {session}`
//! - event `ts://event {session, frame}` where `frame` is the JSON text of a
//!   `vc/1` event (`{"ev":..,"d":..}`, identical to the server's frames)
//! - event `ts://closed {session}` when the connection ended
//!
//! The UI picks its own `session` number (and listens before connecting), so
//! nothing is lost between connecting and the first event. Voice uses the same
//! `voice_start` command as vc servers; see [`Ts::voice_link`].

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use serde::Serialize;
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::mpsc;
use vc_client::{
    ClientError,
    teamspeak::{self, TsAudio, TsHandle, TsOptions},
};
use vc_proto::{ErrorBody, ErrorCode, Request};

/// What voice needs from the active TS connection.
pub struct VoiceLink {
    pub audio_in: mpsc::Receiver<TsAudio>,
    pub audio_out: mpsc::Sender<Vec<u8>>,
}

struct Active {
    session: u64,
    handle: TsHandle,
    /// Where the audio pump delivers frames; replaced on every voice (re)start.
    sink: Arc<Mutex<Option<mpsc::Sender<TsAudio>>>>,
}

#[derive(Default)]
pub struct Ts {
    active: Mutex<Option<Active>>,
    /// Bumped on connect/disconnect so stale forwarders stop.
    generation: AtomicU64,
}

impl Ts {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Active>> {
        self.active.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Hands voice a fresh audio channel and the microphone sender. A voice
    /// restart (device change) just calls this again; the pump keeps running.
    pub fn voice_link(&self) -> Option<VoiceLink> {
        let active = self.lock();
        let active = active.as_ref()?;
        let (tx, rx) = mpsc::channel(256);
        *active.sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
        Some(VoiceLink { audio_in: rx, audio_out: active.handle.audio_out.clone() })
    }

    /// Leaves the TeamSpeak server politely before the process exits.
    pub fn shutdown(&self) {
        if self.lock().is_some() {
            self.close();
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
    }

    fn close(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        // Dropping the handle ends the TS session.
        self.lock().take();
    }
}

#[derive(Clone, Serialize)]
struct EventPayload {
    session: u64,
    frame: String,
}

#[derive(Clone, Serialize)]
struct ClosedPayload {
    session: u64,
}

fn identity_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    Ok(dir.join("teamspeak-identity.json"))
}

#[tauri::command]
pub async fn ts_connect(
    app: AppHandle,
    ts: State<'_, Ts>,
    session: u64,
    address: String,
    nickname: String,
    password: Option<String>,
) -> Result<Value, String> {
    ts.close();
    let path = identity_path(&app)?;
    let identity = tokio::task::spawn_blocking(move || teamspeak::load_identity(&path))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))?;
    tracing::info!("ts_connect session={session} address={address}");
    let connected = teamspeak::connect(TsOptions {
        address,
        nickname,
        server_password: password.filter(|p| !p.is_empty()),
        identity,
    })
    .await
    .map_err(|e| format!("{e:#}"))?;
    let teamspeak::TsConnected { handle, welcome, mut events, mut audio_in } = connected;
    let welcome = serde_json::to_value(&welcome).map_err(|e| e.to_string())?;

    let sink: Arc<Mutex<Option<mpsc::Sender<TsAudio>>>> = Arc::default();
    let generation = ts.generation.fetch_add(1, Ordering::SeqCst) + 1;
    *ts.lock() = Some(Active { session, handle, sink: sink.clone() });

    // Events -> UI.
    let events_app = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            let state = events_app.state::<Ts>();
            if state.generation.load(Ordering::SeqCst) != generation {
                return;
            }
            match serde_json::to_string(&event) {
                Ok(frame) => {
                    let _ = events_app.emit("ts://event", EventPayload { session, frame });
                }
                Err(e) => tracing::warn!("cannot encode TS event: {e}"),
            }
        }
        let state = events_app.state::<Ts>();
        if state.generation.load(Ordering::SeqCst) == generation {
            tracing::info!("TeamSpeak session {session} ended");
            state.lock().take();
            let _ = events_app.emit("ts://closed", ClosedPayload { session });
        }
    });

    // Audio pump: always drains the TS side, delivers to voice when it runs.
    let pump_app = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(frame) = audio_in.recv().await {
            if pump_app.state::<Ts>().generation.load(Ordering::SeqCst) != generation {
                return;
            }
            let target = sink.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if let Some(target) = target {
                let _ = target.try_send(frame);
            }
        }
    });
    Ok(welcome)
}

fn error_body(e: ClientError) -> ErrorBody {
    match e {
        ClientError::Server(body) => body,
        ClientError::Timeout => ErrorBody::new(ErrorCode::Unavailable, "timed out"),
        other => ErrorBody::new(ErrorCode::Unavailable, other.to_string()),
    }
}

#[tauri::command]
pub async fn ts_request(ts: State<'_, Ts>, session: u64, op: String, d: Option<Value>) -> Result<Value, ErrorBody> {
    let handle = {
        let active = ts.lock();
        match active.as_ref() {
            Some(a) if a.session == session => a.handle.clone(),
            _ => return Err(ErrorBody::new(ErrorCode::Unavailable, "not connected")),
        }
    };
    let request: Request = serde_json::from_value(json!({ "op": op, "d": d.unwrap_or_else(|| json!({})) }))
        .map_err(|e| ErrorBody::new(ErrorCode::BadRequest, e.to_string()))?;
    handle.request(request).await.map_err(error_body)
}

#[tauri::command]
pub fn ts_disconnect(ts: State<'_, Ts>, session: u64) {
    let matches = ts.lock().as_ref().is_some_and(|a| a.session == session);
    if matches {
        tracing::info!("ts_disconnect session={session}");
        ts.close();
    }
}
