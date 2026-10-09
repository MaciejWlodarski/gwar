//! Our client in TeamSpeak mode on the official TeamSpeak server that our
//! server bridges: TS users and our users see, hear and message each other.

mod support;

use std::{
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use rtrb::RingBuffer;
use vc_client::voice::{
    self,
    io::{AudioIo, AudioPorts},
};

use tokio::time::timeout;
use vc_client::{ConnectOptions, Identity, connect};
use vc_proto::{Channel, ChannelId, ChatTarget, ClientSoftware, Event, Platform, Request};

async fn until(events: &mut tokio::sync::mpsc::Receiver<Event>, pred: impl Fn(&Event) -> bool) -> Event {
    let mut seen = Vec::new();
    let found = timeout(Duration::from_secs(15), async {
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
        .request(Request::ChatSend { target: ChatTarget::Channel(lobby_ts), text: "from TeamSpeak mode".into() })
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
        .request(Request::ClientUpdate(vc_proto::ClientUpdate { muted: Some(true), ..Default::default() }))
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

struct FakeIo {
    heard: Arc<Mutex<Vec<f32>>>,
    tone: bool,
}

impl AudioIo for FakeIo {
    fn open(self: Box<Self>) -> anyhow::Result<AudioPorts> {
        let (mut mic_tx, mic) = RingBuffer::new(48_000);
        let (speaker, mut speaker_rx) = RingBuffer::new(24_000);
        let (heard, tone) = (self.heard.clone(), self.tone);
        thread::spawn(move || {
            let mut t = 0u64;
            loop {
                for _ in 0..480 {
                    let s = if tone { (t as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.3 } else { 0.0 };
                    t += 1;
                    if mic_tx.push(s).is_err() {
                        return;
                    }
                }
                let mut out = heard.lock().unwrap();
                for _ in 0..480 {
                    out.push(speaker_rx.pop().unwrap_or(0.0));
                }
                drop(out);
                thread::sleep(Duration::from_millis(10));
            }
        });
        Ok(AudioPorts { mic, speaker, _guard: Box::new(()) })
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
        server_password: None,
        identity: Identity::generate(),
        software: ClientSoftware { name: "t".into(), version: "0".into(), platform: Platform::Desktop },
    })
    .await
    .unwrap();
    web.connection.join(web.welcome.server.default_channel, None).await.unwrap();
    let web_heard = Arc::new(Mutex::new(Vec::new()));
    let _web_voice =
        voice::start(&web.connection, Box::new(FakeIo { heard: web_heard.clone(), tone: true }), bridged.media)
            .await
            .unwrap();

    // TS-mode participant speaking a tone.
    let ts = support::ts_client(bridged.ts_voice, "tsuser").await;
    let ts_heard = Arc::new(Mutex::new(Vec::new()));
    let _ts_voice = voice::start_teamspeak(
        Box::new(FakeIo { heard: ts_heard.clone(), tone: true }),
        ts.audio_in,
        ts.handle.audio_out.clone(),
    )
    .await
    .unwrap();

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
