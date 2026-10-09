//! Roles, bans, invites, server settings, mentions, edits and uploads.

mod common;

use common::*;
use serde_json::{Value, json};

fn group_id(reply: &Value) -> u64 {
    reply["group"]["id"].as_u64().expect("group id")
}

/// A member with the groups `groups`, set by `admin`.
async fn with_groups(admin: &mut Client, member: &Client, groups: Value) {
    admin.ok("member.groups", json!({"uid": member.welcome.uid, "groups": groups})).await;
}

// ---------------------------------------------------------------- roles

#[tokio::test]
async fn custom_roles_grant_only_what_their_manager_holds() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mut alice = Client::connect(&server, "alice").await;
    let mut bob = Client::connect(&server, "bob").await;

    let mods = group_id(
        &root
            .ok(
                "group.create",
                json!({"name": "Mods", "permissions": ["client_kick", "group_manage"], "color": "#ff8800"}),
            )
            .await,
    );
    let created = bob.next_event("group.created").await;
    assert_eq!(created["name"], "Mods");
    assert_eq!(created["color"], "#ff8800");
    with_groups(&mut root, &alice, json!([2, mods])).await;
    let updated = bob.next_event_where("member.updated", |d| d["uid"] == alice.welcome.uid).await;
    assert_eq!(updated["groups"], json!([2, mods]));

    // Alice can kick now, but can't hand out what she doesn't have.
    assert_eq!(
        alice.fails("group.create", json!({"name": "Bosses", "permissions": ["server_manage"]})).await,
        "forbidden"
    );
    assert_eq!(alice.fails("member.groups", json!({"uid": bob.welcome.uid, "groups": [1]})).await, "forbidden");
    // Nor touch the admin.
    assert_eq!(alice.fails("member.groups", json!({"uid": root.welcome.uid, "groups": [2]})).await, "forbidden");
    alice.ok("client.kick", json!({"client": bob.id()})).await;
    bob.expect_disconnect().await;

    // Built-ins stay; custom groups can go.
    assert_eq!(root.fails("group.delete", json!({"group": 1})).await, "bad_request");
    assert_eq!(root.fails("group.update", json!({"group": 1, "permissions": []})).await, "bad_request");
    assert_eq!(root.fails("member.groups", json!({"uid": root.welcome.uid, "groups": [2]})).await, "bad_request");
    root.ok("group.delete", json!({"group": mods})).await;
    alice.next_event_where("group.deleted", |d| d["group"] == mods).await;
}

// ----------------------------------------------------------------- bans

#[tokio::test]
async fn bans_disconnect_and_keep_out_until_lifted() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let key = new_key();
    let mut eve = Client::connect_as(&server, key.clone(), "eve", None).await.unwrap();

    let ban = root.ok("ban.create", json!({"client": eve.id(), "reason": "spam", "duration": 3600})).await;
    let ban = &ban["ban"];
    assert_eq!(ban["nickname"], "eve");
    assert_eq!(ban["reason"], "spam");
    assert!(ban["expires_at"].as_i64().unwrap() > ban["created_at"].as_i64().unwrap());
    let gone = eve.expect_disconnect().await;
    assert_eq!(gone["kind"], "banned");

    let refused = Client::connect_as(&server, key.clone(), "eve", None).await.err().expect("banned");
    assert_eq!(refused.code, vc_proto::ErrorCode::Banned);
    assert!(refused.message.contains("spam"));

    let list = root.ok("ban.list", json!({})).await;
    assert_eq!(list["bans"].as_array().unwrap().len(), 1);
    // Members can't see or lift bans.
    let mut bob = Client::connect(&server, "bob").await;
    assert_eq!(bob.fails("ban.list", json!({})).await, "forbidden");
    assert_eq!(bob.fails("ban.create", json!({"client": root.id()})).await, "forbidden");

    root.ok("ban.delete", json!({"ban": ban["id"]})).await;
    Client::connect_as(&server, key, "eve", None).await.expect("ban lifted");
    // Nobody bans themselves or someone stronger.
    assert_eq!(root.fails("ban.create", json!({"client": root.id()})).await, "bad_request");
}

#[tokio::test]
async fn offline_members_can_be_banned_by_uid() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let key = new_key();
    let gone = Client::connect_as(&server, key.clone(), "gone", None).await.unwrap();
    let uid = gone.welcome.uid.clone();
    gone.close().await;
    root.ok("ban.create", json!({"uid": uid})).await;
    assert!(Client::connect_as(&server, key, "gone", None).await.is_err());
}

// -------------------------------------------------------------- invites

#[tokio::test]
async fn invites_admit_without_password_grant_groups_and_run_out() {
    let server = TestServer::start_with(|c| c.server_password = Some("secret".into())).await;
    let mut root = Client::connect_as(&server, new_key(), "root", Some("secret")).await.unwrap();
    root.redeem(&server.admin_token).await;
    let mods = group_id(&root.ok("group.create", json!({"name": "Mods", "permissions": ["client_kick"]})).await);
    let invite = root.ok("invite.create", json!({"max_uses": 1, "expires_in": 3600, "group": mods})).await;
    let code = invite["invite"]["code"].as_str().unwrap().to_owned();

    let with_invite = |code: String| {
        move |raw: &RawConn, key: &_| {
            let mut hello = raw.hello(key, "guest", None);
            hello["invite"] = json!(code);
            hello
        }
    };
    let guest = Client::connect_with(&server, new_key(), with_invite(code.clone())).await.expect("invite admits");
    let me = guest.welcome.clients.iter().find(|c| c.id == guest.id()).unwrap();
    assert!(me.groups.contains(&(mods as u32)), "the invite's group is granted");

    // Single use: the next one needs the password again.
    let err = Client::connect_with(&server, new_key(), with_invite(code)).await.err().expect("used up");
    assert_eq!(err.code, vc_proto::ErrorCode::WrongPassword);
    let listed = root.ok("invite.list", json!({})).await;
    assert_eq!(listed["invites"][0]["uses"], 1);
}

#[tokio::test]
async fn members_create_invites_but_not_for_groups_above_them() {
    let server = TestServer::start().await;
    let mut alice = Client::connect(&server, "alice").await;
    let invite = alice.ok("invite.create", json!({})).await;
    assert!(invite["invite"]["code"].as_str().unwrap().len() >= 8);
    assert_eq!(alice.fails("invite.create", json!({"group": 1})).await, "forbidden");
    let code = invite["invite"]["code"].clone();
    alice.ok("invite.delete", json!({"code": code})).await;
    assert_eq!(alice.fails("invite.delete", json!({"code": code})).await, "forbidden");
}

// ------------------------------------------------------------- settings

#[tokio::test]
async fn server_settings_change_password_default_channel_and_limits() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let general = seed_channel(&root, "General");
    root.ok("server.update", json!({"password": "hunter2", "default_channel": general, "max_clients": 10})).await;
    let info = root.next_event("server.updated").await;
    assert_eq!(info["default_channel"], general);
    assert_eq!(info["max_clients"], 10);

    let mut raw = open(&server).await;
    assert_eq!(raw.challenge["password_required"], true);
    let err = raw.rejected(raw.hello(&new_key(), "x", None)).await;
    assert_eq!(err.code, vc_proto::ErrorCode::WrongPassword);
    Client::connect_as(&server, new_key(), "y", Some("hunter2")).await.expect("new password works");

    root.ok("server.update", json!({"password": ""})).await;
    Client::connect(&server, "z").await;
    let mut member = Client::connect(&server, "m").await;
    assert_eq!(member.fails("server.update", json!({"name": "Mine"})).await, "forbidden");
}

// ------------------------------------------------------------- mentions

#[tokio::test]
async fn mentions_are_kept_and_counted_as_unread() {
    let server = TestServer::start().await;
    let key = new_key();
    let bob = Client::connect_as(&server, key.clone(), "bob", None).await.unwrap();
    let bob_uid = bob.welcome.uid.clone();
    bob.close().await;
    let mut alice = Client::connect(&server, "alice").await;
    let lobby = alice.default_channel();
    let sent = alice
        .ok("chat.send", json!({"target": {"channel": lobby}, "text": "hi @bob", "mentions": [bob_uid, "nobody"]}))
        .await;
    assert_eq!(sent["mentions"], json!([bob_uid]), "unknown users are dropped");
    alice.ok("chat.send", json!({"target": {"channel": lobby}, "text": "plain"})).await;

    let bob = Client::connect_as(&server, key, "bob", None).await.unwrap();
    let unread = bob.welcome.unread.iter().find(|u| u.channel == lobby).unwrap();
    assert_eq!((unread.count, unread.mentions), (2, 1));
}

// --------------------------------------------------------- edit, delete

#[tokio::test]
async fn authors_edit_and_delete_their_messages_moderators_delete_any() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mut alice = Client::connect(&server, "alice").await;
    let mut bob = Client::connect(&server, "bob").await;
    let lobby = alice.default_channel();
    let sent = alice.ok("chat.send", json!({"target": {"channel": lobby}, "text": "helo"})).await;
    let id = sent["id"].clone();

    let edited = alice.ok("chat.edit", json!({"message": id, "text": "hello"})).await;
    assert_eq!(edited["text"], "hello");
    assert!(edited["edited_at"].as_i64().is_some());
    let seen = bob.next_event("chat.edited").await;
    assert_eq!(seen["id"], id);
    assert_eq!(seen["text"], "hello");
    let history = bob.ok("chat.history", json!({"channel": lobby})).await;
    assert_eq!(history["messages"][0]["text"], "hello");

    assert_eq!(bob.fails("chat.edit", json!({"message": id, "text": "mine"})).await, "forbidden");
    assert_eq!(bob.fails("chat.delete", json!({"message": id})).await, "forbidden");
    root.ok("chat.delete", json!({"message": id})).await;
    assert_eq!(bob.next_event("chat.deleted").await, json!({"channel": lobby, "message": id}));
    let history = bob.ok("chat.history", json!({"channel": lobby})).await;
    assert!(history["messages"].as_array().unwrap().is_empty());
    assert_eq!(alice.fails("chat.edit", json!({"message": id, "text": "x"})).await, "not_found");
}

// -------------------------------------------------------------- uploads

const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49,
    0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00,
    0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

async fn upload(server: &TestServer, client: &mut Client, name: &str, mime: &str, bytes: &[u8]) -> String {
    let reserved = client.ok("file.upload", json!({"name": name, "size": bytes.len(), "mime": mime})).await;
    let url = format!("{}{}", server.http(), reserved["upload_url"].as_str().unwrap());
    let status = reqwest::Client::new().put(&url).body(bytes.to_vec()).send().await.unwrap().status();
    assert_eq!(status, 204, "upload accepted");
    // The token works once.
    let again = reqwest::Client::new().put(&url).body(bytes.to_vec()).send().await.unwrap().status();
    assert_eq!(again, 403);
    reserved["file"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn uploads_attach_to_messages_and_are_served_safely() {
    let server = TestServer::start().await;
    let mut alice = Client::connect(&server, "alice").await;
    let mut bob = Client::connect(&server, "bob").await;
    let lobby = alice.default_channel();
    let image = upload(&server, &mut alice, "../dot.png", "image/png", PNG_1X1).await;
    let page = upload(&server, &mut alice, "page.html", "text/html", b"<script>alert(1)</script>").await;

    let sent =
        alice.ok("chat.send", json!({"target": {"channel": lobby}, "text": "", "attachments": [image, page]})).await;
    let files = sent["attachments"].as_array().unwrap();
    assert_eq!(files[0]["name"], "dot.png", "paths are stripped");
    assert_eq!(files[0]["mime"], "image/png");
    assert_eq!((files[0]["width"].as_u64(), files[0]["height"].as_u64()), (Some(1), Some(1)));
    assert_eq!(bob.next_event("chat.message").await["attachments"], sent["attachments"]);

    let get = |path: &str| reqwest::get(format!("{}{path}", server.http()));
    let png = get(files[0]["url"].as_str().unwrap()).await.unwrap();
    assert_eq!(png.headers()["content-type"], "image/png");
    assert_eq!(png.bytes().await.unwrap().as_ref(), PNG_1X1);
    // HTML is never served as HTML.
    let html = get(files[1]["url"].as_str().unwrap()).await.unwrap();
    assert_eq!(html.headers()["content-type"], "application/octet-stream");
    assert!(html.headers()["content-disposition"].to_str().unwrap().starts_with("attachment"));
    assert_eq!(html.headers()["x-content-type-options"], "nosniff");

    // An attachment is used once; deleting the message deletes its files.
    assert_eq!(
        alice.fails("chat.send", json!({"target": {"channel": lobby}, "text": "x", "attachments": [image]})).await,
        "not_found"
    );
    alice.ok("chat.delete", json!({"message": sent["id"]})).await;
    let after = get(files[0]["url"].as_str().unwrap()).await.unwrap();
    assert_eq!(after.status(), 404);
}

#[tokio::test]
async fn uploads_respect_the_size_limit() {
    let server = TestServer::start_with(|c| c.upload_limit = 1000).await;
    let mut alice = Client::connect(&server, "alice").await;
    assert_eq!(server_limit(&alice), 1000);
    assert_eq!(alice.fails("file.upload", json!({"name": "a", "size": 2000, "mime": "x"})).await, "too_large");
    // Sending more than reserved is refused too.
    let reserved = alice.ok("file.upload", json!({"name": "a", "size": 10, "mime": "x"})).await;
    let url = format!("{}{}", server.http(), reserved["upload_url"].as_str().unwrap());
    let status = reqwest::Client::new().put(&url).body(vec![0u8; 500]).send().await.unwrap().status();
    assert_eq!(status, 413);
}

fn server_limit(client: &Client) -> u64 {
    client.welcome.server.upload_limit
}
