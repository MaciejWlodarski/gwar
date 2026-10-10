//! Server member names belong to identities, across sessions and message history.

mod common;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use common::*;
use serde_json::json;
use vc_proto::{Permission, Platform};
use vc_server::{
    core::{ConnectRequest, OUTBOUND_QUEUE},
    store::Store,
};

#[tokio::test]
async fn every_session_uses_the_stored_member_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vc.sqlite3");
    let server = TestServer::start_with(|c| c.database = Some(path.clone())).await;
    let key = new_key();
    let first = Client::connect_as(&server, key.clone(), "  Alice  ", None).await.unwrap();
    let second = Client::connect_as(&server, key.clone(), "Web name", None).await.unwrap();
    assert_eq!(first.welcome.uid, second.welcome.uid);
    let sessions: Vec<_> = second.welcome.clients.iter().filter(|c| c.uid == first.welcome.uid).collect();
    assert_eq!(sessions.len(), 2);
    assert!(sessions.iter().all(|c| c.nickname == "Alice"));
    assert_eq!(second.welcome.members.iter().find(|m| m.uid == first.welcome.uid).unwrap().nickname, "Alice");
    // Returning members do not use (or validate) Hello.nickname at all.
    let third = Client::connect_as(&server, key, "", None).await.unwrap();
    assert!(third.welcome.clients.iter().all(|c| c.nickname == "Alice"));
    assert_eq!(Store::open(&path).unwrap().user_by_uid(&first.welcome.uid).unwrap().unwrap().1.nickname, "Alice");
}

#[tokio::test]
async fn renaming_self_updates_all_sessions_and_current_chat_names() {
    let server = TestServer::start().await;
    let key = new_key();
    let mut laptop = Client::connect_as(&server, key.clone(), "Alice", None).await.unwrap();
    let mut phone = Client::connect_as(&server, key.clone(), "Other", None).await.unwrap();
    let mut observer = Client::connect(&server, "Bob").await;
    let uid = laptop.welcome.uid.clone();
    let channel = laptop.default_channel();
    let old = laptop.ok("chat.send", json!({"target": {"channel": channel}, "text": "before"})).await;
    assert_eq!(old["author_name"], "Alice");
    assert_eq!(laptop.ok("member.nickname", json!({"uid": uid, "nickname": "  Alicia  "})).await, json!({}));
    for client in [&mut laptop, &mut phone, &mut observer] {
        let member = client.next_event_where("member.updated", |m| m["uid"] == uid && m["nickname"] == "Alicia").await;
        assert_eq!(member["tag"].as_str().unwrap().len(), vc_proto::TAG_MIN_LEN);
        let events = client.drain_events("client.updated").await;
        let renamed: Vec<_> = events.iter().filter(|c| c["uid"] == uid).collect();
        assert_eq!(renamed.len(), 2);
        assert!(renamed.iter().all(|c| c["nickname"] == "Alicia"));
    }
    let history = observer.ok("chat.history", json!({"channel": channel})).await;
    assert_eq!(history["messages"][0]["author_name"], "Alicia");
    phone.ok("chat.send", json!({"target": {"channel": channel}, "text": "after"})).await;
    let live = observer.next_event_where("chat.message", |m| m["text"] == "after").await;
    assert_eq!(live["author_name"], "Alicia");
    laptop.close().await;
    phone.close().await;
    let again = Client::connect_as(&server, key, "Ignored", None).await.unwrap();
    assert_eq!(again.welcome.clients.iter().find(|c| c.id == again.id()).unwrap().nickname, "Alicia");
}

#[tokio::test]
async fn another_members_name_requires_only_the_nickname_permission() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mut alice = Client::connect(&server, "alice").await;
    assert!(root.welcome.groups.iter().find(|g| g.id == 1).unwrap().permissions.contains(&Permission::MemberNickname));
    assert!(!alice.welcome.permissions.contains(&Permission::MemberNickname));
    assert_eq!(alice.fails("member.nickname", json!({"uid": root.welcome.uid, "nickname": "no"})).await, "forbidden");
    let role =
        root.ok("group.create", json!({"name": "Names", "permissions": ["member_nickname"]})).await["group"]["id"]
            .clone();
    root.ok("member.groups", json!({"uid": alice.welcome.uid, "groups": [2, role]})).await;
    // This permission intentionally allows renaming even an admin.
    alice.ok("member.nickname", json!({"uid": root.welcome.uid, "nickname": "alice"})).await;
    let root_uid = root.welcome.uid.clone();
    assert_eq!(
        root.next_event_where("member.updated", |m| m["uid"] == root_uid && m["nickname"] == "alice").await["nickname"],
        "alice"
    );
    assert_eq!(alice.fails("member.nickname", json!({"uid": "absent", "nickname": "valid"})).await, "not_found");
    for bad in ["", "   ", &"é".repeat(33), "new\nline", "a\u{7f}b"] {
        assert_eq!(
            alice.fails("member.nickname", json!({"uid": alice.welcome.uid, "nickname": bad})).await,
            "bad_request"
        );
    }
    alice.ok("member.nickname", json!({"uid": alice.welcome.uid, "nickname": "é".repeat(32)})).await;
    // Offline members can be renamed as well.
    let offline = Client::connect(&server, "offline").await;
    let uid = offline.welcome.uid.clone();
    offline.close().await;
    alice.ok("member.nickname", json!({"uid": uid, "nickname": "Remembered"})).await;
}

#[tokio::test]
async fn tag_changes_are_announced_on_member_join_and_removal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vc.sqlite3");
    let store = Store::open(&path).unwrap();
    let a = URL_SAFE_NO_PAD.encode([0u8; 20]);
    let mut bytes = [0u8; 20];
    bytes[7] = 128;
    let b = URL_SAFE_NO_PAD.encode(bytes);
    store.touch_user(&a, "k", "a", 1).unwrap();
    let server = TestServer::start_with(|c| c.database = Some(path)).await;
    let mut root = admin(&server, "root").await;
    assert_eq!(
        root.welcome.members.iter().find(|m| m.uid == a).map(|m| m.tag.len()),
        None,
        "old members may be outside the welcome window"
    );
    let (out, mut rx) = tokio::sync::mpsc::channel(OUTBOUND_QUEUE);
    let id = server
        .running
        .core
        .connect(ConnectRequest {
            request_id: 1,
            uid: b.clone(),
            public_key: "k".into(),
            nickname: "b".into(),
            platform: Platform::Web,
            out,
            ip: None,
            device: None,
            invite: None,
            password_ok: true,
        })
        .await
        .unwrap();
    // The core test injects a constructed uid rather than grinding a 50-bit key collision.
    assert!(rx.recv().await.is_some());
    let expanded = root.next_event_where("member.updated", |m| m["uid"] == a).await;
    assert_eq!(expanded["tag"], "a".repeat(12));
    root.ok("member.remove", json!({"uid": b})).await;
    let shortened = root.next_event_where("member.updated", |m| m["uid"] == a).await;
    assert_eq!(shortened["tag"], "a".repeat(10));
    assert!(id > 0);
}

#[tokio::test]
async fn teamspeak_remotes_use_member_names_when_known_and_native_names_otherwise() {
    use vc_server::core::bridge::{BridgeMsg, RemoteClient, RemoteUpdate};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vc.sqlite3");
    let store = Store::open(&path).unwrap();
    let known = format!("ts:{}", base64::engine::general_purpose::STANDARD.encode([1u8; 20]));
    let id = store.touch_user(&known, "ts-key", "Member name", 1).unwrap().id;
    store.add_user_group(id, 2).unwrap();
    let server = TestServer::start_with(|c| c.database = Some(path)).await;
    let mut observer = Client::connect(&server, "observer").await;
    let remote = |uid| RemoteClient {
        uid,
        nickname: "Native name".into(),
        channel: observer.default_channel(),
        platform: Platform::Ts3,
        muted: false,
        deafened: false,
        away: None,
    };
    let known_session = server.running.core.remote_join(remote(known.clone())).await.unwrap();
    let unknown_session = server.running.core.remote_join(remote("ts:unknown".into())).await.unwrap();
    assert_eq!(
        observer.next_event_where("client.joined", |c| c["id"] == known_session).await["nickname"],
        "Member name"
    );
    assert_eq!(
        observer.next_event_where("client.joined", |c| c["id"] == unknown_session).await["nickname"],
        "Native name"
    );
    for session in [known_session, unknown_session] {
        server
            .running
            .core
            .bridge(BridgeMsg::Update(
                session,
                RemoteUpdate { nickname: Some("Native changed".into()), ..Default::default() },
            ))
            .await;
    }
    assert_eq!(
        observer.next_event_where("client.updated", |c| c["id"] == known_session).await["nickname"],
        "Member name"
    );
    assert_eq!(
        observer.next_event_where("client.updated", |c| c["id"] == unknown_session).await["nickname"],
        "Native changed"
    );
}
