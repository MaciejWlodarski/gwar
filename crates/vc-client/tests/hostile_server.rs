//! What a hostile server cannot do to a library consumer of `vc_client::connect`.
use std::time::Duration;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ed25519_dalek::{Signer, SigningKey};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::mpsc};
use tokio_tungstenite::tungstenite::Message;
use vc_client::{ClientError, ConnectOptions, Connected, Identity, connect};
use vc_proto::{
    ChatMessage, ChatTarget, Client, ClientSoftware, ErrorCode, Event, Hello, MessageCheck, MessageSignature, Platform,
    ServerInfo, Welcome, certificate_hash, dm_conversation, message_body_hash, message_statement,
    server_proof_statement, uid_for_key,
};

const SERVER_KEY: [u8; 32] = [3; 32];

fn server_pk() -> String {
    B64.encode(SigningKey::from_bytes(&SERVER_KEY).verifying_key().as_bytes())
}

fn client(id: u32, uid: &str) -> Client {
    Client {
        id,
        uid: uid.into(),
        nickname: format!("c{id}"),
        channel: None,
        groups: vec![],
        platform: Platform::Desktop,
        muted: false,
        deafened: false,
        away: None,
        talking: false,
        voice: false,
    }
}

/// Completes the handshake, announcing us as client 1 and then `clients`; hands the
/// socket's sending side to the test as frames to send. What the client sends afterwards
/// is ignored.
async fn hostile(clients: Vec<Client>) -> (Connected, mpsc::Sender<String>, String) {
    let (connected, frames, me) =
        start(move |me| json!(std::iter::once(client(1, me)).chain(clients).collect::<Vec<_>>())).await;
    (connected.unwrap(), frames, me)
}

/// Like [`hostile`], with the welcome's client list as `clients(our uid)` makes it.
async fn start(
    clients: impl FnOnce(&str) -> Value + Send + 'static,
) -> (Result<Connected, ClientError>, mpsc::Sender<String>, String) {
    start_with_reply(clients, None).await
}

async fn start_with_reply(
    clients: impl FnOnce(&str) -> Value + Send + 'static,
    reply: Option<Value>,
) -> (Result<Connected, ClientError>, mpsc::Sender<String>, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/ws", listener.local_addr().unwrap());
    let (frames, mut to_send) = mpsc::channel::<String>(16);
    let (me_tx, me_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let challenge = json!({"ev":"challenge","d":{
            "protocol":4,"server_pk":server_pk(),"server_nonce":B64.encode([4;32]),"password_required":false,
            "server":{"name":"test","welcome":"","version":"0","default_channel":1,"max_clients":8,"upload_limit":0}
        }});
        ws.send(Message::Text(challenge.to_string().into())).await.unwrap();
        let Some(Ok(Message::Text(text))) = ws.next().await else { panic!("no hello") };
        let hello: Hello = serde_json::from_value(serde_json::from_str::<Value>(&text).unwrap()["d"].clone()).unwrap();
        let proof = SigningKey::from_bytes(&SERVER_KEY).sign(server_proof_statement(&hello).unwrap().as_bytes());
        ws.send(Message::Text(json!({"re":1,"ok":{"server_proof":B64.encode(proof.to_bytes())}}).to_string().into()))
            .await
            .unwrap();
        let _join = ws.next().await;
        let me = uid_for_key(&vc_proto::decode_fixed::<32>(&hello.client_signing_pk).unwrap());
        let welcome = Welcome {
            session: 1,
            uid: me.clone(),
            server: ServerInfo {
                name: "test".into(),
                welcome: String::new(),
                version: "0".into(),
                default_channel: 1,
                max_clients: 8,
                upload_limit: 0,
                file_access: Default::default(),
            },
            permissions: vec![],
            groups: vec![],
            channels: vec![],
            clients: vec![],
            members: vec![],
            unread: vec![],
            ice_servers: vec![],
            file_access: String::new(),
        };
        let mut welcome = serde_json::to_value(welcome).unwrap();
        welcome["clients"] = clients(&me);
        ws.send(Message::Text(json!({"re":2,"ok":welcome}).to_string().into())).await.unwrap();
        me_tx.send(me).unwrap();
        loop {
            tokio::select! {
                frame = to_send.recv() => match frame {
                    Some(frame) => if ws.send(Message::Text(frame.into())).await.is_err() { break },
                    None => break,
                },
                incoming = ws.next() => {
                    let Some(Ok(Message::Text(text))) = incoming else { break };
                    if let Some(reply) = &reply {
                        let request: Value = serde_json::from_str(&text).unwrap();
                        ws.send(Message::Text(json!({"re":request["id"],"ok":reply}).to_string().into())).await.unwrap();
                    }
                },
            }
        }
    });
    let mut options = ConnectOptions::new(
        url,
        Identity::generate(),
        ClientSoftware { name: "test".into(), version: "0".into(), platform: Platform::Desktop },
    );
    options.expected_server_key = Some(server_pk());
    let connected = connect(options).await;
    (connected, frames, me_rx.await.unwrap())
}

/// `text` signed by `author` for `conversation` on the hostile server.
fn signed(author: &SigningKey, target: ChatTarget, conversation: &str, text: &str) -> ChatMessage {
    let uid = uid_for_key(author.verifying_key().as_bytes());
    let mut s = MessageSignature {
        server_key: server_pk(),
        conversation: conversation.into(),
        group_id: "-".into(),
        epoch: "-".into(),
        author_uid: uid.clone(),
        key: B64.encode(author.verifying_key().as_bytes()),
        certificate: None,
        certificate_hash: certificate_hash(None).unwrap(),
        message_id: B64.encode(rand_id(text)),
        revision: 0,
        previous_revision_hash: "-".into(),
        created_at: 1_800_000_000_000,
        signed_at: 1_800_000_000_000,
        body_hash: message_body_hash(text, &[], &[]),
        signature: String::new(),
    };
    s.signature = B64.encode(
        author.sign(message_statement(&server_pk(), conversation, &uid, &s, &s.body_hash).as_bytes()).to_bytes(),
    );
    ChatMessage {
        id: 1,
        target,
        author: 2,
        author_uid: uid,
        author_name: "alice".into(),
        text: text.into(),
        sent_at: 0,
        mentions: vec![],
        attachments: vec![],
        edited_at: None,
        signature: Some(Box::new(s)),
    }
}

fn rand_id(text: &str) -> [u8; 16] {
    let mut id = [0; 16];
    for (i, b) in text.bytes().enumerate() {
        id[i % 16] ^= b;
    }
    id
}

fn event(ev: &str, d: impl serde::Serialize) -> String {
    json!({"ev": ev, "d": d}).to_string()
}

async fn next_chat(connected: &mut Connected) -> Option<ChatMessage> {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), connected.events.recv()).await.expect("event") {
            Some(Event::ChatMessage(m) | Event::ChatEdited(m)) => return Some(m),
            Some(_) => continue,
            None => return None,
        }
    }
}

#[tokio::test]
async fn forged_chat_never_reaches_the_event_stream() {
    let alice = SigningKey::from_bytes(&[1; 32]);
    let carol = SigningKey::from_bytes(&[7; 32]);
    let alice_uid = uid_for_key(alice.verifying_key().as_bytes());
    let carol_uid = uid_for_key(carol.verifying_key().as_bytes());
    let (mut connected, frames, me) = hostile(vec![client(2, &alice_uid), client(3, &carol_uid)]).await;

    let mut unsigned = signed(&alice, ChatTarget::Channel(1), "channel:1", "unsigned");
    unsigned.signature = None;
    let mut altered = signed(&alice, ChatTarget::Channel(1), "channel:1", "original");
    altered.text = "altered".into();
    let moved = signed(&alice, ChatTarget::Channel(2), "channel:1", "moved");
    // Alice's DM to Carol, relayed to us.
    let relayed = signed(&alice, ChatTarget::Client(1), &dm_conversation(&alice_uid, &carol_uid), "for carol");
    let mut teamspeak = unsigned.clone();
    teamspeak.author_uid = "ts:bob".into();
    teamspeak.text = "from teamspeak".into();
    let to_us = signed(&alice, ChatTarget::Client(1), &dm_conversation(&alice_uid, &me), "for us");
    // Alice's genuine DM to us, shown to us as if it were addressed to Carol.
    let mut readdressed = to_us.clone();
    readdressed.target = ChatTarget::Client(3);
    for (ev, m) in [
        ("chat.message", &unsigned),
        ("chat.edited", &altered),
        ("chat.message", &moved),
        ("chat.message", &relayed),
        ("chat.message", &teamspeak),
        ("chat.message", &readdressed),
        ("chat.message", &signed(&alice, ChatTarget::Channel(1), "channel:1", "genuine")),
        ("chat.message", &to_us),
    ] {
        frames.send(event(ev, m)).await.unwrap();
    }
    let delivered: Vec<_> = [
        next_chat(&mut connected).await.unwrap(),
        next_chat(&mut connected).await.unwrap(),
        next_chat(&mut connected).await.unwrap(),
    ]
    .into_iter()
    .map(|m| m.text)
    .collect();
    assert_eq!(delivered, ["from teamspeak", "genuine", "for us"]);

    // Replies are checked on request, with the same rules.
    let check = |m: &ChatMessage| connected.connection.verify_message(m);
    assert_eq!(check(&to_us), MessageCheck::Verified { certificate_expired: false });
    assert_eq!(check(&relayed), MessageCheck::Invalid);
    assert_eq!(check(&readdressed), MessageCheck::Invalid);
    assert_eq!(check(&altered), MessageCheck::Invalid);
    assert_eq!(check(&unsigned), MessageCheck::Unsigned);
    assert_eq!(check(&teamspeak), MessageCheck::UnsignedTeamSpeak);
}

#[tokio::test]
async fn oversized_frames_end_the_connection() {
    let (mut connected, frames, _) = hostile(vec![]).await;
    let filler = "x".repeat(7 * 1024 * 1024);
    frames.send(event("server.updated", json!({"padding": filler}))).await.unwrap();
    let end = tokio::time::timeout(Duration::from_secs(10), async { while connected.events.recv().await.is_some() {} });
    end.await.expect("the connection closes");
}

#[tokio::test]
async fn a_welcome_with_too_many_clients_is_refused() {
    // Clients as arrays, so that many fit in one frame.
    let (connected, _frames, _) = start(|me| {
        let mut clients = vec![json!([1, me, "me", null, [], "desktop", false, false, null, false, false])];
        clients.extend(
            (10..10 + 65_535).map(|id| json!([id, "u", "c", null, [], "desktop", false, false, null, false, false])),
        );
        json!(clients)
    })
    .await;
    assert!(matches!(connected, Err(ClientError::Protocol(ref e)) if e == "too many clients"), "{:?}", connected.err());
}

#[tokio::test]
async fn announcing_too_many_clients_ends_the_connection() {
    let (mut connected, frames, _) = hostile(vec![]).await;
    let sender = tokio::spawn(async move {
        for id in 10..10 + 65_536 {
            if frames.send(event("client.joined", client(id, &format!("u{id}")))).await.is_err() {
                return id - 10;
            }
        }
        65_536
    });
    let mut joined = 0;
    let ended = tokio::time::timeout(Duration::from_secs(120), async {
        while let Some(event) = connected.events.recv().await {
            joined += usize::from(matches!(event, Event::ClientJoined(_)));
        }
    })
    .await;
    assert!(ended.is_ok(), "the connection closes");
    // We are the first of the 65 535.
    assert_eq!(joined, 65_534);
    sender.abort();
}

#[tokio::test]
async fn requests_without_replies_are_bounded_and_released() {
    let (connected, _frames, _) = hostile(vec![]).await;
    let ping = || vc_proto::Request::ChatHistory { channel: 1, before: None, limit: None };
    let (done, mut finished) = mpsc::unbounded_channel();
    let waiting: Vec<_> = (0..257)
        .map(|_| {
            let (connection, done) = (connected.connection.clone(), done.clone());
            tokio::spawn(async move { done.send(connection.request(ping()).await).unwrap() })
        })
        .collect();
    // The server never replies: one request more than the limit fails at once.
    let first = tokio::time::timeout(Duration::from_secs(10), finished.recv()).await.unwrap().unwrap();
    assert!(matches!(first, Err(ClientError::Server(ref e)) if e.code == ErrorCode::RateLimited), "{first:?}");
    assert!(tokio::time::timeout(Duration::from_millis(300), finished.recv()).await.is_err());
    // Callers who give up release their place.
    for task in waiting {
        task.abort();
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(tokio::time::timeout(Duration::from_millis(300), connected.connection.request(ping())).await.is_err());
}

#[tokio::test]
async fn unsigned_gwar_history_is_returned_and_classified_without_rejection() {
    let alice = SigningKey::from_bytes(&[1; 32]);
    let mut legacy = signed(&alice, ChatTarget::Channel(1), "channel:1", "migrated history");
    legacy.signature = None;
    let (connected, _frames, _) = start_with_reply(|_| json!([]), Some(json!({"messages":[legacy]}))).await;
    let connected = connected.unwrap();
    let response: vc_proto::Response = connected
        .connection
        .request_as(vc_proto::Request::ChatHistory { channel: 1, before: None, limit: None })
        .await
        .unwrap();
    let vc_proto::Response::History { messages } = response else {
        panic!("history reply");
    };
    assert_eq!(messages[0].text, "migrated history");
    assert_eq!(connected.connection.verify_message(&messages[0]), MessageCheck::Unsigned);
}
