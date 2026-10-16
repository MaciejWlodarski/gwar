#![cfg(feature = "teamspeak")]
//! Mirrored channels on the official TeamSpeak server. Run with VC_TS3_DIR and --ignored --test-threads=1.

mod common;

use std::{path::PathBuf, time::Duration};

use common::*;
use futures::StreamExt;
use serde_json::json;
use tsclientlib::{OutCommandExt, StreamItem};
use tsproto_packets::packets::{Direction, Flags, OutCommand, PacketType};
use vc_server::{
    TeamSpeakConfig,
    teamspeak::{
        install,
        query::{Cmd, Query},
    },
};

fn free_port(udp: bool) -> u16 {
    if udp {
        std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    } else {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn channel_permissions(query: &Query, cid: &str) -> Vec<(String, String)> {
    let perms = query.call(Cmd::new("channelpermlist").arg("cid", cid).flag("permsid")).await.unwrap();
    perms.iter().filter_map(|p| Some((p.get("permsid")?.to_owned(), p.get("permvalue")?.to_owned()))).collect()
}

async fn find_channel(query: &Query, name: &str) -> String {
    within("mirrored channel", async {
        loop {
            let channels = query.call(Cmd::new("channellist")).await.unwrap();
            let mirrored = channels.iter().find(|c| c.get("channel_name").is_some_and(|n| n == name));
            if let Some(cid) = mirrored.and_then(|c| c.get("cid")) {
                return cid.to_owned();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
}

async fn wait_for_permission(query: &Query, cid: &str, perm: (&str, &str)) -> Vec<(String, String)> {
    within("the channel permission", async {
        loop {
            let perms = channel_permissions(query, cid).await;
            if perms.contains(&(perm.0.into(), perm.1.into())) {
                return perms;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
}

/// A TeamSpeak client's `channeladdperm` of the channel's needed join power.
async fn set_join_power(
    ts: &mut tsclientlib::Connection,
    cid: &str,
    power: &str,
) -> Result<(), tsclientlib::CommandError> {
    let mut command = OutCommand::new(Direction::C2S, Flags::empty(), PacketType::Command, "channeladdperm");
    command.write_arg("cid", &cid);
    command.write_arg("permsid", &"i_channel_needed_join_power");
    command.write_arg("permvalue", &power);
    let handle = command.send_with_result(ts).unwrap();
    within("channeladdperm answer", async {
        let mut stream = ts.events();
        while let Some(item) = stream.next().await {
            if let StreamItem::MessageResult(h, result) = item.unwrap()
                && h == handle
            {
                return result;
            }
        }
        panic!("TeamSpeak connection closed");
    })
    .await
}

#[tokio::test]
#[ignore]
async fn teamspeak_server_admins_manage_permissions_of_mirrored_channels() {
    let root = PathBuf::from(std::env::var_os("VC_TS3_DIR").expect("VC_TS3_DIR"));
    // One shared installation, a fresh server state.
    let installed = install::ensure_installed(&root).await.unwrap();
    let dir = TempDir(std::env::temp_dir().join(format!("vc-ts-channels-{}", free_port(false))));
    std::fs::create_dir_all(&dir.0).unwrap();
    std::os::unix::fs::symlink(&installed.dir, dir.0.join(installed.dir.file_name().unwrap())).unwrap();
    let voice = format!("127.0.0.1:{}", free_port(true)).parse().unwrap();
    let query_port = free_port(false);
    let server = TestServer::start_with(|c| {
        c.teamspeak = Some(TeamSpeakConfig {
            dir: dir.0.clone(),
            voice,
            query_port,
            filetransfer: format!("127.0.0.1:{}", free_port(false)).parse().unwrap(),
            public_url: None,
        })
    })
    .await;
    let mut ready = server.running.teamspeak_ready.clone().unwrap();
    tokio::time::timeout(Duration::from_secs(120), ready.wait_for(|ready| *ready)).await.unwrap().unwrap();
    let password = std::fs::read_to_string(dir.0.join("query-password")).unwrap();
    let (query, _notes) =
        Query::connect(format!("127.0.0.1:{query_port}").parse().unwrap(), "serveradmin", password.trim(), 1)
            .await
            .unwrap();

    let mut admin = admin(&server, "root").await;
    admin.create_channel(json!({"name": "Mirrored"})).await;
    let sealed_channel = admin.create_channel(json!({"name": "Sealed", "encrypted": true})).await;
    let cid = find_channel(&query, "Mirrored").await;
    // The bridge sets the power right after creating the channel.
    let perms = wait_for_permission(&query, &cid, ("i_channel_needed_permission_modify_power", "75")).await;
    // Deleting stays out of reach: the bridge would only bring the channel back.
    assert!(perms.contains(&("i_channel_needed_delete_power".into(), "100".into())), "{perms:?}");

    // A TeamSpeak user in the stock Server Admin group sets a channel permission.
    let mut ts = tsclientlib::Connection::build(voice.to_string()).name("ts-admin".to_string()).connect().unwrap();
    within("TeamSpeak client", async {
        let mut stream = ts.events();
        while let Some(item) = stream.next().await {
            if matches!(item.unwrap(), StreamItem::BookEvents(_)) {
                break;
            }
        }
    })
    .await;
    let clients = query.call(Cmd::new("clientlist")).await.unwrap();
    let me = clients.iter().find(|c| c.get("client_nickname").is_some_and(|n| n == "ts-admin")).unwrap();
    let cldbid = me.get("client_database_id").unwrap().to_owned();
    query.call(Cmd::new("servergroupaddclient").arg("sgid", 6).arg("cldbid", &cldbid)).await.unwrap();

    let result = set_join_power(&mut ts, &cid, "10").await;
    assert!(result.is_ok(), "Server Admin was refused: {result:?}");
    let perms = channel_permissions(&query, &cid).await;
    assert!(perms.contains(&("i_channel_needed_join_power".into(), "10".into())), "{perms:?}");

    // An encrypted channel is locked, and only ServerQuery could open it.
    let sealed = find_channel(&query, "Sealed").await;
    wait_for_permission(&query, &sealed, ("i_channel_needed_join_power", "1000000")).await;
    let perms = wait_for_permission(&query, &sealed, ("i_channel_needed_permission_modify_power", "100")).await;
    assert!(perms.contains(&("i_channel_needed_join_power".into(), "1000000".into())), "{perms:?}");
    let result = set_join_power(&mut ts, &sealed, "10").await;
    assert!(result.is_err(), "Server Admin lowered the lock");
    let perms = channel_permissions(&query, &sealed).await;
    assert!(perms.contains(&("i_channel_needed_join_power".into(), "1000000".into())), "{perms:?}");
    let sealed_id =
        sealed_channel["channel"]["id"].as_u64().or_else(|| sealed_channel["id"].as_u64()).expect("created channel id")
            as u32;
    let ts_clid = ts.get_state().unwrap().own_client.0;
    let default_cid = ts.get_state().unwrap().clients[&ts.get_state().unwrap().own_client].channel.0;
    s6_payload_boundaries(
        &server,
        &mut admin,
        &query,
        ts,
        sealed_id,
        voice,
        default_cid,
        sealed.parse().unwrap(),
        ts_clid,
    )
    .await;
}

/// A real official-server endpoint. Commands and received bytes stay on its
/// transport task, so every positive control also proves the socket is driven.
enum ProbeCmd {
    Voice(Vec<u8>),
    Whisper(u16, Vec<u8>),
    Text(u8, u16, String),
}
#[derive(Debug)]
enum ProbeEvent {
    Audio(Vec<u8>),
    Text(String),
}
struct Probe {
    tx: tokio::sync::mpsc::Sender<ProbeCmd>,
    rx: tokio::sync::mpsc::Receiver<ProbeEvent>,
    task: tokio::task::JoinHandle<()>,
    forbidden_text: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Probe {
    fn new(mut con: tsclientlib::Connection) -> Self {
        use tsproto_packets::packets::{AudioData, CodecType, OutAudio};
        let (tx, mut commands) = tokio::sync::mpsc::channel(64);
        let (events, rx) = tokio::sync::mpsc::channel(1024);
        let forbidden_text = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let text_count = forbidden_text.clone();
        let task = tokio::spawn(async move {
            let mut id = 0u16;
            loop {
                let command = {
                    let mut stream = con.events();
                    tokio::select! {
                        cmd=commands.recv()=>cmd,
                        item=stream.next()=> {
                            match item.expect("official TS stream").expect("official TS event") {
                                StreamItem::Audio(p)=> {
                                    let data=match p.data().data() {
                                        AudioData::S2C{data,..}|AudioData::S2CWhisper{data,..}=>Some(data.to_vec()), _=>None
                                    };
                                    if let Some(data)=data { events.send(ProbeEvent::Audio(data)).await.unwrap(); }
                                },
                                StreamItem::BookEvents(book)=> for event in book {
                                    if let tsclientlib::events::Event::Message{message,..}=event {
                                        if message.contains("S6-STALE-TEXT") || message.contains("S6-SEALED-TEXT") {
                                            text_count.fetch_add(message.len(),std::sync::atomic::Ordering::Relaxed);
                                        }
                                        events.send(ProbeEvent::Text(message)).await.unwrap();
                                    }
                                },
                                _=>{}
                            }
                            continue;
                        }
                    }
                };
                match command {
                    Some(ProbeCmd::Voice(data)) => {
                        con.send_audio(OutAudio::new(&AudioData::C2S { id, codec: CodecType::OpusVoice, data: &data }))
                            .unwrap();
                        id = id.wrapping_add(1);
                    }
                    Some(ProbeCmd::Whisper(to, data)) => {
                        con.send_audio(OutAudio::new(&AudioData::C2SWhisper {
                            id,
                            codec: CodecType::OpusVoice,
                            channels: vec![],
                            clients: vec![to],
                            data: &data,
                        }))
                        .unwrap();
                        id = id.wrapping_add(1);
                    }
                    Some(ProbeCmd::Text(mode, target, text)) => {
                        let mut cmd =
                            OutCommand::new(Direction::C2S, Flags::empty(), PacketType::Command, "sendtextmessage");
                        cmd.write_arg("targetmode", &mode);
                        cmd.write_arg("msg", &text);
                        if mode == 1 {
                            cmd.write_arg("target", &target);
                        }
                        cmd.send(&mut con).unwrap();
                    }
                    None => return,
                }
            }
        });
        Self { tx, rx, task, forbidden_text }
    }
    async fn audio(&mut self, marker: u8) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(ProbeEvent::Audio(bytes)) = self.rx.recv().await
                    && bytes.get(1) == Some(&marker)
                {
                    return;
                }
            }
        })
        .await
        .expect("working plaintext TeamSpeak audio control");
    }
    async fn text(&mut self, wanted: &str) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(ProbeEvent::Text(text)) = self.rx.recv().await
                    && text.contains(wanted)
                {
                    return;
                }
            }
        })
        .await
        .expect("working plaintext TeamSpeak text control");
    }
    async fn forbidden_bytes(&mut self, marker: u8) -> usize {
        let end = tokio::time::Instant::now() + Duration::from_millis(300);
        let mut bytes = 0;
        while let Ok(Some(event)) = tokio::time::timeout_at(end, self.rx.recv()).await {
            if let ProbeEvent::Audio(data) = event
                && data.get(1) == Some(&marker)
            {
                bytes += data.len();
            }
        }
        bytes
    }
}

async fn puppet_id(query: &Query, name: &str) -> u16 {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let clients = query.call(Cmd::new("clientlist")).await.unwrap();
            if let Some(id) = clients
                .iter()
                .find(|r| r.get("client_nickname").is_some_and(|v| v == name))
                .and_then(|r| r.get("clid"))
                .and_then(|v| v.parse().ok())
            {
                return id;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("puppet connection")
}

#[allow(clippy::too_many_arguments)]
async fn s6_payload_boundaries(
    server: &TestServer,
    admin: &mut Client,
    query: &Query,
    ts: tsclientlib::Connection,
    sealed: u32,
    voice: std::net::SocketAddr,
    default_cid: u64,
    sealed_cid: u64,
    ts_clid: u16,
) {
    use std::sync::Arc;
    use vc_server::media::{AudioKind, AudioPacket, SourceClock, webrtc::MediaCmd};
    let mut probe = Probe::new(ts);
    admin.ok("channel.join", json!({"channel":1})).await;
    let mut peer = common::rtc::Peer::start(admin, 1).await;
    let puppet = puppet_id(query, "root").await;
    // Wait for the initial move acknowledgement using an actual audio control.
    for sequence in 0..20 {
        peer.send(sequence, sequence as u32 * 960, vec![0xf8, 11, sequence as u8]).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    probe.audio(11).await;
    probe.tx.send(ProbeCmd::Voice(vec![0xf8, 12, 1])).await.unwrap();
    assert_eq!(peer.receive().await.payload[1], 12);
    probe.tx.send(ProbeCmd::Whisper(puppet, vec![0xf8, 13, 1])).await.unwrap();
    assert_eq!(peer.receive().await.payload[1], 13);
    probe.tx.send(ProbeCmd::Text(2, 0, "s6 listener plaintext".into())).await.unwrap();
    admin.next_event_where("chat.message", |m| m["text"] == "s6 listener plaintext").await;
    admin.ok("chat.send", json!({"target":{"channel":1},"text":"s6 puppet plaintext"})).await;
    probe.text("s6 puppet plaintext").await;
    admin.ok("chat.send", json!({"target":"server","text":"s6 query plaintext"})).await;
    probe.text("s6 query plaintext").await;
    // A user with no voice/DM puppet takes the ServerQuery text route.
    let mut query_writer = Client::connect(server, "s6-query-writer").await;
    query_writer.ok("chat.send", json!({"target":{"channel":1},"text":"s6 actual query plaintext"})).await;
    probe.text("s6 actual query plaintext").await;
    assert!(
        query_writer.request("chat.send", json!({"target":{"channel":sealed},"text":"S6-SEALED-TEXT"})).await.is_err()
    );
    let listener_name = format!("Gwar chat {default_cid}");
    let old_listener = puppet_id(query, &listener_name).await;
    query
        .call(
            Cmd::new("clientkick")
                .arg("clid", old_listener)
                .arg("reasonid", 5)
                .arg("reasonmsg", "S6 listener reconnect"),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if puppet_id(query, &listener_name).await != old_listener {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("query listener replacement");
    probe.tx.send(ProbeCmd::Text(2, 0, "s6 listener after reconnect".into())).await.unwrap();
    admin.next_event_where("chat.message", |m| m["text"] == "s6 listener after reconnect").await;

    // Keep the puppet for direct messages while leaving voice (also covers its
    // grace-period existence). Direct text remains a working independent route.
    let ts_session = admin.welcome.clients.iter().find(|c| c.nickname == "ts-admin").expect("mirrored TS user").id;
    admin.ok("chat.send", json!({"target":{"client":ts_session},"text":"s6 private plaintext"})).await;
    probe.text("s6 private plaintext").await;
    let origin = server.running.core.audio_origin(admin.welcome.session).await.unwrap();
    let frame = AudioPacket {
        origin: origin.clone(),
        payload: Arc::from([0xf8, 21, 1]),
        kind: AudioKind::Audio,
        span: 960,
        clock: SourceClock::Rtp { ssrc: peer.ssrc, sequence: 50, ticks: 48_000, generation: 0 },
        route: None,
        target: None,
        ingress: None,
        egress: server.running.plane.uplinks.route(admin.welcome.session),
    };
    // Positive control for direct fanout/uplink injection before classification.
    server.running.plane.uplinks.uplink(&server.running.plane.routing.load(), &frame);
    probe.audio(21).await;
    let mut encrypted = frame.clone();
    let mut fixed = (*origin).clone();
    fixed.encrypted = true;
    fixed.channel = sealed;
    encrypted.origin = Arc::new(fixed);
    encrypted.payload = Arc::from([0xff, 99, 0]);
    let attempt = |packet: AudioPacket| {
        let plane = &server.running.plane;
        plane.uplinks.uplink(&plane.routing.load(), &packet);
        plane.sinks.fan_out(&plane.routing.load(), &packet);
        plane.publish(packet.clone());
        plane.deliver(admin.welcome.session, packet.clone());
        plane.webrtc.try_send(MediaCmd::Forward(packet.clone())).unwrap();
        plane.webrtc.try_send(MediaCmd::ForwardTo(admin.welcome.session, packet)).unwrap();
    };
    attempt(encrypted.clone());
    assert_eq!(probe.forbidden_bytes(99).await, 0, "encrypted egress bytes");
    // Voice move invalidates previously admitted plaintext as well as ciphertext.
    admin.ok("channel.join", json!({"channel":2})).await;
    server.running.plane.uplinks.uplink(&server.running.plane.routing.load(), &frame);
    assert_eq!(probe.forbidden_bytes(21).await, 0, "queued old-route bytes");
    // A refused encrypted entry cannot authorize a route or relabel its queue.
    assert!(admin.request("channel.join", json!({"channel":sealed})).await.is_err());
    server.running.plane.publish(encrypted.clone());
    assert_eq!(probe.forbidden_bytes(99).await, 0);
    admin.ok("chat.send", json!({"target":{"client":ts_session},"text":"s6 retained private control"})).await;
    probe.text("s6 retained private control").await;
    probe.tx.send(ProbeCmd::Whisper(puppet, vec![0xf8, 31, 1])).await.unwrap();
    peer.silent().await; // retired recipient transport cannot hear a whisper
    // Return creates a fresh participation and transport; the old frame stays old.
    admin.ok("channel.join", json!({"channel":1})).await;
    peer = common::rtc::Peer::start(admin, 2).await;
    for seq in 0..20 {
        peer.send(seq, seq as u32 * 960, vec![0xf8, 32, 1]).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    probe.audio(32).await;
    server.running.plane.uplinks.uplink(&server.running.plane.routing.load(), &frame);
    assert_eq!(probe.forbidden_bytes(21).await, 0);
    // Official transport replacement: queue old-origin packets, kick only our
    // puppet, and prove the bridge's replacement is alive before asserting zero.
    query
        .call(Cmd::new("clientkick").arg("clid", puppet).arg("reasonid", 5).arg("reasonmsg", "S6 reconnect"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let id = puppet_id(query, "root").await;
            if id != puppet {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("replacement puppet");
    for seq in 20..40 {
        peer.send(seq, seq as u32 * 960, vec![0xf8, 33, 1]).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    probe.audio(33).await;
    server.running.plane.uplinks.uplink(&server.running.plane.routing.load(), &encrypted);
    assert_eq!(probe.forbidden_bytes(99).await, 0);
    eprintln!(
        "S6: official plaintext voice/whisper, puppet/query/listener/private text, move/re-entry/refused-entry/reconnect controls; encrypted egress bytes=0"
    );
    s6_controlled_routes(&mut probe, voice, default_cid, sealed_cid, ts_clid).await;
}

/// Control the actual puppet's command/route boundary against the same official
/// server, using a second in-process Gwar core without another TeamSpeak process.
/// This makes queued-before-move/refused-move/reconnect cutoffs deterministic.
async fn s6_controlled_routes(
    probe: &mut Probe,
    voice: std::net::SocketAddr,
    default_cid: u64,
    sealed_cid: u64,
    ts_clid: u16,
) {
    use std::sync::Arc;
    use vc_server::{
        core::bridge::RemoteClient,
        media::{AudioKind, AudioPacket, AudioRoute, SourceClock},
        teamspeak::puppet::{self, ChannelText, PuppetCmd, PuppetEvent, PuppetSpec, Shared},
    };
    let isolated = TestServer::start().await;
    let mut client = Client::connect(&isolated, "s6-isolated").await;
    client.ok("channel.join", json!({"channel":1})).await;
    let mut peer = common::rtc::Peer::start(&mut client, 1).await;
    let origin = isolated.running.core.audio_origin(client.welcome.session).await.unwrap();
    let remote = isolated
        .running
        .core
        .remote_join(RemoteClient {
            uid: "ts:s6-probe".into(),
            nickname: "s6-probe".into(),
            channel: 1,
            platform: vc_proto::Platform::Desktop,
            muted: false,
            deafened: false,
            away: None,
        })
        .await
        .unwrap();
    let remote_origin = isolated.running.core.audio_origin(remote).await.unwrap();
    let shared = Arc::new(Shared::new(isolated.running.core.clone(), isolated.running.plane.clone()));
    shared.mirror(ts_clid, default_cid, remote_origin.clone());
    let (events, mut event_rx) = tokio::sync::mpsc::channel(64);
    let route = AudioRoute::new(1, default_cid, Some(origin.clone()));
    let spec = |generation, audio: Arc<AudioRoute>| PuppetSpec {
        session: client.welcome.session,
        generation,
        nickname: "s6-controlled".into(),
        identity: tsclientlib::Identity::create(),
        channel: default_cid,
        route: audio.generation,
        audio,
        voice: true,
        muted: false,
        deafened: false,
        away: None,
        delay: Duration::ZERO,
    };
    let handle = puppet::spawn(voice, spec(1, route.clone()), shared.clone(), events.clone());
    let connected = tokio::time::timeout(Duration::from_secs(20), event_rx.recv()).await.unwrap().unwrap();
    let PuppetEvent::Connected { clid, .. } = connected else { panic!("controlled puppet did not connect") };
    for seq in 0..20 {
        peer.send(seq, seq as u32 * 960, vec![0xf8, 40, 1]).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    probe.audio(40).await;
    // Both actual incoming audio routes have plaintext controls.
    probe.tx.send(ProbeCmd::Voice(vec![0xf8, 46, 1])).await.unwrap();
    assert_eq!(peer.receive().await.payload[1], 46);
    probe.tx.send(ProbeCmd::Whisper(clid, vec![0xf8, 47, 1])).await.unwrap();
    assert_eq!(peer.receive().await.payload[1], 47);
    let ssrc = peer.ssrc;
    let packet = |marker, egress: Arc<AudioRoute>| AudioPacket {
        origin: origin.clone(),
        payload: Arc::from([0xf8, marker, 1]),
        kind: AudioKind::Audio,
        span: 960,
        clock: SourceClock::Rtp { ssrc, sequence: 100, ticks: 96_000, generation: 0 },
        route: None,
        target: None,
        ingress: None,
        egress: Some(egress),
    };
    // No await between enqueue and invalidation: all 64 packets are still queued
    // when Move retires their route, even though the audio select branch is first.
    for _ in 0..64 {
        shared.plane.uplinks.uplink(&shared.plane.routing.load(), &packet(41, route.clone()));
    }
    let refused = AudioRoute::new(2, sealed_cid, Some(origin.clone()));
    assert!(handle.send(PuppetCmd::Move { cid: sealed_cid, route: 2, audio: refused.clone(), voice: true }));
    assert!(!route.is_current());
    assert_eq!(probe.forbidden_bytes(41).await, 0, "pre-move queued plaintext egress");
    tokio::time::timeout(Duration::from_secs(15), async {
        while refused.is_current() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("official refusal retires route");
    shared.plane.uplinks.uplink(&shared.plane.routing.load(), &packet(42, refused.clone()));
    assert_eq!(probe.forbidden_bytes(42).await, 0, "refused-route egress");
    let restored = AudioRoute::new(3, default_cid, Some(origin.clone()));
    assert!(handle.send(PuppetCmd::Move { cid: default_cid, route: 3, audio: restored.clone(), voice: true }));
    for seq in 20..40 {
        peer.send(seq, seq as u32 * 960, vec![0xf8, 43, 1]).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    probe.audio(43).await;
    // Fixed source classification at the TS callback: both voice and whispers
    // are dropped before Opus inspection when the classified origin is sealed.
    let mut sealed_origin = (*remote_origin).clone();
    sealed_origin.encrypted = true;
    shared.mirror(ts_clid, default_cid, Arc::new(sealed_origin));
    probe.tx.send(ProbeCmd::Voice(vec![0xff, 99, 0])).await.unwrap();
    probe.tx.send(ProbeCmd::Whisper(clid, vec![0xff, 99, 0])).await.unwrap();
    peer.silent().await; // zero admitted TeamSpeak ciphertext payload bytes
    shared.mirror(ts_clid, default_cid, isolated.running.core.audio_origin(remote).await.unwrap());
    probe.tx.send(ProbeCmd::Voice(vec![0xf8, 48, 1])).await.unwrap();
    assert_eq!(peer.receive().await.payload[1], 48);
    // A retained direct-message puppet has no audio authority at all.
    let private = AudioRoute::new(4, default_cid, None);
    assert!(handle.send(PuppetCmd::Move { cid: default_cid, route: 4, audio: private.clone(), voice: false }));
    probe.tx.send(ProbeCmd::Whisper(clid, vec![0xf8, 49, 1])).await.unwrap();
    peer.silent().await;
    assert!(handle.send(PuppetCmd::Text {
        mode: 1,
        target: ts_clid,
        text: "s6 controlled private".into(),
        channel: None
    }));
    probe.text("s6 controlled private").await;
    // Replacement invalidates queued channel and private text as well as audio.
    assert!(handle.send(PuppetCmd::Text { mode: 1, target: ts_clid, text: "S6-STALE-TEXT".into(), channel: None }));
    drop(handle);
    let replacement = AudioRoute::new(5, default_cid, Some(origin.clone()));
    let fresh = puppet::spawn(voice, spec(2, replacement.clone()), shared.clone(), events);
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if matches!(event_rx.recv().await, Some(PuppetEvent::Connected { generation: 2, .. })) {
                break;
            }
        }
    })
    .await
    .unwrap();
    for seq in 40..60 {
        peer.send(seq, seq as u32 * 960, vec![0xf8, 45, 1]).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    probe.audio(45).await;
    shared.plane.uplinks.uplink(&shared.plane.routing.load(), &packet(44, restored));
    assert_eq!(probe.forbidden_bytes(44).await, 0, "old transport egress");
    assert!(fresh.send(PuppetCmd::Text {
        mode: 2,
        target: 0,
        text: "S6-SEALED-TEXT".into(),
        channel: Some(ChannelText { cid: sealed_cid, encrypted: true, route: 5 })
    }));
    assert!(fresh.send(PuppetCmd::Text {
        mode: 2,
        target: 0,
        text: "s6 replacement text".into(),
        channel: Some(ChannelText { cid: default_cid, encrypted: false, route: 5 })
    }));
    probe.text("s6 replacement text").await;
    assert_eq!(
        probe.forbidden_text.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "queued or sealed text bytes at official egress"
    );
    drop(fresh);
    eprintln!(
        "S6 controlled official routes: queued/move/refusal/reconnect payload bytes=0; sealed incoming voice+whisper payload bytes=0"
    );
}
