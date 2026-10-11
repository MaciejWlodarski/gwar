//! Test harness: an in-process server and a small `vc/3` WebSocket client.
#![allow(dead_code)]

use std::{collections::HashMap, collections::VecDeque, future::Future, time::Duration};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ed25519_dalek::{Signer, SigningKey};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::{net::TcpStream, time::timeout};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
use vc_proto::{
    Attachment, ChatMessage, ErrorBody, Hello, MessageSignature, Welcome, normalize_endpoint, server_proof_statement,
};
use vc_server::{Config, Running};

/// Upper bound for any single wait so a broken test fails instead of hanging.
pub const TIMEOUT: Duration = Duration::from_secs(3);

pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub async fn within<T>(what: &str, fut: impl Future<Output = T>) -> T {
    match timeout(TIMEOUT, fut).await {
        Ok(v) => v,
        Err(_) => panic!("timed out waiting for {what}"),
    }
}

// ------------------------------------------------------------------ server

pub struct TestServer {
    pub running: Running,
    /// The one-time admin token of the fresh database.
    pub admin_token: String,
}

pub fn base_config() -> Config {
    Config {
        database: None,
        http_bind: "127.0.0.1:0".parse().unwrap(),
        media_bind: "127.0.0.1:0".parse().unwrap(),
        media_advertise: "127.0.0.1:0".parse().unwrap(),
        max_clients: 64,
        server_password: None,
        web_root: None,
        ice_servers: Vec::new(),
        teamspeak: None,
        public_url: None,
        data_dir: None,
        public_ip: None,
        insecure_any_host: true,
        public_endpoints: Vec::new(),
        upload_limit: 10 * 1024 * 1024,
        files_dir: None,
        web_origins: Vec::new(),
        tls: None,
        redirect_http: None,
        connect_url: None,
    }
}

impl TestServer {
    pub async fn start() -> Self {
        Self::start_with(|_| {}).await
    }

    pub async fn start_with(configure: impl FnOnce(&mut Config)) -> Self {
        let mut config = base_config();
        configure(&mut config);
        let mut running = vc_server::start(config).await.expect("server starts");
        let admin_token = running.admin_token.take().expect("fresh database issues an admin token");
        Self { running, admin_token }
    }

    pub fn url(&self) -> String {
        format!("ws://{}/ws", self.running.http)
    }

    /// `http://host:port` of the server, for uploads and downloads.
    pub fn http(&self) -> String {
        format!("http://{}", self.running.http)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        for task in &self.running.tasks {
            task.abort();
        }
    }
}

// ---------------------------------------------------------------- identity

pub fn hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    B64.encode(Sha256::digest(bytes))
}

pub fn new_key() -> SigningKey {
    SigningKey::from_bytes(&rand::random::<[u8; 32]>())
}

pub fn public_key(key: &SigningKey) -> String {
    B64.encode(key.verifying_key().as_bytes())
}

pub fn sign(key: &SigningKey, hello: &Value) -> String {
    let h = hello;
    let statement = format!(
        "gwar client hello v3\n{}\n{}\n{}\n{}\n{}\n{}",
        h["endpoint"].as_str().unwrap(),
        h["expected_server_pk"].as_str().unwrap(),
        h["server_nonce"].as_str().unwrap(),
        h["client_nonce"].as_str().unwrap(),
        h["client_signing_pk"].as_str().unwrap(),
        h["certificate_hash"].as_str().unwrap()
    );
    B64.encode(key.sign(statement.as_bytes()).to_bytes())
}

pub fn set_certificate(hello: &mut Value, cert: Value, key: &SigningKey) {
    let certificate: vc_proto::DeviceCertificate = serde_json::from_value(cert.clone()).unwrap();
    hello["certificate_hash"] = json!(vc_proto::sha256_b64(
        format!(
            "gwar device v1\n{}\n{}\n{}\n{}\n{}",
            certificate.account_key,
            certificate.device_key,
            certificate.issued_at,
            certificate.expires_at,
            certificate.signature
        )
        .as_bytes()
    ));
    hello["certificate"] = cert;
    hello["signature"] = json!(sign(key, hello));
}

pub fn uid_of(key: &SigningKey) -> String {
    vc_server::identity::uid_for_key(key.verifying_key().as_bytes())
}

/// A device certificate signed by `account` for `device`.
pub fn certificate(account: &SigningKey, device: &SigningKey, issued_at: i64, expires_at: i64) -> Value {
    let (account_key, device_key) =
        (B64.encode(account.verifying_key().as_bytes()), B64.encode(device.verifying_key().as_bytes()));
    let statement = format!("gwar device v1\n{account_key}\n{device_key}\n{issued_at}\n{expires_at}");
    json!({"account_key": account_key, "device_key": device_key, "issued_at": issued_at, "expires_at": expires_at,
           "signature": B64.encode(account.sign(statement.as_bytes()).to_bytes())})
}

// ---------------------------------------------------------------- raw conn

/// A connection that has received the challenge but not yet said hello.
pub struct RawConn {
    pub ws: Ws,
    pub challenge: Value,
    pub nonce: String,
    pub endpoint: String,
}

pub async fn open(server: &TestServer) -> RawConn {
    let (mut ws, _) = within("websocket connect", connect_async(server.url())).await.expect("connect");
    let frame = recv(&mut ws).await.expect("challenge frame");
    assert_eq!(frame["ev"], "challenge", "first server frame must be the challenge: {frame}");
    let challenge = frame["d"].clone();
    let nonce = challenge["server_nonce"].as_str().expect("nonce").to_owned();
    RawConn { ws, challenge, nonce, endpoint: normalize_endpoint(&server.url()).unwrap() }
}

impl RawConn {
    pub async fn send(&mut self, frame: Value) {
        self.ws.send(Message::text(frame.to_string())).await.expect("send");
    }

    pub async fn recv(&mut self) -> Option<Value> {
        recv(&mut self.ws).await
    }

    /// Reads until the server closes the connection; panics if frames keep coming.
    pub async fn expect_closed(&mut self) {
        within("connection close", async {
            if let Some(frame) = self.recv().await {
                panic!("expected the connection to close, got {frame}");
            }
        })
        .await;
    }

    /// Body of a valid hello. Join secrets live in `join` in the test envelope.
    pub fn hello(&self, key: &SigningKey, nickname: &str, password: Option<&str>) -> Value {
        let mut h = json!({
            "protocol": vc_proto::PROTOCOL_VERSION, "nickname": nickname,
            "endpoint": self.endpoint, "expected_server_pk": self.challenge["server_pk"],
            "server_nonce": self.nonce, "client_nonce": B64.encode(rand::random::<[u8; 32]>()),
            "client_signing_pk": public_key(key), "certificate_hash": "-", "signature": "",
            "client": {"name": "test", "version": "0", "platform": "web"},
        });
        h["signature"] = json!(sign(key, &h));
        h["join"] = json!({"server_password": password});
        h
    }

    pub async fn send_hello(&mut self, mut d: Value) {
        d.as_object_mut().unwrap().remove("join");
        self.send(json!({"id": 1, "op": "hello", "d": d})).await;
    }

    /// Sends `d` as hello and returns the error it was answered with.
    pub async fn rejected(&mut self, d: Value) -> ErrorBody {
        self.send_hello(d).await;
        let frame = self.recv().await.expect("reply to hello");
        assert_eq!(frame["re"], 1, "reply must echo the hello id: {frame}");
        serde_json::from_value(frame["err"].clone()).unwrap_or_else(|_| panic!("expected an error, got {frame}"))
    }
}

/// Next text frame as JSON; `None` once the socket is closed.
pub async fn recv(ws: &mut Ws) -> Option<Value> {
    within("a frame from the server", async {
        loop {
            match ws.next().await? {
                Ok(Message::Text(text)) => return Some(serde_json::from_str(&text).expect("server sends JSON")),
                Ok(Message::Close(_)) | Err(_) => return None,
                Ok(_) => continue,
            }
        }
    })
    .await
}

// ------------------------------------------------------------------ client

pub struct Client {
    ws: Ws,
    pub key: SigningKey,
    pub welcome: Welcome,
    pub challenge: Value,
    pub server_key: String,
    pub certificate: Option<vc_proto::DeviceCertificate>,
    pub files: HashMap<String, Attachment>,
    pub messages: HashMap<u32, ChatMessage>,
    next_id: u32,
    replies: HashMap<u32, Result<Value, ErrorBody>>,
    events: VecDeque<Value>,
}

impl Client {
    pub async fn connect(server: &TestServer, nickname: &str) -> Self {
        Self::connect_as(server, new_key(), nickname, None).await.expect("hello accepted")
    }

    pub async fn connect_as(
        server: &TestServer,
        key: SigningKey,
        nickname: &str,
        password: Option<&str>,
    ) -> Result<Self, ErrorBody> {
        let hello = |raw: &RawConn, key: &SigningKey| raw.hello(key, nickname, password);
        Self::connect_with(server, key, hello).await
    }

    /// Connects with a hello body built by `hello` (e.g. to add an invite).
    pub async fn connect_with(
        server: &TestServer,
        key: SigningKey,
        hello: impl FnOnce(&RawConn, &SigningKey) -> Value,
    ) -> Result<Self, ErrorBody> {
        let mut raw = open(server).await;
        let mut hello = hello(&raw, &key);
        let join = hello.as_object_mut().unwrap().remove("join").unwrap_or_else(|| json!({}));
        raw.send_hello(hello.clone()).await;
        let frame = raw.recv().await.expect("reply to hello");
        assert_eq!(frame["re"], 1, "{frame}");
        if let Some(err) = frame.get("err") {
            return Err(serde_json::from_value(err.clone()).unwrap());
        }
        let h: Hello = serde_json::from_value(hello).unwrap();
        let pk = vc_proto::decode_fixed::<32>(&h.expected_server_pk).unwrap();
        let sig = vc_proto::decode_fixed::<64>(frame["ok"]["server_proof"].as_str().unwrap()).unwrap();
        ed25519_dalek::VerifyingKey::from_bytes(&pk)
            .unwrap()
            .verify_strict(server_proof_statement(&h).unwrap().as_bytes(), &ed25519_dalek::Signature::from_bytes(&sig))
            .unwrap();
        raw.send(json!({"id": 2, "op": "join", "d": join})).await;
        let frame = raw.recv().await.expect("reply to join");
        assert_eq!(frame["re"], 2, "{frame}");
        if let Some(err) = frame.get("err") {
            return Err(serde_json::from_value(err.clone()).unwrap());
        }
        let welcome = serde_json::from_value(frame["ok"].clone()).expect("welcome");
        Ok(Self {
            ws: raw.ws,
            server_key: h.expected_server_pk.clone(),
            certificate: h.certificate.clone(),
            files: HashMap::new(),
            messages: HashMap::new(),
            key,
            welcome,
            challenge: raw.challenge,
            next_id: 2,
            replies: HashMap::new(),
            events: VecDeque::new(),
        })
    }

    pub fn signature(
        &self,
        target: &str,
        text: &str,
        mentions: &[String],
        attachments: &[Attachment],
    ) -> MessageSignature {
        let now = vc_server::core::now_ms();
        let mut sig = MessageSignature {
            server_key: self.server_key.clone(),
            conversation: target.into(),
            author_uid: self.welcome.uid.clone(),
            key: public_key(&self.key),
            certificate: self.certificate.clone(),
            certificate_hash: vc_proto::certificate_hash(self.certificate.as_ref()).unwrap(),
            message_id: B64.encode(rand::random::<[u8; 16]>()),
            revision: 0,
            previous_revision_hash: "-".into(),
            created_at: now,
            signed_at: now,
            body_hash: vc_proto::message_body_hash(text, mentions, attachments),
            signature: String::new(),
        };
        self.resign(&mut sig, target, text, mentions, attachments);
        sig
    }

    pub fn resign(
        &self,
        sig: &mut MessageSignature,
        target: &str,
        text: &str,
        mentions: &[String],
        attachments: &[Attachment],
    ) {
        sig.body_hash = vc_proto::message_body_hash(text, mentions, attachments);
        let statement = vc_proto::message_statement(
            &sig.server_key,
            target,
            &self.welcome.uid,
            sig,
            &vc_proto::message_body_hash(text, mentions, attachments),
        );
        sig.signature = B64.encode(self.key.sign(statement.as_bytes()).to_bytes());
    }

    fn remember(&mut self, v: &Value) {
        if let Ok(message) = serde_json::from_value::<ChatMessage>(v.clone()) {
            self.messages.insert(message.id, message);
        }
        if let Some(messages) = v["messages"].as_array() {
            for message in messages {
                self.remember(message);
            }
        }
        if let Ok(client) = serde_json::from_value::<vc_proto::Client>(v.clone()) {
            self.welcome.clients.retain(|c| c.id != client.id);
            self.welcome.clients.push(client);
        }
    }

    pub fn id(&self) -> u32 {
        self.welcome.session
    }

    pub fn default_channel(&self) -> u32 {
        self.welcome.server.default_channel
    }

    /// Reads one frame from the socket and files it as a reply or an event.
    async fn pump(&mut self) -> Option<()> {
        let frame = recv(&mut self.ws).await?;
        self.remember(frame.get("ok").or_else(|| frame.get("d")).unwrap_or(&Value::Null));
        match frame.get("re").and_then(Value::as_u64) {
            Some(re) => {
                let result = match frame.get("err") {
                    Some(err) => Err(serde_json::from_value(err.clone()).expect("error body")),
                    None => Ok(frame["ok"].clone()),
                };
                self.replies.insert(re as u32, result);
            }
            None => self.events.push_back(frame),
        }
        Some(())
    }

    /// Sends a request without waiting for the reply.
    pub async fn send(&mut self, op: &str, mut d: Value) -> u32 {
        if op == "chat.send"
            && d["target"]["client"]
                .as_u64()
                .is_some_and(|id| !self.welcome.clients.iter().any(|c| u64::from(c.id) == id))
        {
            self.next_id += 1;
            let ping = self.next_id;
            self.ws
                .send(Message::text(json!({"id": ping, "op": "ping", "d": {}}).to_string()))
                .await
                .expect("send ping");
            let _ = self.reply(ping).await;
        }
        if matches!(op, "chat.send" | "chat.edit") && d.get("signature").is_none() {
            let old = d["message"].as_u64().and_then(|id| self.messages.get(&(id as u32))).cloned();
            let target =
                old.as_ref().map(|m| serde_json::to_value(&m.target).unwrap()).unwrap_or_else(|| d["target"].clone());
            let signed_target = if target == "server" {
                "server".into()
            } else if let Some(id) = target["channel"].as_u64() {
                format!("channel:{id}")
            } else {
                vc_proto::dm_conversation(
                    &self.welcome.uid,
                    self.welcome
                        .clients
                        .iter()
                        .find(|c| u64::from(c.id) == target["client"].as_u64().unwrap_or(0))
                        .map(|c| c.uid.as_str())
                        .unwrap_or("unknown"),
                )
            };
            let mentions: Vec<String> =
                serde_json::from_value(d.get("mentions").cloned().unwrap_or(json!([]))).unwrap();
            let attachments = if d["text"] == vc_proto::DELETION_TEXT {
                Vec::new()
            } else {
                old.as_ref().map(|m| m.attachments.clone()).unwrap_or_else(|| {
                    d["attachments"]
                        .as_array()
                        .map(|ids| ids.iter().filter_map(|id| self.files.get(id.as_str()?).cloned()).collect())
                        .unwrap_or_default()
                })
            };
            let mut sig = self.signature(&signed_target, d["text"].as_str().unwrap_or(""), &mentions, &attachments);
            if op == "chat.edit" {
                if let Some(original) = old.and_then(|m| m.signature) {
                    sig.message_id = original.message_id.clone();
                    sig.revision = original.revision + 1;
                    sig.previous_revision_hash = vc_proto::message_revision_hash(&original);
                    sig.created_at = original.created_at;
                }
                sig.signed_at = vc_server::core::now_ms();
                self.resign(&mut sig, &signed_target, d["text"].as_str().unwrap_or(""), &mentions, &attachments);
            }
            d["signature"] = serde_json::to_value(sig).unwrap();
        }
        if op == "file.upload" && d.get("sha256").is_none() {
            d["sha256"] = json!(hash(b""));
        }
        self.next_id += 1;
        let id = self.next_id;
        let frame = json!({"id": id, "op": op, "d": d});
        self.ws.send(Message::text(frame.to_string())).await.expect("send");
        id
    }

    pub async fn send_raw_request(&mut self, frame: Value) -> bool {
        self.ws.send(Message::text(frame.to_string())).await.is_ok()
    }
    pub async fn raw_frame(&mut self) -> Option<Value> {
        recv(&mut self.ws).await
    }

    pub async fn send_raw(&mut self, text: &str) {
        self.ws.send(Message::text(text)).await.expect("send");
    }

    pub async fn reply(&mut self, id: u32) -> Result<Value, ErrorBody> {
        within(&format!("reply to request {id}"), async {
            loop {
                if let Some(result) = self.replies.remove(&id) {
                    return result;
                }
                self.pump().await.expect("connection closed while waiting for a reply");
            }
        })
        .await
    }

    pub async fn request(&mut self, op: &str, d: Value) -> Result<Value, ErrorBody> {
        let id = self.send(op, d).await;
        self.reply(id).await
    }

    /// Request that must succeed.
    pub async fn ok(&mut self, op: &str, d: Value) -> Value {
        match self.request(op, d).await {
            Ok(v) => v,
            Err(e) => panic!("{op} failed: {e:?}"),
        }
    }

    /// Request that must fail; returns the error code as its wire string.
    pub async fn fails(&mut self, op: &str, d: Value) -> String {
        match self.request(op, d).await {
            Ok(v) => panic!("{op} unexpectedly succeeded: {v}"),
            Err(e) => serde_json::to_value(e.code).unwrap().as_str().unwrap().to_owned(),
        }
    }

    /// Next event named `name` (`d` payload), skipping and keeping other events.
    pub async fn next_event(&mut self, name: &str) -> Value {
        self.next_event_where(name, |_| true).await
    }

    pub async fn next_event_where(&mut self, name: &str, pred: impl Fn(&Value) -> bool) -> Value {
        within(&format!("event {name}"), async {
            loop {
                let hit = self.events.iter().position(|e| e["ev"] == name && pred(&e["d"]));
                if let Some(pos) = hit {
                    return self.events.remove(pos).unwrap()["d"].clone();
                }
                self.pump().await.unwrap_or_else(|| panic!("connection closed while waiting for event {name}"));
            }
        })
        .await
    }

    /// Round-trips a ping. Every event queued for this client before the ping
    /// was processed has arrived afterwards, so absence checks are deterministic.
    pub async fn sync(&mut self) {
        self.ok("ping", json!({})).await;
    }

    /// Removes and returns all buffered events named `name`, after a `sync`.
    pub async fn drain_events(&mut self, name: &str) -> Vec<Value> {
        self.sync().await;
        let (hit, rest): (Vec<_>, Vec<_>) = self.events.drain(..).partition(|e| e["ev"] == name);
        self.events = rest.into();
        hit.into_iter().map(|e| e["d"].clone()).collect()
    }

    /// Waits for the `disconnected` event and then for the socket to close.
    pub async fn expect_disconnect(&mut self) -> Value {
        let reason = self.next_event("disconnected").await["reason"].clone();
        within("socket close", async { while self.pump().await.is_some() {} }).await;
        reason
    }

    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }

    pub async fn redeem(&mut self, token: &str) -> Value {
        self.ok("token.redeem", json!({"token": token})).await
    }

    pub async fn create_channel(&mut self, d: Value) -> Value {
        self.ok("channel.create", d).await
    }
}

/// A client holding the Admin group (redeems the server's first-start token).
pub async fn admin(server: &TestServer, nickname: &str) -> Client {
    let mut client = Client::connect(server, nickname).await;
    client.redeem(&server.admin_token).await;
    client
}

pub fn channel_id(channel: &Value) -> u32 {
    channel["id"].as_u64().expect("channel id") as u32
}

/// Id of the seeded channel called `name` in the welcome snapshot.
pub fn seed_channel(client: &Client, name: &str) -> u32 {
    client.welcome.channels.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no seeded channel {name}")).id
}
