//! End-to-end tests of the `vc/2` protocol against a real in-process server.

mod common;

use std::time::Duration;

use common::*;
use serde_json::{Value, json};
use vc_proto::Permission;

const MEMBER: u64 = 2;
const ADMIN: u64 = 1;
const EPHEMERAL_BASE: u64 = 1 << 31;

fn ids(events: &[Value]) -> Vec<u64> {
    events.iter().map(|e| e["channel"].as_u64().unwrap()).collect()
}

// ------------------------------------------------------------ handshake

#[tokio::test]
async fn challenge_is_sent_first_and_describes_the_server() {
    let server = TestServer::start().await;
    let raw = open(&server).await;
    assert_eq!(raw.challenge["protocol"], vc_proto::PROTOCOL_VERSION);
    assert_eq!(raw.challenge["password_required"], false);
    assert_eq!(raw.challenge["server"]["name"], "Gwar");
    assert!(raw.nonce.len() >= 32);
    // Every connection gets its own nonce.
    assert_ne!(open(&server).await.nonce, raw.nonce);
}

#[tokio::test]
async fn bad_signature_is_rejected_and_connection_closed() {
    let server = TestServer::start().await;
    let mut raw = open(&server).await;
    let key = new_key();
    let mut hello = raw.hello(&key, "mallory", None);
    // Signed by a different key than the one announced.
    hello["signature"] = json!(sign(&new_key(), &raw.nonce));
    let err = raw.rejected(hello).await;
    assert_eq!(serde_json::to_value(err.code).unwrap(), "not_authenticated");
    raw.expect_closed().await;
}

#[tokio::test]
async fn signature_over_another_nonce_is_rejected() {
    let server = TestServer::start().await;
    let other = open(&server).await;
    let mut raw = open(&server).await;
    let key = new_key();
    let mut hello = raw.hello(&key, "replay", None);
    hello["signature"] = json!(sign(&key, &other.nonce));
    let err = raw.rejected(hello).await;
    assert_eq!(serde_json::to_value(err.code).unwrap(), "not_authenticated");
    raw.expect_closed().await;
}

#[tokio::test]
async fn malformed_public_key_or_signature_is_rejected() {
    let server = TestServer::start().await;
    for (field, value) in [("public_key", "not base64!"), ("signature", "AAAA"), ("public_key", "")] {
        let mut raw = open(&server).await;
        let mut hello = raw.hello(&new_key(), "x", None);
        hello[field] = json!(value);
        let err = raw.rejected(hello).await;
        assert_eq!(serde_json::to_value(err.code).unwrap(), "not_authenticated", "{field}={value}");
        raw.expect_closed().await;
    }
}

#[tokio::test]
async fn wrong_protocol_version_is_rejected() {
    let server = TestServer::start().await;
    let mut raw = open(&server).await;
    let mut hello = raw.hello(&new_key(), "old", None);
    hello["protocol"] = json!(vc_proto::PROTOCOL_VERSION + 1);
    let err = raw.rejected(hello).await;
    assert_eq!(serde_json::to_value(err.code).unwrap(), "bad_request");
    raw.expect_closed().await;
}

#[tokio::test]
async fn first_frame_must_be_hello() {
    let server = TestServer::start().await;
    let mut raw = open(&server).await;
    raw.send(json!({"id": 5, "op": "ping", "d": {}})).await;
    let frame = raw.recv().await.unwrap();
    assert_eq!(frame["re"], 5);
    assert_eq!(frame["err"]["code"], "not_authenticated");
    raw.expect_closed().await;
}

#[tokio::test]
async fn malformed_first_frame_is_rejected() {
    let server = TestServer::start().await;
    let mut raw = open(&server).await;
    raw.send(json!({"hello": "world"})).await;
    let frame = raw.recv().await.unwrap();
    assert_eq!(frame["err"]["code"], "bad_request");
    raw.expect_closed().await;
}

#[tokio::test]
async fn invalid_nickname_in_hello_is_rejected() {
    let server = TestServer::start().await;
    for nick in ["", "   ", &"x".repeat(33), "bad\nname"] {
        let err = Client::connect_as(&server, new_key(), nick, None).await.err().expect("rejected");
        assert_eq!(serde_json::to_value(err.code).unwrap(), "bad_request", "{nick:?}");
    }
}

#[tokio::test]
async fn server_password_is_required_and_checked() {
    let server = TestServer::start_with(|c| c.server_password = Some("hunter2".into())).await;
    assert_eq!(open(&server).await.challenge["password_required"], true);

    for password in [None, Some("wrong"), Some(""), Some(&*"p".repeat(200))] {
        let err = Client::connect_as(&server, new_key(), "guest", password).await.err().expect("rejected");
        assert_eq!(serde_json::to_value(err.code).unwrap(), "wrong_password", "{password:?}");
    }
    let ok = Client::connect_as(&server, new_key(), "guest", Some("hunter2")).await.expect("accepted");
    assert_eq!(ok.welcome.clients.len(), 1);
}

#[tokio::test]
async fn empty_server_password_means_open_server() {
    let server = TestServer::start_with(|c| c.server_password = Some(String::new())).await;
    assert_eq!(open(&server).await.challenge["password_required"], false);
    Client::connect(&server, "guest").await;
}

#[tokio::test]
async fn full_server_refuses_new_clients() {
    let server = TestServer::start_with(|c| c.max_clients = 1).await;
    let first = Client::connect(&server, "first").await;
    let err = Client::connect_as(&server, new_key(), "second", None).await.err().expect("rejected");
    assert_eq!(serde_json::to_value(err.code).unwrap(), "unavailable");
    // A slot frees up when the first client leaves.
    first.close().await;
    let mut third = None;
    for _ in 0..30 {
        match Client::connect_as(&server, new_key(), "third", None).await {
            Ok(c) => {
                third = Some(c);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    assert!(third.is_some(), "slot was never released");
}

#[tokio::test]
async fn second_hello_and_garbage_after_login_are_bad_requests() {
    let server = TestServer::start().await;
    let mut c = Client::connect(&server, "alice").await;
    let hello = json!({
        "protocol": vc_proto::PROTOCOL_VERSION, "nickname": "again", "public_key": public_key(&c.key), "signature": "x",
        "client": {"name": "t", "version": "0", "platform": "web"},
    });
    assert_eq!(c.fails("hello", hello).await, "bad_request");
    assert_eq!(c.fails("no.such.op", json!({})).await, "bad_request");
    // Malformed request still echoes the id so the client can correlate.
    c.send_raw(r#"{"id": 77, "op": "channel.join", "d": {"channel": "x"}}"#).await;
    assert_eq!(c.reply(77).await.unwrap_err().code, vc_proto::ErrorCode::BadRequest);
    c.sync().await;
}

// -------------------------------------------------------------- welcome

#[tokio::test]
async fn welcome_describes_server_and_own_client() {
    let server = TestServer::start().await;
    let c = Client::connect(&server, "alice").await;
    let w = &c.welcome;

    let names: Vec<_> = w.channels.iter().map(|ch| ch.name.as_str()).collect();
    assert_eq!(names, ["Lobby", "General", "AFK"]);
    assert_eq!(w.server.default_channel, seed_channel(&c, "Lobby"));
    assert_eq!(w.server.max_clients, 64);
    assert!(w.channels.iter().all(|ch| ch.parent.is_none() && !ch.has_password));

    assert_eq!(w.clients.len(), 1);
    let me = &w.clients[0];
    assert_eq!(me.id, w.session);
    assert_eq!(me.nickname, "alice");
    // On the server, not in voice.
    assert_eq!(me.channel, None);
    assert_eq!(me.groups, [MEMBER as u32]);
    assert_eq!(me.uid, w.uid);
    assert_eq!(w.uid, uid_of(&c.key));
    assert!(!me.muted && !me.deafened && me.away.is_none() && !me.talking && !me.voice);

    // Members may only invite and upload; both seeded groups are listed.
    let mut permissions = w.permissions.clone();
    permissions.sort();
    assert_eq!(permissions, [Permission::InviteCreate, Permission::FileUpload]);
    let groups: Vec<_> = w.groups.iter().map(|g| (g.id, g.name.as_str())).collect();
    assert_eq!(groups, [(1, "Admin"), (2, "Member")]);
    assert_eq!(w.groups[0].permissions.len(), vc_proto::Permission::ALL.len());
    assert_eq!(w.groups[1].permissions, vc_proto::Permission::MEMBER_DEFAULT);
}

#[tokio::test]
async fn second_client_sees_join_and_leave() {
    let server = TestServer::start().await;
    let mut a = Client::connect(&server, "alice").await;
    let b = Client::connect(&server, "bob").await;

    // Bob's welcome lists Alice; Alice is told about Bob.
    let listed: Vec<_> = b.welcome.clients.iter().map(|c| c.nickname.as_str()).collect();
    assert_eq!(listed, ["alice", "bob"]);
    let joined = a.next_event("client.joined").await;
    assert_eq!(joined["id"], b.id());
    assert_eq!(joined["nickname"], "bob");
    assert!(joined["channel"].is_null(), "newcomers are on the server, not in voice");

    let bob = b.id();
    b.close().await;
    let left = a.next_event("client.left").await;
    assert_eq!(left, json!({"client": bob, "reason": {"kind": "quit"}}));
    // Alice never receives her own join event.
    assert!(a.drain_events("client.joined").await.is_empty());
}

#[tokio::test]
async fn group_membership_persists_per_identity() {
    let server = TestServer::start().await;
    let key = new_key();
    let mut first = Client::connect_as(&server, key.clone(), "alice", None).await.unwrap();
    first.redeem(&server.admin_token).await;
    first.close().await;
    let again = Client::connect_as(&server, key, "alice2", None).await.unwrap();
    assert!(again.welcome.clients.iter().any(|c| c.groups.contains(&(ADMIN as u32))));
    assert!(again.welcome.permissions.contains(&vc_proto::Permission::ChannelCreate));
}

// -------------------------------------------------------- channel.join

#[tokio::test]
async fn join_moves_client_and_broadcasts() {
    let server = TestServer::start().await;
    let mut a = Client::connect(&server, "alice").await;
    let mut b = Client::connect(&server, "bob").await;
    let rozmowy = seed_channel(&a, "General");

    a.ok("channel.join", json!({"channel": rozmowy})).await;
    let updated = b.next_event_where("client.updated", |d| d["id"] == a.id()).await;
    assert_eq!(updated["channel"], rozmowy);
    // Joining the current channel again is a no-op that still succeeds.
    a.ok("channel.join", json!({"channel": rozmowy})).await;
}

#[tokio::test]
async fn join_nonexistent_channel_is_not_found() {
    let server = TestServer::start().await;
    let mut a = Client::connect(&server, "alice").await;
    assert_eq!(a.fails("channel.join", json!({"channel": 9999})).await, "not_found");
}

#[tokio::test]
async fn join_with_channel_password() {
    let server = TestServer::start().await;
    let mut admin = admin(&server, "root").await;
    let locked = channel_id(&admin.create_channel(json!({"name": "Vault", "password": "s3cret"})).await);
    let created = admin.next_event("channel.created").await;
    assert_eq!(created["has_password"], true);

    let mut b = Client::connect(&server, "bob").await;
    assert!(b.welcome.channels.iter().any(|c| c.id == locked && c.has_password));
    assert_eq!(b.fails("channel.join", json!({"channel": locked})).await, "wrong_password");
    assert_eq!(b.fails("channel.join", json!({"channel": locked, "password": "nope"})).await, "wrong_password");
    assert_eq!(b.fails("channel.join", json!({"channel": locked, "password": "x".repeat(200)})).await, "bad_request");
    b.sync().await;
    assert!(b.drain_events("client.updated").await.is_empty(), "failed joins must not move the client");

    b.ok("channel.join", json!({"channel": locked, "password": "s3cret"})).await;
    let moved = admin.next_event_where("client.updated", |d| d["id"] == b.id()).await;
    assert_eq!(moved["channel"], locked);

    // Admins hold channel_join_locked and need no password.
    admin.ok("channel.join", json!({"channel": locked})).await;

    // Clearing the password opens the channel.
    admin.ok("channel.update", json!({"channel": locked, "password": ""})).await;
    let mut c = Client::connect(&server, "carol").await;
    c.ok("channel.join", json!({"channel": locked})).await;
}

#[tokio::test]
async fn join_respects_max_clients() {
    let server = TestServer::start().await;
    let mut admin = admin(&server, "root").await;
    let small = channel_id(&admin.create_channel(json!({"name": "Duo", "max_clients": 1})).await);
    let mut a = Client::connect(&server, "alice").await;
    let mut b = Client::connect(&server, "bob").await;

    a.ok("channel.join", json!({"channel": small})).await;
    assert_eq!(b.fails("channel.join", json!({"channel": small})).await, "channel_full");
    // Admins (client_move) may exceed the limit.
    admin.ok("channel.join", json!({"channel": small})).await;
    // Alice leaves, but the admin still occupies the single slot.
    a.ok("channel.join", json!({"channel": a.default_channel()})).await;
    assert_eq!(b.fails("channel.join", json!({"channel": small})).await, "channel_full");
    admin.ok("channel.join", json!({"channel": a.default_channel()})).await;
    b.ok("channel.join", json!({"channel": small})).await;
}

#[tokio::test]
async fn max_clients_must_be_in_range() {
    let server = TestServer::start().await;
    let mut admin = admin(&server, "root").await;
    for max in [0, 1001] {
        assert_eq!(admin.fails("channel.create", json!({"name": "X", "max_clients": max})).await, "bad_request");
    }
    let ch = channel_id(&admin.create_channel(json!({"name": "X", "max_clients": 1000})).await);
    assert_eq!(admin.fails("channel.update", json!({"channel": ch, "max_clients": 1001})).await, "bad_request");
    // 0 on update removes the limit.
    let updated = admin.ok("channel.update", json!({"channel": ch, "max_clients": 0})).await;
    assert!(updated["max_clients"].is_null(), "{updated}");
}

// --------------------------------------------------------- permissions

#[tokio::test]
async fn members_cannot_manage_but_admin_token_grants_rights() {
    let server = TestServer::start().await;
    let mut admin_client = Client::connect(&server, "root").await;
    let mut victim = Client::connect(&server, "victim").await;
    let mut m = Client::connect(&server, "mallory").await;
    let rozmowy = seed_channel(&m, "General");
    let victim_id = victim.id();

    let forbidden: [(&str, Value); 8] = [
        ("channel.create", json!({"name": "Mine"})),
        ("channel.update", json!({"channel": rozmowy, "name": "Renamed"})),
        ("channel.delete", json!({"channel": rozmowy})),
        ("client.kick", json!({"client": victim_id})),
        ("client.move", json!({"client": victim_id, "channel": rozmowy})),
        ("server.update", json!({"name": "Mine"})),
        ("token.create", json!({"group": MEMBER})),
        ("token.create", json!({"group": ADMIN})),
    ];
    for (op, d) in &forbidden {
        assert_eq!(m.fails(op, d.clone()).await, "forbidden", "{op}");
    }
    // Nothing leaked to other clients, and the victim is still connected.
    assert!(victim.drain_events("client.left").await.is_empty());
    assert!(victim.drain_events("channel.created").await.is_empty());

    // Redeeming the token upgrades the session in place.
    let groups = m.redeem(&server.admin_token).await;
    assert!(groups["groups"].as_array().unwrap().contains(&json!(ADMIN)));
    let upd = victim.next_event_where("client.updated", |d| d["id"] == m.id()).await;
    assert!(upd["groups"].as_array().unwrap().contains(&json!(ADMIN)));

    m.create_channel(json!({"name": "Mine"})).await;
    m.ok("channel.update", json!({"channel": rozmowy, "name": "Renamed"})).await;
    m.ok("client.move", json!({"client": victim_id, "channel": rozmowy})).await;
    m.ok("server.update", json!({"name": "Mine"})).await;
    m.ok("client.kick", json!({"client": victim_id})).await;
    m.ok("channel.delete", json!({"channel": rozmowy})).await;
    // `admin_client` never redeemed anything and stays a plain member.
    assert_eq!(admin_client.fails("channel.create", json!({"name": "Nope"})).await, "forbidden");
}

#[tokio::test]
async fn tokens_are_single_use() {
    let server = TestServer::start().await;
    let mut first = Client::connect(&server, "first").await;
    let mut second = Client::connect(&server, "second").await;
    // Surrounding whitespace is tolerated.
    first.redeem(&format!("  {}\n", server.admin_token)).await;
    assert_eq!(second.fails("token.redeem", json!({"token": server.admin_token})).await, "not_found");
    assert_eq!(first.fails("token.redeem", json!({"token": server.admin_token})).await, "not_found");
    assert_eq!(second.fails("token.redeem", json!({"token": "garbage"})).await, "not_found");
    assert_eq!(second.fails("channel.create", json!({"name": "Nope"})).await, "forbidden");

    // Admins mint further single-use tokens.
    let minted = first.ok("token.create", json!({"group": ADMIN})).await["token"].as_str().unwrap().to_owned();
    second.redeem(&minted).await;
    second.create_channel(json!({"name": "Yes"})).await;
    let mut third = Client::connect(&server, "third").await;
    assert_eq!(third.fails("token.redeem", json!({"token": minted})).await, "not_found");
    assert_eq!(first.fails("token.create", json!({"group": 999})).await, "not_found");
}

/// Creates a database that already holds a custom "Moderator" group (id 3)
/// with `token_create` and `client_kick` only.
struct ModeratorDb(std::path::PathBuf);

impl ModeratorDb {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("vc-test-{}.sqlite3", rand::random::<u64>()));
        drop(vc_server::store::Store::open(&path).unwrap());
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute(
            "INSERT INTO groups(id,name,permissions) VALUES(3,'Moderator','[\"token_create\",\"client_kick\"]')",
            [],
        )
        .unwrap();
        Self(path)
    }
}

impl Drop for ModeratorDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }
}

#[tokio::test]
async fn token_create_cannot_grant_more_than_you_have() {
    let db = ModeratorDb::new();
    let path = db.0.clone();
    let server = TestServer::start_with(move |c| c.database = Some(path)).await;
    let mut admin = admin(&server, "root").await;
    let mod_token = admin.ok("token.create", json!({"group": 3})).await["token"].as_str().unwrap().to_owned();

    let mut moderator = Client::connect(&server, "mod").await;
    moderator.redeem(&mod_token).await;
    assert!(moderator.fails("channel.create", json!({"name": "X"})).await == "forbidden");

    // Moderator can hand out its own group and the empty Member group, but not Admin.
    assert_eq!(moderator.fails("token.create", json!({"group": ADMIN})).await, "forbidden");
    moderator.ok("token.create", json!({"group": 3})).await;
    moderator.ok("token.create", json!({"group": MEMBER})).await;

    // And it can kick, since it holds client_kick.
    let mut victim = Client::connect(&server, "victim").await;
    moderator.ok("client.kick", json!({"client": victim.id()})).await;
    victim.expect_disconnect().await;
    // But not move.
    assert_eq!(moderator.fails("client.move", json!({"client": moderator.id(), "channel": 1})).await, "forbidden");
}

// -------------------------------------------------------- channel CRUD

#[tokio::test]
async fn channel_create_update_delete_are_broadcast() {
    let server = TestServer::start().await;
    let mut admin = admin(&server, "root").await;
    let mut watcher = Client::connect(&server, "watcher").await;

    let created = admin.create_channel(json!({"name": "  Alpha  ", "topic": "hello"})).await;
    let id = channel_id(&created);
    assert_eq!(created["name"], "Alpha");
    assert_eq!(created["topic"], "hello");
    assert_eq!(created["parent"], Value::Null);
    assert_eq!(created["has_password"], false);
    let ev = watcher.next_event("channel.created").await;
    assert_eq!(ev, created);
    // The creator is notified too.
    assert_eq!(admin.next_event("channel.created").await, created);

    // Late joiners see it in the welcome snapshot.
    let late = Client::connect(&server, "late").await;
    assert!(late.welcome.channels.iter().any(|c| c.id == id && c.name == "Alpha"));

    let updated = admin
        .ok("channel.update", json!({"channel": id, "name": "Beta", "topic": "", "max_clients": 5, "position": -3}))
        .await;
    assert_eq!(updated["name"], "Beta");
    assert_eq!(updated["topic"], "");
    assert_eq!(updated["max_clients"], 5);
    assert_eq!(updated["position"], -3);
    assert_eq!(watcher.next_event("channel.updated").await, updated);

    // Absent fields stay unchanged.
    let again = admin.ok("channel.update", json!({"channel": id, "topic": "t"})).await;
    assert_eq!(again["name"], "Beta");
    assert_eq!(again["max_clients"], 5);

    admin.ok("channel.delete", json!({"channel": id})).await;
    assert_eq!(watcher.next_event("channel.deleted").await, json!({"channel": id}));
    assert_eq!(admin.fails("channel.delete", json!({"channel": id})).await, "not_found");
    assert_eq!(admin.fails("channel.update", json!({"channel": id, "name": "Z"})).await, "not_found");
}

#[tokio::test]
async fn channel_names_and_parents_are_validated() {
    let server = TestServer::start().await;
    let mut admin = admin(&server, "root").await;
    for name in ["", "   ", &"n".repeat(65), "tab\there"] {
        assert_eq!(admin.fails("channel.create", json!({"name": name})).await, "bad_request", "{name:?}");
    }
    assert_eq!(admin.fails("channel.create", json!({"name": "T", "topic": "t".repeat(256)})).await, "bad_request");
    assert_eq!(admin.fails("channel.create", json!({"name": "Orphan", "parent": 9999})).await, "not_found");
}

#[tokio::test]
async fn duplicate_sibling_names_conflict() {
    let server = TestServer::start().await;
    let mut admin = admin(&server, "root").await;
    let a = channel_id(&admin.create_channel(json!({"name": "Alpha"})).await);
    // Case-insensitive, and also against the seeded top-level channels.
    assert_eq!(admin.fails("channel.create", json!({"name": "alpha"})).await, "conflict");
    assert_eq!(admin.fails("channel.create", json!({"name": "lobby"})).await, "conflict");

    // The same name under a different parent is fine.
    let sub = channel_id(&admin.create_channel(json!({"name": "Alpha", "parent": a})).await);
    assert_eq!(admin.fails("channel.create", json!({"name": "ALPHA", "parent": a})).await, "conflict");

    // Renaming or moving into a clash is rejected too; renaming to itself is not.
    let b = channel_id(&admin.create_channel(json!({"name": "Beta"})).await);
    assert_eq!(admin.fails("channel.update", json!({"channel": b, "name": "alpha"})).await, "conflict");
    admin.ok("channel.update", json!({"channel": a, "name": "ALPHA"})).await;
    assert_eq!(admin.fails("channel.update", json!({"channel": sub, "move_to_root": true})).await, "conflict");
    admin.ok("channel.update", json!({"channel": b, "parent": a})).await;
}

#[tokio::test]
async fn channels_can_be_moved_and_reparented() {
    let server = TestServer::start().await;
    let mut admin = admin(&server, "root").await;
    let p = channel_id(&admin.create_channel(json!({"name": "P"})).await);
    let c = channel_id(&admin.create_channel(json!({"name": "C", "parent": p})).await);
    let g = channel_id(&admin.create_channel(json!({"name": "G", "parent": c})).await);

    let moved = admin.ok("channel.update", json!({"channel": g, "parent": p})).await;
    assert_eq!(moved["parent"], p);
    let root = admin.ok("channel.update", json!({"channel": g, "move_to_root": true})).await;
    assert_eq!(root["parent"], Value::Null);
    assert_eq!(admin.fails("channel.update", json!({"channel": g, "parent": 9999})).await, "not_found");
}

#[tokio::test]
async fn moving_a_channel_into_its_own_subtree_is_rejected() {
    let server = TestServer::start().await;
    let mut admin = admin(&server, "root").await;
    let p = channel_id(&admin.create_channel(json!({"name": "P"})).await);
    let c = channel_id(&admin.create_channel(json!({"name": "C", "parent": p})).await);
    let g = channel_id(&admin.create_channel(json!({"name": "G", "parent": c})).await);

    for target in [p, c, g] {
        assert_eq!(
            admin.fails("channel.update", json!({"channel": p, "parent": target})).await,
            "bad_request",
            "into {target}"
        );
    }
    // Nothing changed.
    assert!(admin.drain_events("channel.updated").await.is_empty());
}

#[tokio::test]
async fn deleting_a_channel_evicts_occupants_and_removes_subchannels_deepest_first() {
    let server = TestServer::start().await;
    let mut admin = admin(&server, "root").await;
    let mut a = Client::connect(&server, "alice").await;
    let mut b = Client::connect(&server, "bob").await;
    let mut bystander = Client::connect(&server, "bystander").await;
    let default = a.default_channel();

    let p = channel_id(&admin.create_channel(json!({"name": "P"})).await);
    let c = channel_id(&admin.create_channel(json!({"name": "C", "parent": p})).await);
    let g = channel_id(&admin.create_channel(json!({"name": "G", "parent": c})).await);
    a.ok("channel.join", json!({"channel": p})).await;
    b.ok("channel.join", json!({"channel": g})).await;
    // Keep an unrelated sibling to prove it survives.
    let sibling = channel_id(&admin.create_channel(json!({"name": "Sibling"})).await);

    admin.ok("channel.delete", json!({"channel": p})).await;

    let deleted = ids(&bystander.drain_events("channel.deleted").await);
    assert_eq!(deleted, [g as u64, c as u64, p as u64], "deepest first");
    // Occupants drop out of voice but stay on the server (and everyone is told).
    for id in [a.id(), b.id()] {
        bystander.next_event_where("client.updated", |d| d["id"] == id && d["channel"].is_null()).await;
    }
    let fresh = Client::connect(&server, "fresh").await;
    let me = |id: u32| fresh.welcome.clients.iter().find(|c| c.id == id).unwrap().channel;
    assert_eq!(me(a.id()), None);
    assert_eq!(me(b.id()), None);
    let remaining: Vec<_> = fresh.welcome.channels.iter().map(|c| c.id).collect();
    assert!(remaining.contains(&sibling));
    assert!(![p, c, g].iter().any(|id| remaining.contains(id)));
    // Moved clients can talk in the default channel right away.
    a.ok("chat.send", json!({"target": {"channel": default}, "text": "back"})).await;
}

#[tokio::test]
async fn default_channel_cannot_be_deleted() {
    let server = TestServer::start().await;
    let mut admin = admin(&server, "root").await;
    let default = admin.default_channel();
    assert_eq!(admin.fails("channel.delete", json!({"channel": default})).await, "bad_request");

    // Nor any ancestor of it.
    let parent = channel_id(&admin.create_channel(json!({"name": "Parent"})).await);
    admin.ok("channel.update", json!({"channel": default, "parent": parent})).await;
    assert_eq!(admin.fails("channel.delete", json!({"channel": parent})).await, "bad_request");
    assert_eq!(admin.fails("channel.delete", json!({"channel": 9999})).await, "not_found");
    assert!(admin.drain_events("channel.deleted").await.is_empty());
}

// ---------------------------------------------------------------- chat

#[tokio::test]
async fn channel_chat_reaches_everyone_who_can_read_it() {
    let server = TestServer::start().await;
    let mut a = Client::connect(&server, "alice").await;
    let mut b = Client::connect(&server, "bob").await;
    let mut c = Client::connect(&server, "carol").await;
    let default = a.default_channel();
    let rozmowy = seed_channel(&a, "General");
    c.ok("channel.join", json!({"channel": rozmowy})).await;

    // Alice writes without being in voice anywhere.
    let sent = a.ok("chat.send", json!({"target": {"channel": default}, "text": "  hi there  "})).await;
    assert_eq!(sent["text"], "hi there");
    assert_eq!(sent["author"], a.id());
    assert_eq!(sent["author_name"], "alice");
    assert_eq!(sent["author_uid"], uid_of(&a.key));
    assert_eq!(sent["target"], json!({"channel": default}));
    assert!(sent["id"].as_u64().unwrap() < EPHEMERAL_BASE);
    assert!(sent["sent_at"].as_i64().unwrap() > 1_600_000_000_000);

    // Everyone gets it — also Carol, who is in another channel's voice.
    assert_eq!(b.next_event("chat.message").await, sent);
    assert_eq!(a.next_event("chat.message").await, sent, "the author gets the broadcast too");
    assert_eq!(c.next_event("chat.message").await, sent);
    assert_eq!(a.fails("chat.send", json!({"target": {"channel": 9999}, "text": "x"})).await, "forbidden");
}

#[tokio::test]
async fn password_channel_chat_needs_the_password_once() {
    let server = TestServer::start().await;
    let mut admin = admin(&server, "root").await;
    let secret = channel_id(&admin.create_channel(json!({"name": "Secret", "password": "pw"})).await);
    let mut a = Client::connect(&server, "alice").await;
    let mut b = Client::connect(&server, "bob").await;
    assert_eq!(a.fails("chat.send", json!({"target": {"channel": secret}, "text": "x"})).await, "forbidden");
    assert_eq!(a.fails("chat.history", json!({"channel": secret})).await, "forbidden");

    // Entering once unlocks the chat for the session, even after leaving voice.
    a.ok("channel.join", json!({"channel": secret, "password": "pw"})).await;
    a.ok("channel.leave", json!({})).await;
    a.ok("chat.send", json!({"target": {"channel": secret}, "text": "inside"})).await;
    // Admins (channel_join_locked) read along; Bob does not.
    assert_eq!(admin.next_event("chat.message").await["text"], "inside");
    b.sync().await;
    assert!(b.drain_events("chat.message").await.is_empty());
    assert_eq!(a.ok("chat.history", json!({"channel": secret})).await["messages"][0]["text"], "inside");
}

#[tokio::test]
async fn leaving_voice_keeps_you_on_the_server() {
    let server = TestServer::start().await;
    let mut a = Client::connect(&server, "alice").await;
    let mut b = Client::connect(&server, "bob").await;
    let default = a.default_channel();
    a.ok("channel.join", json!({"channel": default})).await;
    b.next_event_where("client.updated", |d| d["id"] == a.id() && d["channel"] == default).await;

    a.ok("channel.leave", json!({})).await;
    b.next_event_where("client.updated", |d| d["id"] == a.id() && d["channel"].is_null()).await;
    // Leaving twice is harmless and Alice can still chat.
    a.ok("channel.leave", json!({})).await;
    a.ok("chat.send", json!({"target": {"channel": default}, "text": "still here"})).await;
    assert_eq!(b.next_event("chat.message").await["text"], "still here");
    assert!(b.drain_events("client.left").await.is_empty());
}

#[tokio::test]
async fn read_state_follows_the_user_across_devices() {
    let server = TestServer::start().await;
    let key = new_key();
    let mut phone = Client::connect_as(&server, key.clone(), "alice", None).await.unwrap();
    let default = phone.default_channel();
    // A channel never opened counts as read up to now.
    assert!(phone.welcome.unread.iter().all(|u| u.count == 0));

    let mut b = Client::connect(&server, "bob").await;
    let mut last = 0;
    for i in 0..3 {
        last = b.ok("chat.send", json!({"target": {"channel": default}, "text": format!("m{i}")})).await["id"]
            .as_u64()
            .unwrap();
    }
    let laptop = Client::connect_as(&server, key.clone(), "alice", None).await.unwrap();
    let unread = |c: &Client| c.welcome.unread.iter().find(|u| u.channel == default).unwrap().count;
    assert_eq!(unread(&laptop), 3);

    // Reading on one device is reflected on the others.
    phone.ok("chat.read", json!({"channel": default, "message": last})).await;
    let mut laptop = laptop;
    let read = laptop.next_event("chat.read").await;
    assert_eq!(read, json!({"channel": default, "message": last}));
    assert!(b.drain_events("chat.read").await.is_empty(), "other users are not told");
    let tablet = Client::connect_as(&server, key, "alice", None).await.unwrap();
    assert_eq!(unread(&tablet), 0);
}

#[tokio::test]
async fn welcome_lists_members_including_offline_ones() {
    let server = TestServer::start().await;
    let gone = Client::connect(&server, "gone").await;
    let gone_uid = gone.welcome.uid.clone();
    gone.close().await;
    let a = Client::connect(&server, "alice").await;
    let member = a.welcome.members.iter().find(|m| m.uid == gone_uid).expect("offline member listed");
    assert_eq!(member.nickname, "gone");
    assert!(!a.welcome.clients.iter().any(|c| c.uid == gone_uid));
    assert!(a.welcome.members.iter().any(|m| m.uid == a.welcome.uid));
}

#[tokio::test]
async fn private_messages_go_to_target_and_sender_only() {
    let server = TestServer::start().await;
    let mut a = Client::connect(&server, "alice").await;
    let mut b = Client::connect(&server, "bob").await;
    let mut c = Client::connect(&server, "carol").await;

    let first = a.ok("chat.send", json!({"target": {"client": c.id()}, "text": "psst"})).await;
    assert_eq!(first["target"], json!({"client": c.id()}));
    assert!(first["id"].as_u64().unwrap() >= EPHEMERAL_BASE);
    assert_eq!(c.next_event("chat.message").await, first);
    assert_eq!(a.next_event("chat.message").await, first);
    assert!(b.drain_events("chat.message").await.is_empty());

    // Private messages work across channels and ids keep increasing.
    c.ok("channel.join", json!({"channel": seed_channel(&c, "AFK")})).await;
    let second = b.ok("chat.send", json!({"target": {"client": c.id()}, "text": "again"})).await;
    assert!(second["id"].as_u64().unwrap() > first["id"].as_u64().unwrap());
    assert_eq!(c.next_event("chat.message").await["text"], "again");
    assert!(a.drain_events("chat.message").await.is_empty());

    assert_eq!(a.fails("chat.send", json!({"target": {"client": 9999}, "text": "x"})).await, "not_found");
    // Messaging yourself delivers once.
    a.ok("chat.send", json!({"target": {"client": a.id()}, "text": "note"})).await;
    assert_eq!(a.drain_events("chat.message").await.len(), 1);
}

#[tokio::test]
async fn server_messages_reach_everyone() {
    let server = TestServer::start().await;
    let mut a = Client::connect(&server, "alice").await;
    let mut b = Client::connect(&server, "bob").await;
    b.ok("channel.join", json!({"channel": seed_channel(&b, "AFK")})).await;

    let sent = a.ok("chat.send", json!({"target": "server", "text": "hello all"})).await;
    assert_eq!(sent["target"], "server");
    assert!(sent["id"].as_u64().unwrap() >= EPHEMERAL_BASE);
    assert_eq!(b.next_event("chat.message").await, sent);
    assert_eq!(a.next_event("chat.message").await, sent);

    // Server messages are not stored.
    let history = a.ok("chat.history", json!({"channel": a.default_channel()})).await;
    assert_eq!(history["messages"], json!([]));
}

#[tokio::test]
async fn chat_text_is_validated() {
    let server = TestServer::start().await;
    let mut a = Client::connect(&server, "alice").await;
    let to = json!({"channel": a.default_channel()});
    assert_eq!(a.fails("chat.send", json!({"target": to, "text": ""})).await, "bad_request");
    assert_eq!(a.fails("chat.send", json!({"target": to, "text": "  \n "})).await, "bad_request");
    assert_eq!(a.fails("chat.send", json!({"target": to, "text": "x".repeat(4001)})).await, "bad_request");
    a.ok("chat.send", json!({"target": to, "text": "x".repeat(4000)})).await;
}

#[tokio::test]
async fn chat_history_pages_backwards() {
    let server = TestServer::start().await;
    let mut a = Client::connect(&server, "alice").await;
    let channel = a.default_channel();
    let mut sent = Vec::new();
    for i in 0..5 {
        let m = a.ok("chat.send", json!({"target": {"channel": channel}, "text": format!("m{i}")})).await;
        sent.push(m["id"].as_u64().unwrap());
    }
    // Interleave ephemeral traffic, which must never show up in history.
    a.ok("chat.send", json!({"target": "server", "text": "ephemeral"})).await;

    let texts = |h: &Value| -> Vec<String> {
        h["messages"].as_array().unwrap().iter().map(|m| m["text"].as_str().unwrap().to_owned()).collect()
    };
    let all = a.ok("chat.history", json!({"channel": channel})).await;
    assert_eq!(texts(&all), ["m0", "m1", "m2", "m3", "m4"], "oldest first");
    let first = &all["messages"][0];
    assert_eq!(first["author_name"], "alice");
    assert_eq!(first["author_uid"], uid_of(&a.key));
    assert_eq!(first["target"], json!({"channel": channel}));
    assert_eq!(first["id"], sent[0]);

    let page1 = a.ok("chat.history", json!({"channel": channel, "limit": 2})).await;
    assert_eq!(texts(&page1), ["m3", "m4"]);
    let page2 =
        a.ok("chat.history", json!({"channel": channel, "limit": 2, "before": page1["messages"][0]["id"]})).await;
    assert_eq!(texts(&page2), ["m1", "m2"]);
    let page3 =
        a.ok("chat.history", json!({"channel": channel, "limit": 2, "before": page2["messages"][0]["id"]})).await;
    assert_eq!(texts(&page3), ["m0"]);
    let page4 = a.ok("chat.history", json!({"channel": channel, "before": page3["messages"][0]["id"]})).await;
    assert_eq!(texts(&page4), Vec::<String>::new());

    // limit is clamped to 1..=100 rather than rejected.
    assert_eq!(texts(&a.ok("chat.history", json!({"channel": channel, "limit": 0})).await), ["m4"]);

    // Another client can read the history of a channel it is not in; unknown channels are 404.
    let mut b = Client::connect(&server, "bob").await;
    b.ok("channel.join", json!({"channel": seed_channel(&b, "AFK")})).await;
    assert_eq!(texts(&b.ok("chat.history", json!({"channel": channel})).await).len(), 5);
    assert_eq!(b.fails("chat.history", json!({"channel": 9999})).await, "not_found");
}

#[tokio::test]
async fn history_of_a_locked_channel_requires_being_inside() {
    let server = TestServer::start().await;
    let mut admin = admin(&server, "root").await;
    let vault = channel_id(&admin.create_channel(json!({"name": "Vault", "password": "pw"})).await);
    admin.ok("channel.join", json!({"channel": vault})).await;
    admin.ok("chat.send", json!({"target": {"channel": vault}, "text": "secret"})).await;

    let mut outsider = Client::connect(&server, "outsider").await;
    assert_eq!(outsider.fails("chat.history", json!({"channel": vault})).await, "forbidden");
    outsider.ok("channel.join", json!({"channel": vault, "password": "pw"})).await;
    let h = outsider.ok("chat.history", json!({"channel": vault})).await;
    assert_eq!(h["messages"][0]["text"], "secret");
}

#[tokio::test]
async fn deleting_a_channel_drops_its_history() {
    let server = TestServer::start().await;
    let mut admin = admin(&server, "root").await;
    let ch = channel_id(&admin.create_channel(json!({"name": "Temp"})).await);
    admin.ok("channel.join", json!({"channel": ch})).await;
    admin.ok("chat.send", json!({"target": {"channel": ch}, "text": "bye"})).await;
    admin.ok("channel.delete", json!({"channel": ch})).await;
    assert_eq!(admin.fails("chat.history", json!({"channel": ch})).await, "not_found");
}

// -------------------------------------------------------- client.update

#[tokio::test]
async fn member_nickname_is_validated_and_broadcast() {
    let server = TestServer::start().await;
    let mut a = Client::connect(&server, "alice").await;
    let mut b = Client::connect(&server, "bob").await;

    for bad in ["", "   ", &"x".repeat(33), "new\nline"] {
        assert_eq!(
            a.fails("member.nickname", json!({"uid": a.welcome.uid, "nickname": bad})).await,
            "bad_request",
            "{bad:?}"
        );
    }
    assert!(b.drain_events("client.updated").await.is_empty(), "rejected updates are silent");

    a.ok("member.nickname", json!({"uid": a.welcome.uid, "nickname": "  Alicia "})).await;
    let ev = b.next_event_where("client.updated", |d| d["id"] == a.id()).await;
    assert_eq!(ev["nickname"], "Alicia");
    assert_eq!(a.next_event("client.updated").await["nickname"], "Alicia");
    // A 32-character nickname is the maximum.
    a.ok("member.nickname", json!({"uid": a.welcome.uid, "nickname": "n".repeat(32)})).await;
    // Authors of later messages carry the new nickname.
    a.ok("member.nickname", json!({"uid": a.welcome.uid, "nickname": "Final"})).await;
    let msg = a.ok("chat.send", json!({"target": "server", "text": "x"})).await;
    assert_eq!(msg["author_name"], "Final");
}

#[tokio::test]
async fn client_update_mute_deafen_and_away() {
    let server = TestServer::start().await;
    let mut a = Client::connect(&server, "alice").await;
    let mut b = Client::connect(&server, "bob").await;

    a.ok("client.update", json!({"muted": true})).await;
    let ev = b.next_event_where("client.updated", |d| d["id"] == a.id()).await;
    assert_eq!((ev["muted"].as_bool(), ev["deafened"].as_bool()), (Some(true), Some(false)));

    a.ok("client.update", json!({"deafened": true, "muted": false})).await;
    let ev = b.next_event_where("client.updated", |d| d["deafened"] == true).await;
    assert_eq!(ev["muted"], false);

    a.ok("client.update", json!({"away": " lunch "})).await;
    let ev = b.next_event_where("client.updated", |d| d["away"] == "lunch").await;
    assert_eq!(ev["deafened"], true, "unrelated flags are preserved");
    assert_eq!(a.fails("client.update", json!({"away": "a".repeat(81)})).await, "bad_request");

    a.ok("client.update", json!({"away": ""})).await;
    let ev = b.next_event_where("client.updated", |d| d["away"].is_null()).await;
    assert_eq!(ev["id"], a.id());

    // Nothing to change is still a valid request.
    a.ok("client.update", json!({})).await;
}

#[tokio::test]
async fn client_move_requires_permission_and_valid_targets() {
    let server = TestServer::start().await;
    let mut admin = admin(&server, "root").await;
    let mut m = Client::connect(&server, "member").await;
    let rozmowy = seed_channel(&m, "General");

    admin.ok("client.move", json!({"client": m.id(), "channel": rozmowy})).await;
    let mid = m.id();
    assert_eq!(m.next_event_where("client.updated", |d| d["id"] == mid).await["channel"], rozmowy);
    assert_eq!(admin.fails("client.move", json!({"client": m.id(), "channel": 9999})).await, "not_found");
    assert_eq!(admin.fails("client.move", json!({"client": 9999, "channel": rozmowy})).await, "not_found");
}

#[tokio::test]
async fn server_update_is_broadcast_and_visible_to_new_clients() {
    let server = TestServer::start().await;
    let mut admin = admin(&server, "root").await;
    let mut a = Client::connect(&server, "alice").await;
    admin.ok("server.update", json!({"name": " My Server ", "welcome": "Be nice"})).await;
    let ev = a.next_event("server.updated").await;
    assert_eq!(ev["name"], "My Server");
    assert_eq!(ev["welcome"], "Be nice");
    assert_eq!(admin.fails("server.update", json!({"name": ""})).await, "bad_request");
    assert_eq!(admin.fails("server.update", json!({"welcome": "w".repeat(1001)})).await, "bad_request");
    let b = Client::connect(&server, "bob").await;
    assert_eq!(b.welcome.server.name, "My Server");
    assert_eq!(b.welcome.server.welcome, "Be nice");
}

// ---------------------------------------------------------------- kick

#[tokio::test]
async fn kick_disconnects_target_and_notifies_others() {
    let server = TestServer::start().await;
    let mut admin = admin(&server, "root").await;
    let mut victim = Client::connect(&server, "victim").await;
    let mut bystander = Client::connect(&server, "bystander").await;
    let victim_id = victim.id();

    assert_eq!(admin.fails("client.kick", json!({"client": admin.id()})).await, "bad_request");
    assert_eq!(admin.fails("client.kick", json!({"client": 9999})).await, "not_found");

    admin.ok("client.kick", json!({"client": victim_id, "reason": "too loud"})).await;

    let reason = victim.expect_disconnect().await;
    assert_eq!(reason, json!({"kind": "kicked", "by": "root", "reason": "too loud"}));
    for observer in [&mut admin, &mut bystander] {
        let left = observer.next_event("client.left").await;
        assert_eq!(left["client"], victim_id);
        assert_eq!(left["reason"], json!({"kind": "kicked", "by": "root", "reason": "too loud"}));
    }
    // The kicked session is gone from the roster.
    let fresh = Client::connect(&server, "fresh").await;
    assert!(fresh.welcome.clients.iter().all(|c| c.id != victim_id));
}

#[tokio::test]
async fn kick_without_reason() {
    let server = TestServer::start().await;
    let mut admin = admin(&server, "root").await;
    let mut victim = Client::connect(&server, "victim").await;
    admin.ok("client.kick", json!({"client": victim.id()})).await;
    assert_eq!(victim.expect_disconnect().await, json!({"kind": "kicked", "by": "root", "reason": null}));
}

// ---------------------------------------------------------- rate limits

#[tokio::test]
async fn request_bursts_are_rate_limited() {
    let server = TestServer::start().await;
    let mut a = Client::connect(&server, "alice").await;

    // Burst allowance is 40 requests; fire well beyond it without waiting.
    let mut sent = Vec::new();
    for _ in 0..80 {
        sent.push(a.send("ping", json!({})).await);
    }
    let (mut ok, mut limited) = (0, 0);
    for id in sent {
        match a.reply(id).await {
            Ok(_) => ok += 1,
            Err(e) => {
                assert_eq!(e.code, vc_proto::ErrorCode::RateLimited);
                limited += 1;
            }
        }
    }
    assert!(limited >= 1, "expected rate limiting, got {ok} ok / {limited} limited");
    assert!(ok >= 40, "the burst allowance must be honoured, got {ok} ok");

    // Tokens refill over time, so the connection stays usable.
    tokio::time::sleep(Duration::from_millis(500)).await;
    a.sync().await;
}
