//! Per-channel overrides (docs/roles.md, "Overrides"): resolution, inheritance, who may set them.

mod common;

use common::*;
use serde_json::{Value, json};

fn group_id(reply: &Value) -> u64 {
    reply["group"]["id"].as_u64().expect("group id")
}

async fn role(root: &mut Client, name: &str, permissions: Value) -> u64 {
    group_id(&root.ok("group.create", json!({"name": name, "permissions": permissions})).await)
}

async fn give(root: &mut Client, member: &Client, groups: Value) {
    root.ok("member.groups", json!({"uid": member.welcome.uid, "groups": groups})).await;
}

fn for_role(channel: u32, role: u64, allow: Value, deny: Value) -> Value {
    json!({"channel": channel, "override": {"role": role, "allow": allow, "deny": deny}})
}

#[tokio::test]
async fn overrides_resolve_member_then_roles_then_the_person() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mut alice = Client::connect(&server, "alice").await;
    let mut bob = Client::connect(&server, "bob").await;
    let crew = role(&mut root, "Crew", json!([])).await;
    give(&mut root, &alice, json!([2, crew])).await;
    give(&mut root, &bob, json!([2, crew])).await;
    let stage = channel_id(&root.create_channel(json!({"name": "Stage"})).await);

    // Only Crew may join: Member denied, Crew allowed.
    root.ok("channel.override", for_role(stage, 2, json!([]), json!(["connect", "send_messages"]))).await;
    let mut carol = Client::connect(&server, "carol").await;
    assert_eq!(carol.fails("channel.join", json!({"channel": stage})).await, "forbidden");
    let chat = |text: &str| json!({"target": {"channel": stage}, "text": text});
    assert_eq!(carol.fails("chat.send", chat("hi")).await, "forbidden");
    root.ok("channel.override", for_role(stage, crew, json!(["connect"]), json!([]))).await;
    alice.ok("channel.join", json!({"channel": stage})).await;
    // A member's own override comes last: Bob is kept out despite Crew.
    let bob_uid = bob.welcome.uid.clone();
    root.ok("channel.override", json!({"channel": stage, "override": {"user": bob_uid, "deny": ["connect"]}})).await;
    assert_eq!(bob.fails("channel.join", json!({"channel": stage})).await, "forbidden");
    // Administrators skip overrides.
    root.ok("channel.join", json!({"channel": stage})).await;
    // Without connect there is no speak either; send_messages stays denied for Crew.
    assert_eq!(alice.fails("chat.send", chat("hi")).await, "forbidden");
    root.ok("channel.override", for_role(stage, crew, json!(["connect", "send_messages"]), json!([]))).await;
    alice.ok("chat.send", chat("now")).await;
}

#[tokio::test]
async fn losing_connect_takes_people_out_of_voice() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mut alice = Client::connect(&server, "alice").await;
    let room = channel_id(&root.create_channel(json!({"name": "Room"})).await);
    alice.ok("channel.join", json!({"channel": room})).await;
    root.ok("channel.override", for_role(room, 2, json!([]), json!(["connect"]))).await;
    let me = alice.id();
    alice.next_event_where("client.updated", |c| c["id"] == me && c["channel"].is_null()).await;
}

#[tokio::test]
async fn losing_the_role_that_let_one_join_takes_one_out_of_voice() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mut alice = Client::connect(&server, "alice").await;
    let crew = role(&mut root, "Crew", json!([])).await;
    give(&mut root, &alice, json!([2, crew])).await;
    let room = channel_id(&root.create_channel(json!({"name": "Room"})).await);
    root.ok("channel.override", for_role(room, 2, json!([]), json!(["connect"]))).await;
    root.ok("channel.override", for_role(room, crew, json!(["connect"]), json!([]))).await;
    alice.ok("channel.join", json!({"channel": room})).await;
    let me = alice.id();
    // Deleting the role drops its override too.
    root.ok("group.delete", json!({"group": crew})).await;
    alice.next_event_where("client.updated", |c| c["id"] == me && c["channel"].is_null()).await;
    let view = alice.next_event_where("channel.updated", |c| c["id"] == room).await;
    assert_eq!(view["overrides"].as_array().map_or(0, Vec::len), 1, "only Member's remains");
}

#[tokio::test]
async fn subchannels_follow_their_parent_until_they_get_their_own() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mut alice = Client::connect(&server, "alice").await;
    let clan = channel_id(&root.create_channel(json!({"name": "Clan"})).await);
    let room = channel_id(&root.create_channel(json!({"name": "Room", "parent": clan})).await);
    let created = root.ok("channel.update", json!({"channel": room})).await;
    assert_eq!(created["synced"], true, "a new subchannel follows its parent");
    root.ok("channel.override", for_role(clan, 2, json!([]), json!(["connect"]))).await;
    assert_eq!(alice.fails("channel.join", json!({"channel": room})).await, "forbidden");
    // Its own override starts from the parent's.
    let own = root.ok("channel.override", for_role(room, 2, json!([]), json!(["connect", "send_messages"]))).await;
    assert_eq!(own["synced"], false);
    assert_eq!(own["overrides"].as_array().unwrap().len(), 1);
    // Back in sync, the parent's rule applies again; breaking it keeps a copy.
    root.ok("channel.update", json!({"channel": room, "synced": true})).await;
    let copy = root.ok("channel.update", json!({"channel": room, "synced": false})).await;
    assert_eq!(copy["overrides"][0]["deny"], json!(["connect"]));
    root.ok("channel.override", for_role(clan, 2, json!([]), json!([]))).await;
    assert_eq!(alice.fails("channel.join", json!({"channel": room})).await, "forbidden", "its own copy stays");
    assert_eq!(root.fails("channel.update", json!({"channel": clan, "synced": true})).await, "bad_request");
}

#[tokio::test]
async fn who_may_set_an_override() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mut alice = Client::connect(&server, "alice").await;
    let bob = Client::connect(&server, "bob").await;
    let mods = role(&mut root, "Mods", json!(["manage_channels", "move_members"])).await;
    let crew = role(&mut root, "Crew", json!([])).await;
    give(&mut root, &alice, json!([2, mods])).await;
    let room = channel_id(&root.create_channel(json!({"name": "Room"})).await);

    // Only channel permissions, never both ways at once.
    assert_eq!(
        root.fails("channel.override", for_role(room, crew, json!(["ban_members"]), json!([]))).await,
        "bad_request"
    );
    assert_eq!(
        root.fails("channel.override", for_role(room, crew, json!(["connect"]), json!(["connect"]))).await,
        "bad_request"
    );
    assert_eq!(
        root.fails("channel.override", json!({"channel": room, "override": {"allow": ["connect"]}})).await,
        "bad_request"
    );
    // Roles below her rank and members ranked below her; not her own role, not Admin, not the owner.
    alice.ok("channel.override", for_role(room, crew, json!(["move_members"]), json!([]))).await;
    assert_eq!(alice.fails("channel.override", for_role(room, mods, json!([]), json!(["connect"]))).await, "forbidden");
    assert_eq!(alice.fails("channel.override", for_role(room, 1, json!([]), json!(["connect"]))).await, "forbidden");
    let root_uid = root.welcome.uid.clone();
    assert_eq!(
        alice
            .fails("channel.override", json!({"channel": room, "override": {"user": root_uid, "deny": ["connect"]}}))
            .await,
        "forbidden"
    );
    let bob_uid = bob.welcome.uid.clone();
    alice
        .ok("channel.override", json!({"channel": room, "override": {"user": bob_uid, "deny": ["send_messages"]}}))
        .await;
    // Not a permission she lacks there, either way.
    assert_eq!(
        alice
            .fails("channel.override", for_role(room, crew, json!(["move_members", "manage_messages"]), json!([])))
            .await,
        "forbidden"
    );
    // Without manage_channels in the channel, nothing.
    root.ok("channel.override", for_role(room, mods, json!([]), json!(["manage_channels"]))).await;
    assert_eq!(alice.fails("channel.override", for_role(room, crew, json!([]), json!([]))).await, "forbidden");
    assert_eq!(alice.fails("channel.update", json!({"channel": room, "name": "Mine"})).await, "forbidden");
    assert_eq!(alice.fails("channel.delete", json!({"channel": room})).await, "forbidden");
    // An override can also give manage_channels in one channel only.
    let other = channel_id(&root.create_channel(json!({"name": "Other"})).await);
    let mut carol = Client::connect(&server, "carol").await;
    give(&mut root, &carol, json!([2, crew])).await;
    root.ok("channel.override", for_role(other, crew, json!(["manage_channels"]), json!([]))).await;
    carol.ok("channel.update", json!({"channel": other, "topic": "ours"})).await;
    assert_eq!(carol.fails("channel.update", json!({"channel": room, "topic": "no"})).await, "forbidden");
}
