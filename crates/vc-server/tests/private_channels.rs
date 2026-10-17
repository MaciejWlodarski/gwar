//! Private channels (docs/e2e-build.md, section 6, 6a): hidden from everyone
//! their access does not admit, with roles, users and admins.

mod common;

use common::*;
use serde_json::{Value, json};

fn ids(events: &[Value], key: &str) -> Vec<u64> {
    events.iter().filter_map(|e| e[key].as_u64()).collect()
}

fn sees(client: &Client, channel: u32) -> bool {
    client.welcome.channels.iter().any(|c| c.id == channel)
}

#[tokio::test]
async fn a_private_channel_is_hidden_from_everyone_it_does_not_admit() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mut alice = Client::connect(&server, "alice").await;
    let mut bob = Client::connect(&server, "bob").await;
    let carol_key = new_key();
    let private = channel_id(
        &root
            .create_channel(json!({"name": "secret", "private": true,
                "access": {"roles": [], "users": [alice.welcome.uid, uid_of(&carol_key)]}}))
            .await,
    );
    // Only the admitted (and admins) learn it exists.
    let created = alice.next_event("channel.created").await;
    assert_eq!(created["id"], private);
    assert_eq!(created["private"], true);
    assert!(created["access"]["users"].as_array().unwrap().contains(&json!(alice.welcome.uid)));
    assert!(ids(&bob.drain_events("channel.created").await, "id").is_empty());
    let late = Client::connect(&server, "late").await;
    assert!(!sees(&late, private));
    let carol = Client::connect_as(&server, carol_key, "carol", None).await.unwrap();
    assert!(sees(&carol, private), "a listed user who connects later sees it");

    // Every request naming it answers as for a channel that does not exist.
    for (op, d) in [
        ("channel.join", json!({"channel": private})),
        ("chat.history", json!({"channel": private})),
        ("chat.read", json!({"channel": private, "message": 1})),
        ("channel.create", json!({"name": "child", "parent": private})),
    ] {
        assert_eq!(bob.fails(op, d).await, "not_found", "{op}");
    }
    assert_eq!(bob.fails("chat.send", json!({"target": {"channel": private}, "text": "hi"})).await, "not_found");

    // A client inside is "on the server, not in voice" for everyone else.
    alice.ok("channel.join", json!({"channel": private})).await;
    let masked = bob.next_event_where("client.updated", |c| c["id"] == alice.id()).await;
    assert!(masked["channel"].is_null() && masked["voice_binding"].is_null() && masked["voice"] == false, "{masked}");
    let seen = root.next_event_where("client.updated", |c| c["id"] == alice.id()).await;
    assert_eq!(seen["channel"], private, "admins see every channel");
    let late = Client::connect(&server, "late2").await;
    let listed = late.welcome.clients.iter().find(|c| c.id == alice.id()).unwrap();
    assert_eq!(listed.channel, None);

    // Its chat reaches only those admitted.
    alice.ok("chat.send", json!({"target": {"channel": private}, "text": "inside"})).await;
    assert!(bob.drain_events("chat.message").await.is_empty());
}

#[tokio::test]
async fn roles_admit_live_and_losing_access_moves_out_and_hides() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mut alice = Client::connect(&server, "alice").await;
    let mut bob = Client::connect(&server, "bob").await;
    let role = root.ok("group.create", json!({"name": "Crew", "permissions": []})).await["group"]["id"].clone();
    let private = channel_id(
        &root.create_channel(json!({"name": "crew", "private": true, "access": {"roles": [role], "users": []}})).await,
    );
    root.ok("member.groups", json!({"uid": alice.welcome.uid, "groups": [2, role]})).await;
    assert_eq!(alice.next_event("channel.created").await["id"], private);
    alice.ok("channel.join", json!({"channel": private})).await;

    // Bob gains the role: the channel and its occupant appear.
    root.ok("member.groups", json!({"uid": bob.welcome.uid, "groups": [2, role]})).await;
    assert_eq!(bob.next_event("channel.created").await["id"], private);
    let inside = bob.next_event_where("client.updated", |c| c["id"] == alice.id() && c["channel"] == private).await;
    assert_eq!(inside["channel"], private);

    // Alice loses it while inside: out of the channel, and it is gone for her.
    root.ok("member.groups", json!({"uid": alice.welcome.uid, "groups": [2]})).await;
    assert_eq!(alice.next_event("channel.hidden").await["channel"], private);
    let moved = bob.next_event_where("client.updated", |c| c["id"] == alice.id()).await;
    assert!(moved["channel"].is_null());
    assert_eq!(alice.fails("channel.join", json!({"channel": private})).await, "not_found");

    // Editing access: listing a user admits them; the deleted role stops being listed.
    root.ok("channel.update", json!({"channel": private, "access": {"roles": [role], "users": [alice.welcome.uid]}}))
        .await;
    assert_eq!(alice.next_event("channel.created").await["id"], private);
    root.ok("group.delete", json!({"group": role})).await;
    assert_eq!(bob.next_event("channel.hidden").await["channel"], private);
    let updated =
        alice.next_event_where("channel.updated", |c| c["id"] == private && c["access"]["roles"] == json!([])).await;
    assert_eq!(updated["access"]["roles"], json!([]));

    // Deleting it tells only those who could see it.
    root.ok("channel.delete", json!({"channel": private})).await;
    assert_eq!(alice.next_event("channel.deleted").await["channel"], private);
    assert!(bob.drain_events("channel.deleted").await.is_empty());
}

#[tokio::test]
async fn private_channel_rules() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mut bob = Client::connect(&server, "bob").await;
    let private = channel_id(&root.create_channel(json!({"name": "p", "private": true})).await);
    let public = channel_id(&root.create_channel(json!({"name": "open"})).await);
    for d in [
        json!({"name": "pw", "private": true, "password": "secret12"}),
        json!({"name": "enc", "private": true, "encrypted": true}),
        json!({"name": "kid", "parent": private}),
        json!({"name": "acc", "access": {"roles": [], "users": []}}),
        json!({"name": "role", "private": true, "access": {"roles": [999], "users": []}}),
    ] {
        assert!(matches!(root.fails("channel.create", d.clone()).await.as_str(), "bad_request" | "not_found"), "{d}");
    }
    assert_eq!(root.fails("channel.update", json!({"channel": public, "parent": private})).await, "bad_request");
    assert_eq!(root.fails("channel.update", json!({"channel": private, "password": "secret12"})).await, "bad_request");
    assert_eq!(
        root.fails("channel.update", json!({"channel": public, "access": {"roles": [], "users": []}})).await,
        "bad_request"
    );
    assert_eq!(root.fails("server.update", json!({"default_channel": private})).await, "bad_request");
    // The creator is admitted by default; others cannot be moved in.
    let created = root.ok("channel.update", json!({"channel": private, "name": "p2"})).await;
    assert_eq!(created["access"]["users"], json!([root.welcome.uid]));
    assert_eq!(root.fails("client.move", json!({"client": bob.id(), "channel": private})).await, "bad_request");
    assert!(bob.drain_events("channel.updated").await.iter().all(|c| c["id"] != private));
}

async fn upload(server: &TestServer, client: &mut Client, bytes: &[u8]) -> String {
    let reserved = client
        .ok(
            "file.upload",
            json!({"name": "notes.txt", "size": bytes.len(), "sha256": hash(bytes), "mime": "text/plain"}),
        )
        .await;
    let url = format!("{}{}", server.http(), reserved["upload_url"].as_str().unwrap());
    let response = reqwest::Client::new().put(&url).body(bytes.to_vec()).send().await.unwrap();
    assert_eq!(response.status(), 200);
    // The test client signs messages over its attachments' metadata.
    let att: vc_proto::Attachment = response.json().await.unwrap();
    client.files.insert(att.id.clone(), att);
    reserved["file"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn private_attachments_need_an_admitted_session_even_in_link_mode() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mut alice = Client::connect(&server, "alice").await;
    let bob = Client::connect(&server, "bob").await;
    let private = channel_id(
        &root
            .create_channel(
                json!({"name": "p", "private": true, "access": {"roles": [], "users": [alice.welcome.uid]}}),
            )
            .await,
    );
    let file = upload(&server, &mut alice, b"secret notes").await;
    let sent = alice.ok("chat.send", json!({"target": {"channel": private}, "text": "", "attachments": [file]})).await;
    let url = format!("{}{}", server.http(), sent["attachments"][0]["url"].as_str().unwrap());
    let get = |bearer: Option<String>| {
        let request = reqwest::Client::new().get(&url);
        async move {
            match bearer {
                Some(b) => request.bearer_auth(b),
                None => request,
            }
            .send()
            .await
            .unwrap()
            .status()
        }
    };
    assert_eq!(get(None).await, 404, "a bare link is not enough");
    assert_eq!(get(Some(bob.welcome.file_access.clone())).await, 404, "not admitted");
    assert_eq!(get(Some(alice.welcome.file_access.clone())).await, 200);
    // Losing access ends it for a retained link too.
    root.ok("channel.update", json!({"channel": private, "access": {"roles": [], "users": []}})).await;
    assert_eq!(get(Some(alice.welcome.file_access.clone())).await, 404);
}

#[tokio::test]
async fn access_bearing_roles_are_given_out_only_by_those_admitted() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mut mallory = Client::connect(&server, "mallory").await;
    let bystander = Client::connect(&server, "bystander").await;
    // A role without permissions that admits to a private channel; mallory may manage groups and invite.
    let crew = root.ok("group.create", json!({"name": "Crew", "permissions": []})).await["group"]["id"].clone();
    let mods = root
        .ok("group.create", json!({"name": "Mods", "permissions": ["group_manage", "invite_create", "token_create"]}))
        .await["group"]["id"]
        .clone();
    root.ok("member.groups", json!({"uid": mallory.welcome.uid, "groups": [2, mods]})).await;
    let private = channel_id(
        &root.create_channel(json!({"name": "p", "private": true, "access": {"roles": [crew], "users": []}})).await,
    );
    for (op, d) in [
        ("member.groups", json!({"uid": mallory.welcome.uid, "groups": [2, mods, crew]})),
        ("member.groups", json!({"uid": bystander.welcome.uid, "groups": [2, crew]})),
        ("invite.create", json!({"group": crew})),
        ("token.create", json!({"group": crew})),
    ] {
        assert_eq!(mallory.fails(op, d.clone()).await, "forbidden", "{op} {d}");
    }
    // A token root made is refused at redemption once root no longer could give it out... (still admitted: works)
    let token = root.ok("token.create", json!({"group": crew})).await["token"].as_str().unwrap().to_owned();
    mallory.ok("token.redeem", json!({"token": token})).await;
    assert_eq!(mallory.next_event("channel.created").await["id"], private);
}

#[tokio::test]
async fn a_hidden_sibling_is_no_name_oracle() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let editors = root
        .ok("group.create", json!({"name": "Editors", "permissions": ["channel_create", "channel_edit"]}))
        .await["group"]["id"]
        .clone();
    let mut eve = Client::connect(&server, "eve").await;
    root.ok("member.groups", json!({"uid": eve.welcome.uid, "groups": [2, editors]})).await;
    root.create_channel(json!({"name": "Acquisition", "private": true})).await;
    let visible = channel_id(&eve.create_channel(json!({"name": "acquisition"})).await);
    let other = channel_id(&eve.create_channel(json!({"name": "Other"})).await);
    eve.ok("channel.update", json!({"channel": other, "name": "ACQUISITION 2"})).await;
    assert_eq!(
        eve.fails("channel.update", json!({"channel": other, "name": "acquisition"})).await,
        "conflict",
        "visible siblings still clash"
    );
    assert!(visible > 0);
}
