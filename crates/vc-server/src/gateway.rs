//! WebSocket gateway for the `vc/1` protocol: authentication, framing,
//! rate limiting and the per-connection write queue.

use std::{
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};

use axum::{
    extract::{
        ConnectInfo, State, WebSocketUpgrade,
        ws::{Message, Utf8Bytes, WebSocket},
    },
    http::HeaderMap,
    response::Response,
};
use futures::{SinkExt, StreamExt, stream::SplitStream};
use tokio::{sync::mpsc, time::timeout};
use tracing::debug;
use vc_proto::{
    Challenge, ClientFrame, ErrorBody, ErrorCode, Event, LeaveReason, PROTOCOL_VERSION, Request, ServerFrame,
};

use crate::{
    core::{ConnectRequest, CoreHandle, OUTBOUND_QUEUE, Outbound, ServerPassword, encode},
    identity,
};

#[derive(Clone)]
pub struct Gateway {
    pub core: CoreHandle,
    /// Argon2 hash of the server password, if one is set (changes with settings).
    pub password: ServerPassword,
}

const HELLO_TIMEOUT: Duration = Duration::from_secs(15);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const PING_INTERVAL: Duration = Duration::from_secs(20);
const MAX_FRAME: usize = 96 * 1024;
const RATE_BURST: f64 = 40.0;
const RATE_PER_SEC: f64 = 20.0;

pub async fn upgrade(
    ws: WebSocketUpgrade,
    State(gateway): State<Gateway>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let ip = client_ip(peer, &headers);
    ws.max_message_size(MAX_FRAME).on_upgrade(move |socket| connection(socket, gateway, ip))
}

/// The client's address. Behind a reverse proxy on the same host the TCP peer
/// is loopback, so the proxy's `X-Real-IP` / `X-Forwarded-For` is trusted then
/// (and only then: anyone could send those headers directly).
pub fn client_ip(peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
    if !peer.ip().is_loopback() {
        return peer.ip();
    }
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    header("x-real-ip")
        .or_else(|| header("x-forwarded-for").and_then(|v| v.rsplit(',').next()))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(peer.ip())
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

async fn connection(socket: WebSocket, gateway: Gateway, ip: IpAddr) {
    let (mut sink, mut stream) = socket.split();
    let Some(server) = gateway.core.info().await else { return };
    let nonce = identity::new_nonce();
    let challenge = Challenge {
        protocol: PROTOCOL_VERSION,
        nonce: nonce.clone(),
        server,
        password_required: gateway.password.load().is_some(),
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
    let Some(mut uid) = identity::verify_hello(&nonce, &hello.public_key, &hello.signature) else {
        let _ = sink.send(error(id, ErrorCode::NotAuthenticated, "invalid identity signature")).await;
        return;
    };
    // A Gwar Connect device speaks for its account: the identity is the account key.
    let mut identity_key = hello.public_key.clone();
    let mut device = None;
    if let Some(certificate) = &hello.device {
        match identity::verify_device(certificate, &hello.public_key, crate::core::now_ms()) {
            Ok(account_uid) => {
                uid = account_uid;
                identity_key = certificate.account_key.clone();
                device = Some(certificate.device_key.clone());
            }
            Err(problem) => {
                let _ = sink.send(error(id, ErrorCode::NotAuthenticated, problem)).await;
                return;
            }
        }
    }
    // Whether the password matched; the core decides what that means (members
    // come back without it, an invite admits newcomers).
    let current = gateway.password.load_full();
    let password_ok = match current.as_ref().clone() {
        None => true,
        Some(hash) => {
            let password = hello.server_password.clone().unwrap_or_default();
            password.len() <= 128
                && tokio::task::spawn_blocking(move || crate::core::verify_secret(&password, &hash, false))
                    .await
                    .unwrap_or(false)
        }
    };

    let (out, mut outbound) = mpsc::channel(OUTBOUND_QUEUE);
    let request = ConnectRequest {
        request_id: id,
        uid,
        public_key: identity_key,
        device,
        nickname: hello.nickname,
        platform: hello.client.platform,
        out: out.clone(),
        ip: Some(ip),
        invite: hello.invite,
        password_ok,
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
