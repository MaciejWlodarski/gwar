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

#[tokio::test]
async fn moderators_cannot_kick_or_move_someone_stronger() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mut alice = Client::connect(&server, "alice").await;
    let mut bob = Client::connect(&server, "bob").await;
    let lobby = seed_channel(&root, "Lobby");
    let mods = group_id(
        &root.ok("group.create", json!({"name": "Mods", "permissions": ["client_kick", "client_move"]})).await,
    );
    with_groups(&mut root, &alice, json!([2, mods])).await;

    assert_eq!(alice.fails("client.kick", json!({"client": root.id()})).await, "forbidden");
    assert_eq!(alice.fails("client.move", json!({"client": root.id(), "channel": lobby})).await, "forbidden");
    // Equals and weaker are fine, and so is moving yourself.
    alice.ok("client.move", json!({"client": alice.id(), "channel": lobby})).await;
    alice.ok("client.move", json!({"client": bob.id(), "channel": lobby})).await;
    alice.ok("client.kick", json!({"client": bob.id()})).await;
    bob.expect_disconnect().await;
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
    let notice = refused.ban.expect("structured ban details");
    assert_eq!((notice.by.as_str(), notice.reason.as_deref()), ("root", Some("spam")));
    assert!(notice.until.is_some());

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
            hello["join"]["invite"] = json!(code);
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
async fn members_return_without_the_password_and_keep_invites_unspent() {
    let server = TestServer::start_with(|c| c.server_password = Some("secret".into())).await;
    let mut root = Client::connect_as(&server, new_key(), "root", Some("secret")).await.unwrap();
    let invite = root.ok("invite.create", json!({"max_uses": 2})).await;
    let code = invite["invite"]["code"].as_str().unwrap().to_owned();
    let hello = |code: String| {
        move |raw: &RawConn, key: &_| {
            let mut hello = raw.hello(key, "guest", None);
            hello["join"]["invite"] = json!(code);
            hello
        }
    };
    let key = new_key();
    Client::connect_with(&server, key.clone(), hello(code.clone())).await.unwrap().close().await;
    // Back again, with the link or without it: no password, no extra use.
    Client::connect_with(&server, key.clone(), hello(code.clone())).await.unwrap().close().await;
    Client::connect_as(&server, key, "guest", None).await.expect("members come back").close().await;
    let listed = root.ok("invite.list", json!({})).await;
    assert_eq!(listed["invites"][0]["uses"], 1);
    // Strangers still need the password.
    assert!(Client::connect_as(&server, new_key(), "stranger", None).await.is_err());
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
    assert_eq!(alice.fails("invite.delete", json!({"code": code})).await, "not_found");
}

#[tokio::test]
async fn invites_belong_to_their_creator_not_to_a_nickname() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mods = group_id(&root.ok("group.create", json!({"name": "Mods", "permissions": ["client_kick"]})).await);
    let invite = root.ok("invite.create", json!({"group": mods})).await;
    let code = invite["invite"]["code"].clone();

    // Anyone may take any nickname, including the admin's: it must not open the admin's invites.
    let mut mallory = Client::connect(&server, "mallory").await;
    let own = mallory.ok("invite.create", json!({})).await["invite"]["code"].clone();
    mallory.ok("member.nickname", json!({"uid": mallory.welcome.uid, "nickname": "root"})).await;
    let listed = mallory.ok("invite.list", json!({})).await;
    assert_eq!(listed["invites"].as_array().unwrap().iter().map(|i| &i["code"]).collect::<Vec<_>>(), vec![&own]);
    assert_eq!(listed["invites"][0]["created_by"], "mallory", "the shown name is the one at creation");
    assert_eq!(mallory.fails("invite.delete", json!({"code": code})).await, "forbidden");

    // Managers see and remove every invite.
    let all = root.ok("invite.list", json!({})).await;
    assert_eq!(all["invites"].as_array().unwrap().len(), 2);
    root.ok("invite.delete", json!({"code": own})).await;
    root.ok("invite.delete", json!({"code": code})).await;
}

#[tokio::test]
async fn invites_and_tokens_lapse_with_their_makers_rights() {
    let server = TestServer::start_with(|c| c.server_password = Some("secret".into())).await;
    let mut root = Client::connect_as(&server, new_key(), "root", Some("secret")).await.unwrap();
    root.redeem(&server.admin_token).await;
    let mods = group_id(
        &root
            .ok(
                "group.create",
                json!({"name": "Mods", "permissions": ["client_kick", "invite_create", "token_create"]}),
            )
            .await,
    );
    let mut alice = Client::connect_as(&server, new_key(), "alice", Some("secret")).await.unwrap();
    with_groups(&mut root, &alice, json!([2, mods])).await;
    let mut codes = Vec::new();
    for group in [json!(mods), json!(mods), Value::Null] {
        codes.push(
            alice.ok("invite.create", json!({"group": group})).await["invite"]["code"].as_str().unwrap().to_owned(),
        );
    }
    let plain = group_id(&root.ok("group.create", json!({"name": "Plain", "permissions": []})).await);
    // Roles are given out only by those who hold them.
    with_groups(&mut root, &alice, json!([2, mods, plain])).await;
    let mut token =
        async |group| alice.ok("token.create", json!({"group": group})).await["token"].as_str().unwrap().to_owned();
    let (first_token, second_token, plain_token) = (token(mods).await, token(mods).await, token(plain).await);
    let with_invite = |code: String| {
        move |raw: &RawConn, key: &_| {
            let mut hello = raw.hello(key, "guest", None);
            hello["join"]["invite"] = json!(code);
            hello
        }
    };
    let in_mods =
        |c: &Client| c.welcome.clients.iter().find(|x| x.id == c.id()).unwrap().groups.contains(&(mods as u32));

    // While Alice holds the rights, what she made works.
    let guest = Client::connect_with(&server, new_key(), with_invite(codes[0].clone())).await.expect("invite admits");
    assert!(in_mods(&guest));
    let mut bob = Client::connect_as(&server, new_key(), "bob", Some("secret")).await.unwrap();
    bob.redeem(&first_token).await;

    // Demoted, she keeps only a member's rights: her group grants lapse, a plain invite still admits.
    with_groups(&mut root, &alice, json!([2])).await;
    let refused = Client::connect_with(&server, new_key(), with_invite(codes[1].clone())).await.err().expect("lapsed");
    assert_eq!(refused.code, vc_proto::ErrorCode::WrongPassword);
    assert!(!in_mods(&Client::connect_with(&server, new_key(), with_invite(codes[2].clone())).await.expect("admits")));
    let mut carol = Client::connect_as(&server, new_key(), "carol", Some("secret")).await.unwrap();
    assert_eq!(carol.fails("token.redeem", json!({"token": second_token})).await, "forbidden");
    // Even for a group with nothing in it: she can no longer make tokens at all.
    assert_eq!(carol.fails("token.redeem", json!({"token": plain_token})).await, "forbidden");
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

    let raw = open(&server).await;
    assert_eq!(raw.challenge["password_required"], true);
    let err = Client::connect_as(&server, new_key(), "x", None).await.err().expect("join requires the password");
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
    let sent =
        alice.ok("chat.send", json!({"target": {"channel": lobby}, "text": "hi @bob", "mentions": [bob_uid]})).await;
    assert_eq!(sent["mentions"], json!([bob_uid]), "signed mentions are kept");
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
    let reserved =
        client.ok("file.upload", json!({"name": name, "size": bytes.len(), "sha256": hash(bytes), "mime": mime})).await;
    let url = format!("{}{}", server.http(), reserved["upload_url"].as_str().unwrap());
    let response = reqwest::Client::new().put(&url).body(bytes.to_vec()).send().await.unwrap();
    assert_eq!(response.status(), 200, "upload accepted");
    let att: vc_proto::Attachment = response.json().await.unwrap();
    client.files.insert(att.id.clone(), att);
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
    alice.ok("chat.edit", json!({"message": sent["id"], "text": vc_proto::DELETION_TEXT})).await;
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

#[tokio::test]
async fn stored_uploads_count_against_member_and_server_limits() {
    let server = TestServer::start_with(|c| {
        c.upload_quota = 100;
        c.storage_limit = 250;
    })
    .await;
    let mut alice = Client::connect(&server, "alice").await;
    let mut bob = Client::connect(&server, "bob").await;
    let mut carol = Client::connect(&server, "carol").await;
    let lobby = alice.default_channel();
    let first = upload(&server, &mut alice, "a", "x", &[1; 60]).await;
    // Alice's reserved (not yet sent) bytes count too.
    alice.ok("file.upload", json!({"name": "b", "size": 30, "mime": "x"})).await;
    assert_eq!(alice.fails("file.upload", json!({"name": "c", "size": 11, "mime": "x"})).await, "quota_exceeded");
    alice.ok("file.upload", json!({"name": "c", "size": 10, "mime": "x"})).await;
    // Alice's files do not use up Bob's quota...
    upload(&server, &mut bob, "d", "x", &[2; 70]).await;
    bob.ok("file.upload", json!({"name": "e", "size": 30, "mime": "x"})).await;
    // ...but the server total is shared: 200 of 250 bytes are taken.
    assert_eq!(carol.fails("file.upload", json!({"name": "f", "size": 51, "mime": "x"})).await, "quota_exceeded");
    carol.ok("file.upload", json!({"name": "f", "size": 50, "mime": "x"})).await;
    // Deleting a message frees its files (once their bytes are gone, just after).
    let sent = alice.ok("chat.send", json!({"target": {"channel": lobby}, "text": "", "attachments": [first]})).await;
    alice.ok("chat.edit", json!({"message": sent["id"], "text": vc_proto::DELETION_TEXT})).await;
    upload_soon(&mut alice, 60).await.expect("space freed after deleting the message");
}

/// Reserves `size` bytes, retrying for a second while space is being freed.
async fn upload_soon(client: &mut Client, size: usize) -> Option<Value> {
    for _ in 0..50 {
        if let Ok(reserved) = client.request("file.upload", json!({"name": "later", "size": size, "mime": "x"})).await {
            return Some(reserved);
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    None
}

#[tokio::test]
async fn a_file_being_downloaded_keeps_its_space_until_the_download_ends() {
    // Large enough not to fit in the socket buffers: the download stays open until read.
    const SIZE: usize = 32 << 20;
    let server = TestServer::start_with(|c| {
        c.upload_limit = SIZE as u64;
        c.storage_limit = SIZE as u64;
    })
    .await;
    let mut alice = Client::connect(&server, "alice").await;
    let lobby = alice.default_channel();
    let bytes = vec![7u8; SIZE];
    let reserved =
        alice.ok("file.upload", json!({"name": "big", "size": SIZE, "sha256": hash(&bytes), "mime": "x"})).await;
    let put = format!("{}{}", server.http(), reserved["upload_url"].as_str().unwrap());
    // (Not `upload`: a refused second PUT of this size ends in a reset connection.)
    let response = reqwest::Client::new().put(&put).body(bytes).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let file: vc_proto::Attachment = response.json().await.unwrap();
    alice.files.insert(file.id.clone(), file.clone());
    let file = file.id;
    let sent = alice.ok("chat.send", json!({"target": {"channel": lobby}, "text": "", "attachments": [file]})).await;
    let url = format!("{}{}", server.http(), sent["attachments"][0]["url"].as_str().unwrap());
    let download = reqwest::get(&url).await.unwrap();
    assert_eq!(download.status(), 200);
    alice.ok("chat.edit", json!({"message": sent["id"], "text": vc_proto::DELETION_TEXT})).await;
    assert_eq!(reqwest::get(&url).await.unwrap().status(), 404);
    // The bytes are still on disk for the open download, so they still count.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(alice.fails("file.upload", json!({"name": "b", "size": 1, "mime": "x"})).await, "quota_exceeded");
    assert_eq!(download.bytes().await.unwrap().len(), SIZE);
    upload_soon(&mut alice, SIZE).await.expect("space freed after the download ended");
}

#[tokio::test]
async fn failed_uploads_give_their_space_back() {
    let server = TestServer::start_with(|c| c.storage_limit = 100).await;
    let mut alice = Client::connect(&server, "alice").await;
    let mut bob = Client::connect(&server, "bob").await;
    let put = |reserved: serde_json::Value, body: Vec<u8>| {
        let url = format!("{}{}", server.http(), reserved["upload_url"].as_str().unwrap());
        async move { reqwest::Client::new().put(&url).body(body).send().await.unwrap().status() }
    };
    let reserved = alice.ok("file.upload", json!({"name": "a", "size": 100, "mime": "x"})).await;
    assert_eq!(bob.fails("file.upload", json!({"name": "b", "size": 1, "mime": "x"})).await, "quota_exceeded");
    // Too many bytes, then the wrong bytes: each failure releases the reservation (soon after).
    assert_eq!(put(reserved, vec![0; 500]).await, 413);
    let mut reserved = None;
    for _ in 0..50 {
        match bob.request("file.upload", json!({"name": "b", "size": 100, "mime": "x"})).await {
            Ok(ok) => {
                reserved = Some(ok);
                break;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
        }
    }
    assert_eq!(put(reserved.expect("space released after a refused upload"), vec![1; 100]).await, 400);
    for _ in 0..50 {
        if alice.request("file.upload", json!({"name": "c", "size": 100, "mime": "x"})).await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("space not released after a mismatched upload");
}

fn server_limit(client: &Client) -> u64 {
    client.welcome.server.upload_limit
}
