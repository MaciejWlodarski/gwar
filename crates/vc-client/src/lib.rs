//! Native `vc/3` client: identity, handshake, request/response correlation
//! and the event stream. Media (WebRTC + audio devices) builds on top of it.

pub mod identity;
pub mod teamspeak;
pub mod ts_identities;
pub mod ts_identity;
pub mod ts_import;
pub mod voice;

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, VerifyingKey};
use futures::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde_json::Value;
use thiserror::Error;
use tokio::{
    sync::{mpsc, oneshot},
    time::timeout,
};
use tokio_tungstenite::tungstenite::{Message, protocol::WebSocketConfig};
use tracing::debug;
use vc_proto::{
    ChannelId, ChatMessage, ChatTarget, ClientFrame, ClientSoftware, DeviceCertificate, ErrorBody, ErrorCode, Event,
    Hello, Join, MessageCheck, MessageSignature, PROTOCOL_VERSION, Request, Response, ServerFrame, Welcome,
    certificate_hash, decode_fixed, dm_conversation, hello_statement, message_body_hash, message_statement,
    normalize_endpoint, server_proof_statement, uid_for_key,
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
    pub invite: Option<String>,
    pub certificate: Option<DeviceCertificate>,
    /// A configured pin or invite fingerprint. Checked before signing.
    pub expected_server_key: Option<String>,
    /// Explicit first-contact candidate, without persistent pins. Off by default.
    pub trust_on_first_use: bool,
    /// Explicit per-server plaintext LAN opt-in; loopback needs no opt-in.
    pub insecure_lan: bool,
    pub identity: Identity,
    pub software: ClientSoftware,
}

impl ConnectOptions {
    pub fn new(url: String, identity: Identity, software: ClientSoftware) -> Self {
        Self {
            url,
            nickname: "gwar".into(),
            server_password: None,
            invite: None,
            certificate: None,
            expected_server_key: None,
            trust_on_first_use: false,
            insecure_lan: false,
            identity,
            software,
        }
    }
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// Largest server frame, in bytes: the web app's 2 Mi UTF-16 units in UTF-8.
const MAX_FRAME: usize = 6 * 1024 * 1024;
/// Requests awaiting a reply; more fail at once rather than piling up.
const MAX_PENDING: usize = 256;
/// Most clients a server can announce (`ServerInfo::max_clients`).
const MAX_CLIENTS: usize = 65_535;

type Pending = oneshot::Sender<Result<Value, ErrorBody>>;

/// Cheap, cloneable handle for issuing requests.
#[derive(Clone)]
pub struct Connection {
    outgoing: mpsc::Sender<(Request, Pending)>,
    /// Verified identity epoch for this server connection.
    pub server_key: String,
    identity: Identity,
    certificate: Option<DeviceCertificate>,
    uid: String,
    clients: Arc<RwLock<HashMap<u32, String>>>,
}

pub struct Connected {
    pub connection: Connection,
    pub welcome: Welcome,
    /// Server events in order. Ends when the connection closes, also when the
    /// server breaks the protocol's bounds. Chat messages and edits arrive only
    /// if [`Connection::verify_message`] verifies them or they come unsigned
    /// from TeamSpeak; the server's word alone is not enough.
    pub events: mpsc::Receiver<Event>,
}

pub async fn connect(options: ConnectOptions) -> Result<Connected, ClientError> {
    let endpoint =
        normalize_endpoint(&options.url).ok_or_else(|| ClientError::Protocol("invalid server URL".into()))?;
    let url = url::Url::parse(&endpoint).map_err(|e| ClientError::Protocol(e.to_string()))?;
    let host = url.host_str().unwrap_or_default();
    let loopback =
        host == "localhost" || host == "[::1]" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
    if url.scheme() == "ws" && !loopback && !options.insecure_lan {
        return Err(ClientError::Protocol("TLS required; plaintext LAN needs explicit opt-in".into()));
    }
    let limits = WebSocketConfig::default().max_message_size(Some(MAX_FRAME)).max_frame_size(Some(MAX_FRAME));
    let (mut ws, _) =
        timeout(HANDSHAKE_TIMEOUT, tokio_tungstenite::connect_async_with_config(&options.url, Some(limits), false))
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
    if decode_fixed::<32>(&challenge.server_pk).is_none() || decode_fixed::<32>(&challenge.server_nonce).is_none() {
        return Err(ClientError::Protocol("invalid challenge encoding".into()));
    }
    let key = VerifyingKey::from_bytes(&decode_fixed::<32>(&challenge.server_pk).expect("validated key"))
        .map_err(|e| ClientError::Protocol(e.to_string()))?;
    if key.is_weak() {
        return Err(ClientError::Protocol("weak server key".into()));
    }
    match &options.expected_server_key {
        Some(key) if key != &challenge.server_pk => {
            return Err(ClientError::Protocol("server identity changed".into()));
        }
        None if !options.trust_on_first_use => {
            return Err(ClientError::Protocol("expected server key required (TOFU disabled)".into()));
        }
        _ => {}
    }
    let mut hello = Hello {
        protocol: PROTOCOL_VERSION,
        nickname: options.nickname,
        endpoint,
        expected_server_pk: challenge.server_pk.clone(),
        server_nonce: challenge.server_nonce,
        client_nonce: URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>()),
        client_signing_pk: options.identity.public_key(),
        certificate_hash: certificate_hash(options.certificate.as_ref())
            .ok_or_else(|| ClientError::Protocol("invalid certificate".into()))?,
        signature: String::new(),
        certificate: options.certificate,
        client: options.software,
    };
    hello.signature = options
        .identity
        .sign(hello_statement(&hello).ok_or_else(|| ClientError::Protocol("invalid hello".into()))?.as_bytes());
    ws.send(Message::Text(
        serde_json::to_string(&ClientFrame { id: 1, request: Request::Hello(Box::new(hello.clone())) })
            .expect("serializable")
            .into(),
    ))
    .await?;
    let proof = match read_frame(&mut ws).await? {
        ServerFrame::Ok { re: 1, ok: Response::ServerProof(proof) } => proof,
        ServerFrame::Err { err, .. } => return Err(ClientError::Server(err)),
        _ => return Err(ClientError::Protocol("expected server proof".into())),
    };
    let sig = Signature::from_bytes(
        &decode_fixed::<64>(&proof.server_proof)
            .ok_or_else(|| ClientError::Protocol("invalid proof encoding".into()))?,
    );
    key.verify_strict(server_proof_statement(&hello).expect("validated hello").as_bytes(), &sig)
        .map_err(|_| ClientError::Protocol("invalid server proof".into()))?;
    ws.send(Message::Text(
        serde_json::to_string(&ClientFrame {
            id: 2,
            request: Request::Join(Join { server_password: options.server_password, invite: options.invite }),
        })
        .expect("serializable")
        .into(),
    ))
    .await?;
    let welcome = match read_frame(&mut ws).await? {
        ServerFrame::Ok { re: 2, ok: Response::Welcome(welcome) } => *welcome,
        ServerFrame::Err { err, .. } => return Err(ClientError::Server(err)),
        _ => return Err(ClientError::Protocol("expected welcome".into())),
    };

    let uid = hello
        .certificate
        .as_ref()
        .map(|c| uid_for_key(&decode_fixed::<32>(&c.account_key).expect("certificate checked")))
        .unwrap_or_else(|| options.identity.uid());
    let clients =
        Arc::new(RwLock::new(welcome.clients.iter().map(|c| (c.id, c.uid.clone())).collect::<HashMap<_, _>>()));
    let current_clients = clients.clone();
    let (me, server_key) = (uid.clone(), challenge.server_pk.clone());
    let (outgoing, mut requests) = mpsc::channel::<(Request, Pending)>(64);
    let (events_tx, events) = mpsc::channel(1024);
    tokio::spawn(async move {
        let mut pending: HashMap<u32, Pending> = HashMap::new();
        let mut next_id: u32 = 3;
        loop {
            tokio::select! {
                request = requests.recv() => {
                    let Some((request, reply)) = request else { break };
                    pending.retain(|_, waiting| !waiting.is_closed());
                    if pending.len() >= MAX_PENDING {
                        let _ = reply.send(Err(ErrorBody::new(ErrorCode::RateLimited, "too many requests awaiting a reply")));
                        continue;
                    }
                    let id = next_id;
                    next_id = next_id.wrapping_add(1).max(3);
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
                            match &event {
                                Event::ClientJoined(c) | Event::ClientUpdated(c) => {
                                    let mut clients = current_clients.write().unwrap();
                                    if clients.len() >= MAX_CLIENTS && !clients.contains_key(&c.id) {
                                        debug!("server announced more than {MAX_CLIENTS} clients");
                                        break;
                                    }
                                    clients.insert(c.id, c.uid.clone());
                                }
                                Event::ClientLeft { client, .. } => { current_clients.write().unwrap().remove(client); }
                                Event::ChatMessage(m) | Event::ChatEdited(m) => {
                                    let check = verify(m, &me, &server_key, &current_clients.read().unwrap());
                                    if matches!(check, MessageCheck::Invalid | MessageCheck::Unsigned) {
                                        debug!("dropping chat message {} from {}: {check:?}", m.id, m.author_uid);
                                        continue;
                                    }
                                }
                                _ => {}
                            }
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

    Ok(Connected {
        connection: Connection {
            outgoing,
            server_key: challenge.server_pk,
            identity: options.identity,
            certificate: hello.certificate,
            uid,
            clients,
        },
        welcome,
        events,
    })
}

/// The conversation `msg` must be signed for, as its reader sees it: a direct
/// message from someone else must be to us, and our own names its recipient.
fn verify(msg: &ChatMessage, me: &str, server_key: &str, clients: &HashMap<u32, String>) -> MessageCheck {
    let conversation = match msg.target {
        ChatTarget::Channel(id) => format!("channel:{id}"),
        ChatTarget::Server => "server".into(),
        ChatTarget::Client(_) if msg.author_uid != me => dm_conversation(me, &msg.author_uid),
        ChatTarget::Client(to) => match clients.get(&to) {
            Some(peer) => dm_conversation(me, peer),
            None if msg.signature.is_none() => String::new(),
            None => return MessageCheck::Invalid,
        },
    };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64;
    vc_proto::verify_message(msg, server_key, &conversation, now)
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
    /// What `msg`'s own signature proves here. Live chat events are checked
    /// before delivery; messages in replies, such as `chat.history`, are the
    /// server's word until checked with this. Device revocations and the
    /// continuity of edits are not checked.
    pub fn verify_message(&self, msg: &ChatMessage) -> MessageCheck {
        verify(msg, &self.uid, &self.server_key, &self.clients.read().unwrap())
    }

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
        self.send_chat(target, text.into(), Vec::new(), &[]).await
    }

    pub async fn send_chat(
        &self,
        target: ChatTarget,
        text: String,
        mentions: Vec<String>,
        attachments: &[vc_proto::Attachment],
    ) -> Result<ChatMessage, ClientError> {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

        let signed_target = match target {
            ChatTarget::Channel(id) => format!("channel:{id}"),
            ChatTarget::Server => "server".into(),
            ChatTarget::Client(id) => vc_proto::dm_conversation(
                &self.uid,
                self.clients
                    .read()
                    .unwrap()
                    .get(&id)
                    .ok_or_else(|| ClientError::Protocol("unknown recipient".into()))?,
            ),
        };
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64;
        let mut signature = MessageSignature {
            server_key: self.server_key.clone(),
            conversation: signed_target.clone(),
            author_uid: self.uid.clone(),
            key: self.identity.public_key(),
            certificate: self.certificate.clone(),
            certificate_hash: certificate_hash(self.certificate.as_ref())
                .ok_or_else(|| ClientError::Protocol("invalid certificate".into()))?,
            message_id: URL_SAFE_NO_PAD.encode(rand::random::<[u8; 16]>()),
            revision: 0,
            previous_revision_hash: "-".into(),
            created_at: now,
            signed_at: now,
            body_hash: message_body_hash(&text, &mentions, attachments),
            signature: String::new(),
        };
        signature.signature = self.identity.sign(
            message_statement(&self.server_key, &signed_target, &self.uid, &signature, &signature.body_hash).as_bytes(),
        );
        self.request_as(Request::ChatSend {
            target,
            text,
            mentions,
            attachments: attachments.iter().map(|a| a.id.clone()).collect(),
            signature,
        })
        .await
    }

    /// Exchanges a WebRTC offer for the server's answer SDP.
    pub async fn voice_offer(&self, sdp: String) -> Result<String, ClientError> {
        let value = self.request(Request::VoiceOffer { sdp }).await?;
        value["sdp"].as_str().map(str::to_owned).ok_or_else(|| ClientError::Protocol("answer without sdp".into()))
    }
}
