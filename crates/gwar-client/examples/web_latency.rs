//! Local server and native peer for apps/web/e2e/latency.mjs (docs/latency.md "Web client").
//!   web_latency http://127.0.0.1:WEB_PORT OUTPUT_DIR
//! Starts a fresh loopback-only server whose data lives in OUTPUT_DIR/server-data (removed on
//! exit). LATENCY_NET inserts the native harness's seeded UDP relay in front of every media
//! client, browsers included (see latency/netem.rs). Commands on stdin, one JSON reply per line:
//!   send | receive   start a native peer in the default channel, as marker sender or receiver
//!   arm WARMUP_MS COUNT   start recording (receiver) or the markers after WARMUP_MS (sender)
//!   finish           stop the peer; writes OUTPUT_DIR/native-playout.f32 for a receiver
//!   quit
//! Times are Unix milliseconds on the system clock, which browsers' `Date.now()` also reads.
#[path = "latency/netem.rs"]
mod netem;

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
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
use tokio::io::{AsyncBufReadExt, BufReader};

// The marker scheme of examples/latency.rs, generated here and detected by the web harness.
const RATE: usize = 48_000;
const BLOCK: usize = 480;
const PERIOD: usize = 12_000; // 250 ms
const MARKER: usize = 960; // 20 ms
const TONES: [f64; 8] = [900.0, 1100.0, 1300.0, 1500.0, 1700.0, 1900.0, 2300.0, 2700.0];

/// Monotonic time as Unix milliseconds, anchored once to the system clock.
#[derive(Clone, Copy)]
struct Clock {
    instant: Instant,
    unix_ms: f64,
}
impl Clock {
    fn new() -> Self {
        let unix_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64() * 1000.0;
        Self { instant: Instant::now(), unix_ms }
    }

    fn ms(&self, at: Instant) -> f64 {
        self.unix_ms + at.duration_since(self.instant).as_secs_f64() * 1000.0
    }
}

#[derive(Default)]
struct Shared {
    /// Warmup samples and marker count, once armed.
    arm: Option<(usize, usize)>,
    /// Sender: (capture time, identity) of every marker.
    captures: Vec<(f64, usize)>,
    /// Receiver: speaker samples since arming, and the time of each callback's first sample.
    playout: Vec<f32>,
    blocks: Vec<(usize, f64)>,
    underflows: usize,
}

struct MarkerIo {
    sender: bool,
    clock: Clock,
    shared: Arc<Mutex<Shared>>,
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

impl AudioIo for MarkerIo {
    fn open(self: Box<Self>) -> anyhow::Result<AudioPorts> {
        let (mut mic_tx, mic) = RingBuffer::new(RATE);
        let (speaker, mut speaker_rx) = RingBuffer::new(RATE / 2);
        let output = Arc::new(OutputTiming::default());
        let timing = output.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = thread::Builder::new().name("web-latency-audio".into()).spawn(move || {
            // Virtual 10 ms callbacks, as in examples/latency.rs.
            let start = Instant::now();
            let mut position = 0usize;
            let mut marker_start = None;
            while !thread_stop.load(Ordering::Relaxed) {
                let now = self.clock.ms(Instant::now());
                let mut missing = 0;
                let mut data = self.shared.lock().unwrap();
                let armed = data.arm;
                if let (None, Some((warmup, _))) = (marker_start, armed) {
                    marker_start = Some(position + warmup);
                }
                let count = armed.map_or(0, |(_, count)| count);
                let first = data.playout.len();
                for i in 0..BLOCK {
                    let at = position + i;
                    let marker = marker_start
                        .filter(|&start| at >= start && (at - start) / PERIOD < count)
                        .map(|start| ((at - start) / PERIOD, (at - start) % PERIOD));
                    if let (true, Some((index, 0))) = (self.sender, marker) {
                        data.captures.push((now + i as f64 * 1000.0 / RATE as f64, index % TONES.len()));
                    }
                    let tone =
                        marker.filter(|&(_, offset)| offset < MARKER).map(|(index, _)| TONES[index % TONES.len()]);
                    let value = if self.sender {
                        let (frequency, amplitude) = tone.map_or((400.0, 0.08), |frequency| (frequency, 0.6));
                        (at as f64 * frequency * std::f64::consts::TAU / RATE as f64).sin() as f32 * amplitude
                    } else {
                        0.0
                    };
                    if mic_tx.push(CaptureSample { captured_at: Instant::now(), position: at as u64, value }).is_err() {
                        return;
                    }
                    let played = speaker_rx.pop().unwrap_or_else(|_| {
                        missing += 1;
                        0.0
                    });
                    if !self.sender && armed.is_some() {
                        data.playout.push(played);
                    }
                }
                if !self.sender && armed.is_some() {
                    data.blocks.push((first, now));
                    data.underflows += missing;
                }
                drop(data);
                timing.report(BLOCK, missing);
                position += BLOCK;
                let due = start + Duration::from_secs_f64(position as f64 / RATE as f64);
                if let Some(wait) = due.checked_duration_since(Instant::now()) {
                    thread::sleep(wait);
                }
            }
        })?;
        Ok(AudioPorts { mic, speaker, output, _guard: Box::new(Guard { stop, thread: Some(thread) }) })
    }
}

struct Peer {
    sender: bool,
    client: Connected,
    voice: VoiceHandle,
    shared: Arc<Mutex<Shared>>,
}

impl Peer {
    async fn start(url: &str, media: std::net::SocketAddr, sender: bool, clock: Clock) -> anyhow::Result<Self> {
        let client = connect(ConnectOptions {
            url: url.into(),
            nickname: if sender { "native sender" } else { "native receiver" }.into(),
            invite: None,
            certificate: None,
            expected_server_key: None,
            trust_on_first_use: true,
            insecure_lan: false,
            server_password: None,
            identity: Identity::generate(),
            software: ClientSoftware {
                name: "Gwar web latency".into(),
                version: "1".into(),
                platform: Platform::Desktop,
            },
        })
        .await?;
        client.connection.join(client.welcome.server.default_channel, None).await?;
        let shared = Arc::new(Mutex::new(Shared::default()));
        let io = Box::new(MarkerIo { sender, clock, shared: shared.clone() });
        let voice = voice::start(&client.connection, io, media).await?;
        Ok(Self { sender, client, voice, shared })
    }

    fn stats(&self) -> Value {
        let s = self.voice.stats();
        json!({
            "link": format!("{:?}", s.link),
            "frames_sent": s.frames_sent,
            "frames_received": s.frames_received,
            "speaker_samples": s.speaker_samples,
            "callback_samples": s.callback_samples,
            "headroom_samples": s.headroom_samples,
            "underruns": s.underruns,
            "underrun_samples": s.underrun_samples,
            "slots": s.slots.iter().filter(|slot| slot.generation > 0).map(|slot| json!({
                "target_samples": slot.target_samples,
                "queued_samples": slot.queued_samples,
                "staged_samples": slot.staged_samples,
                "concealments": slot.concealments,
                "late_drops": slot.late_drops,
                "overflow_drops": slot.overflow_drops,
                "shrink_samples": slot.shrink_samples,
                "trim_events": slot.trim_events,
            })).collect::<Vec<_>>(),
        })
    }

    /// Stops the peer; a receiver's speaker samples go to `pcm` (f32 little-endian).
    async fn finish(self, pcm: &Path) -> anyhow::Result<Value> {
        let stats = self.stats();
        self.client.connection.request(Request::ChannelLeave {}).await?;
        drop(self.voice);
        let data = std::mem::take(&mut *self.shared.lock().unwrap());
        if !self.sender {
            let bytes: Vec<u8> = data.playout.iter().flat_map(|value| value.to_le_bytes()).collect();
            std::fs::write(pcm, bytes)?;
        }
        Ok(json!({
            "sender": self.sender,
            "session": self.client.welcome.session,
            "captures": data.captures.iter().map(|(ms, identity)| json!({"capture_ms": ms, "identity": identity})).collect::<Vec<_>>(),
            "blocks": data.blocks,
            "samples": data.playout.len(),
            "underflow_samples": data.underflows,
            "stats": stats,
        }))
    }
}

fn reply(value: Value) -> anyhow::Result<()> {
    println!("{value}");
    std::io::stdout().flush()?;
    Ok(())
}

struct DataDir(PathBuf);
impl Drop for DataDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: web_latency http://127.0.0.1:PORT OUTPUT_DIR";
    let origin = args.next().context(usage)?;
    let out = PathBuf::from(args.next().context(usage)?);
    let origin_url = url::Url::parse(&origin)?;
    anyhow::ensure!(
        origin_url.scheme() == "http" && origin_url.host_str() == Some("127.0.0.1"),
        "loopback origin required"
    );
    let net = std::env::var("LATENCY_NET").ok().map(|source| netem::Spec::parse(&source)).transpose()?;
    // The guard removes only a directory this run created.
    let data_path = out.join("server-data");
    std::fs::create_dir(&data_path).context("create the server data directory")?;
    let data = DataDir(data_path);
    let mut relay = if net.is_some() { Some(netem::Relay::bind_for_browsers().await?) } else { None };
    let any = "127.0.0.1:0".parse()?;
    let advertise = relay.as_ref().map(|relay| relay.address()).transpose()?.unwrap_or(any);
    let server = gwar_server::start(gwar_server::Config {
        database: Some(data.0.join("server.sqlite3")),
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
        data_dir: Some(data.0.clone()),
        public_ip: None,
        insecure_any_host: true,
        public_endpoints: vec![],
        client_ip_header: None,
        upload_limit: 0,
        upload_quota: 0,
        storage_limit: 0,
        files_dir: Some(data.0.join("files")),
        web_origins: vec![origin],
        tls: None,
        redirect_http: None,
        connect_url: None,
    })
    .await?;
    anyhow::ensure!(server.http.ip().is_loopback() && server.media.ip().is_loopback(), "loopback server required");
    let clock = Clock::new();
    if let (Some(relay), Some(spec)) = (&mut relay, &net) {
        relay.start(server.media_bind, spec.clone(), clock.instant);
    }
    let result = run(&server, &out, clock, &mut relay).await;
    for task in &server.tasks {
        task.abort();
    }
    for task in server.tasks {
        let _ = task.await;
    }
    drop(data);
    result
}

async fn run(
    server: &gwar_server::Running,
    out: &Path,
    clock: Clock,
    relay: &mut Option<netem::Relay>,
) -> anyhow::Result<()> {
    let url = format!("ws://{}/ws", server.http);
    reply(json!({
        "event": "ready", "url": url, "media": server.media, "mediaBind": server.media_bind,
        "adminToken": server.admin_token, "relayEpochMs": clock.unix_ms,
    }))?;
    let mut peer: Option<Peer> = None;
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let words: Vec<_> = line.split_whitespace().collect();
        match words.as_slice() {
            [role @ ("send" | "receive")] => {
                anyhow::ensure!(peer.is_none(), "a native peer is already running");
                let started = Peer::start(&url, server.media, *role == "send", clock).await?;
                reply(json!({"event": role, "session": started.client.welcome.session}))?;
                peer = Some(started);
            }
            ["link"] => {
                let link = peer.as_ref().map(|peer| format!("{:?}", peer.voice.stats().link));
                reply(json!({"event": "link", "link": link}))?;
            }
            ["arm", warmup_ms, count] => {
                let warmup = warmup_ms.parse::<usize>().context("warmup ms")? * RATE / 1000;
                let count = count.parse::<usize>().context("marker count")?;
                let peer = peer.as_ref().context("no native peer")?;
                peer.shared.lock().unwrap().arm = Some((warmup, count));
                reply(json!({"event": "arm", "atMs": clock.ms(Instant::now())}))?;
            }
            ["finish"] => {
                let native = match peer.take() {
                    Some(peer) => Some(peer.finish(&out.join("native-playout.f32")).await?),
                    None => None,
                };
                let network = match relay {
                    Some(relay) => Some(serde_json::to_value(relay.finish().await?)?),
                    None => None,
                };
                reply(json!({"event": "finish", "native": native, "network": network}))?;
            }
            ["quit"] => break,
            _ => anyhow::bail!("unknown command: {line}"),
        }
    }
    Ok(())
}
