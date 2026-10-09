//! WebSocket gateway for the `vc/1` protocol: authentication, framing,
//! rate limiting and the per-connection write queue.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, Utf8Bytes, WebSocket},
    },
    response::Response,
};
use futures::{SinkExt, StreamExt, stream::SplitStream};
use tokio::{sync::mpsc, time::timeout};
use tracing::debug;
use vc_proto::{
    Challenge, ClientFrame, ErrorBody, ErrorCode, Event, LeaveReason, PROTOCOL_VERSION, Request, ServerFrame,
};

use crate::{
    core::{ConnectRequest, CoreHandle, OUTBOUND_QUEUE, Outbound, encode},
    identity,
};

#[derive(Clone)]
pub struct Gateway {
    pub core: CoreHandle,
    /// Argon2 hash of the server password, if one is set.
    pub password_hash: Option<Arc<str>>,
}

const HELLO_TIMEOUT: Duration = Duration::from_secs(15);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const PING_INTERVAL: Duration = Duration::from_secs(20);
const MAX_FRAME: usize = 96 * 1024;
const RATE_BURST: f64 = 40.0;
const RATE_PER_SEC: f64 = 20.0;

pub async fn upgrade(ws: WebSocketUpgrade, State(gateway): State<Gateway>) -> Response {
    ws.max_message_size(MAX_FRAME).on_upgrade(move |socket| connection(socket, gateway))
}

fn text(frame: &ServerFrame) -> Message {
    Message::Text(Utf8Bytes::from(&*encode(frame)))
}

fn error(re: u32, code: ErrorCode, message: &str) -> Message {
    text(&ServerFrame::Err { re, err: ErrorBody::new(code, message) })
}

async fn next_text(stream: &mut SplitStream<WebSocket>, wait: Duration) -> Option<Utf8Bytes> {
    loop {
        match timeout(wait, stream.next()).await.ok()?? {
            Ok(Message::Text(text)) => return Some(text),
            Ok(Message::Ping(_) | Message::Pong(_)) => continue,
            _ => return None,
        }
    }
}

async fn connection(socket: WebSocket, gateway: Gateway) {
    let (mut sink, mut stream) = socket.split();
    let Some(server) = gateway.core.info().await else { return };
    let nonce = identity::new_nonce();
    let challenge = Challenge {
        protocol: PROTOCOL_VERSION,
        nonce: nonce.clone(),
        server,
        password_required: gateway.password_hash.is_some(),
    };
    if sink.send(text(&ServerFrame::Event(Event::Challenge(challenge)))).await.is_err() {
        return;
    }

    let Some(raw) = next_text(&mut stream, HELLO_TIMEOUT).await else { return };
    let (id, hello) = match serde_json::from_str::<ClientFrame>(&raw) {
        Ok(ClientFrame { id, request: Request::Hello(hello) }) => (id, hello),
        Ok(frame) => {
            let _ = sink.send(error(frame.id, ErrorCode::NotAuthenticated, "send hello first")).await;
            return;
        }
        Err(_) => {
            let _ = sink.send(error(0, ErrorCode::BadRequest, "malformed hello")).await;
            return;
        }
    };
    if hello.protocol != PROTOCOL_VERSION {
        let _ = sink.send(error(id, ErrorCode::BadRequest, "unsupported protocol version")).await;
        return;
    }
    let Some(uid) = identity::verify_hello(&nonce, &hello.public_key, &hello.signature) else {
        let _ = sink.send(error(id, ErrorCode::NotAuthenticated, "invalid identity signature")).await;
        return;
    };
    if let Some(hash) = gateway.password_hash.clone() {
        let password = hello.server_password.clone().unwrap_or_default();
        let ok = password.len() <= 128
            && tokio::task::spawn_blocking(move || crate::core::verify_secret(&password, &hash, false))
                .await
                .unwrap_or(false);
        if !ok {
            let _ = sink.send(error(id, ErrorCode::WrongPassword, "wrong server password")).await;
            return;
        }
    }

    let (out, mut outbound) = mpsc::channel(OUTBOUND_QUEUE);
    let request = ConnectRequest {
        request_id: id,
        uid,
        public_key: hello.public_key,
        nickname: hello.nickname,
        platform: hello.client.platform,
        out: out.clone(),
    };
    let session = gateway.core.connect(request).await;

    let writer = tokio::spawn(async move {
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.tick().await;
        loop {
            tokio::select! {
                item = outbound.recv() => match item {
                    Some(Outbound::Frame(frame)) => {
                        if sink.send(Message::Text(Utf8Bytes::from(&*frame))).await.is_err() {
                            break;
                        }
                    }
                    Some(Outbound::Close(reason)) => {
                        let _ = sink.send(text(&ServerFrame::Event(Event::Disconnected { reason }))).await;
                        break;
                    }
                    None => break,
                },
                _ = ping.tick() => {
                    if sink.send(Message::Ping(Default::default())).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = sink.close().await;
    });

    let Some(session) = session else {
        // The core queued the rejection; let the writer flush it.
        drop(out);
        let _ = writer.await;
        return;
    };

    let mut writer = writer;
    let mut writer_done = false;
    let mut tokens = RATE_BURST;
    let mut refilled = Instant::now();
    let reason = loop {
        let raw = tokio::select! {
            raw = next_text(&mut stream, IDLE_TIMEOUT) => raw,
            _ = &mut writer => {
                writer_done = true;
                None
            }
        };
        let Some(raw) = raw else {
            break LeaveReason::Quit;
        };
        let now = Instant::now();
        tokens = (tokens + now.duration_since(refilled).as_secs_f64() * RATE_PER_SEC).min(RATE_BURST);
        refilled = now;
        let frame = match serde_json::from_str::<ClientFrame>(&raw) {
            Ok(frame) => frame,
            Err(e) => {
                debug!(session, "bad frame: {e}");
                let re = serde_json::from_str::<serde_json::Value>(&raw)
                    .ok()
                    .and_then(|v| v.get("id")?.as_u64())
                    .unwrap_or(0) as u32;
                let reply = ServerFrame::Err { re, err: ErrorBody::new(ErrorCode::BadRequest, "malformed request") };
                if out.try_send(Outbound::Frame(encode(&reply))).is_err() {
                    break LeaveReason::Timeout;
                }
                continue;
            }
        };
        if tokens < 1.0 {
            let reply = ServerFrame::Err { re: frame.id, err: ErrorBody::new(ErrorCode::RateLimited, "slow down") };
            if out.try_send(Outbound::Frame(encode(&reply))).is_err() {
                break LeaveReason::Timeout;
            }
            continue;
        }
        tokens -= 1.0;
        gateway.core.request(session, frame.id, frame.request).await;
    };
    gateway.core.disconnect(session, reason).await;
    drop(out);
    if !writer_done {
        let _ = writer.await;
    }
}
