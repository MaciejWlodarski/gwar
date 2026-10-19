//! TeamSpeak 3/6 servers ("our client on their server"), bridged to the UI.
//!
//! `gwar_client::teamspeak` presents a TS server as `gwar/1`: a `Welcome`, a stream
//! of events and request handling. This module forwards them to the web UI:
//!
//! - `ts_connect {session, address, nickname, password?, identity?}` -> `Welcome` JSON; `identity`
//!   is the uid of the TeamSpeak identity to use (see `ts_identity`), default the list's default
//! - `ts_request {session, op, d}` -> reply value, or rejects with an `ErrorBody`
//! - `ts_disconnect {session}`
//! - event `ts://event {session, frame}` where `frame` is the JSON text of a
//!   `gwar/1` event (`{"ev":..,"d":..}`, identical to the server's frames)
//! - event `ts://closed {session}` when the connection ended
//!
//! The UI picks its own `session` number (and listens before connecting), so
//! nothing is lost between connecting and the first event. Voice uses the same
//! `voice_start` command as Gwar servers; see [`Ts::voice_link`].

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

use gwar_client::{
    ClientError,
    teamspeak::{self, TsAudio, TsHandle, TsOptions},
};
use gwar_proto::{ErrorBody, ErrorCode, Request};
use serde::Serialize;
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::mpsc;

use crate::ts_identity;

/// What voice needs from the active TS connection.
pub struct VoiceLink {
    pub audio_in: mpsc::Receiver<TsAudio>,
    pub audio_out: mpsc::Sender<gwar_client::teamspeak::TsMicrophone>,
}

#[derive(Default)]
pub(crate) struct VoiceSink {
    target: Mutex<Option<(u64, mpsc::Sender<TsAudio>)>>,
}

impl VoiceSink {
    pub(crate) fn link(
        self: &Arc<Self>,
        run: u64,
        audio_out: mpsc::Sender<gwar_client::teamspeak::TsMicrophone>,
    ) -> (VoiceLink, VoiceSinkLease) {
        let (tx, rx) = mpsc::channel(256);
        *self.target.lock().unwrap_or_else(|e| e.into_inner()) = Some((run, tx));
        (VoiceLink { audio_in: rx, audio_out }, VoiceSinkLease { sink: self.clone(), run })
    }

    fn sender(&self) -> Option<mpsc::Sender<TsAudio>> {
        self.target.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|(_, sender)| sender.clone())
    }
}

pub(crate) struct VoiceSinkLease {
    sink: Arc<VoiceSink>,
    run: u64,
}

impl Drop for VoiceSinkLease {
    fn drop(&mut self) {
        let mut target = self.sink.target.lock().unwrap_or_else(|e| e.into_inner());
        if target.as_ref().is_some_and(|(run, _)| *run == self.run) {
            *target = None;
        }
    }
}

struct Active {
    session: u64,
    handle: TsHandle,
    /// Where the audio pump delivers frames; replaced on every voice (re)start.
    sink: Arc<VoiceSink>,
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
    pub fn has_voice_connection(&self) -> bool {
        self.lock().is_some()
    }

    pub(crate) fn voice_link(&self, run: u64) -> Option<(VoiceLink, VoiceSinkLease)> {
        let active = self.lock();
        let active = active.as_ref()?;
        Some(active.sink.link(run, active.handle.audio_out.clone()))
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

#[tauri::command]
pub async fn ts_connect(
    app: AppHandle,
    ts: State<'_, Ts>,
    session: u64,
    address: String,
    nickname: String,
    password: Option<String>,
    identity: Option<String>,
) -> Result<Value, String> {
    ts.close();
    // From the account's identities if signed in to Gwar Connect, else this device's own.
    let identity = {
        let app = app.clone();
        tokio::task::spawn_blocking(move || ts_identity::load_for_connect(&app, identity.as_deref()))
            .await
            .map_err(|e| e.to_string())??
    };
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

    let sink: Arc<VoiceSink> = Arc::default();
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
            let target = sink.sender();
            if let Some(target) = target {
                let _ = target.try_send(frame);
            }
        }
    });
    Ok(welcome)
}

#[cfg(test)]
mod voice_sink_tests {
    use super::*;

    #[test]
    fn dropping_an_old_sink_lease_preserves_the_replacement() {
        let sink = Arc::new(VoiceSink::default());
        let (audio_out, _rx) = mpsc::channel(1);
        let (_old, old_lease) = sink.link(1, audio_out.clone());
        let (mut current, current_lease) = sink.link(2, audio_out);
        drop(old_lease);
        sink.sender()
            .unwrap()
            .try_send(TsAudio { from: 1, slot: 0, rtp_time: 0, generation: 0, opus: vec![1] })
            .unwrap();
        assert_eq!(current.audio_in.try_recv().unwrap().opus, vec![1]);
        drop(current_lease);
        assert!(sink.sender().is_none());
    }
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
