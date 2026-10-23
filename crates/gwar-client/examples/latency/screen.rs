//! Screen load beside the voice measurement (docs/screen-share.md, "Voice headroom and
//! acceptance"): str0m frame-mode publishers send synthetic VP9-sized frames at a preset's rate
//! into real shares, and str0m viewers watch them, through the same server as the voice clients.
//!
//! `LATENCY_SCREEN=shares=1,bitrate=8mbit,fps=60,keyframe=200000,viewers=5,via=none`:
//! - `shares` publishers, each with its own sharer connection in the voice channel;
//! - `bitrate` per share, `fps` frames a second, `keyframe` bytes per keyframe (the first frame
//!   and each answer to the SFU's keyframe requests);
//! - `viewers` connections, each watching every share;
//! - `via`: which screen transports go through the `LATENCY_NET` relay (`publishers`, `viewers`,
//!   `all`) and which reach the server directly (`none`: all of them).
use anyhow::{Context, Result, bail, ensure};
use gwar_client::{ClientError, ConnectOptions, Connected, Identity, connect};
use gwar_proto::{
    ClientSoftware, ErrorCode, Platform, Request, ScreenPreset, ScreenPublishOffer, ScreenStart, ScreenSurface,
    ScreenWatchOffer,
};
use serde::Serialize;
use std::{
    collections::HashSet,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use str0m::{
    Candidate, Event, Input, Output, Rtc,
    bwe::Bitrate,
    change::{SdpAnswer, SdpOffer},
    format::Codec,
    media::{Direction, MediaKind, MediaTime, Mid},
    net::{Protocol, Receive},
};
use tokio::{net::UdpSocket, task::JoinHandle};

const MAX_SHARES: usize = 4;
const MAX_VIEWERS: usize = 13;
/// The server takes 16 connections from one address; the call has two.
const MAX_CONNECTIONS: usize = 14;
/// The steady rate counts from here: past the SFU's 10 s ramp of a new slot, and settled.
const STEADY_AFTER: Duration = Duration::from_secs(12);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    None,
    Publishers,
    Viewers,
    All,
}

#[derive(Clone, Debug, Serialize)]
pub struct Spec {
    pub source: String,
    pub shares: usize,
    pub bitrate: u64,
    pub fps: u32,
    pub keyframe: usize,
    pub viewers: usize,
    pub via: Via,
}

impl Spec {
    pub fn parse(source: &str) -> Result<Self> {
        let mut spec = Self {
            source: source.into(),
            shares: 1,
            bitrate: 8_000_000,
            fps: 60,
            keyframe: 200_000,
            viewers: 1,
            via: Via::None,
        };
        let mut keys = HashSet::new();
        for option in source.split(',').map(str::trim) {
            let (key, value) = option.split_once('=').context("expected key=value in LATENCY_SCREEN")?;
            ensure!(keys.insert(key), "duplicate LATENCY_SCREEN option: {key}");
            match key {
                "shares" => spec.shares = value.parse().context("shares must be a count")?,
                "bitrate" => {
                    let lower = value.to_ascii_lowercase();
                    let (number, scale) = match (lower.strip_suffix("mbit"), lower.strip_suffix("kbit")) {
                        (Some(n), _) => (n.to_owned(), 1e6),
                        (_, Some(n)) => (n.to_owned(), 1e3),
                        _ => (lower, 1.0),
                    };
                    spec.bitrate = (number.parse::<f64>().context("bitrate needs bits per second")? * scale) as u64;
                }
                "fps" => spec.fps = value.parse().context("fps must be a count")?,
                "keyframe" => spec.keyframe = value.parse().context("keyframe must be bytes")?,
                "viewers" => spec.viewers = value.parse().context("viewers must be a count")?,
                "via" => {
                    spec.via = match value {
                        "none" => Via::None,
                        "publishers" => Via::Publishers,
                        "viewers" => Via::Viewers,
                        "all" => Via::All,
                        _ => bail!("via must be none, publishers, viewers or all"),
                    }
                }
                _ => bail!("unknown LATENCY_SCREEN option: {key}"),
            }
        }
        ensure!((1..=MAX_SHARES).contains(&spec.shares), "shares must be within 1..{MAX_SHARES}");
        ensure!((1..=MAX_VIEWERS).contains(&spec.viewers), "viewers must be within 1..{MAX_VIEWERS}");
        ensure!(spec.shares + spec.viewers <= MAX_CONNECTIONS, "shares and viewers may be {MAX_CONNECTIONS} together");
        ensure!((100_000..=8_000_000).contains(&spec.bitrate), "bitrate must be within 100kbit..8mbit (Game's cap)");
        ensure!((1..=120).contains(&spec.fps), "fps must be within 1..120");
        ensure!((1_000..=2_000_000).contains(&spec.keyframe), "keyframe must be within 1000..2000000 bytes");
        Ok(spec)
    }
}

/// The encoder preset whose rate fits `bitrate` (docs/screen-share.md, "Presets").
/// The SFU's admission ceiling is server-wide; viewer demand uses measured media.
fn preset(bitrate: u64) -> ScreenPreset {
    match bitrate {
        ..=1_500_000 => ScreenPreset::Saver,
        1_500_001..=2_500_000 => ScreenPreset::Text,
        2_500_001..=4_000_000 => ScreenPreset::Smooth,
        _ => ScreenPreset::Game,
    }
}

/// What the load did, for the run's JSON.
#[derive(Default)]
struct Counters {
    frames: AtomicU64,
    keyframes: AtomicU64,
    keyframe_requests: AtomicU64,
    published_bytes: AtomicU64,
    received_bytes: AtomicU64,
    received_packets: AtomicU64,
    pauses: AtomicU64,
    resumes: AtomicU64,
    viewer_bytes: Vec<AtomicU64>,
    receivers: Vec<ReceiveScheduling>,
    transitions: std::sync::Mutex<Vec<serde_json::Value>>,
}

/// Per-viewer receive-loop timing, independent of the relay's queue residence.
#[derive(Default)]
struct ReceiveScheduling {
    datagrams: AtomicU64,
    wake_lateness_max_us: AtomicU64,
    wakes_over_5ms: AtomicU64,
    processing_max_us: AtomicU64,
    send_errors: AtomicU64,
}

impl ReceiveScheduling {
    fn wake(&self, due: Instant, now: Instant) {
        let late = now.saturating_duration_since(due).as_micros() as u64;
        self.wake_lateness_max_us.fetch_max(late, Ordering::Relaxed);
        if late > 5_000 {
            self.wakes_over_5ms.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn report(&self) -> serde_json::Value {
        serde_json::json!({
            "datagrams": self.datagrams.load(Ordering::Relaxed),
            "wake_lateness_max_ms": self.wake_lateness_max_us.load(Ordering::Relaxed) as f64 / 1000.0,
            "wakes_over_5ms": self.wakes_over_5ms.load(Ordering::Relaxed),
            "processing_max_ms": self.processing_max_us.load(Ordering::Relaxed) as f64 / 1000.0,
            "send_errors": self.send_errors.load(Ordering::Relaxed),
        })
    }
}

#[derive(Clone, Serialize)]
struct Sample {
    unix_s: f64,
    received_bytes: u64,
    viewer_bytes: Vec<u64>,
    receive_scheduling: Vec<serde_json::Value>,
}

fn unix_s() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64()
}

fn sample(counters: &Counters) -> Sample {
    Sample {
        unix_s: unix_s(),
        received_bytes: counters.received_bytes.load(Ordering::Relaxed),
        viewer_bytes: counters.viewer_bytes.iter().map(|n| n.load(Ordering::Relaxed)).collect(),
        receive_scheduling: counters.receivers.iter().map(ReceiveScheduling::report).collect(),
    }
}

/// The load has a separate runtime to avoid sharing task queues with voice and the server.
/// CPU contention can still delay receiving; per-viewer timing distinguishes that delay from
/// relay queue residence, since late reads also affect TWCC arrival times.
pub struct Load {
    spec: Spec,
    counters: Arc<Counters>,
    /// Received bytes when the viewers' slots are past their ramp, for the steady rate.
    samples: Arc<std::sync::Mutex<Vec<Sample>>>,
    running: Running,
    runtime: Option<tokio::runtime::Runtime>,
    started: Instant,
}

impl Drop for Load {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

/// What keeps the load going on its runtime: the str0m loops and the gwar connections.
struct Running {
    tasks: Vec<JoinHandle<Result<()>>>,
    _connections: Vec<Connected>,
}

fn options(url: &str, name: &str) -> ConnectOptions {
    ConnectOptions {
        url: url.into(),
        nickname: name.into(),
        invite: None,
        certificate: None,
        expected_server_key: None,
        trust_on_first_use: true,
        insecure_lan: false,
        server_password: None,
        identity: Identity::generate(),
        software: ClientSoftware { name: "Gwar screen load".into(), version: "1".into(), platform: Platform::Desktop },
    }
}

/// Connects, waiting out the server's hello limit for one address (a burst of 20, then one a
/// second, and a login takes two): the load opens up to 14 connections at once.
async fn join(url: &str, name: &str) -> Result<Connected> {
    for _ in 0..20 {
        match connect(options(url, name)).await {
            Err(ClientError::Server(e)) if e.code == ErrorCode::RateLimited => {
                tokio::time::sleep(Duration::from_millis(2500)).await;
            }
            connected => return Ok(connected?),
        }
    }
    bail!("{name}: the server kept refusing hellos")
}

async fn socket() -> Result<(Arc<UdpSocket>, SocketAddr)> {
    let socket = UdpSocket::bind("127.0.0.1:0").await.context("bind screen load socket")?;
    let local = socket.local_addr()?;
    Ok((Arc::new(socket), local))
}

/// The server's answer, with its media address swapped for `media` (the relay or the server).
fn answer(sdp: &str, advertised: SocketAddr, media: SocketAddr) -> Result<SdpAnswer> {
    let sdp =
        sdp.replace(&format!("{} {}", advertised.ip(), advertised.port()), &format!("{} {}", media.ip(), media.port()));
    SdpAnswer::from_sdp_string(&sdp).context("screen answer")
}

impl Load {
    /// Starts the shares and their viewers in `channel`. `advertised`: the media address the
    /// server's answers name; `relay` and `server`: where screen transports go, by `spec.via`.
    pub async fn start(
        url: &str,
        spec: Spec,
        channel: u32,
        advertised: SocketAddr,
        relay: Option<SocketAddr>,
        server: SocketAddr,
    ) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("screen-load")
            .enable_all()
            .build()?;
        let counters = Arc::new(Counters {
            viewer_bytes: (0..spec.viewers).map(|_| AtomicU64::new(0)).collect(),
            receivers: (0..spec.viewers).map(|_| ReceiveScheduling::default()).collect(),
            ..Counters::default()
        });
        let (url, load) = (url.to_owned(), spec.clone());
        let running = runtime.spawn(run(url, load, channel, advertised, relay, server, counters.clone()));
        let running = match running.await {
            Ok(Ok(running)) => running,
            failed => {
                // Dropping a runtime inside the caller's would panic.
                runtime.shutdown_background();
                return Err(failed.map_err(anyhow::Error::from).and_then(|r| r.map(drop)).unwrap_err());
            }
        };
        let samples = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (mark, received) = (samples.clone(), counters.clone());
        runtime.spawn(async move {
            tokio::time::sleep(STEADY_AFTER).await;
            loop {
                mark.lock().unwrap().push(sample(&received));
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        });
        Ok(Self { spec, counters, samples, running, runtime: Some(runtime), started: Instant::now() })
    }
}

async fn run(
    url: String,
    spec: Spec,
    channel: u32,
    advertised: SocketAddr,
    relay: Option<SocketAddr>,
    server: SocketAddr,
    counters: Arc<Counters>,
) -> Result<Running> {
    let url = url.as_str();
    let mut tasks = Vec::new();
    let mut connections = Vec::new();
    let route = |through: bool| if through { relay.unwrap_or(server) } else { server };
    let mut shares = Vec::new();
    for n in 0..spec.shares {
        let mut sharer = join(url, &format!("screen sharer {n}")).await?;
        sharer.connection.join(channel, None).await?;
        let state = sharer.connection.request(Request::VoiceState {}).await?;
        let binding = |key: &str| state["voice"][key].as_str().unwrap_or("AAAAAAAAAAAAAAAAAAAAAA").to_owned();
        let start = ScreenStart {
            call: binding("call"),
            participation: binding("participation"),
            preset: preset(spec.bitrate),
            surface: ScreenSurface::Screen,
            guest_link: false,
        };
        let share = sharer.connection.request(Request::ScreenStart(start)).await.context("screen.start")?;
        let share = share["share"].as_str().context("share id")?.to_owned();
        let through = matches!(spec.via, Via::Publishers | Via::All);
        tasks.push(publish(&sharer, &share, &spec, advertised, route(through), counters.clone()).await?);
        tasks.push(drain(&mut sharer, counters.clone(), None));
        connections.push(sharer);
        shares.push(share);
    }
    for n in 0..spec.viewers {
        let mut viewer = join(url, &format!("screen viewer {n}")).await?;
        let through = matches!(spec.via, Via::Viewers | Via::All);
        tasks.push(watch(&viewer, n, advertised, route(through), counters.clone()).await?);
        for share in &shares {
            watch_registered_share(|| viewer.connection.request(Request::ScreenWatch { share: share.clone() })).await?;
        }
        tasks.push(drain(&mut viewer, counters.clone(), Some(n)));
        connections.push(viewer);
    }
    Ok(Running { tasks, _connections: connections })
}

/// Publish negotiation can finish before the SFU registers the share, especially under loss.
/// Bound the whole operation, including a request that stops receiving replies.
async fn watch_registered_share<F, Fut>(mut request: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<serde_json::Value, ClientError>>,
{
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut backoff = Duration::from_millis(50);
        loop {
            match request().await {
                Ok(_) => return Ok(()),
                Err(ClientError::Server(error)) if error.code == ErrorCode::NotFound => {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_millis(500));
                }
                Err(error) => return Err(error),
            }
        }
    })
    .await
    .context("screen.watch: share not available within 10 seconds")?
    .context("screen.watch")
}

impl Load {
    pub async fn report(&mut self) -> Result<serde_json::Value> {
        if let Some(index) = self.running.tasks.iter().position(JoinHandle::is_finished) {
            self.running.tasks.swap_remove(index).await.context("screen load task panicked")??;
            bail!("screen load task stopped unexpectedly");
        }
        let c = &self.counters;
        let seconds = self.started.elapsed().as_secs_f64();
        let received = c.received_bytes.load(Ordering::Relaxed);
        let mut samples = self.samples.lock().unwrap().clone();
        if !samples.is_empty() {
            samples.push(sample(c));
        }
        let first = samples.first();
        let last = samples.last();
        let steady_rates: Vec<f64> = match (first, last) {
            (Some(a), Some(b)) if b.unix_s > a.unix_s => a
                .viewer_bytes
                .iter()
                .zip(&b.viewer_bytes)
                .map(|(a_bytes, b_bytes)| (b_bytes - a_bytes) as f64 * 8.0 / (b.unix_s - a.unix_s) / 1e6)
                .collect(),
            _ => Vec::new(),
        };
        let steady = (!steady_rates.is_empty()).then(|| steady_rates.iter().sum::<f64>() / self.spec.viewers as f64);
        Ok(serde_json::json!({
            "spec": self.spec,
            "seconds": seconds,
            "frames": c.frames.load(Ordering::Relaxed),
            "keyframes": c.keyframes.load(Ordering::Relaxed),
            "keyframe_requests": c.keyframe_requests.load(Ordering::Relaxed),
            "published_bytes": c.published_bytes.load(Ordering::Relaxed),
            "received_bytes": received,
            "received_packets": c.received_packets.load(Ordering::Relaxed),
            "received_mbps_per_viewer": received as f64 * 8.0 / seconds / 1e6 / self.spec.viewers as f64,
            "steady_after_s": STEADY_AFTER.as_secs(),
            "steady_mbps_per_viewer": steady,
            "steady_viewer_mbps": steady_rates,
            "settled_unix_s": first.map(|s| s.unix_s),
            "samples": samples,
            "pauses": c.pauses.load(Ordering::Relaxed),
            "resumes": c.resumes.load(Ordering::Relaxed),
            "receive_scheduling": c.receivers.iter().map(ReceiveScheduling::report).collect::<Vec<_>>(),
            "transitions": *c.transitions.lock().unwrap(),
        }))
    }
}

/// Reads a connection's events, so that they never back up, counting pauses and resumes.
fn drain(client: &mut Connected, counters: Arc<Counters>, viewer: Option<usize>) -> JoinHandle<Result<()>> {
    let mut events = std::mem::replace(&mut client.events, tokio::sync::mpsc::channel(1).1);
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            match event {
                gwar_proto::Event::ScreenPaused { share, reason, .. } => {
                    counters.pauses.fetch_add(1, Ordering::Relaxed);
                    counters.transitions.lock().unwrap().push(serde_json::json!({
                        "unix_s": unix_s(), "viewer": viewer,
                        "share": share, "event": "paused", "reason": reason,
                    }));
                }
                gwar_proto::Event::ScreenResumed { share, .. } => {
                    counters.resumes.fetch_add(1, Ordering::Relaxed);
                    counters.transitions.lock().unwrap().push(serde_json::json!({
                        "unix_s": unix_s(), "viewer": viewer,
                        "share": share, "event": "resumed",
                    }));
                }
                _ => {}
            };
        }
        bail!("screen control connection closed")
    })
}

/// A frame-mode VP9 sender. str0m's packetizer tells keyframes by the frame's first byte
/// (`0x80`: a keyframe's frame marker, profile 0 and show-frame; `0x84`: an inter frame).
async fn publish(
    sharer: &Connected,
    share: &str,
    spec: &Spec,
    advertised: SocketAddr,
    media: SocketAddr,
    counters: Arc<Counters>,
) -> Result<JoinHandle<Result<()>>> {
    let (socket, local) = socket().await?;
    // Paced like a browser's sender: str0m's pacer at the share's rate, adapting to TWCC.
    let mut rtc = Rtc::builder()
        .clear_codecs()
        .enable_vp9(true)
        .enable_bwe(Some(Bitrate::bps(spec.bitrate * 3 / 2)))
        .build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp").map_err(|e| anyhow::anyhow!("{e}"))?);
    let mut api = rtc.sdp_api();
    let mid = api.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
    let (offer, pending) = api.apply().context("publish offer")?;
    let sdp = offer.to_sdp_string();
    let group = sdp.lines().find_map(|l| l.strip_prefix("a=ssrc-group:FID ")).context("RTX group")?;
    let mut ssrcs = group.split_whitespace().map(|s| s.parse::<u32>());
    let (ssrc, rtx_ssrc) = (ssrcs.next().context("SSRC")??, ssrcs.next().context("RTX SSRC")??);
    let offer = ScreenPublishOffer { share: share.to_owned(), ssrc, rtx_ssrc, sdp };
    let reply = sharer.connection.request(Request::ScreenPublishOffer(offer)).await.context("screen.publish.offer")?;
    rtc.sdp_api().accept_answer(pending, answer(reply["sdp"].as_str().context("answer")?, advertised, media)?)?;
    rtc.bwe().set_desired_bitrate(Bitrate::bps(spec.bitrate * 3 / 2));
    let pt = rtc.codec_config().params().iter().find(|p| p.spec().codec == Codec::Vp9).context("VP9")?.pt();
    let (bitrate, fps, keyframe) = (spec.bitrate, spec.fps, spec.keyframe);
    Ok(tokio::spawn(sender(rtc, socket, local, mid, pt, bitrate, fps, keyframe, counters)))
}

#[allow(clippy::too_many_arguments)]
async fn sender(
    mut rtc: Rtc,
    socket: Arc<UdpSocket>,
    local: SocketAddr,
    mid: Mid,
    pt: str0m::media::Pt,
    bitrate: u64,
    fps: u32,
    keyframe: usize,
    counters: Arc<Counters>,
) -> Result<()> {
    let interval = Duration::from_secs(1) / fps;
    let delta = (bitrate / 8 / fps as u64) as usize;
    let mut next = tokio::time::Instant::now();
    let mut connected = false;
    let mut wants_key = true;
    let mut frame = 0u64;
    let mut buffer = vec![0; 2048];
    loop {
        let deadline = loop {
            match rtc.poll_output()? {
                Output::Timeout(at) => break at,
                Output::Transmit(t) => {
                    let _ = socket.try_send_to(&t.contents, t.destination);
                }
                Output::Event(Event::Connected) => connected = true,
                Output::Event(Event::KeyframeRequest(_)) => {
                    wants_key = true;
                    counters.keyframe_requests.fetch_add(1, Ordering::Relaxed);
                }
                Output::Event(_) => {}
            }
        };
        tokio::select! {
            got = socket.recv_from(&mut buffer) => {
                let (n, source) = got?;
                let Ok(contents) = buffer[..n].try_into() else { continue };
                rtc.handle_input(Input::Receive(Instant::now(), Receive { proto: Protocol::Udp, source, destination: local, contents }))?;
            }
            _ = tokio::time::sleep_until(next), if connected => {
                next += interval;
                let key = std::mem::take(&mut wants_key);
                let size = if key { keyframe } else { delta };
                let mut data = vec![0u8; size.max(16)];
                data[0] = if key { 0x80 } else { 0x84 };
                for (i, byte) in data.iter_mut().enumerate().skip(1) {
                    *byte = (i as u64).wrapping_mul(frame | 1) as u8;
                }
                let time = MediaTime::from_90khz(frame * 90_000 / fps as u64);
                if let Some(writer) = rtc.writer(mid) {
                    writer.write(pt, Instant::now(), time, data)?;
                }
                frame += 1;
                counters.frames.fetch_add(1, Ordering::Relaxed);
                counters.keyframes.fetch_add(u64::from(key), Ordering::Relaxed);
                counters.published_bytes.fetch_add(size as u64, Ordering::Relaxed);
            }
            _ = tokio::time::sleep_until(deadline.min(Instant::now() + Duration::from_millis(50)).into()) => {}
        }
        rtc.handle_input(Input::Timeout(Instant::now()))?;
    }
}

/// A viewer's watch transport with four receive-only slots, counting what arrives.
async fn watch(
    viewer: &Connected,
    viewer_index: usize,
    advertised: SocketAddr,
    media: SocketAddr,
    counters: Arc<Counters>,
) -> Result<JoinHandle<Result<()>>> {
    let (socket, local) = socket().await?;
    let mut rtc =
        Rtc::builder().set_rtp_mode(true).clear_codecs().enable_vp9(true).enable_vp8(true).build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp").map_err(|e| anyhow::anyhow!("{e}"))?);
    // str0m's random mids can collide within one offer: redraw.
    let (offer, pending) = loop {
        let mut api = rtc.sdp_api();
        let mids: HashSet<Mid> = (0..gwar_proto::SCREEN_SLOTS)
            .map(|_| api.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None))
            .collect();
        if mids.len() == gwar_proto::SCREEN_SLOTS {
            break api.apply().context("watch offer")?;
        }
    };
    let _: &SdpOffer = &offer;
    let request = ScreenWatchOffer { run: 1, sdp: offer.to_sdp_string(), browser: false };
    let reply = viewer.connection.request(Request::ScreenWatchOffer(request)).await.context("screen.watch.offer")?;
    rtc.sdp_api().accept_answer(pending, answer(reply["sdp"].as_str().context("answer")?, advertised, media)?)?;
    let (ready, connected) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(receiver(rtc, socket, local, counters, viewer_index, ready));
    tokio::time::timeout(Duration::from_secs(10), connected).await.context("watch transport connects")??;
    Ok(task)
}

async fn receiver(
    mut rtc: Rtc,
    socket: Arc<UdpSocket>,
    local: SocketAddr,
    counters: Arc<Counters>,
    viewer_index: usize,
    ready: tokio::sync::oneshot::Sender<()>,
) -> Result<()> {
    let mut ready = Some(ready);
    let mut buffer = vec![0; 2048];
    let timing = &counters.receivers[viewer_index];
    let mut processing = Instant::now();
    let heartbeat = Duration::from_millis(5);
    let mut check = processing + heartbeat;
    loop {
        let deadline = loop {
            match rtc.poll_output()? {
                Output::Timeout(at) => break at,
                Output::Transmit(t) => {
                    if socket.try_send_to(&t.contents, t.destination).is_err() {
                        timing.send_errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Output::Event(Event::Connected) => {
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(());
                    }
                }
                Output::Event(Event::RtpPacket(p)) => {
                    counters.viewer_bytes[viewer_index].fetch_add(p.payload.len() as u64, Ordering::Relaxed);
                    counters.received_packets.fetch_add(1, Ordering::Relaxed);
                    counters.received_bytes.fetch_add(p.payload.len() as u64, Ordering::Relaxed);
                }
                Output::Event(_) => {}
            }
        };
        timing.processing_max_us.fetch_max(processing.elapsed().as_micros() as u64, Ordering::Relaxed);
        let wake = deadline.min(check).max(Instant::now());
        let got = tokio::select! {
            got = socket.recv_from(&mut buffer) => Some(got?),
            _ = tokio::time::sleep_until(wake.into()) => None,
        };
        processing = Instant::now();
        timing.wake(wake, processing);
        if processing >= check {
            check = processing + heartbeat;
        }
        if let Some((n, source)) = got {
            timing.datagrams.fetch_add(1, Ordering::Relaxed);
            if let Ok(contents) = buffer[..n].try_into() {
                rtc.handle_input(Input::Receive(
                    processing,
                    Receive { proto: Protocol::Udp, source, destination: local, contents },
                ))?;
            }
        }
        rtc.handle_input(Input::Timeout(Instant::now()))?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn not_found() -> ClientError {
        ClientError::Server(gwar_proto::ErrorBody::new(ErrorCode::NotFound, "no such share"))
    }

    #[tokio::test(start_paused = true)]
    async fn watch_waits_for_share_registration() {
        let started = tokio::time::Instant::now();
        let attempts = std::cell::Cell::new(0);
        watch_registered_share(|| {
            attempts.set(attempts.get() + 1);
            std::future::ready(if started.elapsed() < Duration::from_millis(700) {
                Err(not_found())
            } else {
                Ok(serde_json::json!({}))
            })
        })
        .await
        .unwrap();
        assert_eq!(attempts.get(), 5);
        assert_eq!(started.elapsed(), Duration::from_millis(750));
    }

    #[tokio::test(start_paused = true)]
    async fn watch_does_not_retry_other_errors() {
        for failure in [
            ClientError::Server(gwar_proto::ErrorBody::new(ErrorCode::Forbidden, "not allowed")),
            ClientError::Closed,
            ClientError::Timeout,
        ] {
            let started = tokio::time::Instant::now();
            let mut failure = Some(failure);
            let error = watch_registered_share(|| std::future::ready(Err(failure.take().expect("no retry"))))
                .await
                .unwrap_err();
            assert!(error.downcast_ref::<ClientError>().is_some());
            assert_eq!(started.elapsed(), Duration::ZERO);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn watch_registration_and_unanswered_requests_share_a_ten_second_deadline() {
        let started = tokio::time::Instant::now();
        let error = watch_registered_share(|| std::future::ready(Err(not_found()))).await.unwrap_err();
        assert!(error.to_string().contains("within 10 seconds"));
        assert_eq!(started.elapsed(), Duration::from_secs(10));

        let started = tokio::time::Instant::now();
        let error = watch_registered_share(std::future::pending).await.unwrap_err();
        assert!(error.to_string().contains("within 10 seconds"));
        assert_eq!(started.elapsed(), Duration::from_secs(10));
    }

    #[tokio::test]
    async fn load_task_failures_propagate_to_the_run() {
        let task = tokio::spawn(async { anyhow::bail!("injected publisher failure") });
        while !task.is_finished() {
            tokio::task::yield_now().await;
        }
        let mut load = Load {
            spec: Spec::parse("shares=1,viewers=1").unwrap(),
            counters: Arc::new(Counters::default()),
            samples: Arc::new(std::sync::Mutex::new(Vec::new())),
            running: Running { tasks: vec![task], _connections: Vec::new() },
            runtime: None,
            started: Instant::now(),
        };
        assert!(load.report().await.unwrap_err().to_string().contains("injected publisher failure"));
    }

    #[test]
    fn specs_parse_strictly() {
        let spec = Spec::parse("shares=2,bitrate=4mbit,fps=30,keyframe=150000,viewers=1,via=viewers").unwrap();
        assert_eq!((spec.shares, spec.bitrate, spec.fps, spec.keyframe, spec.viewers), (2, 4_000_000, 30, 150_000, 1));
        assert_eq!(spec.via, Via::Viewers);
        for invalid in [
            "",
            "shares=0",
            "shares=5",
            "bitrate=1",
            "bitrate=9mbit",
            "via=up",
            "fps=1,fps=2",
            "colour=red",
            "viewers=14",
            "shares=4,viewers=11",
        ] {
            assert!(Spec::parse(invalid).is_err(), "accepted {invalid}");
        }
    }
}
