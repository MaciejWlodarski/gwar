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
    // An encrypted private channel (6c) is created only by someone who may sign its member list.
    assert_eq!(
        bob.fails("channel.create", json!({"name": "enc", "private": true, "encrypted": true})).await,
        "forbidden"
    );
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
async fn roles_and_the_channels_they_open_are_given_out_by_rank() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mut mallory = Client::connect(&server, "mallory").await;
    let bystander = Client::connect(&server, "bystander").await;
    // Mallory holds every permission but administrator through Boss; Crew starts above Boss.
    let all: Vec<_> = vc_proto::Permission::ALL
        .iter()
        .filter(|p| **p != vc_proto::Permission::Administrator)
        .map(|p| json!(p))
        .collect();
    let crew = root.ok("group.create", json!({"name": "Crew", "permissions": []})).await["group"]["id"].clone();
    let boss = root.ok("group.create", json!({"name": "Boss", "permissions": all})).await["group"]["id"].clone();
    root.ok("member.groups", json!({"uid": mallory.welcome.uid, "groups": [2, boss]})).await;
    let (me, them) = (mallory.welcome.uid.clone(), bystander.welcome.uid.clone());
    let giving = |role: &Value| {
        [
            ("member.groups", json!({"uid": me, "groups": [2, boss, role]})),
            ("member.groups", json!({"uid": them, "groups": [2, role]})),
            ("invite.create", json!({"group": role})),
            ("token.create", json!({"group": role})),
        ]
    };
    // Admin, Crew and her own role are not below her rank.
    for role in [json!(1), crew.clone(), boss.clone()] {
        // (Asking for a role she already holds changes nothing.)
        for (op, d) in giving(&role).into_iter().skip(usize::from(role == boss)) {
            assert_eq!(mallory.fails(op, d.clone()).await, "forbidden", "{role}: {op} {d}");
        }
    }
    // The outcome does not depend on hidden channels: the same after Crew starts admitting one.
    let private = channel_id(
        &root.create_channel(json!({"name": "p", "private": true, "access": {"roles": [crew], "users": []}})).await,
    );
    for (op, d) in giving(&crew) {
        assert_eq!(mallory.fails(op, d.clone()).await, "forbidden", "after: {op} {d}");
    }
    // Moved below Boss, Crew (and its channel) is hers to give: the order says whom the admin trusts.
    root.ok("group.reorder", json!({"groups": [1, boss, crew]})).await;
    mallory.ok("member.groups", json!({"uid": them, "groups": [2, crew]})).await;
    mallory.ok("member.groups", json!({"uid": me, "groups": [2, boss, crew]})).await;
    assert_eq!(mallory.next_event("channel.created").await["id"], private);
    let invite = mallory.ok("invite.create", json!({"group": crew})).await;
    assert!(invite.get("invite").is_some(), "{invite}");
}

#[tokio::test]
async fn a_private_channels_roles_change_only_below_ones_rank_or_for_ones_own() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mut mallory = Client::connect(&server, "mallory").await;
    let seniors = root.ok("group.create", json!({"name": "Seniors", "permissions": []})).await["group"]["id"].clone();
    let editors =
        root.ok("group.create", json!({"name": "Editors", "permissions": ["manage_channels"]})).await["group"]["id"]
            .clone();
    let crew = root.ok("group.create", json!({"name": "Crew", "permissions": []})).await["group"]["id"].clone();
    root.ok("member.groups", json!({"uid": mallory.welcome.uid, "groups": [2, editors]})).await;
    // She is listed by name, so she sees the channel.
    let me = mallory.welcome.uid.clone();
    let access = |roles: Value| json!({"roles": roles, "users": [me]});
    let private = channel_id(
        &root.create_channel(json!({"name": "p", "private": true, "access": access(json!([seniors]))})).await,
    );
    mallory.next_event("channel.created").await;
    let update = |roles: Value| json!({"channel": private, "access": access(roles)});
    // Not taking away a higher role, not adding one.
    assert_eq!(mallory.fails("channel.update", update(json!([]))).await, "forbidden");
    // Her own role and a lower one, keeping the higher one listed.
    mallory.ok("channel.update", update(json!([seniors, editors, crew]))).await;
}

#[tokio::test]
async fn a_hidden_sibling_is_no_name_oracle() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let editors = root
        .ok("group.create", json!({"name": "Editors", "permissions": ["manage_channels", "manage_channels"]}))
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
    // A new channel's position counts only public siblings, private or not.
    let hidden_position = root.create_channel(json!({"name": "Later", "private": true})).await["position"].clone();
    let mine = eve.create_channel(json!({"name": "Mine", "private": true})).await;
    assert_eq!(mine["position"], hidden_position);
}

#[tokio::test]
async fn a_hidden_channel_answers_exactly_as_a_missing_one() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let managers = root
        .ok("group.create", json!({"name": "Managers", "permissions": ["manage_server", "manage_channels"]}))
        .await["group"]["id"]
        .clone();
    let mut bob = Client::connect(&server, "bob").await;
    let mut eve = Client::connect(&server, "eve").await;
    root.ok("member.groups", json!({"uid": eve.welcome.uid, "groups": [2, managers]})).await;
    let hidden = channel_id(&root.create_channel(json!({"name": "p", "private": true})).await);
    let missing = hidden + 100;
    for client in [&mut bob, &mut eve] {
        for (op, d) in [
            ("chat.read", json!({"channel": "{}", "message": 1})),
            ("channel.delete", json!({"channel": "{}"})),
            ("channel.update", json!({"channel": "{}", "topic": "x"})),
            ("channel.join", json!({"channel": "{}"})),
            ("chat.history", json!({"channel": "{}"})),
            ("server.update", json!({"default_channel": "{}"})),
            ("chat.read", json!({"conversation": "channel:{}", "message": 1})),
            ("chat.sealed.history", json!({"conversation": "channel:{}"})),
            ("chat.sealed.remove", json!({"conversation": "channel:{}", "id": 1})),
            ("mls.group_history", json!({"conversation": "channel:{}"})),
        ] {
            let with = |id: u32| {
                let mut d = d.clone();
                for v in d.as_object_mut().unwrap().values_mut() {
                    if v == "{}" {
                        *v = json!(id);
                    } else if v == "channel:{}" {
                        *v = json!(format!("channel:{id}"));
                    }
                }
                d
            };
            let (a, b) = (client.fails(op, with(hidden)).await, client.fails(op, with(missing)).await);
            assert_eq!(a, b, "{op}: hidden and missing differ");
        }
    }
}

#[tokio::test]
async fn deleting_a_role_needs_a_higher_rank() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mut mallory = Client::connect(&server, "mallory").await;
    let crew = root.ok("group.create", json!({"name": "Crew", "permissions": []})).await["group"]["id"].clone();
    let boss =
        root.ok("group.create", json!({"name": "Boss", "permissions": ["manage_roles"]})).await["group"]["id"].clone();
    root.ok("member.groups", json!({"uid": mallory.welcome.uid, "groups": [2, boss]})).await;
    assert_eq!(mallory.fails("group.delete", json!({"group": crew})).await, "forbidden");
    assert_eq!(mallory.fails("group.delete", json!({"group": boss})).await, "forbidden");
    root.ok("group.reorder", json!({"groups": [1, boss, crew]})).await;
    mallory.ok("group.delete", json!({"group": crew})).await;
}

#[tokio::test]
async fn an_invite_redeemed_on_another_device_admits_the_devices_already_connected() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let crew = root.ok("group.create", json!({"name": "Crew", "permissions": []})).await["group"]["id"].clone();
    let private = channel_id(
        &root.create_channel(json!({"name": "crew", "private": true, "access": {"roles": [crew], "users": []}})).await,
    );
    let code = root.ok("invite.create", json!({"group": crew})).await["invite"]["code"].as_str().unwrap().to_owned();
    let account = new_key();
    let connect = |device: ed25519_dalek::SigningKey, invite: Option<String>| {
        let now = vc_server::core::now_ms();
        let cert = certificate(&account, &device, now, now + 24 * 3600 * 1000);
        Client::connect_with(&server, device, move |raw, key| {
            let mut hello = raw.hello(key, "alice", None);
            set_certificate(&mut hello, cert, key);
            if let Some(code) = invite {
                hello["join"]["invite"] = json!(code);
            }
            hello
        })
    };
    let mut first = connect(new_key(), None).await.unwrap();
    assert!(!sees(&first, private));
    let second = connect(new_key(), Some(code)).await.unwrap();
    assert!(sees(&second, private));
    assert_eq!(first.next_event("channel.created").await["id"], private);
}
