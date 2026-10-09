//! Native `vc/1` client: identity, handshake, request/response correlation
//! and the event stream. Media (WebRTC + audio devices) builds on top of it.

pub mod identity;
pub mod teamspeak;
pub mod ts_identities;
pub mod ts_identity;
pub mod ts_import;
pub mod voice;

use std::{collections::HashMap, time::Duration};

use futures::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde_json::Value;
use thiserror::Error;
use tokio::{
    sync::{mpsc, oneshot},
    time::timeout,
};
use tokio_tungstenite::tungstenite::Message;
use tracing::debug;
use vc_proto::{
    ChannelId, ChatMessage, ChatTarget, ClientFrame, ClientSoftware, ErrorBody, Event, Hello, PROTOCOL_VERSION,
    Request, ServerFrame, Welcome,
};

pub use identity::Identity;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("server refused: {0:?}")]
    Server(ErrorBody),
    #[error("connection closed")]
    Closed,
    #[error("timed out")]
    Timeout,
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("network error: {0}")]
    Network(Box<tokio_tungstenite::tungstenite::Error>),
}

impl From<tokio_tungstenite::tungstenite::Error> for ClientError {
    fn from(e: tokio_tungstenite::tungstenite::Error) -> Self {
        Self::Network(Box::new(e))
    }
}

pub struct ConnectOptions {
    /// `ws://host:port/ws` or `wss://host/ws`.
    pub url: String,
    pub nickname: String,
    pub server_password: Option<String>,
    pub identity: Identity,
    pub software: ClientSoftware,
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

type Pending = oneshot::Sender<Result<Value, ErrorBody>>;

/// Cheap, cloneable handle for issuing requests.
#[derive(Clone)]
pub struct Connection {
    outgoing: mpsc::Sender<(Request, Pending)>,
}

pub struct Connected {
    pub connection: Connection,
    pub welcome: Welcome,
    /// Server events in order. Ends when the connection closes.
    pub events: mpsc::Receiver<Event>,
}

pub async fn connect(options: ConnectOptions) -> Result<Connected, ClientError> {
    let (mut ws, _) = timeout(HANDSHAKE_TIMEOUT, tokio_tungstenite::connect_async(&options.url))
        .await
        .map_err(|_| ClientError::Timeout)??;

    let challenge = match read_frame(&mut ws).await? {
        ServerFrame::Event(Event::Challenge(challenge)) => challenge,
        other => {
            return Err(ClientError::Protocol(format!("expected challenge, got {other:?}")));
        }
    };
    if challenge.protocol != PROTOCOL_VERSION {
        return Err(ClientError::Protocol(format!("server speaks vc/{}", challenge.protocol)));
    }
    let public_key = options.identity.public_key();
    let hello = ClientFrame {
        id: 1,
        request: Request::Hello(Hello {
            protocol: PROTOCOL_VERSION,
            nickname: options.nickname,
            signature: options.identity.sign_challenge(&challenge.nonce),
            public_key,
            server_password: options.server_password,
            invite: None,
            device: None,
            client: options.software,
        }),
    };
    ws.send(Message::Text(serde_json::to_string(&hello).expect("serializable").into())).await?;
    let welcome = match read_frame(&mut ws).await? {
        ServerFrame::Ok { re: 1, ok } => serde_json::from_value(serde_json::to_value(ok).expect("serializable"))
            .map_err(|e| ClientError::Protocol(e.to_string()))?,
        ServerFrame::Err { err, .. } => return Err(ClientError::Server(err)),
        other => {
            return Err(ClientError::Protocol(format!("expected welcome, got {other:?}")));
        }
    };

    let (outgoing, mut requests) = mpsc::channel::<(Request, Pending)>(64);
    let (events_tx, events) = mpsc::channel(1024);
    tokio::spawn(async move {
        let mut pending: HashMap<u32, Pending> = HashMap::new();
        let mut next_id: u32 = 2;
        loop {
            tokio::select! {
                request = requests.recv() => {
                    let Some((request, reply)) = request else { break };
                    let id = next_id;
                    next_id = next_id.wrapping_add(1).max(2);
                    let text = serde_json::to_string(&ClientFrame { id, request }).expect("serializable");
                    if ws.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                    pending.insert(id, reply);
                }
                message = ws.next() => {
                    let text = match message {
                        Some(Ok(Message::Text(text))) => text,
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                        Some(Ok(_)) => continue,
                    };
                    // Replies are decoded as JSON first: `Response` is untagged, so
                    // the caller, who knows what it asked for, picks the type.
                    let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
                    if let Some(re) = value.get("re").and_then(Value::as_u64) {
                        if let Some(reply) = pending.remove(&(re as u32)) {
                            let result = match value.get("err") {
                                Some(err) => Err(serde_json::from_value(err.clone()).unwrap_or_else(|_| {
                                    ErrorBody::new(vc_proto::ErrorCode::Internal, "malformed error")
                                })),
                                None => Ok(value.get("ok").cloned().unwrap_or(Value::Null)),
                            };
                            let _ = reply.send(result);
                        }
                        continue;
                    }
                    match serde_json::from_value::<Event>(value) {
                        Ok(event) => {
                            if events_tx.send(event).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => debug!("ignoring unknown event: {e}"),
                    }
                }
            }
        }
        let _ = ws.close(None).await;
    });

    Ok(Connected { connection: Connection { outgoing }, welcome, events })
}

async fn read_frame<S>(ws: &mut S) -> Result<ServerFrame, ClientError>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match timeout(HANDSHAKE_TIMEOUT, ws.next()).await.map_err(|_| ClientError::Timeout)? {
            Some(Ok(Message::Text(text))) => {
                return serde_json::from_str(&text).map_err(|e| ClientError::Protocol(e.to_string()));
            }
            Some(Ok(Message::Close(_))) | None => return Err(ClientError::Closed),
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(e.into()),
        }
    }
}

impl Connection {
    /// Sends a request and returns the raw `ok` payload.
    pub async fn request(&self, request: Request) -> Result<Value, ClientError> {
        let (tx, rx) = oneshot::channel();
        self.outgoing.send((request, tx)).await.map_err(|_| ClientError::Closed)?;
        match timeout(REQUEST_TIMEOUT, rx).await {
            Err(_) => Err(ClientError::Timeout),
            Ok(Err(_)) => Err(ClientError::Closed),
            Ok(Ok(result)) => result.map_err(ClientError::Server),
        }
    }

    pub async fn request_as<T: DeserializeOwned>(&self, request: Request) -> Result<T, ClientError> {
        let value = self.request(request).await?;
        serde_json::from_value(value).map_err(|e| ClientError::Protocol(e.to_string()))
    }

    pub async fn join(&self, channel: ChannelId, password: Option<String>) -> Result<(), ClientError> {
        self.request(Request::ChannelJoin { channel, password }).await.map(drop)
    }

    pub async fn chat(&self, target: ChatTarget, text: impl Into<String>) -> Result<ChatMessage, ClientError> {
        self.request_as(Request::ChatSend { target, text: text.into(), mentions: Vec::new(), attachments: Vec::new() })
            .await
    }

    /// Exchanges a WebRTC offer for the server's answer SDP.
    pub async fn voice_offer(&self, sdp: String) -> Result<String, ClientError> {
        let value = self.request(Request::VoiceOffer { sdp }).await?;
        value["sdp"].as_str().map(str::to_owned).ok_or_else(|| ClientError::Protocol("answer without sdp".into()))
    }
}
