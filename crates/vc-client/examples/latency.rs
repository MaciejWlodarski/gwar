//! Synthetic mouth-to-ear latency through two native clients and a Gwar server.
//! Uses virtual audio devices; no microphone or speaker device is opened.
//!   cargo run --release -p vc-client --example latency -- output.csv [marker-count]
//! Marker count defaults to 100. Writes output.csv and output.csv.json.
//! Set both LATENCY_URL (ws(s)://host:port/ws) and LATENCY_SERVER (IP:media-port)
//! to use an existing server; otherwise starts a fresh loopback-only server.
//! Exits with an error if more than 5% of markers are unmatched.
use anyhow::Context;
use rtrb::RingBuffer;
use std::{
    io::Write,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use vc_client::{
    ConnectOptions, Identity, connect,
    voice::{
        self,
        io::{AudioIo, AudioPorts},
    },
};
use vc_proto::{ClientSoftware, Platform};

const RATE: usize = 48_000;
const BLOCK: usize = 480;
const PERIOD: usize = 12_000; // 250 ms
const WARMUP: usize = RATE * 2;
const MARKER: usize = 960; // 20 ms

#[derive(Default)]
struct Measurements {
    captures: Vec<f64>,
    samples: Vec<(f64, f32)>,
    underflows: usize,
}
struct FakeIo {
    data: Arc<Mutex<Measurements>>,
    tone: bool,
    epoch: Instant,
    stop: Arc<AtomicBool>,
    count: usize,
}
struct Guard {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wait for the last callback before taking the measurement snapshot.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
impl AudioIo for FakeIo {
    fn open(self: Box<Self>) -> anyhow::Result<AudioPorts> {
        let (mut mic_tx, mic) = RingBuffer::new(RATE);
        let (speaker, mut speaker_rx) = RingBuffer::new(RATE / 2);
        let stop = self.stop.clone();
        let thread_stop = stop.clone();
        let thread = thread::Builder::new().name("latency-audio".into()).spawn(move || {
            // Both virtual devices share the same 10 ms callback phase. This
            // avoids attributing random sender/receiver device phase to SFU cost.
            let start = self.epoch + Duration::from_millis(200);
            if let Some(wait) = start.checked_duration_since(Instant::now()) {
                thread::sleep(wait);
            }
            let mut position = 0usize;
            while !thread_stop.load(Ordering::Relaxed) {
                let now = Instant::now();
                let mut data = self.data.lock().unwrap();
                for i in 0..BLOCK {
                    let at = position + i;
                    let in_marker =
                        at >= WARMUP && (at - WARMUP) / PERIOD < self.count && (at - WARMUP) % PERIOD < MARKER;
                    if self.tone
                        && at >= WARMUP
                        && (at - WARMUP).is_multiple_of(PERIOD)
                        && data.captures.len() < self.count
                    {
                        data.captures.push((now - self.epoch).as_secs_f64() + i as f64 / RATE as f64);
                    }
                    let frequency = if in_marker { 1800.0 } else { 400.0 };
                    let amplitude = if in_marker { 0.6 } else { 0.08 };
                    let value = if self.tone {
                        (at as f64 * frequency * std::f64::consts::TAU / RATE as f64).sin() as f32 * amplitude
                    } else {
                        0.0
                    };
                    if mic_tx.push(value).is_err() {
                        return;
                    }
                    let value = speaker_rx.pop().unwrap_or_else(|_| {
                        data.underflows += 1;
                        0.0
                    });
                    if !self.tone {
                        data.samples.push(((now - self.epoch).as_secs_f64() + i as f64 / RATE as f64, value));
                    }
                }
                drop(data);
                position += BLOCK;
                let due = start + Duration::from_secs_f64(position as f64 / RATE as f64);
                if let Some(wait) = due.checked_duration_since(Instant::now()) {
                    thread::sleep(wait);
                }
            }
        })?;
        Ok(AudioPorts { mic, speaker, _guard: Box::new(Guard { stop, thread: Some(thread) }) })
    }
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
        software: ClientSoftware { name: "Gwar latency".into(), version: "1".into(), platform: Platform::Desktop },
    }
}
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let output = args.next().context("usage: latency output.csv [marker-count]")?;
    let count: usize = args.next().map(|s| s.parse()).transpose().context("invalid marker count")?.unwrap_or(100);
    anyhow::ensure!(args.next().is_none(), "usage: latency output.csv [marker-count]");
    anyhow::ensure!(count > 0, "marker count must be positive");
    let remote = match (std::env::var("LATENCY_URL"), std::env::var("LATENCY_SERVER")) {
        (Ok(url), Ok(addr)) => Some((url, addr.parse().context("LATENCY_SERVER must be IP:port")?)),
        (Err(std::env::VarError::NotPresent), Err(std::env::VarError::NotPresent)) => None,
        _ => anyhow::bail!("set both LATENCY_URL and LATENCY_SERVER, or neither"),
    };
    let mut tasks = ServerTasks { tasks: Vec::new(), data_dir: None };
    let result = tokio::select! {
        result = measure(&output, count, remote, &mut tasks) => result,
        signal = shutdown_signal() => {
            signal?;
            Err(anyhow::anyhow!("measurement interrupted"))
        }
    };
    tasks.shutdown().await;
    result
}

async fn shutdown_signal() -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}

struct ServerTasks {
    tasks: Vec<tokio::task::JoinHandle<()>>,
    data_dir: Option<DataDir>,
}
impl ServerTasks {
    async fn shutdown(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }
        self.data_dir.take();
    }
}
impl Drop for ServerTasks {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

struct DataDir(PathBuf);
impl Drop for DataDir {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            eprintln!("could not remove temporary data directory {}: {error}", self.0.display());
        }
    }
}

async fn measure(
    output: &str,
    count: usize,
    remote: Option<(String, std::net::SocketAddr)>,
    tasks: &mut ServerTasks,
) -> anyhow::Result<()> {
    if let Some((url, addr)) = remote {
        return run(output, count, &url, addr).await;
    }
    // Keep transient server files beside the output, and never reuse state.
    let data_path = PathBuf::from(format!("{output}.data-{}", std::process::id()));
    std::fs::create_dir(&data_path).context("create temporary server data directory")?;
    let data = DataDir(data_path);
    let any = "127.0.0.1:0".parse().unwrap();
    let server = vc_server::start(vc_server::Config {
        database: Some(data.0.join("server.sqlite3")),
        http_bind: any,
        media_bind: any,
        media_advertise: any,
        max_clients: 8,
        server_password: None,
        web_root: None,
        ice_servers: vec![],
        teamspeak: None,
        public_url: None,
        data_dir: Some(data.0.clone()),
        public_ip: None,
        insecure_any_host: true,
        public_endpoints: Vec::new(),
        client_ip_header: None,
        upload_limit: 10 * 1024 * 1024,
        upload_quota: 0,
        storage_limit: 0,
        files_dir: Some(data.0.join("files")),
        web_origins: Vec::new(),
        tls: None,
        redirect_http: None,
        connect_url: None,
    })
    .await
    .context("start local latency server")?;

    assert!(server.http.ip().is_loopback() && server.media.ip().is_loopback());
    println!("local server http={} media={}", server.http, server.media);
    let url = format!("ws://{}/ws", server.http);
    tasks.tasks = server.tasks;
    tasks.data_dir = Some(data);
    let result = run(output, count, &url, server.media).await;
    drop(server.core);
    drop(server.plane);
    tasks.shutdown().await;
    result
}

async fn run(output: &str, count: usize, url: &str, server_addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let url = url.to_string();
    let a = connect(options(&url, "latency sender")).await?;
    let mut b = connect(options(&url, "latency receiver")).await?;
    for client in [&a, &b] {
        client.connection.join(client.welcome.server.default_channel, None).await?;
    }
    let epoch = Instant::now();
    let sent = Arc::new(Mutex::new(Measurements::default()));
    let heard = Arc::new(Mutex::new(Measurements::default()));
    let va = voice::start(
        &a.connection,
        Box::new(FakeIo { data: sent.clone(), tone: true, epoch, stop: Arc::new(AtomicBool::new(false)), count }),
        server_addr,
    )
    .await?;
    let vb = voice::start(
        &b.connection,
        Box::new(FakeIo { data: heard.clone(), tone: false, epoch, stop: Arc::new(AtomicBool::new(false)), count }),
        server_addr,
    )
    .await?;
    tokio::time::sleep(Duration::from_secs_f64(3.0 + count as f64 * 0.25)).await;
    let receiver_stats = vb.stats();
    println!("sender={:?} receiver={receiver_stats:?}", va.stats());
    let mut voice_events = Vec::new();
    while let Ok(event) = b.events.try_recv() {
        match event {
            vc_proto::Event::VoiceSlot { slot, client } => voice_events.push(
                serde_json::json!({"type":"slot","slot":slot,"speaker_is_sender":client==Some(a.welcome.session)}),
            ),
            vc_proto::Event::VoiceTalking { client, talking } => voice_events.push(
                serde_json::json!({"type":"talking","speaker_is_sender":client==a.welcome.session,"talking":talking}),
            ),
            _ => {}
        }
    }
    drop((va, vb));
    let sent = sent.lock().unwrap();
    let heard = heard.lock().unwrap();
    // A trailing 5 ms RMS window detects the high-amplitude marker. The
    // detector's small onset delay is identical in both builds (no correction).
    let window = 240;
    let mut square = 0.0f64;
    let mut high = false;
    let mut onsets = Vec::new();
    for i in 0..heard.samples.len() {
        square += f64::from(heard.samples[i].1).powi(2);
        if i >= window {
            square -= f64::from(heard.samples[i - window].1).powi(2);
        }
        let rms = (square.max(0.0) / window as f64).sqrt();
        if !high && rms > 0.18 {
            onsets.push((heard.samples[i].0, i));
            high = true;
        }
        if high && rms < 0.11 {
            high = false;
        }
    }
    anyhow::ensure!(sent.captures.len() == count, "only {} captures", sent.captures.len());
    // Each marker is matched to the first onset within 250 ms (one marker
    // period) after its capture; a marker lost on the network has none and
    // is counted as lost instead of shifting every later match.
    let mut csv = std::fs::File::create(output)?;
    writeln!(csv, "marker,capture_s,playout_s,latency_ms,playout_sample")?;
    let mut latencies = Vec::new();
    let mut lost = Vec::new();
    for (i, &capture) in sent.captures.iter().enumerate() {
        let found = onsets.iter().find(|&&(playout, _)| (0.0..0.25).contains(&(playout - capture)));
        match found {
            Some(&(playout, sample)) => {
                let latency = (playout - capture) * 1000.0;
                writeln!(csv, "{i},{capture:.9},{playout:.9},{latency:.6},{sample}")?;
                latencies.push(latency);
            }
            None => lost.push(i),
        }
    }
    let marker_count = count;
    let count = latencies.len();
    latencies.sort_by(f64::total_cmp);
    let percentile = |q: f64| latencies.get(((q * count as f64).ceil() as usize).saturating_sub(1)).copied();
    println!(
        "n={count} median={:.3} p95={:.3} p99={:.3} ms underflow_samples={}",
        percentile(0.5).unwrap_or(f64::NAN),
        percentile(0.95).unwrap_or(f64::NAN),
        percentile(0.99).unwrap_or(f64::NAN),
        heard.underflows
    );
    let metadata = serde_json::json!({
        "voice_events": voice_events,
        "markers": marker_count,
        "n": count,
        "lost_markers": lost,
        "onsets": onsets.len(),
        "median_ms": percentile(0.5),
        "p95_ms": percentile(0.95),
        "p99_ms": percentile(0.99),
        "receiver_underflow_samples": heard.underflows,
        "receiver_frames_sent": receiver_stats.frames_sent,
        "receiver_frames_received": receiver_stats.frames_received,
        "url": url,
        "server": server_addr.to_string(),
        "sample_rate": RATE,
        "callback_samples": BLOCK,
        "rms_window_samples": window,
        "rms_on_threshold": 0.18,
        "rms_off_threshold": 0.11,
        "marker_samples": MARKER,
        "marker_interval_samples": PERIOD,
        "warmup_samples": WARMUP,
    });
    std::fs::write(format!("{output}.json"), serde_json::to_string_pretty(&metadata)?)?;
    anyhow::ensure!(
        lost.len() <= marker_count / 20,
        "lost {} of {marker_count} markers ({:.1}%); at least 95% must match; see {output}.json",
        lost.len(),
        lost.len() as f64 * 100.0 / marker_count as f64
    );
    anyhow::ensure!(
        voice_events.iter().any(|e| e["type"] == "slot" && e["speaker_is_sender"] == true),
        "no sender slot; see {output}.json"
    );
    anyhow::ensure!(
        voice_events.iter().any(|e| e["type"] == "talking" && e["speaker_is_sender"] == true && e["talking"] == true),
        "no sender talking indicator; see {output}.json"
    );
    Ok(())
}
