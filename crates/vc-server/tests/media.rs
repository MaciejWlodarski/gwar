//! End-to-end media test: two WebRTC clients (str0m acting as the browser)
//! negotiate through the real gateway and exchange Opus frames via the SFU.

use std::{
    collections::VecDeque,
    net::SocketAddr,
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signer, SigningKey};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use str0m::{
    Candidate, Event as RtcEvent, Input, Output, Rtc,
    change::SdpAnswer,
    format::Codec,
    media::{Direction, Frequency, MediaKind, MediaTime, Mid},
    net::{Protocol, Receive},
};
use tokio::{
    net::{TcpStream, UdpSocket},
    sync::mpsc,
    time::{sleep_until, timeout},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};
use vc_proto::{AUDIO_SLOTS, challenge_message};
use vc_server::{Config, Running};

const WAIT: Duration = Duration::from_secs(5);

async fn server() -> Running {
    let any: SocketAddr = "127.0.0.1:0".parse().unwrap();
    vc_server::start(Config {
        database: None,
        http_bind: any,
        media_bind: any,
        media_advertise: any,
        max_clients: 16,
        server_password: None,
        web_root: None,
        ice_servers: vec![],
        teamspeak: None,
        public_url: None,
        upload_limit: 10 * 1024 * 1024,
        files_dir: None,
        web_origins: Vec::new(),
        tls: None,
        redirect_http: None,
        connect_url: None,
    })
    .await
    .unwrap()
}

struct Ws {
    stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    events: VecDeque<Value>,
    next_id: u32,
    session: u64,
}

impl Ws {
    async fn connect(http: SocketAddr, nickname: &str) -> Ws {
        let (mut stream, _) = tokio_tungstenite::connect_async(format!("ws://{http}/ws")).await.unwrap();
        let challenge = read(&mut stream).await;
        let nonce = challenge["d"]["nonce"].as_str().unwrap();
        let key = SigningKey::from_bytes(&rand::random());
        let public_key = URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes());
        let signature = URL_SAFE_NO_PAD.encode(key.sign(&challenge_message(nonce, &public_key)).to_bytes());
        let hello = json!({"id": 1, "op": "hello", "d": {
            "protocol": 1, "nickname": nickname, "public_key": public_key, "signature": signature,
            "client": {"name": "test", "version": "0", "platform": "desktop"}}});
        stream.send(Message::Text(hello.to_string().into())).await.unwrap();
        let welcome = read(&mut stream).await;
        let session = welcome["ok"]["session"].as_u64().expect("welcome");
        Ws { stream, events: VecDeque::new(), next_id: 2, session }
    }

    async fn request(&mut self, op: &str, d: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let frame = json!({"id": id, "op": op, "d": d});
        self.stream.send(Message::Text(frame.to_string().into())).await.unwrap();
        loop {
            let frame = read(&mut self.stream).await;
            if frame["re"] == id {
                assert!(frame.get("err").is_none(), "{op} failed: {frame}");
                return frame["ok"].clone();
            }
            self.events.push_back(frame);
        }
    }

    async fn event(&mut self, matches: impl Fn(&Value) -> bool) -> Value {
        if let Some(i) = self.events.iter().position(&matches) {
            return self.events.remove(i).unwrap();
        }
        loop {
            let frame = read(&mut self.stream).await;
            if matches(&frame) {
                return frame;
            }
            self.events.push_back(frame);
        }
    }
}

async fn read(stream: &mut WebSocketStream<MaybeTlsStream<TcpStream>>) -> Value {
    loop {
        match timeout(WAIT, stream.next()).await.expect("timed out waiting for frame").unwrap().unwrap() {
            Message::Text(text) => return serde_json::from_str(&text).unwrap(),
            _ => continue,
        }
    }
}

enum Cmd {
    Send { payload: Vec<u8>, ticks: u64, level: i8 },
}

/// Received frame: (receive-slot index, payload).
type Received = (usize, Vec<u8>);

struct RtcClient {
    commands: mpsc::Sender<Cmd>,
    received: mpsc::Receiver<Received>,
    connected: mpsc::Receiver<()>,
}

async fn rtc_client(ws: &mut Ws) -> RtcClient {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let local = socket.local_addr().unwrap();
    let mut rtc = Rtc::builder().clear_codecs().enable_opus(true, false).build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
    let mut api = rtc.sdp_api();
    let mic = api.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None);
    let slots: Vec<Mid> =
        (0..AUDIO_SLOTS).map(|_| api.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None)).collect();
    let (offer, pending) = api.apply().unwrap();
    let answer = ws.request("voice.offer", json!({"sdp": offer.to_sdp_string()})).await;
    let answer = SdpAnswer::from_sdp_string(answer["sdp"].as_str().unwrap()).unwrap();
    rtc.sdp_api().accept_answer(pending, answer).unwrap();

    let (commands, mut command_rx) = mpsc::channel(64);
    let (received_tx, received) = mpsc::channel(256);
    let (connected_tx, connected) = mpsc::channel(1);
    tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        loop {
            let deadline = loop {
                match rtc.poll_output().unwrap() {
                    Output::Timeout(t) => break t,
                    Output::Transmit(t) => {
                        let _ = socket.send_to(&t.contents, t.destination).await;
                    }
                    Output::Event(RtcEvent::Connected) => {
                        let _ = connected_tx.try_send(());
                    }
                    Output::Event(RtcEvent::MediaData(data)) => {
                        if let Some(slot) = slots.iter().position(|m| *m == data.mid) {
                            let _ = received_tx.try_send((slot, data.data.to_vec()));
                        }
                    }
                    Output::Event(_) => {}
                }
            };
            tokio::select! {
                cmd = command_rx.recv() => match cmd {
                    Some(Cmd::Send { payload, ticks, level }) => {
                        let writer = rtc.writer(mic).unwrap();
                        let pt = writer.payload_params().find(|p| p.spec().codec == Codec::Opus).unwrap().pt();
                        writer
                            .audio_level(level, level > -40)
                            .write(pt, Instant::now(), MediaTime::new(ticks, Frequency::FORTY_EIGHT_KHZ), payload)
                            .unwrap();
                    }
                    None => return,
                },
                r = socket.recv_from(&mut buf) => {
                    let (n, source) = r.unwrap();
                    let input = Input::Receive(Instant::now(), Receive {
                        proto: Protocol::Udp, source, destination: local, contents: buf[..n].try_into().unwrap(),
                    });
                    rtc.handle_input(input).unwrap();
                }
                _ = sleep_until(deadline.into()) => {}
            }
            rtc.handle_input(Input::Timeout(Instant::now())).unwrap();
        }
    });
    RtcClient { commands, received, connected }
}

/// Sends `count` 20 ms frames whose payload is TOC 0xf8 + marker byte.
async fn speak(client: &RtcClient, marker: u8, count: u64, level: i8) {
    for i in 0..count {
        let payload = vec![0xf8, marker, i as u8, 0x55, 0xaa];
        client.commands.send(Cmd::Send { payload, ticks: i * 960, level }).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn drain(client: &mut RtcClient, for_ms: u64) -> Vec<Received> {
    let mut out = Vec::new();
    let end = tokio::time::Instant::now() + Duration::from_millis(for_ms);
    while let Ok(Some(item)) = tokio::time::timeout_at(end, client.received.recv()).await {
        out.push(item);
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opus_is_forwarded_unchanged_within_channel_only() {
    let running = server().await;
    let mut alice = Ws::connect(running.http, "alice").await;
    let mut bob = Ws::connect(running.http, "bob").await;
    // Both join the default channel's voice (connecting alone does not).
    alice.request("channel.join", json!({"channel": 1})).await;
    bob.request("channel.join", json!({"channel": 1})).await;
    let mut a = rtc_client(&mut alice).await;
    let mut b = rtc_client(&mut bob).await;
    timeout(WAIT, a.connected.recv()).await.expect("alice ICE/DTLS");
    timeout(WAIT, b.connected.recv()).await.expect("bob ICE/DTLS");
    let alice_id = alice.session;
    bob.event(|e| e["ev"] == "client.updated" && e["d"]["id"] == alice_id && e["d"]["voice"] == true).await;

    // Loud frames are forwarded byte-for-byte into one of bob's slots.
    speak(&a, 7, 15, -20).await;
    let got = drain(&mut b, 300).await;
    assert!(got.len() >= 10, "bob received {} frames", got.len());
    assert!(got.iter().all(|(slot, p)| *slot == got[0].0 && p[0] == 0xf8 && p[1] == 7));
    let slot = bob.event(|e| e["ev"] == "voice.slot").await;
    assert_eq!(slot["d"]["client"], alice_id);
    assert_eq!(slot["d"]["slot"], got[0].0);
    bob.event(|e| e["ev"] == "voice.talking" && e["d"]["client"] == alice_id && e["d"]["talking"] == true).await;
    bob.event(|e| e["ev"] == "voice.talking" && e["d"]["client"] == alice_id && e["d"]["talking"] == false).await;
    assert!(drain(&mut a, 100).await.is_empty(), "speaker must not hear itself");

    // Silence (below the speech gate) is not forwarded at all.
    speak(&a, 8, 10, -90).await;
    assert!(drain(&mut b, 200).await.is_empty(), "silence must be gated");

    // A muted speaker is not forwarded.
    alice.request("client.update", json!({"muted": true})).await;
    speak(&a, 9, 10, -20).await;
    assert!(drain(&mut b, 200).await.is_empty(), "muted speaker leaked audio");
    alice.request("client.update", json!({"muted": false})).await;

    // Different channels do not hear each other.
    let channels = bob.request("chat.history", json!({"channel": 1})).await;
    assert!(channels["messages"].is_array());
    bob.request("channel.join", json!({"channel": 2})).await;
    speak(&a, 10, 10, -20).await;
    assert!(drain(&mut b, 200).await.is_empty(), "audio crossed channels");

    // Back together: audio flows again.
    bob.request("channel.join", json!({"channel": 1})).await;
    speak(&a, 11, 10, -20).await;
    assert!(drain(&mut b, 300).await.iter().any(|(_, p)| p[1] == 11));
}
