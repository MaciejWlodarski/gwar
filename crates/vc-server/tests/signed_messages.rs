//! Message signatures are enforced before any body is stored or broadcast.
mod common;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use common::*;
use ed25519_dalek::Signer;
use serde_json::{Value, json};
use vc_proto::{Attachment, DeviceCertificate, MessageSignature};
use vc_server::core::now_ms;

async fn device_client(server: &TestServer) -> Client {
    let account = new_key();
    let device = new_key();
    let mut cert = DeviceCertificate {
        account_key: public_key(&account),
        device_key: public_key(&device),
        issued_at: now_ms() - 1000,
        expires_at: now_ms() + 86_400_000,
        signature: String::new(),
    };
    cert.signature = B64.encode(account.sign(vc_server::identity::device_statement(&cert).as_bytes()).to_bytes());
    Client::connect_with(server, device, |raw, key| {
        let mut hello = raw.hello(key, "device", None);
        set_certificate(&mut hello, serde_json::to_value(cert).unwrap(), key);
        hello
    })
    .await
    .unwrap()
}

fn send(target: Value, text: &str, sig: &MessageSignature) -> Value {
    json!({"target": target, "text": text, "signature": sig})
}

async fn valid_flow(connect: bool) {
    let server = TestServer::start().await;
    let mut alice = if connect { device_client(&server).await } else { Client::connect(&server, "alice").await };
    let mut observer = Client::connect(&server, "observer").await;
    let channel = alice.default_channel();
    let target = format!("channel:{channel}");
    let sig = alice.signature(&target, "hello", &[], &[]);
    let sent = alice.ok("chat.send", send(json!({"channel": channel}), "hello", &sig)).await;
    assert_eq!(sent["signature"], json!(sig));
    assert_eq!(sent["sent_at"], sig.created_at);
    assert_eq!(observer.next_event("chat.message").await["signature"], json!(sig));
    let id = sent["id"].as_u64().unwrap();
    let mut edit = sig.clone();
    edit.revision = sig.revision + 1;
    edit.previous_revision_hash = vc_proto::message_revision_hash(&sig);
    edit.signed_at = now_ms();
    alice.resign(&mut edit, &target, "edited", &[], &[]);
    let edited = alice.ok("chat.edit", json!({"message": id, "text": "edited", "signature": edit})).await;
    assert_eq!(edited["signature"], json!(edit));
    assert_eq!(edited["edited_at"], json!(edit.signed_at));
    assert_eq!(observer.next_event("chat.edited").await["signature"], json!(edit));
    let history = observer.ok("chat.history", json!({"channel": channel})).await;
    assert_eq!(history["messages"][0]["signature"], json!(edit));
    assert_eq!(history["messages"][0]["sent_at"], sig.created_at);
}

#[tokio::test]
async fn local_identity_send_edit_and_history() {
    valid_flow(false).await;
}
#[tokio::test]
async fn connect_device_send_edit_and_history() {
    valid_flow(true).await;
}

#[tokio::test]
async fn rejects_bad_signatures_hosts_targets_authors_times_and_mentions() {
    let server = TestServer::start().await;
    let mut alice = Client::connect(&server, "alice").await;
    let channel = alice.default_channel();
    let target = format!("channel:{channel}");
    let good = alice.signature(&target, "hello", &[], &[]);
    let mut cases = Vec::new();
    let mut bad = good.clone();
    bad.signature = B64.encode([0; 64]);
    cases.push(send(json!({"channel": channel}), "hello", &bad));
    let mut bad = good.clone();
    let statement = vc_proto::message_statement(
        "wrong.example",
        &target,
        &alice.welcome.uid,
        &bad,
        &vc_proto::message_body_hash("hello", &[], &[]),
    );
    bad.signature = B64.encode(alice.key.sign(statement.as_bytes()).to_bytes());
    cases.push(send(json!({"channel": channel}), "hello", &bad));
    let bad = alice.signature("server", "hello", &[], &[]);
    cases.push(send(json!({"channel": channel}), "hello", &bad));
    let mut bad = good.clone();
    let other = new_key();
    bad.key = public_key(&other);
    let statement = vc_proto::message_statement(
        &alice.server_key,
        &target,
        &alice.welcome.uid,
        &bad,
        &vc_proto::message_body_hash("hello", &[], &[]),
    );
    bad.signature = B64.encode(other.sign(statement.as_bytes()).to_bytes());
    cases.push(send(json!({"channel": channel}), "hello", &bad));
    let mut bad = good.clone();
    bad.certificate = Some(DeviceCertificate {
        account_key: good.key.clone(),
        device_key: good.key.clone(),
        issued_at: 0,
        expires_at: i64::MAX,
        signature: "wrong".into(),
    });
    alice.resign(&mut bad, &target, "hello", &[], &[]);
    cases.push(send(json!({"channel": channel}), "hello", &bad));
    for delta in [-600_000, 600_000] {
        let mut bad = good.clone();
        bad.created_at = now_ms() + delta;
        alice.resign(&mut bad, &target, "hello", &[], &[]);
        cases.push(send(json!({"channel": channel}), "hello", &bad));
    }
    let mut changed_mentions = send(json!({"channel": channel}), "hello", &good);
    changed_mentions["mentions"] = json!([alice.welcome.uid]);
    cases.push(changed_mentions);
    let unknown = alice.signature(&target, "hello", &["unknown".into()], &[]);
    let mut unknown_mentions = send(json!({"channel": channel}), "hello", &unknown);
    unknown_mentions["mentions"] = json!(["unknown"]);
    cases.push(unknown_mentions);
    for case in cases {
        assert_eq!(alice.fails("chat.send", case).await, "bad_request");
    }
    assert_eq!(
        alice.fails("chat.send", json!({"target": "server", "text": "unsigned", "signature": null})).await,
        "bad_request"
    );
    assert!(alice.ok("chat.history", json!({"channel": channel})).await["messages"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn edit_rejects_changed_nonce_creation_time_stale_edit_and_body() {
    let server = TestServer::start().await;
    let mut alice = Client::connect(&server, "alice").await;
    let channel = alice.default_channel();
    let target = format!("channel:{channel}");
    let good = alice.signature(&target, "hello", &[], &[]);
    let sent = alice.ok("chat.send", send(json!({"channel": channel}), "hello", &good)).await;
    for kind in ["nonce", "created", "stale", "body", "mentions", "missing time", "predecessor", "revision"] {
        let mut edit = good.clone();
        edit.revision = good.revision + 1;
        edit.previous_revision_hash = vc_proto::message_revision_hash(&good);
        edit.signed_at = now_ms();
        match kind {
            "nonce" => edit.message_id = B64.encode([0; 16]),
            "created" => edit.created_at += 1,
            "stale" => edit.signed_at = now_ms() - 300_001,
            "missing time" => edit.revision = 0,
            "predecessor" => edit.previous_revision_hash = hash(b"substituted"),
            "revision" => edit.revision = 2,
            _ => {}
        }
        alice.resign(&mut edit, &target, "edited", &[], &[]);
        let mut request = json!({"message": sent["id"], "text": if kind == "body" { "tampered" } else { "edited" }, "signature": edit});
        if kind == "mentions" {
            request["mentions"] = json!([alice.welcome.uid]);
        }
        assert_eq!(alice.fails("chat.edit", request).await, "bad_request", "{kind}");
    }
    let history = alice.ok("chat.history", json!({"channel": channel})).await;
    assert_eq!(history["messages"][0]["signature"], json!(good));
    assert_eq!(history["messages"][0]["text"], "hello");
}

async fn upload(server: &TestServer, client: &mut Client, name: &str, bytes: &[u8]) -> Attachment {
    let slot = client
        .ok("file.upload", json!({"name": name, "mime": "text/plain", "size": bytes.len(), "sha256": hash(bytes)}))
        .await;
    let response = reqwest::Client::new()
        .put(format!("{}{}", server.http(), slot["upload_url"].as_str().unwrap()))
        .body(bytes.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

#[tokio::test]
async fn attachment_hashes_and_order_are_enforced_and_persisted() {
    let server = TestServer::start().await;
    let mut alice = Client::connect(&server, "alice").await;
    let channel = alice.default_channel();
    let target = format!("channel:{channel}");
    let first = upload(&server, &mut alice, "first.txt", b"first").await;
    let second = upload(&server, &mut alice, "second.txt", b"second").await;
    let files = vec![second, first];
    let mut wrong = files.clone();
    wrong[0].sha256 = hash(b"forged");
    let bad = alice.signature(&target, "files", &[], &wrong);
    let request = |sig: &MessageSignature| json!({"target": {"channel": channel}, "text": "files", "attachments": files.iter().map(|a| &a.id).collect::<Vec<_>>(), "signature": sig});
    assert_eq!(alice.fails("chat.send", request(&bad)).await, "bad_request");
    let sig = alice.signature(&target, "files", &[], &files);
    let sent = alice.ok("chat.send", request(&sig)).await;
    assert_eq!(sent["attachments"], json!(files));
    let history = alice.ok("chat.history", json!({"channel": channel})).await;
    assert_eq!(history["messages"][0]["attachments"], json!(files));
    assert_eq!(history["messages"][0]["signature"], json!(sig));
}

#[tokio::test]
async fn upload_rejects_different_received_bytes() {
    let server = TestServer::start().await;
    let mut alice = Client::connect(&server, "alice").await;
    let slot = alice
        .ok("file.upload", json!({"name": "file", "mime": "text/plain", "size": 5, "sha256": hash(b"hello")}))
        .await;
    let response = reqwest::Client::new()
        .put(format!("{}{}", server.http(), slot["upload_url"].as_str().unwrap()))
        .body("other")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(
        reqwest::get(format!("{}/files/{}/file", server.http(), slot["file"].as_str().unwrap()))
            .await
            .unwrap()
            .status(),
        404
    );
}

#[tokio::test]
async fn private_targets_bind_the_recipient_uid() {
    let server = TestServer::start().await;
    let mut alice = Client::connect(&server, "alice").await;
    let bob = Client::connect(&server, "bob").await;
    let good = alice.signature(&vc_proto::dm_conversation(&alice.welcome.uid, &bob.welcome.uid), "private", &[], &[]);
    alice.ok("chat.send", send(json!({"client": bob.id()}), "private", &good)).await;
    let bad = alice.signature(&vc_proto::dm_conversation(&alice.welcome.uid, &alice.welcome.uid), "private", &[], &[]);
    assert_eq!(alice.fails("chat.send", send(json!({"client": bob.id()}), "private", &bad)).await, "bad_request");
}

#[tokio::test]
async fn server_key_and_exact_session_certificate_are_enforced() {
    let server = TestServer::start().await;
    let mut alice = Client::connect(&server, "alice").await;
    let mut sig = alice.signature("server", "replay", &[], &[]);
    sig.server_key = public_key(&new_key());
    alice.resign(&mut sig, "server", "replay", &[], &[]);
    assert_eq!(alice.fails("chat.send", send(json!("server"), "replay", &sig)).await, "bad_request");
    let mut device = device_client(&server).await;
    let mut sig = device.signature("server", "device", &[], &[]);
    sig.certificate.as_mut().unwrap().expires_at -= 1;
    device.resign(&mut sig, "server", "device", &[], &[]);
    assert_eq!(device.fails("chat.send", send(json!("server"), "device", &sig)).await, "bad_request");
}

#[tokio::test]
async fn removing_a_mentioned_member_preserves_the_signed_body() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let bob = Client::connect(&server, "bob").await;
    let channel = root.default_channel();
    let msg = root
        .ok("chat.send", json!({"target": {"channel": channel}, "text": "mention", "mentions": [bob.welcome.uid]}))
        .await;
    root.ok("member.remove", json!({"uid": bob.welcome.uid})).await;
    let history = root.ok("chat.history", json!({"channel": channel})).await;
    assert_eq!(history["messages"][0]["mentions"], msg["mentions"]);
    assert_eq!(history["messages"][0]["signature"], msg["signature"]);
}

#[tokio::test]
async fn message_id_reuse_is_refused_even_after_server_removal() {
    let server = TestServer::start().await;
    let mut alice = admin(&server, "alice").await;
    let channel = alice.default_channel();
    let sig = alice.signature(&format!("channel:{channel}"), "once", &[], &[]);
    let sent = alice.ok("chat.send", send(json!({"channel": channel}), "once", &sig)).await;
    assert_eq!(alice.fails("chat.send", send(json!({"channel": channel}), "once", &sig)).await, "conflict");
    alice.ok("chat.delete", json!({"message": sent["id"]})).await;
    assert_eq!(alice.fails("chat.send", send(json!({"channel": channel}), "once", &sig)).await, "conflict");
}

#[tokio::test]
async fn author_deletion_is_a_final_signed_revision_and_server_removal_needs_permission() {
    let server = TestServer::start().await;
    let mut alice = Client::connect(&server, "alice").await;
    let channel = alice.default_channel();
    let sent = alice.ok("chat.send", json!({"target": {"channel": channel}, "text": "original"})).await;
    assert_eq!(alice.fails("chat.delete", json!({"message": sent["id"]})).await, "forbidden");
    let deleted = alice.ok("chat.edit", json!({"message": sent["id"], "text": vc_proto::DELETION_TEXT})).await;
    assert_eq!(deleted["signature"]["revision"], 1);
    assert_eq!(deleted["text"], vc_proto::DELETION_TEXT);
    assert!(deleted.get("attachments").is_none());
    assert_eq!(alice.fails("chat.edit", json!({"message": sent["id"], "text": "resurrected"})).await, "bad_request");
    let history = alice.ok("chat.history", json!({"channel": channel})).await;
    assert_eq!(history["messages"][0]["signature"], deleted["signature"]);
}

#[tokio::test]
async fn post_expiry_edit_is_refused_and_preserves_stored_body() {
    let server = TestServer::start().await;
    let account = new_key();
    let device = new_key();
    let certificate = certificate(&account, &device, now_ms() - 1000, now_ms() + 150);
    let mut alice = Client::connect_with(&server, device, |raw, key| {
        let mut hello = raw.hello(key, "device", None);
        set_certificate(&mut hello, certificate, key);
        hello
    })
    .await
    .unwrap();
    let channel = alice.default_channel();
    let original = alice.ok("chat.send", json!({"target": {"channel": channel}, "text": "original"})).await;
    let previous: MessageSignature = serde_json::from_value(original["signature"].clone()).unwrap();
    let mut sig = previous.clone();
    sig.revision = 1;
    sig.previous_revision_hash = vc_proto::message_revision_hash(&previous);
    // Even with a forged pre-expiry signing time, the session itself has expired.
    sig.signed_at = previous.created_at;
    alice.resign(&mut sig, &format!("channel:{channel}"), "backdated edit", &[], &[]);
    tokio::time::sleep(std::time::Duration::from_millis(180)).await;
    // Gateway/core closes the session exactly at expiry; no edit can get an OK reply.
    if alice.send_raw_request(json!({"id": 999, "op": "chat.edit", "d": {"message": original["id"], "text": "backdated edit", "signature": sig}})).await {
        while let Some(frame) = alice.raw_frame().await { if frame.get("re") == Some(&json!(999)) { assert!(frame.get("err").is_some(), "expired edit accepted: {frame}"); } }
    }
    let mut observer = Client::connect(&server, "observer").await;
    let history = observer.ok("chat.history", json!({"channel": channel})).await;
    assert_eq!(history["messages"][0]["text"], "original");
}

#[tokio::test]
async fn revoked_session_cannot_backdate_an_edit() {
    let server = TestServer::start().await;
    let account = new_key();
    let device = new_key();
    let cert = certificate(&account, &device, now_ms() - 1000, now_ms() + 86_400_000);
    let mut alice = Client::connect_with(&server, device, |raw, key| {
        let mut hello = raw.hello(key, "device", None);
        set_certificate(&mut hello, cert, key);
        hello
    })
    .await
    .unwrap();
    let channel = alice.default_channel();
    let original = alice.ok("chat.send", json!({"target": {"channel": channel}, "text": "original"})).await;
    let mut sig: MessageSignature = serde_json::from_value(original["signature"].clone()).unwrap();
    sig.previous_revision_hash = vc_proto::message_revision_hash(&sig);
    sig.revision = 1;
    alice.resign(&mut sig, &format!("channel:{channel}"), "backdated", &[], &[]);
    let c = alice.certificate.as_ref().unwrap();
    let at = now_ms();
    let signature = B64.encode(
        account.sign(vc_server::identity::revoke_statement(&c.account_key, &c.device_key, at).as_bytes()).to_bytes(),
    );
    server
        .running
        .core
        .revoked(
            vec![vc_server::core::RevokedDevice {
                seq: 1,
                signature,
                account_key: c.account_key.clone(),
                device_key: c.device_key.clone(),
                revoked_at: at,
            }],
            1,
        )
        .await;
    server.running.core.info().await.unwrap();
    if alice.send_raw_request(json!({"id": 999, "op": "chat.edit", "d": {"message": original["id"], "text": "backdated", "signature": sig}})).await {
        while let Some(frame) = alice.raw_frame().await { if frame.get("re") == Some(&json!(999)) { assert!(frame.get("err").is_some(), "revoked edit accepted: {frame}"); } }
    }
    let mut observer = Client::connect(&server, "observer").await;
    let history = observer.ok("chat.history", json!({"channel": channel})).await;
    assert_eq!(history["messages"][0]["text"], "original");
}

#[tokio::test]
async fn edit_after_certificate_renewal_keeps_the_message_identity() {
    let server = TestServer::start().await;
    let account = new_key();
    let device = new_key();
    let connect = |cert: Value| {
        Client::connect_with(&server, device.clone(), move |raw, key| {
            let mut hello = raw.hello(key, "device", None);
            set_certificate(&mut hello, cert, key);
            hello
        })
    };
    let mut alice = connect(certificate(&account, &device, now_ms() - 2000, now_ms() + 86_400_000)).await.unwrap();
    let channel = alice.default_channel();
    let target = format!("channel:{channel}");
    let sig = alice.signature(&target, "before renewal", &[], &[]);
    let id = alice.ok("chat.send", send(json!({"channel": channel}), "before renewal", &sig)).await["id"].clone();
    drop(alice);

    // The same device key, certified again after the message was written: a different certificate hash.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let renewed_at = now_ms();
    assert!(renewed_at > sig.created_at);
    let mut alice = connect(certificate(&account, &device, renewed_at, renewed_at + 2 * 86_400_000)).await.unwrap();
    assert_eq!(alice.certificate.as_ref().unwrap().issued_at, renewed_at);
    let mut edit = sig.clone();
    edit.certificate = alice.certificate.clone();
    edit.certificate_hash = vc_proto::certificate_hash(alice.certificate.as_ref()).unwrap();
    assert_ne!(edit.certificate_hash, sig.certificate_hash);
    edit.revision = 1;
    edit.previous_revision_hash = vc_proto::message_revision_hash(&sig);
    edit.signed_at = now_ms();
    alice.resign(&mut edit, &target, "after renewal", &[], &[]);
    alice.ok("chat.edit", json!({"message": id, "text": "after renewal", "signature": edit})).await;
    let mut observer = Client::connect(&server, "observer").await;
    let history = observer.ok("chat.history", json!({"channel": channel})).await;
    assert_eq!(history["messages"][0]["signature"], json!(edit));
    assert_eq!(history["messages"][0]["text"], "after renewal");
}

#[tokio::test]
async fn members_only_files_open_for_readers_of_their_channel() {
    let server = TestServer::start().await;
    let mut admin = admin(&server, "admin").await;
    let mut bob = Client::connect(&server, "bob").await;
    let get = |file: &Attachment, token: Option<&str>| {
        let mut request = reqwest::Client::new().get(format!("{}{}", server.http(), file.url));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        async move { request.send().await.unwrap().status().as_u16() }
    };
    let post = async |client: &mut Client, channel: u32, file: &Attachment| {
        let target = format!("channel:{channel}");
        let sig = client.signature(&target, "file", &[], std::slice::from_ref(file));
        client
            .ok(
                "chat.send",
                json!({"target": {"channel": channel}, "text": "file", "attachments": [&file.id], "signature": sig}),
            )
            .await;
    };
    let lobby = admin.default_channel();
    let open = upload(&server, &mut admin, "open.txt", b"open").await;
    post(&mut admin, lobby, &open).await;
    assert_eq!(admin.welcome.server.file_access, vc_proto::FileAccess::Link);
    assert_eq!(get(&open, None).await, 200, "links open for anyone by default");

    assert_eq!(bob.fails("server.update", json!({"file_access": "members"})).await, "forbidden");
    admin.ok("server.update", json!({"file_access": "members"})).await;
    let updated = bob.next_event("server.updated").await;
    assert_eq!(updated["file_access"], "members");
    let (admin_token, bob_token) = (admin.welcome.file_access.clone(), bob.welcome.file_access.clone());
    assert!(admin_token.len() >= 40 && admin_token != bob_token);
    assert_eq!(get(&open, None).await, 404);
    assert_eq!(get(&open, Some("not-a-session")).await, 404);
    assert_eq!(get(&open, Some(&bob_token)).await, 200);

    // A password channel's files are for those who can read it; an upload in no message is for nobody.
    let locked = admin.ok("channel.create", json!({"name": "Locked", "password": "secret"})).await["id"]
        .as_u64()
        .unwrap() as u32;
    let hidden = upload(&server, &mut admin, "hidden.txt", b"hidden").await;
    post(&mut admin, locked, &hidden).await;
    assert_eq!(get(&hidden, Some(&admin_token)).await, 200);
    assert_eq!(get(&hidden, Some(&bob_token)).await, 404);
    let draft = upload(&server, &mut admin, "draft.txt", b"draft").await;
    assert_eq!(get(&draft, Some(&admin_token)).await, 404);
    assert_eq!(get(&draft, Some(&bob_token)).await, 404);

    // The token ends with its session, and the server can go back to links.
    bob.close().await;
    admin.sync().await;
    assert_eq!(get(&open, Some(&bob_token)).await, 404);
    admin.ok("server.update", json!({"file_access": "link"})).await;
    assert_eq!(get(&hidden, None).await, 200);
}

#[tokio::test]
async fn migrated_v2_history_is_readable_edits_fail_cleanly_and_moderators_can_delete() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("gwar.sqlite");
    let store = vc_server::store::Store::open(&database).unwrap();
    let channel = store.channels().unwrap()[0].id;
    let key = new_key();
    let id = store.add_message(channel, &uid_of(&key), "legacy", "preserved body", 1).unwrap();
    drop(store);
    let db = rusqlite::Connection::open(&database).unwrap();
    let version: u32 = db.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    // Restore the pre-change schema and populate its old signature format.
    db.execute_batch("DROP INDEX mls_history_hashes; DROP INDEX mls_history_chunks; DROP INDEX mls_history_group_inbox; ALTER TABLE mls_history DROP COLUMN chunk_hash;").unwrap();
    db.pragma_update(None, "user_version", version - 2).unwrap();
    db.execute(
        "UPDATE messages SET signature=?1 WHERE id=?2",
        rusqlite::params![json!({"key":public_key(&key),"signature":"v2"}).to_string(), id],
    )
    .unwrap();
    drop(db);
    let server = TestServer::start_with(|c| {
        c.database = Some(database.clone());
        c.data_dir = Some(dir.path().to_path_buf());
    })
    .await;
    let mut author = Client::connect_with(&server, key, |raw, key| raw.hello(key, "legacy", None)).await.unwrap();
    let history = author.ok("chat.history", json!({"channel":channel})).await;
    assert_eq!(history["messages"][0]["text"], "preserved body");
    assert!(history["messages"][0].get("signature").is_none());
    let signature = author.signature(&format!("channel:{channel}"), "edited", &[], &[]);
    assert_eq!(
        author.fails("chat.edit", json!({"message":id,"text":"edited","signature":signature})).await,
        "bad_request"
    );
    author.redeem(&server.admin_token).await;
    author.ok("chat.delete", json!({"message":id})).await;
    assert_eq!(author.ok("chat.history", json!({"channel":channel})).await["messages"], json!([]));
}
