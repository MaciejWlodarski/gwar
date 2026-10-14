//! WebSocket gateway for the `vc/4` protocol: authentication, framing,
//! rate limiting and the per-connection write queue.

mod transport;

use std::{
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    body::Body,
    extract::{ConnectInfo, Request as HttpRequest, State},
    http::{HeaderMap, HeaderName, StatusCode},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signer, SigningKey};
use futures::{SinkExt, StreamExt};
use hyper_util::rt::TokioIo;
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message, Utf8Bytes,
        handshake::server::create_response_with_body,
        protocol::{Role, WebSocketConfig},
    },
};

use tokio::{sync::mpsc, time::timeout};
use tracing::debug;
use vc_proto::{
    Challenge, ClientFrame, ErrorBody, ErrorCode, Event, LeaveReason, PROTOCOL_VERSION, Request,
    Response as ProtocolResponse, ServerFrame, ServerProof, server_proof_statement,
};

use crate::{
    core::{ConnectRequest, CoreHandle, OUTBOUND_QUEUE, Outbound, ServerPassword, encode},
    identity,
};

#[derive(Clone)]
pub struct Gateway {
    pub core: CoreHandle,
    pub preauth: crate::preauth::PreAuth,
    /// Argon2 hash of the server password, if one is set (changes with settings).
    pub password: ServerPassword,
    pub public_endpoints: Arc<HashSet<String>>,
    pub insecure_any_host: bool,
    pub server_key: Arc<SigningKey>,
    /// Header a reverse proxy on this host sets to the client's address.
    pub client_ip_header: Option<HeaderName>,
}

type WebSocket = WebSocketStream<transport::Transport<TokioIo<hyper::upgrade::Upgraded>>>;

const HELLO_TIMEOUT: Duration = Duration::from_secs(15);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const PING_INTERVAL: Duration = Duration::from_secs(20);
const MAX_FRAME: usize = 96 * 1024;
const AUTHENTICATED_MAX_FRAME: usize = 640 * 1024;
const RATE_BURST: f64 = 40.0;
/// Refused frames in a row (each still answered) after which the connection is dropped:
/// refusing costs work too, so a client that ignores `rate_limited` is cut off.
const RATE_OVERRUN: u32 = 80;
const RATE_PER_SEC: f64 = 20.0;

pub async fn upgrade(
    State(gateway): State<Gateway>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut request: HttpRequest,
) -> Response {
    let ip = client_ip(peer, request.headers(), gateway.client_ip_header.as_ref());
    let Some(permit) = gateway.preauth.connection(ip) else {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    let Ok(response) = create_response_with_body(&request, Body::empty) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(upgrade) = request.extensions_mut().remove::<hyper::upgrade::OnUpgrade>() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    tokio::spawn(async move {
        let _permit = permit;
        let Ok(upgraded) = upgrade.await else { return };
        let config = WebSocketConfig::default()
            .read_buffer_size(16 * 1024)
            .max_message_size(Some(AUTHENTICATED_MAX_FRAME))
            .max_frame_size(Some(AUTHENTICATED_MAX_FRAME));
        let socket = WebSocketStream::from_raw_socket(
            transport::Transport::new(TokioIo::new(upgraded)),
            Role::Server,
            Some(config),
        )
        .await;
        connection(socket, gateway, ip).await;
    });
    response
}

/// The client's address. Behind a reverse proxy on the same host the TCP peer is
/// loopback; only then, and only when the operator names the header the proxy
/// sets (`--client-ip-header`), is that header believed: a proxy that passes a
/// client's own `X-Real-IP` through would otherwise let anyone choose their address.
/// For a list (`X-Forwarded-For`) the last entry, the one the proxy added, counts.
pub fn client_ip(peer: SocketAddr, headers: &HeaderMap, trusted: Option<&HeaderName>) -> IpAddr {
    if !peer.ip().to_canonical().is_loopback() {
        return peer.ip();
    }
    let Some(name) = trusted else {
        static WARNED: std::sync::Once = std::sync::Once::new();
        if headers.contains_key("x-real-ip") || headers.contains_key("x-forwarded-for") {
            WARNED.call_once(|| {
                tracing::warn!(
                    "requests come through a local proxy, but --client-ip-header is not set: \
                     every client counts as {} for per-address limits and bans",
                    peer.ip()
                )
            });
        }
        return peer.ip();
    };
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit(',').next())
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(peer.ip())
}

fn text(frame: &ServerFrame) -> Message {
    Message::Text(Utf8Bytes::from(&*encode(frame)))
}

fn error(re: u32, code: ErrorCode, message: &str) -> Message {
    text(&ServerFrame::Err { re, err: ErrorBody::new(code, message) })
}

async fn next_text<S>(stream: &mut S, wait: Duration, limit: usize) -> Option<Utf8Bytes>
where
    S: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    timeout(wait, async {
        loop {
            match stream.next().await? {
                Ok(Message::Text(text)) if text.len() <= limit => return Some(text),
                Ok(Message::Ping(_) | Message::Pong(_)) => continue,
                _ => return None,
            }
        }
    })
    .await
    .ok()
    .flatten()
}

async fn connection(mut socket: WebSocket, gateway: Gateway, ip: IpAddr) {
    let Some(server) = gateway.core.info().await else { return };
    let nonce = identity::new_nonce();
    let challenge = Challenge {
        protocol: PROTOCOL_VERSION,
        server_nonce: nonce.clone(),
        server_pk: URL_SAFE_NO_PAD.encode(gateway.server_key.verifying_key().as_bytes()),
        server,
        password_required: gateway.password.load().is_some(),
    };
    if socket.send(text(&ServerFrame::Event(Event::Challenge(challenge.clone())))).await.is_err() {
        return;
    }

    let Some(raw) = next_text(&mut socket, HELLO_TIMEOUT, MAX_FRAME).await else { return };
    if !gateway.preauth.hello(ip) {
        let _ = socket.send(error(0, ErrorCode::RateLimited, "too many hellos")).await;
        return;
    }
    let (id, hello) = match serde_json::from_str::<ClientFrame>(&raw) {
        Ok(ClientFrame { id, request: Request::Hello(hello) }) => (id, hello),
        Ok(frame) => {
            let _ = socket.send(error(frame.id, ErrorCode::NotAuthenticated, "send hello first")).await;
            return;
        }
        Err(_) => {
            let _ = socket.send(error(0, ErrorCode::BadRequest, "malformed hello")).await;
            return;
        }
    };
    if [&hello.nickname, &hello.client.name, &hello.client.version].iter().any(|field| field.contains(['\r', '\n'])) {
        let _ = socket.send(error(id, ErrorCode::BadRequest, "invalid hello fields")).await;
        return;
    }
    if hello.protocol != PROTOCOL_VERSION {
        let _ = socket.send(error(id, ErrorCode::BadRequest, "unsupported protocol version")).await;
        return;
    }
    if !gateway.insecure_any_host && !gateway.public_endpoints.contains(&hello.endpoint) {
        let _ = socket.send(error(id, ErrorCode::NotAuthenticated, "this signature is for another server")).await;
        return;
    }
    if hello.expected_server_pk != challenge.server_pk || hello.server_nonce != nonce {
        let _ = socket.send(error(id, ErrorCode::NotAuthenticated, "wrong server key or nonce")).await;
        return;
    }
    let Some(mut uid) = identity::verify_hello(&hello) else {
        let _ = socket.send(error(id, ErrorCode::NotAuthenticated, "invalid identity signature")).await;
        return;
    };
    // A Gwar Connect device speaks for its account: the identity is the account key.
    let mut identity_key = hello.client_signing_pk.clone();
    let mut device = None;
    if let Some(certificate) = &hello.certificate {
        match identity::verify_device(certificate, &hello.client_signing_pk, crate::core::now_ms()) {
            Ok(account_uid) => {
                uid = account_uid;
                identity_key = certificate.account_key.clone();
                device = Some(certificate.device_key.clone());
            }
            Err((code, problem)) => {
                let _ = socket.send(error(id, code, problem)).await;
                return;
            }
        }
    }
    if let Some(key) = &device
        && !gateway.core.device_authorized(&identity_key, key).await
    {
        let _ =
            socket.send(error(id, ErrorCode::NotAuthenticated, "this device was signed out of its Gwar account")).await;
        return;
    }
    let proof = ServerProof {
        server_proof: URL_SAFE_NO_PAD.encode(
            gateway.server_key.sign(server_proof_statement(&hello).expect("validated hello").as_bytes()).to_bytes(),
        ),
    };
    if socket.send(text(&ServerFrame::Ok { re: id, ok: ProtocolResponse::ServerProof(proof) })).await.is_err() {
        return;
    }
    let Some(raw) = next_text(&mut socket, HELLO_TIMEOUT, MAX_FRAME).await else { return };
    if !gateway.preauth.hello(ip) {
        let _ = socket.send(error(0, ErrorCode::RateLimited, "too many authentication frames")).await;
        return;
    }
    let (id, join) = match serde_json::from_str::<ClientFrame>(&raw) {
        Ok(ClientFrame { id, request: Request::Join(join) }) => (id, join),
        Ok(frame) => {
            let _ = socket.send(error(frame.id, ErrorCode::NotAuthenticated, "send join after server proof")).await;
            return;
        }
        Err(_) => {
            let _ = socket.send(error(0, ErrorCode::BadRequest, "malformed join")).await;
            return;
        }
    };
    // A certificate may expire while waiting for join.
    if let Some(certificate) = &hello.certificate
        && let Err((code, problem)) =
            identity::verify_device(certificate, &hello.client_signing_pk, crate::core::now_ms())
    {
        let _ = socket.send(error(id, code, problem)).await;
        return;
    }
    // Whether the password matched; the core decides what that means (members
    // come back without it, an invite admits newcomers).
    let current = gateway.password.load_full();
    let password_ok = match current.as_ref().clone() {
        None => true,
        Some(hash) => {
            let Ok(permit) = gateway.preauth.passwords.clone().try_acquire_owned() else {
                let _ = socket.send(error(id, ErrorCode::RateLimited, "password verification busy")).await;
                return;
            };
            let password = join.server_password.clone().unwrap_or_default();
            password.len() <= 128
                && tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    crate::core::verify_secret(&password, &hash, false)
                })
                .await
                .unwrap_or(false)
        }
    };

    let (out, mut outbound) = mpsc::channel(OUTBOUND_QUEUE);
    let request = ConnectRequest {
        request_id: id,
        hello_key: hello.client_signing_pk.clone(),
        certificate: hello.certificate.clone(),
        uid,
        public_key: identity_key,
        device,
        device_expires_at: hello.certificate.as_ref().map(|c| c.expires_at),
        nickname: hello.nickname,
        platform: hello.client.platform,
        out: out.clone(),
        ip: Some(ip),
        invite: join.invite,
        password_ok,
    };
    let session = gateway.core.connect(request).await;

    if session.is_some() {
        socket.get_mut().authenticated = true;
    }
    let (mut sink, mut stream) = socket.split();
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
    let mut refused = 0u32;
    let reason = loop {
        let raw = tokio::select! {
            raw = next_text(&mut stream, IDLE_TIMEOUT, AUTHENTICATED_MAX_FRAME) => raw,
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
        // Every frame costs a token, malformed ones too.
        let allowed = tokens >= 1.0;
        if allowed {
            tokens -= 1.0;
            refused = 0;
        } else {
            refused += 1;
            if refused > RATE_OVERRUN {
                debug!(session, "dropping a connection that keeps sending past the rate limit");
                break LeaveReason::Timeout;
            }
        }
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
        if !allowed {
            let reply = ServerFrame::Err { re: frame.id, err: ErrorBody::new(ErrorCode::RateLimited, "slow down") };
            if out.try_send(Outbound::Frame(encode(&reply))).is_err() {
                break LeaveReason::Timeout;
            }
            continue;
        }
        gateway.core.request(session, frame.id, frame.request).await;
    };
    gateway.core.disconnect(session, reason).await;
    drop(out);
    if !writer_done {
        let _ = writer.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwarded_addresses_count_only_from_a_named_header_behind_loopback() {
        let local: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let remote: SocketAddr = "198.51.100.7:5000".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", "203.0.113.9".parse().unwrap());
        headers.insert("x-forwarded-for", "192.0.2.66, 203.0.113.10".parse().unwrap());
        let real = HeaderName::from_static("x-real-ip");
        let forwarded = HeaderName::from_static("x-forwarded-for");
        // Without a configured header nothing a client sends is believed.
        assert_eq!(client_ip(local, &headers, None), local.ip());
        // Nor from anyone but a local proxy.
        assert_eq!(client_ip(remote, &headers, Some(&real)), remote.ip());
        assert_eq!(client_ip(local, &headers, Some(&real)).to_string(), "203.0.113.9");
        // A local IPv4 proxy seen through a dual-stack listener is still local.
        let mapped: SocketAddr = "[::ffff:127.0.0.1]:5000".parse().unwrap();
        assert_eq!(client_ip(mapped, &headers, Some(&real)).to_string(), "203.0.113.9");
        // The proxy appends its peer: the client's own first entry does not count.
        assert_eq!(client_ip(local, &headers, Some(&forwarded)).to_string(), "203.0.113.10");
        // A missing or unparsable header falls back to the peer.
        headers.insert("x-real-ip", "not an address".parse().unwrap());
        assert_eq!(client_ip(local, &headers, Some(&real)), local.ip());
        assert_eq!(client_ip(local, &HeaderMap::new(), Some(&real)), local.ip());
    }
}
