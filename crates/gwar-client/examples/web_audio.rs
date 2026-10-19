//! Local native/browser media fixture for apps/web/e2e/native-audio.mjs.
//! Commands on stdin: start, replace, prepare, activate, mute, resume, snapshot, stop, packets, quit. Replies on stdout.
//! Pass --trace-rtp after the origin to record headers through a local UDP relay.
//! Uses the real server and native voice engine with virtual 10 ms audio devices.

use std::{
    collections::HashMap,
    io::Write,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::Context;
use gwar_client::{
    ConnectOptions, Connected, Identity, connect,
    voice::{
        self, VoiceHandle,
        io::{AudioIo, AudioPorts, CaptureSample, OutputTiming},
    },
};
use gwar_proto::{ClientSoftware, Platform, Request};
use rtrb::RingBuffer;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::UdpSocket,
};

const RATE: usize = 48_000;
const BLOCK: usize = 480;
const BROWSER_TONE: f64 = 700.0;

#[derive(Default)]
struct Playout {
    samples: u64,
    missing: u64,
    energy: f64,
    tone_energy: f64,
}

struct ToneIo {
    frequency: f64,
    playout: Arc<Mutex<Playout>>,
}

struct Guard {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl AudioIo for ToneIo {
    fn open(self: Box<Self>) -> anyhow::Result<AudioPorts> {
        let (mut mic_tx, mic) = RingBuffer::new(RATE);
        let (speaker, mut speaker_rx) = RingBuffer::new(RATE / 2);
        let output = Arc::new(OutputTiming::default());
        let timing = output.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = thread::Builder::new().name("web-audio-fixture".into()).spawn(move || {
            let epoch = Instant::now();
            let mut position = 0u64;
            while !thread_stop.load(Ordering::Relaxed) {
                let mut missing = 0;
                let mut energy = 0.0;
                let mut sine = 0.0;
                let mut cosine = 0.0;
                for i in 0..BLOCK {
                    let phase = (position + i as u64) as f64 * std::f64::consts::TAU / RATE as f64;
                    let value = (phase * self.frequency).sin() as f32 * 0.25;
                    // Match physical capture: position advances even if the ring is full.
                    let _ = mic_tx.push(CaptureSample {
                        captured_at: std::time::Instant::now(),
                        position: position + i as u64,
                        value,
                    });
                    let played = f64::from(speaker_rx.pop().unwrap_or_else(|_| {
                        missing += 1;
                        0.0
                    }));
                    energy += played * played;
                    let phase = i as f64 * BROWSER_TONE * std::f64::consts::TAU / RATE as f64;
                    sine += played * phase.sin();
                    cosine += played * phase.cos();
                }
                {
                    let mut data = self.playout.lock().unwrap();
                    data.samples += BLOCK as u64;
                    data.missing += missing as u64;
                    data.energy += energy;
                    data.tone_energy += 2.0 * (sine * sine + cosine * cosine) / BLOCK as f64;
                }
                timing.report(BLOCK, missing);
                position += BLOCK as u64;
                let due = epoch + Duration::from_secs_f64(position as f64 / RATE as f64);
                if let Some(wait) = due.checked_duration_since(Instant::now()) {
                    thread::sleep(wait);
                }
            }
        })?;
        Ok(AudioPorts { mic, speaker, output, _guard: Box::new(Guard { stop, thread: Some(thread) }) })
    }
}

// Optional diagnostic relay: preserve datagrams and record only clear RTP headers.
// It forwards to media_bind, never back to the server's advertised relay address.
struct RtpTrace {
    socket: Arc<UdpSocket>,
    packets: Arc<Mutex<Vec<Value>>>,
    control_reply_errors: Arc<Mutex<Vec<Value>>>,
    tasks: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl RtpTrace {
    async fn bind() -> anyhow::Result<Self> {
        Ok(Self {
            socket: Arc::new(UdpSocket::bind("127.0.0.1:0").await?),
            packets: Arc::default(),
            control_reply_errors: Arc::default(),
            tasks: Arc::default(),
        })
    }

    fn start(&self, server: SocketAddr) {
        let front = self.socket.clone();
        let packets = self.packets.clone();
        let control_reply_errors = self.control_reply_errors.clone();
        let tasks = self.tasks.clone();
        let epoch = Instant::now();
        let main = tokio::spawn(async move {
            let result: anyhow::Result<()> = async move {
                let mut flows = HashMap::<SocketAddr, Arc<UdpSocket>>::new();
                let mut buf = vec![0u8; 4096];
                loop {
                    let (n, client) = front.recv_from(&mut buf).await?;
                    // Chromium can use a host-interface source address even when its
                    // destination is this loopback-only socket. No off-host listener exists.
                    if let std::collections::hash_map::Entry::Vacant(entry) = flows.entry(client) {
                        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
                        entry.insert(socket.clone());
                        let front = front.clone();
                        let packets = packets.clone();
                        let control_reply_errors = control_reply_errors.clone();
                        let task = tokio::spawn(async move {
                            let result: anyhow::Result<()> = async move {
                                let mut buf = vec![0u8; 4096];
                                loop {
                                    let (n, source) = socket.recv_from(&mut buf).await?;
                                    anyhow::ensure!(source == server, "unexpected relay upstream");
                                    record_rtp(&packets, epoch, "down", client, &buf[..n]);
                                    if let Err(error) = front.send_to(&buf[..n], client).await {
                                        // macOS may reject a reply from 127.0.0.1 to an unused
                                        // host-interface ICE candidate. RTP delivery errors are fatal.
                                        anyhow::ensure!(!is_rtp(&buf[..n]), "relay RTP reply to {client}: {error}");
                                        control_reply_errors
                                            .lock()
                                            .unwrap()
                                            .push(json!({"client": client, "error": error.to_string()}));
                                    }
                                }
                                #[allow(unreachable_code)]
                                Ok(())
                            }
                            .await;
                            if let Err(error) = result {
                                eprintln!("RTP trace downlink: {error:#}");
                            }
                        });
                        tasks.lock().unwrap().push(task);
                    }
                    record_rtp(&packets, epoch, "up", client, &buf[..n]);
                    flows[&client].send_to(&buf[..n], server).await?;
                }
                #[allow(unreachable_code)]
                Ok(())
            }
            .await;
            if let Err(error) = result {
                eprintln!("RTP trace uplink: {error:#}");
            }
        });
        self.tasks.lock().unwrap().push(main);
    }
}

impl Drop for RtpTrace {
    fn drop(&mut self) {
        for task in self.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
    }
}

fn is_rtp(packet: &[u8]) -> bool {
    // DTLS/STUN are not RTP v2; the RTCP payload-type range is excluded.
    packet.len() >= 12 && packet[0] >> 6 == 2 && !(192..=223).contains(&packet[1])
}

fn record_rtp(packets: &Mutex<Vec<Value>>, epoch: Instant, direction: &str, client: SocketAddr, packet: &[u8]) {
    if !is_rtp(packet) {
        return;
    }
    packets.lock().unwrap().push(json!({
        "direction": direction, "client": client, "seconds": epoch.elapsed().as_secs_f64(),
        "marker": packet[1] & 0x80 != 0,
        "sequence": u16::from_be_bytes(packet[2..4].try_into().unwrap()),
        "timestamp": u32::from_be_bytes(packet[4..8].try_into().unwrap()),
        "ssrc": u32::from_be_bytes(packet[8..12].try_into().unwrap()),
    }));
}

struct Native {
    client: Connected,
    voice: VoiceHandle,
    playout: Arc<Mutex<Playout>>,
    offer: String,
    answer: String,
    frequency: f64,
}

impl Native {
    async fn start(server: &gwar_server::Running, frequency: f64, muted: bool) -> anyhow::Result<Self> {
        anyhow::ensure!(voice::FRAME == BLOCK, "native sender is no longer using 10 ms frames");
        let client = connect(ConnectOptions {
            url: format!("ws://{}/ws", server.http),
            nickname: format!("native {frequency} Hz"),
            invite: None,
            certificate: None,
            expected_server_key: None,
            trust_on_first_use: true,
            insecure_lan: false,
            server_password: None,
            identity: Identity::generate(),
            software: ClientSoftware {
                name: "Gwar web audio test".into(),
                version: "1".into(),
                platform: Platform::Desktop,
            },
        })
        .await?;
        client.connection.join(client.welcome.server.default_channel, None).await?;
        let playout = Arc::new(Mutex::new(Playout::default()));
        let sdp = Arc::new(Mutex::new((String::new(), String::new())));
        let recorded = sdp.clone();
        let connection = client.connection.clone();
        let voice = voice::start_with(
            Box::new(ToneIo { frequency, playout: playout.clone() }),
            server.media.ip(),
            move |offer| async move {
                let answer = connection.voice_offer(offer.clone()).await?;
                *recorded.lock().unwrap() = (offer, answer.clone());
                Ok(answer)
            },
        )
        .await?;
        voice.set_muted(muted);
        let (offer, answer) = sdp.lock().unwrap().clone();
        anyhow::ensure!(offer.contains("minptime=10;useinbandfec=1"), "native offer is missing 10 ms Opus fmtp");
        Ok(Self { client, voice, playout, offer, answer, frequency })
    }

    fn snapshot(&self) -> Value {
        let stats = self.voice.stats();
        let data = self.playout.lock().unwrap();
        json!({
            "session": self.client.welcome.session,
            "frequency": self.frequency,
            "framesSent": stats.frames_sent,
            "framesReceived": stats.frames_received,
            "link": format!("{:?}", stats.link),
            "samples": data.samples,
            "missing": data.missing,
            "energy": data.energy,
            "toneEnergy": data.tone_energy,
            "underruns": stats.underruns,
            "underrunSamples": stats.underrun_samples,
            "slots": stats.slots.iter().map(|s| json!({
                "generation": s.generation, "concealments": s.concealments,
                "lateDrops": s.late_drops, "targetSamples": s.target_samples,
            })).collect::<Vec<_>>(),
        })
    }

    async fn leave(self) -> anyhow::Result<()> {
        self.client.connection.request(Request::ChannelLeave {}).await?;
        drop(self);
        Ok(())
    }
}

fn reply(value: Value) -> anyhow::Result<()> {
    println!("{value}");
    std::io::stdout().flush()?;
    Ok(())
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    let origin = std::env::args().nth(1).context("usage: web_audio http://127.0.0.1:PORT")?;
    let origin_url = url::Url::parse(&origin)?;
    anyhow::ensure!(
        origin_url.scheme() == "http" && origin_url.host_str() == Some("127.0.0.1"),
        "loopback origin required"
    );
    let trace =
        if std::env::args().nth(2).as_deref() == Some("--trace-rtp") { Some(RtpTrace::bind().await?) } else { None };
    let any = "127.0.0.1:0".parse()?;
    let advertise = trace.as_ref().map(|trace| trace.socket.local_addr()).transpose()?.unwrap_or(any);
    let server = gwar_server::start(gwar_server::Config {
        database: None,
        http_bind: any,
        media_bind: any,
        media_advertise: advertise,
        max_clients: 8,
        encrypted_server_chat: false,
        server_password: None,
        web_root: None,
        ice_servers: vec![],
        teamspeak: None,
        public_url: None,
        data_dir: None,
        public_ip: None,
        insecure_any_host: true,
        public_endpoints: vec![],
        client_ip_header: None,
        upload_limit: 0,
        upload_quota: 0,
        storage_limit: 0,
        files_dir: None,
        web_origins: vec![origin],
        tls: None,
        redirect_http: None,
        connect_url: None,
    })
    .await?;
    anyhow::ensure!(server.http.ip().is_loopback() && server.media.ip().is_loopback(), "loopback server required");
    if let Some(trace) = &trace {
        trace.start(server.media_bind);
    }
    let result = run(&server, trace.as_ref()).await;
    for task in &server.tasks {
        task.abort();
    }
    for task in server.tasks {
        let _ = task.await;
    }
    result
}

async fn run(server: &gwar_server::Running, trace: Option<&RtpTrace>) -> anyhow::Result<()> {
    reply(
        json!({"event": "ready", "url": format!("ws://{}/ws", server.http), "media": server.media, "mediaBind": server.media_bind}),
    )?;
    let mut native: Option<Native> = None;
    let mut prepared: Option<Native> = None;
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        match line.as_str() {
            "start" | "replace" => {
                if let Some(old) = native.take() {
                    old.leave().await?;
                }
                let frequency = if line == "start" { 1000.0 } else { 1500.0 };
                let next = Native::start(server, frequency, false).await?;
                reply(json!({"event": line, "native": next.snapshot(), "offer": next.offer, "answer": next.answer}))?;
                native = Some(next);
            }
            "prepare" => {
                anyhow::ensure!(prepared.is_none(), "a sender is already prepared");
                prepared = Some(Native::start(server, 1500.0, true).await?);
                reply(json!({"event": "prepare", "native": prepared.as_ref().unwrap().snapshot()}))?;
            }
            "activate" => {
                if let Some(old) = native.take() {
                    old.leave().await?;
                }
                let next = prepared.take().context("no prepared speaker")?;
                next.voice.set_muted(false);
                reply(
                    json!({"event": "activate", "native": next.snapshot(), "offer": next.offer, "answer": next.answer}),
                )?;
                native = Some(next);
            }
            "mute" | "resume" => {
                let speaker = native.as_ref().context("no native speaker")?;
                speaker.voice.set_muted(line == "mute");
                reply(json!({"event": line, "native": speaker.snapshot()}))?;
            }
            "snapshot" => {
                let speaker = native.as_ref().context("no native speaker")?;
                // Long measurements must keep signaling alive, like the app's heartbeat.
                speaker.client.connection.request(Request::Ping {}).await?;
                reply(json!({"event": "snapshot", "native": speaker.snapshot()}))?;
            }
            "stop" => {
                native.take().context("no native speaker")?.leave().await?;
                reply(json!({"event": "stop"}))?;
            }
            "packets" => {
                if let Some(trace) = trace {
                    anyhow::ensure!(
                        trace.tasks.lock().unwrap().iter().all(|task| !task.is_finished()),
                        "RTP trace stopped; see fixture stderr"
                    );
                }
                reply(
                    json!({"event": "packets", "packets": trace.map(|trace| trace.packets.lock().unwrap().clone()), "controlReplyErrors": trace.map(|trace| trace.control_reply_errors.lock().unwrap().clone())}),
                )?
            }
            "quit" => break,
            _ => anyhow::bail!("unknown command: {line}"),
        }
    }
    if let Some(native) = native {
        native.leave().await?;
    }
    if let Some(prepared) = prepared {
        prepared.leave().await?;
    }
    Ok(())
}
