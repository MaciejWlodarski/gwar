#![cfg(all(feature = "teamspeak", feature = "test-hooks"))]
//! Mirrored channels on the official TeamSpeak server. Run with GWAR_TS3_DIR and --ignored --test-threads=1.

mod common;

use std::{path::PathBuf, time::Duration};

use common::*;
use futures::StreamExt;
use gwar_server::{
    TeamSpeakConfig,
    teamspeak::{
        install,
        query::{Cmd, Query},
    },
};
use serde_json::json;
use tsclientlib::{OutCommandExt, StreamItem};
use tsproto_packets::packets::{Direction, Flags, OutCommand, PacketType};

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
    let root = PathBuf::from(std::env::var_os("GWAR_TS3_DIR").expect("GWAR_TS3_DIR"));
    // One shared installation, a fresh server state.
    let installed = install::ensure_installed(&root).await.unwrap();
    let dir = TempDir(std::env::temp_dir().join(format!("gwar-ts-channels-{}", free_port(false))));
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
        query_port,
    )
    .await;
    #[cfg(feature = "test-hooks")]
    queued_listener_reconnect(&query, query_port, password.trim(), cid.parse().unwrap()).await;
}

/// A real official-server endpoint. Commands and received bytes stay on its
/// transport task, so every positive control also proves the socket is driven.
enum ProbeCmd {
    Barrier(tokio::sync::oneshot::Sender<()>),
    Voice(Vec<u8>),
    Whisper(u16, Vec<u8>),
    Text(u8, u16, String),
}
#[derive(Debug)]
enum ProbeEvent {
    Audio(Vec<u8>),
    Text(String),
}
#[path = "common/probe_support.rs"]
mod probe_support;

struct Probe {
    tx: tokio::sync::mpsc::Sender<ProbeCmd>,
    rx: tokio::sync::mpsc::Receiver<ProbeEvent>,
    task: tokio::task::JoinHandle<()>,
    forbidden_text: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    audio_counts: std::sync::Arc<probe_support::AudioCounts>,
    allowed: [u64; 256],
    forbidden: std::collections::BTreeSet<u8>,
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
        let audio_counts = std::sync::Arc::new(probe_support::AudioCounts::default());
        let received_audio = audio_counts.clone();
        let task = tokio::spawn(async move {
            let mut id = 0u16;
            let mut barriers: std::collections::HashMap<tsclientlib::MessageHandle, tokio::sync::oneshot::Sender<()>> =
                std::collections::HashMap::new();
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
                                    if let Some(data)=data {
                                        received_audio.record(&data);
                                        let _ = events.try_send(ProbeEvent::Audio(data));
                                    }
                                },
                                StreamItem::BookEvents(book)=> for event in book {
                                    if let tsclientlib::events::Event::Message{message,..}=event {
                                        if message.contains("S6-STALE-TEXT") || message.contains("S6-SEALED-TEXT") {
                                            text_count.fetch_add(message.len(),std::sync::atomic::Ordering::Relaxed);
                                        }
                                        let _ = events.try_send(ProbeEvent::Text(message));
                                    }
                                },
                                StreamItem::MessageResult(handle, _) => {
                                    if let Some(reply) = barriers.remove(&handle) { let _ = reply.send(()); }
                                },
                                _=>{}
                            }
                            continue;
                        }
                    }
                };
                match command {
                    Some(ProbeCmd::Barrier(reply)) => {
                        let mut cmd =
                            OutCommand::new(Direction::C2S, Flags::empty(), PacketType::Command, "clientupdate");
                        // The official server did not acknowledge the native
                        // clientgetconnectioninfo request with a return_code.
                        cmd.write_arg("client_input_muted", &0);
                        let handle = cmd.send_with_result(&mut con).unwrap();
                        barriers.insert(handle, reply);
                    }
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
        Self { tx, rx, task, forbidden_text, audio_counts, allowed: [0; 256], forbidden: Default::default() }
    }
    async fn audio(&mut self, marker: u8) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let ProbeEvent::Audio(bytes) = self.rx.recv().await.expect("official probe ended")
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
                if let ProbeEvent::Text(text) = self.rx.recv().await.expect("official probe ended")
                    && text.contains(wanted)
                {
                    return;
                }
            }
        })
        .await
        .expect("working plaintext TeamSpeak text control");
    }
    async fn barrier(&self) {
        let (reply, done) = tokio::sync::oneshot::channel();
        self.tx.send(ProbeCmd::Barrier(reply)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), done)
            .await
            .expect("official probe command acknowledgement")
            .unwrap();
    }
    async fn forbidden_bytes(&mut self, marker: u8) -> u64 {
        self.forbidden.insert(marker);
        self.barrier().await;
        self.audio_counts.bytes(marker) - self.allowed[usize::from(marker)]
    }
    async fn assert_forbidden_totals(&self) {
        self.barrier().await;
        for &marker in &self.forbidden {
            assert_eq!(
                self.audio_counts.bytes(marker),
                self.allowed[usize::from(marker)],
                "cumulative forbidden marker {marker}"
            );
        }
    }
    async fn control(&self, peer: &common::rtc::Peer, marker: u8, sequence: &mut u64) {
        let before = self.audio_counts.bytes(marker);
        probe_support::control(
            |seq| peer.send(seq, seq as u32 * 960, vec![0xf8, marker, seq as u8]),
            || self.audio_counts.bytes(marker) > before,
            sequence,
        )
        .await;
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
    query_port: u16,
) {
    use gwar_server::media::{AudioKind, AudioPacket, SourceClock};
    use std::sync::Arc;
    let mut probe = Probe::new(ts);
    admin.ok("channel.join", json!({"channel":1})).await;
    let mut peer = common::rtc::Peer::start(admin, 1).await;
    let puppet = puppet_id(query, "root").await;
    let mut sequence = 0;
    // Send fresh packets until route confirmation and actual delivery.
    probe.control(&peer, 11, &mut sequence).await;
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
    Query::reconnect_listener(format!("127.0.0.1:{query_port}").parse().unwrap(), default_cid).await;
    probe.tx.send(ProbeCmd::Text(2, 0, "s6 listener after reconnect".into())).await.unwrap();
    admin.next_event_where("chat.message", |m| m["text"] == "s6 listener after reconnect").await;

    // No private conversation has retained this puppet yet.
    let ts_session = admin.next_event_where("client.joined", |c| c["nickname"] == "ts-admin").await["id"]
        .as_u64()
        .expect("mirrored TS user") as u32;

    let origin = server.running.core.audio_origin(admin.welcome.session).await.unwrap();
    let frame = AudioPacket {
        admitted_at: std::time::Instant::now(),
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
    probe.allowed[21] = probe.audio_counts.bytes(21);
    // Voice move invalidates previously admitted plaintext as well as ciphertext.
    admin.ok("channel.join", json!({"channel":2})).await;
    // Channel 2 has no TS remote and no private chat retention: this is the
    // actual 20-second grace-only puppet, not a DM puppet described as grace.
    assert_eq!(puppet_id(query, "root").await, puppet);
    server.running.plane.uplinks.uplink(&server.running.plane.routing.load(), &frame);
    assert_eq!(probe.forbidden_bytes(21).await, 0, "queued old-route bytes");
    // A refused encrypted entry cannot authorize a route or relabel its queue.
    assert!(admin.request("channel.join", json!({"channel":sealed})).await.is_err());
    admin.ok("chat.send", json!({"target":{"client":ts_session},"text":"s6 retained private control"})).await;
    probe.text("s6 retained private control").await;
    probe.tx.send(ProbeCmd::Whisper(puppet, vec![0xf8, 31, 1])).await.unwrap();
    peer.silent().await; // retired recipient transport cannot hear a whisper
    // Return creates a fresh participation and transport; the old frame stays old.
    admin.ok("channel.join", json!({"channel":1})).await;
    peer = common::rtc::Peer::start(admin, 2).await;
    let mut sequence = 0;
    probe.control(&peer, 32, &mut sequence).await;
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
    probe.control(&peer, 33, &mut sequence).await;
    // This traverses the actual source puppet after the injections. Its audio
    // queue has priority over commands; receiving this text proves queue progress.
    admin.ok("chat.send", json!({"target":{"client":ts_session},"text":"s6 final source barrier"})).await;
    probe.text("s6 final source barrier").await;
    eprintln!(
        "S6: official plaintext voice/whisper, puppet/query/listener/private text, move/re-entry/refused-entry/reconnect controls"
    );
    s6_controlled_routes(&mut probe, voice, default_cid, sealed_cid, ts_clid).await;
    probe.assert_forbidden_totals().await;
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
    use gwar_server::{
        core::bridge::RemoteClient,
        media::{AudioKind, AudioPacket, AudioRoute, SourceClock},
        teamspeak::puppet::{self, ChannelText, PuppetCmd, PuppetEvent, PuppetSpec, Shared},
    };
    use std::sync::Arc;
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
            platform: gwar_proto::Platform::Desktop,
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
        server_password: None,
        delay: Duration::ZERO,
    };
    let handle = puppet::spawn(voice, spec(1, route.clone()), shared.clone(), events.clone());
    let connected = tokio::time::timeout(Duration::from_secs(20), event_rx.recv()).await.unwrap().unwrap();
    let PuppetEvent::Connected { clid, .. } = connected else { panic!("controlled puppet did not connect") };
    tokio::time::timeout(Duration::from_secs(20), handle.wait_ingress_ready(ts_clid)).await.unwrap().unwrap();
    let mut sequence = 0;
    probe.control(&peer, 40, &mut sequence).await;
    // Both actual incoming audio routes have plaintext controls.
    probe.tx.send(ProbeCmd::Voice(vec![0xf8, 46, 1])).await.unwrap();
    assert_eq!(peer.receive().await.payload[1], 46);
    probe.tx.send(ProbeCmd::Whisper(clid, vec![0xf8, 47, 1])).await.unwrap();
    assert_eq!(peer.receive().await.payload[1], 47);
    let ssrc = peer.ssrc;
    let packet = |marker, egress: Arc<AudioRoute>| AudioPacket {
        admitted_at: std::time::Instant::now(),
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
    #[allow(unused_mut)]
    let mut restored = AudioRoute::new(3, default_cid, Some(origin.clone()));
    assert!(handle.send(PuppetCmd::Move { cid: default_cid, route: 3, audio: restored.clone(), voice: true }));
    probe.control(&peer, 43, &mut sequence).await;
    #[cfg(feature = "test-hooks")]
    {
        let (armed, arm) = tokio::sync::oneshot::channel();
        let (held, tail) = tokio::sync::oneshot::channel();
        let (release, resume) = tokio::sync::oneshot::channel();
        assert!(handle.send(PuppetCmd::HoldTail { armed, held, release: resume }));
        arm.await.unwrap();
        shared.sent_end_route.store(0, std::sync::atomic::Ordering::Release);
        let mut half = packet(60, restored.clone());
        half.payload = Arc::from([0xf0, 60, 1]);
        half.span = 480;
        shared.plane.uplinks.uplink(&shared.plane.routing.load(), &half);
        tokio::time::timeout(Duration::from_secs(5), tail).await.unwrap().unwrap();
        let mut end = AudioPacket::end_of_speech(origin.clone(), None);
        end.egress = Some(restored.clone());
        shared.plane.uplinks.uplink(&shared.plane.routing.load(), &end);
        let next = AudioRoute::new(30, default_cid, Some(origin.clone()));
        assert!(handle.send(PuppetCmd::Move { cid: default_cid, route: 30, audio: next.clone(), voice: true }));
        release.send(()).unwrap();
        probe.control(&peer, 61, &mut sequence).await;
        assert_eq!(probe.forbidden_bytes(60).await, 0, "held native tail crossed retirement");
        assert_ne!(
            shared.sent_end_route.load(std::sync::atomic::Ordering::Acquire),
            restored.generation,
            "queued old end marker reached transport"
        );
        restored = next;
    }

    // Delayed classification: observe and discard now, then classify. These
    // packets cannot be retrospectively assigned a newer source route.
    tokio::time::timeout(Duration::from_secs(20), handle.wait_ingress_ready(ts_clid)).await.unwrap().unwrap();
    shared.forget(ts_clid);
    ingress_attempt(probe, &shared, &handle, client.welcome.session, ts_clid, clid, false, 50).await;
    ingress_attempt(probe, &shared, &handle, client.welcome.session, ts_clid, clid, true, 51).await;
    shared.mirror(ts_clid, default_cid, remote_origin.clone());
    peer.silent().await;
    // Source identity is admitted by MLS and SRTP, and matches both routing
    // and mirror binding. Only AudioRoute's plaintext boundary can reject it.
    let (sealed_client, mut sealed_peer) = common::rtc::encrypted_peer(&isolated).await;
    let sealed_origin = isolated.running.core.audio_origin(sealed_client.welcome.session).await.unwrap();
    assert!(sealed_origin.encrypted);
    assert_eq!(shared.plane.routing.load().origin(sealed_origin.session), Some(sealed_origin.clone()));
    shared.mirror(ts_clid, default_cid, sealed_origin.clone());
    let source_route = shared.mirrors.read().unwrap()[&ts_clid].clone();
    assert_eq!(source_route.origin, Some(sealed_origin.clone()));
    assert!(source_route.is_current());
    ingress_attempt(probe, &shared, &handle, client.welcome.session, ts_clid, clid, false, 52).await;
    ingress_attempt(probe, &shared, &handle, client.welcome.session, ts_clid, clid, true, 53).await;
    common::rtc::assert_encrypted_source_boundaries(
        &isolated,
        sealed_client.welcome.session,
        client.welcome.session,
        sealed_peer.ssrc,
    );
    peer.silent().await;
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
    let events_for_encrypted = events.clone();
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
    tokio::time::timeout(Duration::from_secs(20), fresh.wait_ingress_ready(ts_clid)).await.unwrap().unwrap();
    probe.control(&peer, 45, &mut sequence).await;
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
    let sealed_route = AudioRoute::new(6, default_cid, Some(sealed_origin));
    let encrypted_handle = puppet::spawn(
        voice,
        PuppetSpec {
            session: sealed_client.welcome.session,
            nickname: "s6-encrypted-target".into(),
            ..spec(3, sealed_route)
        },
        shared.clone(),
        events_for_encrypted,
    );
    let encrypted_clid = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(PuppetEvent::Connected { generation: 3, clid, .. }) = event_rx.recv().await {
                break clid;
            }
        }
    })
    .await
    .unwrap();
    // A normal bridge would never authorize this puppet for encrypted voice.
    // Force a real TS receive callback against an admitted encrypted SRTP peer:
    // each valid Opus route must be observed, with zero admission independently.
    ingress_attempt(
        probe,
        &shared,
        &encrypted_handle,
        sealed_client.welcome.session,
        ts_clid,
        encrypted_clid,
        false,
        54,
    )
    .await;
    sealed_peer.silent().await;
    ingress_attempt(
        probe,
        &shared,
        &encrypted_handle,
        sealed_client.welcome.session,
        ts_clid,
        encrypted_clid,
        true,
        55,
    )
    .await;
    sealed_peer.silent().await;
    drop(encrypted_handle);
    eprintln!(
        "S6 controlled official routes: queued/move/refusal/reconnect payload bytes=0; sealed incoming voice+whisper payload bytes=0"
    );
}

/// Exactly one injection is outstanding. A matching callback must finish on
/// the specified puppet before classification can change; no aggregate counter
/// or earlier retry can satisfy this acknowledgement.
#[allow(clippy::too_many_arguments)]
async fn ingress_attempt(
    probe: &Probe,
    shared: &gwar_server::teamspeak::puppet::Shared,
    handle: &gwar_server::teamspeak::puppet::PuppetHandle,
    receiver: u32,
    from: u16,
    clid: u16,
    whisper: bool,
    marker: u8,
) {
    if marker >= 52 {
        tokio::time::timeout(Duration::from_secs(20), handle.wait_ingress_ready(from))
            .await
            .expect("receiving puppet knows its own channel and the matching source")
            .unwrap();
    }
    let packet = vec![0xf8, marker, 1];
    assert_eq!(gwar_proto::opus::packet_samples(&packet), Some(960));
    let done = shared.observe_ingress(gwar_server::teamspeak::puppet::IngressKey {
        receiver,
        from,
        whisper,
        payload: packet.clone(),
    });
    probe.tx.send(if whisper { ProbeCmd::Whisper(clid, packet) } else { ProbeCmd::Voice(packet) }).await.unwrap();
    let observation = tokio::time::timeout(Duration::from_secs(5), done)
        .await
        .expect("exact injected packet completed on the receiving puppet")
        .unwrap();
    assert!(observation.receiver_ready, "receiver must be ready: {observation:?}");
    assert_eq!(observation.source_matches, marker >= 52, "source classification marker={marker}: {observation:?}");
    assert!(!observation.admitted, "forbidden TS admission, marker={marker}, whisper={whisper}");
}

#[cfg(feature = "test-hooks")]
async fn queued_listener_reconnect(query: &Query, port: u16, password: &str, cid: u64) {
    use gwar_server::teamspeak::listeners::ChannelListeners;
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let mut listeners = ChannelListeners::new(format!("127.0.0.1:{port}").parse().unwrap(), password.into(), tx);
    listeners.reconcile([cid]);
    let addr = format!("127.0.0.1:{port}").parse().unwrap();
    Query::listener_ready(addr, cid).await;
    let me = query.call(Cmd::new("whoami")).await.unwrap()[0]["client_id"].clone();
    query.call(Cmd::new("clientmove").arg("clid", me).arg("cid", cid)).await.unwrap();
    query
        .call(Cmd::new("sendtextmessage").arg("targetmode", 2).arg("target", cid).arg("msg", "queued listener old"))
        .await
        .unwrap();
    let old = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
    assert!(listeners.accepts(&old));
    Query::reconnect_listener(addr, cid).await;
    assert!(!listeners.accepts(&old), "queued text survived its listener connection");
    query
        .call(Cmd::new("sendtextmessage").arg("targetmode", 2).arg("target", cid).arg("msg", "queued listener fresh"))
        .await
        .unwrap();
    let fresh = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
    assert!(listeners.accepts(&fresh));
    assert_eq!(fresh.text, "queued listener fresh");
}

/// A TeamSpeak client of the bridged server, ready once it has the server's state.
async fn teamspeak_user(voice: std::net::SocketAddr, name: &str) -> (tsclientlib::Connection, u16, u64) {
    let mut con = tsclientlib::Connection::build(voice.to_string()).name(name.to_string()).connect().unwrap();
    // Until the server's state includes this client itself.
    within("TeamSpeak client", async {
        loop {
            con.events().next().await.expect("TeamSpeak events").unwrap();
            if let Ok(state) = con.get_state()
                && state.clients.contains_key(&state.own_client)
            {
                break;
            }
        }
    })
    .await;
    let state = con.get_state().unwrap();
    let clid = state.own_client.0;
    let cid = state.clients[&state.own_client].channel.0;
    (con, clid, cid)
}

/// Waits until ServerQuery reports `clid` as a talker or not.
async fn wait_for_talker(query: &Query, clid: u16, talker: bool) {
    let want = if talker { "1" } else { "0" };
    within("talker flag", async {
        loop {
            let info = query.call(Cmd::new("clientinfo").arg("clid", clid)).await.unwrap();
            if info.first().and_then(|r| r.get("client_is_talker")).map(String::as_str) == Some(want) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
}

/// `from` talks with `marker` until `to` hears it.
async fn heard(from: &Probe, to: &Probe, marker: u8) {
    let mut sequence = 0;
    probe_support::control(
        |seq| {
            let tx = from.tx.clone();
            async move { tx.send(ProbeCmd::Voice(vec![0xf8, marker, seq as u8])).await.unwrap() }
        },
        || to.audio_counts.bytes(marker) > 0,
        &mut sequence,
    )
    .await;
}

/// `from` whispers `to` (its clid) with `marker` until `to` hears it.
async fn whispered(from: &Probe, to: &Probe, to_clid: u16, marker: u8) {
    let mut sequence = 0;
    probe_support::control(
        |seq| {
            let tx = from.tx.clone();
            async move { tx.send(ProbeCmd::Whisper(to_clid, vec![0xf8, marker, seq as u8])).await.unwrap() }
        },
        || to.audio_counts.bytes(marker) > 0,
        &mut sequence,
    )
    .await;
}

/// `from` whispers `to` for a while; `to` must hear none of it.
async fn unwhispered(from: &Probe, to: &mut Probe, to_clid: u16, marker: u8) {
    for seq in 0..50u8 {
        from.tx.send(ProbeCmd::Whisper(to_clid, vec![0xf8, marker, seq])).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    from.barrier().await;
    assert_eq!(to.forbidden_bytes(marker).await, 0, "a TeamSpeak user who may not speak was heard whispering");
}

/// `from` talks with `marker` for a while; `to` must hear none of it.
async fn unheard(from: &Probe, to: &mut Probe, marker: u8) {
    for seq in 0..50u8 {
        from.tx.send(ProbeCmd::Voice(vec![0xf8, marker, seq])).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    from.barrier().await;
    assert_eq!(to.forbidden_bytes(marker).await, 0, "a TeamSpeak user who may not speak was heard");
}

#[tokio::test]
#[ignore]
async fn on_teamspeak_only_those_who_may_speak_talk() {
    let root = PathBuf::from(std::env::var_os("GWAR_TS3_DIR").expect("GWAR_TS3_DIR"));
    let installed = install::ensure_installed(&root).await.unwrap();
    let dir = TempDir(std::env::temp_dir().join(format!("gwar-ts-talk-{}", free_port(false))));
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

    let (a, a_clid, lobby) = teamspeak_user(voice, "ts-speaker").await;
    let (b, b_clid, _) = teamspeak_user(voice, "ts-listener").await;
    let (a, mut b) = (Probe::new(a), Probe::new(b));
    // Mirrored channels need talk power nobody has; the bridge makes those who may speak talkers.
    let info = query.call(Cmd::new("channelinfo").arg("cid", lobby)).await.unwrap();
    assert_eq!(info[0].get("channel_needed_talk_power").map(String::as_str), Some("1000000"), "{info:?}");
    wait_for_talker(&query, a_clid, true).await;
    heard(&a, &b, 60).await;

    // Muted for everyone by a Gwar moderator: nobody on TeamSpeak hears them either, nor a whisper.
    let speaker = admin.next_event_where("client.joined", |c| c["nickname"] == "ts-speaker").await["id"].clone();
    admin.ok("client.voice", json!({"client": speaker, "muted": true})).await;
    wait_for_talker(&query, a_clid, false).await;
    unheard(&a, &mut b, 61).await;
    unwhispered(&a, &mut b, b_clid, 64).await;
    admin.ok("client.voice", json!({"client": speaker, "muted": false})).await;
    wait_for_talker(&query, a_clid, true).await;
    heard(&a, &b, 62).await;
    whispered(&a, &b, b_clid, 65).await;

    let channel_id =
        |created: serde_json::Value| created["channel"]["id"].as_u64().or_else(|| created["id"].as_u64()).unwrap();
    admin.create_channel(json!({"name": "Open"})).await;
    let stage = channel_id(admin.create_channel(json!({"name": "Stage"})).await);
    let closed = channel_id(admin.create_channel(json!({"name": "Closed"})).await);
    for (channel, deny) in [(stage, "speak"), (closed, "connect")] {
        let entry = json!({"channel": channel, "override": {"role": 2, "allow": [], "deny": [deny]}});
        admin.ok("channel.override", entry).await;
    }
    let (open_cid, stage_cid, closed_cid) =
        (find_channel(&query, "Open").await, find_channel(&query, "Stage").await, find_channel(&query, "Closed").await);
    let move_to = |clid: u16, cid: String| {
        let query = &query;
        async move { query.call(Cmd::new("clientmove").arg("clid", clid).arg("cid", cid)).await.unwrap() }
    };
    // Moved where they may speak: a talker again there.
    move_to(a_clid, open_cid.clone()).await;
    move_to(b_clid, open_cid.clone()).await;
    wait_for_talker(&query, a_clid, true).await;
    heard(&a, &b, 66).await;
    // Into a channel they may not join: back in Open, and heard there again.
    move_to(a_clid, closed_cid).await;
    within("moved back from Closed", async {
        loop {
            let info = query.call(Cmd::new("clientinfo").arg("clid", a_clid)).await.unwrap();
            if info[0].get("cid") == Some(&open_cid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    wait_for_talker(&query, a_clid, true).await;
    heard(&a, &b, 67).await;
    // A channel where Member may not speak: silent there.
    move_to(a_clid, stage_cid.clone()).await;
    move_to(b_clid, stage_cid).await;
    wait_for_talker(&query, a_clid, false).await;
    unheard(&a, &mut b, 63).await;
}

/// A TeamSpeak client with this identity, connected and in the server's state.
async fn teamspeak_user_with(
    voice: std::net::SocketAddr,
    name: &str,
    identity: tsclientlib::Identity,
) -> (tsclientlib::Connection, u16) {
    let mut con =
        tsclientlib::Connection::build(voice.to_string()).name(name.to_string()).identity(identity).connect().unwrap();
    within("TeamSpeak client", async {
        loop {
            con.events().next().await.expect("TeamSpeak events").unwrap();
            if let Ok(state) = con.get_state()
                && state.clients.contains_key(&state.own_client)
            {
                break;
            }
        }
    })
    .await;
    let clid = con.get_state().unwrap().own_client.0;
    (con, clid)
}

#[tokio::test]
#[ignore]
async fn a_claimed_teamspeak_identity_acts_as_its_member_on_teamspeak() {
    let root = PathBuf::from(std::env::var_os("GWAR_TS3_DIR").expect("GWAR_TS3_DIR"));
    let installed = install::ensure_installed(&root).await.unwrap();
    let dir = TempDir(std::env::temp_dir().join(format!("gwar-ts-claim-{}", free_port(false))));
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

    // Only Speakers may speak on the Stage; Alice is one.
    let created = admin.ok("group.create", json!({"name": "Speakers", "permissions": []})).await;
    let speakers = created["group"]["id"].as_u64().unwrap();
    let stage = channel_id(&admin.create_channel(json!({"name": "Stage"})).await);
    for (role, allow, deny) in [(2, json!([]), json!(["speak"])), (speakers, json!(["speak"]), json!([]))] {
        let entry = json!({"channel": stage, "override": {"role": role, "allow": allow, "deny": deny}});
        admin.ok("channel.override", entry).await;
    }
    let mut alice = Client::connect(&server, "alice").await;
    let alice_uid = alice.welcome.uid.clone();
    admin.ok("member.groups", json!({"uid": alice_uid, "groups": [2, speakers]})).await;

    // Alice proves she holds the TeamSpeak identity.
    let identity = tsclientlib::Identity::create();
    let key = identity.key().to_pub();
    let nonce = alice.ok("teamspeak.challenge", json!({})).await["nonce"].as_str().unwrap().to_owned();
    let statement = format!("gwar teamspeak claim v1\n{}\n{}\n{}", alice.server_key, alice_uid, nonce);
    let signature = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        identity.key().clone().sign(statement.as_bytes()),
    );
    let claimed = alice.ok("teamspeak.claim", json!({"key": key.to_ts(), "signature": signature})).await;
    // The uid the server derived is the one TeamSpeak shows for that client.
    assert_eq!(claimed["teamspeak"], format!("ts:{}", key.get_uid()));

    let stage_cid = find_channel(&query, "Stage").await;
    let (_linked, linked_clid) = teamspeak_user_with(voice, "ts-alice", identity).await;
    let info = query.call(Cmd::new("clientinfo").arg("clid", linked_clid)).await.unwrap();
    assert_eq!(info[0].get("client_unique_identifier").map(String::as_str), Some(key.get_uid().as_str()));
    let (_other, other_clid, _) = teamspeak_user(voice, "ts-other").await;
    for clid in [linked_clid, other_clid] {
        query.call(Cmd::new("clientmove").arg("clid", clid).arg("cid", stage_cid.clone())).await.unwrap();
    }
    // On the Stage, the linked identity speaks with Alice's role; an unlinked one does not.
    wait_for_talker(&query, linked_clid, true).await;
    wait_for_talker(&query, other_clid, false).await;
}

/// Connects a TeamSpeak client with `password`: whether TeamSpeak lets it in.
async fn teamspeak_admits(voice: std::net::SocketAddr, name: &str, password: Option<&str>) -> bool {
    let mut builder = tsclientlib::Connection::build(voice.to_string()).name(name.to_string());
    if let Some(password) = password {
        builder = builder.password(password.to_string());
    }
    let mut con = builder.connect().unwrap();
    let admitted = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let failed = !matches!(con.events().next().await, Some(Ok(_)));
            if failed {
                return false;
            }
            if let Ok(state) = con.get_state()
                && state.clients.contains_key(&state.own_client)
            {
                return true;
            }
        }
    })
    .await
    .unwrap_or(false);
    if admitted {
        let _ = con.disconnect(tsclientlib::DisconnectOptions::new());
    }
    admitted
}

#[tokio::test]
#[ignore]
async fn the_servers_password_holds_on_teamspeak_too() {
    let root = PathBuf::from(std::env::var_os("GWAR_TS3_DIR").expect("GWAR_TS3_DIR"));
    let installed = install::ensure_installed(&root).await.unwrap();
    let dir = TempDir(std::env::temp_dir().join(format!("gwar-ts-password-{}", free_port(false))));
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
    let flag = || async {
        let info = query.call(Cmd::new("serverinfo")).await.unwrap();
        info[0].get("virtualserver_flag_password").cloned().unwrap_or_default()
    };
    assert_eq!(flag().await, "0");
    let mut admin = admin(&server, "root").await;

    // Set in Gwar: TeamSpeak takes the same password.
    admin.ok("server.update", json!({"password": "hunter2"})).await;
    tokio::time::timeout(Duration::from_secs(20), async {
        while flag().await != "1" {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("TeamSpeak gets a server password");
    assert!(!teamspeak_admits(voice, "no-password", None).await);
    assert!(!teamspeak_admits(voice, "wrong-password", Some("hunter3")).await);
    assert!(teamspeak_admits(voice, "right-password", Some("hunter2")).await);

    // Our puppets still get in: a Gwar user sharing a channel with a TeamSpeak user has one.
    let mut ts = tsclientlib::Connection::build(voice.to_string())
        .name("ts-user".to_string())
        .password("hunter2".to_string())
        .connect()
        .unwrap();
    within("TeamSpeak client", async {
        loop {
            ts.events().next().await.expect("TeamSpeak events").unwrap();
            if let Ok(state) = ts.get_state()
                && state.clients.contains_key(&state.own_client)
            {
                break;
            }
        }
    })
    .await;
    admin.ok("channel.join", json!({"channel": 1})).await;
    puppet_id(&query, "root").await;

    // Removed in Gwar: open on TeamSpeak again.
    admin.ok("server.update", json!({"password": ""})).await;
    tokio::time::timeout(Duration::from_secs(20), async {
        while flag().await != "0" {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("TeamSpeak's server password removed");
    assert!(teamspeak_admits(voice, "open-again", None).await);
}
