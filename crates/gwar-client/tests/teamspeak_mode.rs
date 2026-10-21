//! Our client in TeamSpeak mode on the official TeamSpeak server that our
//! server bridges: TS users and our users see, hear and message each other.

mod support;

use std::{
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use gwar_client::voice::{
    self,
    io::{AudioIo, AudioPorts, CaptureSample, OutputTiming},
};
use rtrb::RingBuffer;

use gwar_client::{
    ClientError, ConnectOptions, Identity, connect,
    teamspeak::{self},
};
use gwar_proto::{BadgeKind, Channel, ChannelId, ChatTarget, ClientSoftware, ErrorCode, Event, Platform, Request};
use gwar_server::teamspeak::query::{Cmd, Query};
use tokio::time::timeout;

async fn until(events: &mut tokio::sync::mpsc::Receiver<Event>, pred: impl Fn(&Event) -> bool) -> Event {
    let mut seen = Vec::new();
    // A puppet whose first connection meets the TeamSpeak server just started by the bridge tries
    // again after 10 s (bridge.rs, `retry_at`), so 15 s was too tight for its arrival.
    let found = timeout(Duration::from_secs(30), async {
        loop {
            let event = events.recv().await.expect("events ended");
            if pred(&event) {
                return event;
            }
            seen.push(event);
        }
    })
    .await;
    found.unwrap_or_else(|_| panic!("timed out waiting for an event; saw instead: {seen:#?}"))
}

fn named(channels: &[Channel], name: &str) -> ChannelId {
    channels.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no channel {name}: {channels:?}")).id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teamspeak_mode_maps_state_chat_and_moves() {
    let Some(bridged) = support::start().await else { return };

    let mut alice = connect(ConnectOptions {
        url: format!("ws://{}/ws", bridged.server.http),
        nickname: "alice".into(),
        invite: None,
        certificate: None,
        expected_server_key: None,
        trust_on_first_use: true,
        insecure_lan: false,
        server_password: None,
        identity: Identity::generate(),
        software: ClientSoftware { name: "t".into(), version: "0".into(), platform: Platform::Web },
    })
    .await
    .unwrap();
    // Our users are on the server without voice until they join a channel.
    alice.connection.join(alice.welcome.server.default_channel, None).await.unwrap();

    let mut ts = support::ts_client(bridged.ts_voice, "bob").await;
    let bob = ts.welcome.session;
    // Our channel tree, mirrored onto TeamSpeak (it uses its own ids).
    let ts_names: Vec<_> = ts.welcome.channels.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(ts.welcome.channels.len(), 3, "{ts_names:?}");
    for name in ["Lobby", "General", "AFK"] {
        assert!(ts_names.contains(&name), "{name} missing on TeamSpeak: {ts_names:?}");
    }
    let lobby_ts = ts.welcome.clients.iter().find(|c| c.id == bob).unwrap().channel.unwrap();
    assert_eq!(lobby_ts, named(&ts.welcome.channels, "Lobby"));
    // Our user is there through her puppet; bob shows up on our side.
    if !ts.welcome.clients.iter().any(|c| c.nickname == "alice") {
        until(&mut ts.events, |e| matches!(e, Event::ClientJoined(c) if c.nickname == "alice")).await;
    }
    let joined = until(&mut alice.events, |e| matches!(e, Event::ClientJoined(c) if c.nickname == "bob")).await;
    let Event::ClientJoined(bob_here) = joined else { unreachable!() };
    assert_eq!(bob_here.platform, Platform::Ts3);
    let lobby = named(&alice.welcome.channels, "Lobby");
    assert_eq!(bob_here.channel, Some(lobby));

    // Chat both ways.
    ts.handle
        .request(Request::ChatSend {
            signature: gwar_proto::MessageSignature::default(),
            target: ChatTarget::Channel(lobby_ts),
            text: "from TeamSpeak mode".into(),
            mentions: Vec::new(),
            attachments: Vec::new(),
        })
        .await
        .unwrap();
    until(
        &mut alice.events,
        |e| matches!(e, Event::ChatMessage(m) if m.text == "from TeamSpeak mode" && m.author == bob_here.id),
    )
    .await;
    alice.connection.chat(ChatTarget::Channel(lobby), "from the web").await.unwrap();
    until(
        &mut ts.events,
        |e| matches!(e, Event::ChatMessage(m) if m.text == "from the web" && m.author_name == "alice"),
    )
    .await;

    // Moves on either side are visible on the other.
    let talks_ts = named(&ts.welcome.channels, "General");
    ts.handle.request(Request::ChannelJoin { channel: talks_ts, password: None }).await.unwrap();
    let talks = named(&alice.welcome.channels, "General");
    until(
        &mut alice.events,
        |e| matches!(e, Event::ClientUpdated(c) if c.id == bob_here.id && c.channel == Some(talks)),
    )
    .await;
    alice.connection.join(named(&alice.welcome.channels, "AFK"), None).await.unwrap();
    let afk_ts = named(&ts.welcome.channels, "AFK");
    until(
        &mut ts.events,
        |e| matches!(e, Event::ClientUpdated(c) if c.nickname == "alice" && c.channel == Some(afk_ts)),
    )
    .await;

    // Mute is mapped to TS input_muted.
    ts.handle
        .request(Request::ClientUpdate(gwar_proto::ClientUpdate { muted: Some(true), ..Default::default() }))
        .await
        .unwrap();
    until(&mut alice.events, |e| matches!(e, Event::ClientUpdated(c) if c.id == bob_here.id && c.muted)).await;
}

/// Our users take a TeamSpeak slot only while they share a channel with TeamSpeak users.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn puppets_only_join_channels_with_teamspeak_users() {
    let Some(bridged) = support::start().await else { return };
    let alice = connect(ConnectOptions {
        url: format!("ws://{}/ws", bridged.server.http),
        nickname: "alice".into(),
        invite: None,
        certificate: None,
        expected_server_key: None,
        trust_on_first_use: true,
        insecure_lan: false,
        server_password: None,
        identity: Identity::generate(),
        software: ClientSoftware { name: "t".into(), version: "0".into(), platform: Platform::Web },
    })
    .await
    .unwrap();
    alice.connection.join(named(&alice.welcome.channels, "AFK"), None).await.unwrap();

    // bob lands in the default channel: alice is alone in AFK, so no puppet.
    let mut ts = support::ts_client(bridged.ts_voice, "bob").await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let mut seen_alice = ts.welcome.clients.iter().any(|c| c.nickname == "alice");
    while let Ok(event) = ts.events.try_recv() {
        seen_alice |= matches!(&event, Event::ClientJoined(c) if c.nickname == "alice");
    }
    assert!(!seen_alice, "alice got a puppet without sharing a channel with TeamSpeak users");

    // Once bob joins her channel, her puppet appears there.
    let afk_ts = named(&ts.welcome.channels, "AFK");
    ts.handle.request(Request::ChannelJoin { channel: afk_ts, password: None }).await.unwrap();
    until(
        &mut ts.events,
        |e| matches!(e, Event::ClientJoined(c) if c.nickname == "alice" && c.channel == Some(afk_ts)),
    )
    .await;
}

/// Our admin bans a TeamSpeak user, and attachments reach TeamSpeak as links.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teamspeak_users_get_attachment_links_and_can_be_banned() {
    let Some(bridged) = support::start().await else { return };
    let token = bridged.server.admin_token.clone().expect("fresh server");
    let mut alice = connect(ConnectOptions {
        url: format!("ws://{}/ws", bridged.server.http),
        nickname: "alice".into(),
        invite: None,
        certificate: None,
        expected_server_key: None,
        trust_on_first_use: true,
        insecure_lan: false,
        server_password: None,
        identity: Identity::generate(),
        software: ClientSoftware { name: "t".into(), version: "0".into(), platform: Platform::Web },
    })
    .await
    .unwrap();
    alice.connection.request(Request::TokenRedeem { token }).await.unwrap();
    let lobby = alice.welcome.server.default_channel;
    alice.connection.join(lobby, None).await.unwrap();

    let identity = tsclientlib::Identity::create();
    let mut ts = support::ts_client_as(bridged.ts_voice, "bob", identity.clone()).await;
    let joined = until(&mut alice.events, |e| matches!(e, Event::ClientJoined(c) if c.nickname == "bob")).await;
    let Event::ClientJoined(bob) = joined else { unreachable!() };
    // Alice's puppet is in the lobby once bob is there.
    if !ts.welcome.clients.iter().any(|c| c.nickname == "alice") {
        until(&mut ts.events, |e| matches!(e, Event::ClientJoined(c) if c.nickname == "alice")).await;
    }

    // An attachment arrives on TeamSpeak as a link.
    let reserved = alice
        .connection
        .request(Request::FileUpload {
            name: "notes.txt".into(),
            size: 5,
            mime: "text/plain".into(),
            sha256: "LPJNul-wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ".into(),
            sealed: false,
        })
        .await
        .unwrap();
    let url = format!("http://{}{}", bridged.server.http, reserved["upload_url"].as_str().unwrap());
    let put = reqwest::Client::new().put(url).body("hello").send().await.unwrap();
    assert_eq!(put.status(), 200);
    let attachment: gwar_proto::Attachment = put.json().await.unwrap();
    let file = reserved["file"].as_str().unwrap().to_owned();
    alice.connection.send_chat(ChatTarget::Channel(lobby), "see this".into(), Vec::new(), &[attachment]).await.unwrap();
    let link = format!("{}/files/{file}/notes.txt", support::PUBLIC_URL);
    until(
        &mut ts.events,
        |e| matches!(e, Event::ChatMessage(m) if m.text.contains("see this") && m.text.contains(&link)),
    )
    .await;

    // A ban takes the TeamSpeak user off the TeamSpeak server, and keeps them off.
    alice
        .connection
        .request(Request::BanCreate(gwar_proto::BanCreate {
            client: Some(bob.id),
            reason: Some("test".into()),
            ..Default::default()
        }))
        .await
        .unwrap();
    until(&mut ts.events, |e| matches!(e, Event::Disconnected { .. })).await;
    let again = teamspeak::connect(teamspeak::TsOptions {
        address: bridged.ts_voice.to_string(),
        nickname: "bob".into(),
        server_password: None,
        identity,
        channel: None,
        channel_password: None,
    })
    .await;
    assert!(again.is_err(), "banned TeamSpeak users can't come back");
}

/// Password channels behave as on TeamSpeak: the password is asked for only when the server
/// refuses, Server Admins enter without one, and chat is answered like on a Gwar server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teamspeak_mode_password_channels_admins_and_chat_replies() {
    let Some(bridged) = support::start().await else { return };
    let token = bridged.server.admin_token.clone().expect("fresh server");
    let alice = connect(ConnectOptions {
        url: format!("ws://{}/ws", bridged.server.http),
        nickname: "alice".into(),
        invite: None,
        certificate: None,
        expected_server_key: None,
        trust_on_first_use: true,
        insecure_lan: false,
        server_password: None,
        identity: Identity::generate(),
        software: ClientSoftware { name: "t".into(), version: "0".into(), platform: Platform::Web },
    })
    .await
    .unwrap();
    alice.connection.request(Request::TokenRedeem { token }).await.unwrap();
    // The bridge gives the TeamSpeak channel the same password.
    alice
        .connection
        .request(Request::ChannelCreate(gwar_proto::ChannelCreate {
            name: "Secret".into(),
            password: Some("hunter2".into()),
            ..Default::default()
        }))
        .await
        .unwrap();

    let mut ts = support::ts_client(bridged.ts_voice, "bob").await;
    let secret = match ts.welcome.channels.iter().find(|c| c.name == "Secret") {
        Some(channel) => channel.clone(),
        None => match until(&mut ts.events, |e| matches!(e, Event::ChannelCreated(c) if c.name == "Secret")).await {
            Event::ChannelCreated(channel) => channel,
            _ => unreachable!(),
        },
    };
    assert!(secret.has_password, "{secret:?}");
    let join = |password: Option<&str>| Request::ChannelJoin { channel: secret.id, password: password.map(Into::into) };
    for password in [None, Some("wrong")] {
        let refused = ts.handle.request(join(password)).await.unwrap_err();
        assert!(matches!(&refused, ClientError::Server(e) if e.code == ErrorCode::WrongPassword), "{refused:?}");
    }
    ts.handle.request(join(Some("hunter2"))).await.unwrap();
    // Joining the channel I am already in is no error (TeamSpeak checks the password first).
    ts.handle.request(join(Some("hunter2"))).await.unwrap();

    // `chat.send` answers with the message, which arrives exactly once.
    let send = |text: &str| Request::ChatSend {
        signature: gwar_proto::MessageSignature::default(),
        target: ChatTarget::Channel(secret.id),
        text: text.into(),
        mentions: Vec::new(),
        attachments: Vec::new(),
    };
    let sent: gwar_proto::ChatMessage =
        serde_json::from_value(ts.handle.request(send("only once")).await.unwrap()).unwrap();
    assert_eq!((sent.author, sent.text.as_str()), (ts.welcome.session, "only once"));
    ts.handle.request(send("marker")).await.unwrap();
    let mut copies = Vec::new();
    timeout(Duration::from_secs(15), async {
        loop {
            match ts.events.recv().await.expect("events ended") {
                Event::ChatMessage(m) if m.text == "marker" => break,
                Event::ChatMessage(m) if m.text == "only once" => copies.push(m),
                _ => {}
            }
        }
    })
    .await
    .expect("my own messages arrive");
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert_eq!((copies[0].id, &copies[0].author_uid), (sent.id, &sent.author_uid));

    // A Server Admin (b_channel_join_ignore_password) needs no password.
    let carol = support::ts_client(bridged.ts_voice, "carol").await;
    let (query, _notifications) =
        Query::connect(bridged.query, "serveradmin", &bridged.query_password(), 1).await.unwrap();
    let clients = query.call(Cmd::new("clientlist")).await.unwrap();
    let cldbid = clients
        .iter()
        .find(|c| c.get("client_nickname").is_some_and(|n| n == "carol"))
        .and_then(|c| c.get("client_database_id"))
        .expect("carol on TeamSpeak")
        .to_owned();
    query.call(Cmd::new("servergroupaddclient").arg("sgid", 6).arg("cldbid", &cldbid)).await.unwrap();
    carol.handle.request(join(None)).await.unwrap();

    // One `channel.update` that renames and moves does both (TeamSpeak takes two commands).
    let create =
        |name: &str| Request::ChannelCreate(gwar_proto::ChannelCreate { name: name.into(), ..Default::default() });
    let parent: Channel = serde_json::from_value(carol.handle.request(create("Parent")).await.unwrap()).unwrap();
    let child: Channel = serde_json::from_value(carol.handle.request(create("Child")).await.unwrap()).unwrap();
    let update = Request::ChannelUpdate(gwar_proto::ChannelUpdate {
        channel: child.id,
        name: Some("Moved child".into()),
        topic: Some("kept".into()),
        parent: Some(parent.id),
        ..Default::default()
    });
    let updated: Channel = serde_json::from_value(carol.handle.request(update).await.unwrap()).unwrap();
    assert_eq!(
        (updated.name.as_str(), updated.topic.as_str(), updated.parent),
        ("Moved child", "kept", Some(parent.id)),
        "{updated:?}"
    );
}

/// TeamSpeak's server and channel groups show as badges: groups set elsewhere reach the client
/// as badges and as the client's groups, and the defaults everyone has get none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teamspeak_mode_shows_group_badges() {
    let Some(bridged) = support::start().await else { return };
    let mut bob = support::ts_client(bridged.ts_voice, "bob").await;
    let (query, _notifications) =
        Query::connect(bridged.query, "serveradmin", &bridged.query_password(), 1).await.unwrap();
    let clients = query.call(Cmd::new("clientlist")).await.unwrap();
    let cldbid = clients
        .iter()
        .find(|c| c.get("client_nickname").is_some_and(|n| n == "bob"))
        .and_then(|c| c.get("client_database_id"))
        .expect("bob on TeamSpeak")
        .to_owned();
    let (me, lobby) = (bob.welcome.session, bob.welcome.server.default_channel);
    // Server Admin (6) and, in bob's channel, Channel Admin (5).
    query.call(Cmd::new("servergroupaddclient").arg("sgid", 6).arg("cldbid", &cldbid)).await.unwrap();
    let channel_admin = Cmd::new("setclientchannelgroup").arg("cgid", 5).arg("cid", lobby).arg("cldbid", &cldbid);
    query.call(channel_admin).await.unwrap();

    let badges = std::cell::RefCell::new(bob.welcome.server.badges.clone());
    let client = std::cell::RefCell::new(bob.welcome.clients.iter().find(|c| c.id == me).cloned().expect("bob"));
    let shown = || {
        let (badges, client) = (badges.borrow(), client.borrow());
        let has = |kind, id| badges.iter().any(|b| b.kind == kind && b.id == id);
        has(BadgeKind::Server, 6)
            && has(BadgeKind::Channel, 5)
            && client.groups.contains(&6)
            && client.channel_group == Some(5)
    };
    if !shown() {
        until(&mut bob.events, |e| {
            match e {
                Event::ServerUpdated(server) => *badges.borrow_mut() = server.badges.clone(),
                Event::ClientUpdated(c) if c.id == me => *client.borrow_mut() = c.clone(),
                _ => {}
            }
            shown()
        })
        .await;
    }
    let badges = badges.into_inner();
    let named = |kind, id| badges.iter().find(|b| b.kind == kind && b.id == id).map(|b| b.name.as_str());
    assert_eq!(
        (named(BadgeKind::Server, 6), named(BadgeKind::Channel, 5)),
        (Some("Server Admin"), Some("Channel Admin"))
    );
    let info = query.call(Cmd::new("serverinfo")).await.unwrap();
    let default = |key: &str| info[0].get(key).and_then(|v| v.parse::<u32>().ok()).expect(key);
    let (server_default, channel_default) =
        (default("virtualserver_default_server_group"), default("virtualserver_default_channel_group"));
    assert!(named(BadgeKind::Server, server_default).is_none(), "{badges:?}");
    assert!(named(BadgeKind::Channel, channel_default).is_none(), "{badges:?}");

    // A new group's hostile name arrives as plain text, a rename replaces it, and a deleted
    // group's badge goes although the book keeps the group.
    let added = query.call(Cmd::new("servergroupadd").arg("name", "Q\u{202e}A\tZ")).await.unwrap();
    let sgid: u32 = added[0].get("sgid").and_then(|v| v.parse().ok()).expect("sgid");
    let badge = |server: &gwar_proto::ServerInfo| {
        server.badges.iter().find(|b| b.kind == BadgeKind::Server && b.id == sgid).map(|b| b.name.clone())
    };
    let named_as =
        |name: &'static str| move |e: &Event| matches!(e, Event::ServerUpdated(s) if badge(s).as_deref() == Some(name));
    until(&mut bob.events, named_as("QA Z")).await;
    query.call(Cmd::new("servergrouprename").arg("sgid", sgid).arg("name", "Renamed")).await.unwrap();
    until(&mut bob.events, named_as("Renamed")).await;
    query.call(Cmd::new("servergroupdel").arg("sgid", sgid).arg("force", 1)).await.unwrap();
    until(&mut bob.events, |e| matches!(e, Event::ServerUpdated(s) if badge(s).is_none())).await;
}

/// TeamSpeak's own actions through our client: channel order, descriptions and family
/// limits, pokes, kicks from a channel, bans by identity, and landing in a saved channel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teamspeak_mode_pokes_kicks_bans_and_channel_details() {
    let Some(bridged) = support::start().await else { return };
    let mut admin = support::ts_client(bridged.ts_voice, "admin").await;
    let (query, _notifications) =
        Query::connect(bridged.query, "serveradmin", &bridged.query_password(), 1).await.unwrap();
    let database_id = async |nickname: &str| {
        let clients = query.call(Cmd::new("clientlist")).await.unwrap();
        clients
            .iter()
            .find(|c| c.get("client_nickname").is_some_and(|n| n == nickname))
            .and_then(|c| c.get("client_database_id"))
            .unwrap_or_else(|| panic!("{nickname} on TeamSpeak"))
            .to_owned()
    };
    // A Server Admin.
    let cldbid = database_id("admin").await;
    query.call(Cmd::new("servergroupaddclient").arg("sgid", 6).arg("cldbid", &cldbid)).await.unwrap();
    let request = async |ts: &teamspeak::TsConnected, request: Request| ts.handle.request(request).await.unwrap();

    // Order: Beta goes first among the top-level channels.
    let create = |name: &str, password: Option<&str>| {
        Request::ChannelCreate(gwar_proto::ChannelCreate {
            name: name.into(),
            password: password.map(Into::into),
            ..Default::default()
        })
    };
    let alpha: Channel = serde_json::from_value(request(&admin, create("Alpha", None)).await).unwrap();
    let beta: Channel = serde_json::from_value(request(&admin, create("Beta", None)).await).unwrap();
    let first =
        Request::ChannelUpdate(gwar_proto::ChannelUpdate { channel: beta.id, position: Some(0), ..Default::default() });
    let beta: Channel = serde_json::from_value(request(&admin, first).await).unwrap();
    assert_eq!(beta.position, 0, "{beta:?}");

    // A description set elsewhere, read as plain text; a family limit shows on the channel.
    let edit = Cmd::new("channeledit")
        .arg("cid", alpha.id)
        .arg("channel_description", "[b]Welcome[/b] to [url=https://x.test]Alpha[/url]")
        .arg("channel_maxfamilyclients", 3)
        .arg("channel_flag_maxfamilyclients_unlimited", 0)
        .arg("channel_flag_maxfamilyclients_inherited", 0);
    query.call(edit).await.unwrap();
    until(
        &mut admin.events,
        |e| matches!(e, Event::ChannelUpdated(c) if c.id == alpha.id && c.max_family_clients == Some(3)),
    )
    .await;
    let described = request(&admin, Request::ChannelDescription { channel: alpha.id }).await;
    assert_eq!(described, serde_json::json!({ "description": "Welcome to Alpha" }));

    // A poke reaches bob as a poke, not as a message, without the formatting that would
    // reorder or break its text. It names bob as the admin's client sees him.
    let mut bob = support::ts_client(bridged.ts_voice, "bob").await;
    let (bob_id, bob_uid) = (bob.welcome.session, bob.welcome.uid.clone());
    until(&mut admin.events, |e| matches!(e, Event::ClientJoined(c) if c.id == bob_id)).await;
    let poke = Request::ClientPoke { client: bob_id, uid: Some(bob_uid.clone()), message: "over\u{202e} here".into() };
    request(&admin, poke).await;
    until(
        &mut bob.events,
        |e| matches!(e, Event::ClientPoked { nickname, message, .. } if nickname == "admin" && message == "over here"),
    )
    .await;

    // Kicked from Beta, bob is back in the default channel, still on the server.
    request(&bob, Request::ChannelJoin { channel: beta.id, password: None }).await;
    let kick =
        Request::ClientKick { client: bob_id, uid: Some(bob_uid), reason: Some("out".into()), from_channel: true };
    request(&admin, kick).await;
    let (me, lobby) = (bob.welcome.session, bob.welcome.server.default_channel);
    until(&mut bob.events, |e| matches!(e, Event::ClientUpdated(c) if c.id == me && c.channel == Some(lobby))).await;

    // A saved channel with its password is where carol lands.
    let gate: Channel = serde_json::from_value(request(&admin, create("Gate", Some("pw"))).await).unwrap();
    let carol =
        support::ts_client_into(bridged.ts_voice, "carol", tsclientlib::Identity::create(), Some(("Gate", "pw"))).await;
    let carol_in = carol.welcome.clients.iter().find(|c| c.id == carol.welcome.session).and_then(|c| c.channel);
    assert_eq!(carol_in, Some(gate.id));

    // Names and topics arrive without formatting that would reorder the text around them.
    let _mallory = support::ts_client(bridged.ts_voice, "Mal\u{202e}lory").await;
    until(&mut admin.events, |e| matches!(e, Event::ClientJoined(c) if c.nickname == "Mallory")).await;
    let garden = Cmd::new("channelcreate")
        .arg("channel_name", "Gar\u{202e}den")
        .arg("channel_topic", "Hi\u{202e} there")
        .arg("channel_flag_permanent", 1);
    query.call(garden).await.unwrap();
    until(&mut admin.events, |e| matches!(e, Event::ChannelCreated(c) if c.name == "Garden" && c.topic == "Hi there"))
        .await;

    // A ban by identity alone also kicks, and TeamSpeak keeps it.
    let mut dave = support::ts_client(bridged.ts_voice, "dave").await;
    let ban = Request::BanCreate(gwar_proto::BanCreate {
        client: Some(dave.welcome.session),
        duration: Some(600),
        reason: Some("spam".into()),
        ..Default::default()
    });
    let banned = request(&admin, ban).await;
    assert_eq!((banned["ban"]["nickname"].as_str(), banned["ban"]["reason"].as_str()), (Some("dave"), Some("spam")));
    until(&mut dave.events, |e| matches!(e, Event::Disconnected { .. })).await;
    let bans = query.call(Cmd::new("banlist")).await.unwrap();
    let uid = dave.welcome.uid.strip_prefix("ts:").unwrap();
    assert!(bans.iter().any(|b| b.get("uid").is_some_and(|u| u == uid)), "{bans:?}");
}

struct FakeIo {
    heard: Arc<Mutex<Vec<f32>>>,
    tone: bool,
}

impl AudioIo for FakeIo {
    fn open(self: Box<Self>) -> anyhow::Result<AudioPorts> {
        let (mut mic_tx, mic) = RingBuffer::new(48_000);
        let (speaker, mut speaker_rx) = RingBuffer::new(24_000);
        let (heard, tone) = (self.heard.clone(), self.tone);
        let output = Arc::new(OutputTiming::default());
        let timing = output.clone();
        thread::spawn(move || {
            let mut t = 0u64;
            loop {
                for _ in 0..480 {
                    let s = if tone { (t as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.3 } else { 0.0 };
                    t += 1;
                    if mic_tx
                        .push(CaptureSample { captured_at: std::time::Instant::now(), position: t - 1, value: s })
                        .is_err()
                    {
                        return;
                    }
                }
                let mut out = heard.lock().unwrap();
                let missing = 480usize.saturating_sub(speaker_rx.slots());
                for _ in 0..480 {
                    out.push(speaker_rx.pop().unwrap_or(0.0));
                }
                timing.report(480, missing);
                drop(out);
                thread::sleep(Duration::from_millis(10));
            }
        });
        Ok(AudioPorts { mic, speaker, output, _guard: Box::new(()) })
    }
}

fn rms_last_second(samples: &Arc<Mutex<Vec<f32>>>) -> f32 {
    let samples = samples.lock().unwrap();
    let tail = &samples[samples.len().saturating_sub(48_000)..];
    (tail.iter().map(|s| s * s).sum::<f32>() / tail.len().max(1) as f32).sqrt()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn teamspeak_mode_voice_reaches_webrtc_clients_and_back() {
    let Some(bridged) = support::start().await else { return };

    // WebRTC participant speaking and listening.
    let web = connect(ConnectOptions {
        url: format!("ws://{}/ws", bridged.server.http),
        nickname: "web".into(),
        invite: None,
        certificate: None,
        expected_server_key: None,
        trust_on_first_use: true,
        insecure_lan: false,
        server_password: None,
        identity: Identity::generate(),
        software: ClientSoftware { name: "t".into(), version: "0".into(), platform: Platform::Desktop },
    })
    .await
    .unwrap();
    web.connection.join(web.welcome.server.default_channel, None).await.unwrap();
    let web_heard = Arc::new(Mutex::new(Vec::new()));
    let web_voice =
        voice::start(&web.connection, Box::new(FakeIo { heard: web_heard.clone(), tone: true }), bridged.media)
            .await
            .unwrap();

    // TS-mode participant speaking a tone.
    let ts = support::ts_client(bridged.ts_voice, "tsuser").await;
    let ts_heard = Arc::new(Mutex::new(Vec::new()));
    let ts_voice = voice::start_teamspeak(
        Box::new(FakeIo { heard: ts_heard.clone(), tone: true }),
        ts.audio_in,
        ts.handle.audio_out.clone(),
    )
    .await
    .unwrap();
    // Both speak the same tone: to an echo canceller, each hears its own microphone played back.
    // These devices have no acoustic path, so there is no echo to cancel.
    for handle in [&web_voice, &ts_voice] {
        handle.set_capture_processing(false, true);
    }

    // The web user's puppet joins once the bridge is up.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let (mut at_web, mut at_ts) = (0.0, 0.0);
    while tokio::time::Instant::now() < deadline && (at_web <= 0.05 || at_ts <= 0.05) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        at_web = rms_last_second(&web_heard);
        at_ts = rms_last_second(&ts_heard);
    }
    assert!(at_web > 0.05, "WebRTC client heard rms {at_web} from the TS-mode client");
    assert!(at_ts > 0.05, "TS-mode client heard rms {at_ts} from the WebRTC client");
    drop(ts.handle);
}
