//! Synthetic mouth-to-ear latency through two native clients and a Gwar server.
//! Uses virtual audio devices; no microphone or speaker device is opened.
//!   cargo run --release -p vc-client --example latency -- output.csv [marker-count]
//! Marker count defaults to 100. Writes output.csv and output.csv.json.
//! Set both LATENCY_URL (ws(s)://host:port/ws) and LATENCY_SERVER (IP:media-port)
//! to use an existing server; otherwise starts a fresh loopback-only server.
//! LATENCY_MIN_MS defaults to 5; below-floor matches fail the run.
//! LATENCY_EXTRA_DELAY_MS adds a sample delay line to the fake output device.
//! Exits with an error if more than 5% of markers are unmatched.
use anyhow::Context;
use rtrb::RingBuffer;
use serde::Serialize;
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
        io::{AudioIo, AudioPorts, OutputTiming},
    },
};
use vc_proto::{ClientSoftware, Platform};

const RATE: usize = 48_000;
const BLOCK: usize = 480;
const PERIOD: usize = 12_000; // 250 ms
const WARMUP: usize = RATE * 2;
const MARKER: usize = 960; // 20 ms
// At least 200 Hz apart, and 100 Hz away from the 400 Hz carrier's harmonics.
const TONES: [f64; 8] = [900.0, 1100.0, 1300.0, 1500.0, 1700.0, 1900.0, 2300.0, 2700.0];
const MATCH_WINDOW_MS: f64 = TONES.len() as f64 * PERIOD as f64 * 1000.0 / RATE as f64;
const RMS_WINDOW: usize = 240; // 5 ms
const IDENTITY_WINDOW: usize = 480; // 10 ms after the detected onset
const MIN_IDENTITY_CONFIDENCE: f64 = 0.8;
const MIN_TONE_ENERGY_FRACTION: f64 = 0.6;

#[derive(Clone, Copy)]
struct Settings {
    minimum_ms: f64,
    extra_delay_ms: usize,
    /// Receiver callbacks cycle through 10/20/5 ms (`LATENCY_CALLBACK_JITTER=1`).
    callback_jitter: bool,
}
impl Settings {
    fn from_env() -> anyhow::Result<Self> {
        let minimum_ms = std::env::var("LATENCY_MIN_MS")
            .ok()
            .map(|value| value.parse::<f64>())
            .transpose()
            .context("LATENCY_MIN_MS must be a number")?
            .unwrap_or(5.0);
        anyhow::ensure!(
            minimum_ms.is_finite() && minimum_ms > 0.0 && minimum_ms < MATCH_WINDOW_MS,
            "LATENCY_MIN_MS must be positive and below {MATCH_WINDOW_MS}"
        );
        let extra_delay_ms = std::env::var("LATENCY_EXTRA_DELAY_MS")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()
            .context("LATENCY_EXTRA_DELAY_MS must be a nonnegative integer")?
            .unwrap_or(0);
        anyhow::ensure!(extra_delay_ms < MATCH_WINDOW_MS as usize, "extra delay must be below {MATCH_WINDOW_MS} ms");
        let callback_jitter = match std::env::var("LATENCY_CALLBACK_JITTER") {
            Ok(value) if value == "1" => true,
            Ok(value) if value == "0" => false,
            Err(std::env::VarError::NotPresent) => false,
            _ => anyhow::bail!("LATENCY_CALLBACK_JITTER must be 0 or 1"),
        };
        Ok(Self { minimum_ms, extra_delay_ms, callback_jitter })
    }
}

struct DelayLine {
    samples: Vec<f32>,
    position: usize,
}
impl DelayLine {
    fn new(samples: usize) -> Self {
        Self { samples: vec![0.0; samples], position: 0 }
    }

    fn process(&mut self, value: f32) -> f32 {
        if self.samples.is_empty() {
            return value;
        }
        let output = std::mem::replace(&mut self.samples[self.position], value);
        self.position = (self.position + 1) % self.samples.len();
        output
    }
}

#[derive(Clone, Copy, Serialize)]
struct Marker {
    capture_s: f64,
    identity: usize,
}
#[derive(Serialize)]
struct Onset {
    playout_s: f64,
    sample: usize,
    identity: Option<usize>,
    identity_confidence: f64,
    tone_energy_fraction: f64,
}
struct Match {
    marker: usize,
    onset: usize,
    latency_ms: f64,
}
#[derive(Serialize)]
struct RejectedOnset {
    marker: usize,
    onset: usize,
    latency_ms: f64,
    reason: &'static str,
}
struct Matching {
    matched: Vec<Match>,
    lost: Vec<usize>,
    rejected: Vec<RejectedOnset>,
    unused: Vec<usize>,
}

fn match_markers(markers: &[Marker], onsets: &[Onset], minimum_ms: f64, window_ms: f64) -> Matching {
    let mut used = vec![false; onsets.len()];
    let mut result = Matching { matched: Vec::new(), lost: Vec::new(), rejected: Vec::new(), unused: Vec::new() };
    for (marker, capture) in markers.iter().enumerate() {
        let found = onsets.iter().enumerate().find(|&(index, onset)| {
            !used[index]
                && onset.identity == Some(capture.identity)
                && (0.0..window_ms).contains(&((onset.playout_s - capture.capture_s) * 1000.0))
        });
        if let Some((onset, detected)) = found {
            // Consume even a rejected onset, so it can never measure another marker.
            used[onset] = true;
            let latency_ms = (detected.playout_s - capture.capture_s) * 1000.0;
            if latency_ms < minimum_ms {
                result.rejected.push(RejectedOnset { marker, onset, latency_ms, reason: "below_minimum_latency" });
                result.lost.push(marker);
            } else {
                result.matched.push(Match { marker, onset, latency_ms });
            }
        } else {
            result.lost.push(marker);
        }
    }
    result.unused = used.iter().enumerate().filter_map(|(i, &used)| (!used).then_some(i)).collect();
    result
}

fn identify_tone(samples: &[(f64, f32)]) -> (Option<usize>, f64, f64) {
    if samples.len() != IDENTITY_WINDOW {
        return (None, 0.0, 0.0);
    }
    // Hann-weighted Goertzel bins reject leakage from the carrier and nearby tones.
    let weights: Vec<_> = (0..samples.len())
        .map(|i| 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / (samples.len() - 1) as f64).cos())
        .collect();
    let energy: f64 = samples.iter().zip(&weights).map(|((_, value), weight)| f64::from(*value).powi(2) * weight).sum();
    let powers: Vec<_> = TONES
        .iter()
        .map(|frequency| {
            let coefficient = 2.0 * (std::f64::consts::TAU * frequency / RATE as f64).cos();
            let (mut previous, mut before_previous) = (0.0, 0.0);
            for ((_, value), weight) in samples.iter().zip(&weights) {
                let current = f64::from(*value) * weight + coefficient * previous - before_previous;
                before_previous = previous;
                previous = current;
            }
            (previous.powi(2) + before_previous.powi(2) - coefficient * previous * before_previous).max(0.0)
        })
        .collect();
    let identity = (0..TONES.len()).max_by(|&a, &b| powers[a].total_cmp(&powers[b])).unwrap();
    let total: f64 = powers.iter().sum();
    let confidence = if total > 0.0 { powers[identity] / total } else { 0.0 };
    let weighted_energy = energy * weights.iter().sum::<f64>();
    let fraction = if weighted_energy > 0.0 { 2.0 * powers[identity] / weighted_energy } else { 0.0 };
    let identified = confidence >= MIN_IDENTITY_CONFIDENCE && fraction >= MIN_TONE_ENERGY_FRACTION;
    (identified.then_some(identity), confidence, fraction)
}

fn detect_onsets(samples: &[(f64, f32)]) -> Vec<Onset> {
    let mut square = 0.0f64;
    let mut high = false;
    let mut onsets = Vec::new();
    for i in 0..samples.len() {
        square += f64::from(samples[i].1).powi(2);
        if i >= RMS_WINDOW {
            square -= f64::from(samples[i - RMS_WINDOW].1).powi(2);
        }
        let rms = (square.max(0.0) / RMS_WINDOW as f64).sqrt();
        if !high && rms > 0.18 {
            let end = (i + IDENTITY_WINDOW).min(samples.len());
            let (identity, identity_confidence, tone_energy_fraction) = identify_tone(&samples[i..end]);
            onsets.push(Onset {
                playout_s: samples[i].0,
                sample: i,
                identity,
                identity_confidence,
                tone_energy_fraction,
            });
            high = true;
        }
        if high && rms < 0.11 {
            high = false;
        }
    }
    onsets
}

#[derive(Default)]
struct Measurements {
    captures: Vec<Marker>,
    samples: Vec<(f64, f32)>,
    underflows: usize,
}
struct FakeIo {
    data: Arc<Mutex<Measurements>>,
    tone: bool,
    epoch: Instant,
    stop: Arc<AtomicBool>,
    count: usize,
    delay_samples: usize,
    callback_jitter: bool,
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
        let output = Arc::new(OutputTiming::default());
        let timing = output.clone();
        let stop = self.stop.clone();
        let thread_stop = stop.clone();
        let thread = thread::Builder::new().name("latency-audio".into()).spawn(move || {
            // Both devices start on the same phase; only the receiver varies its
            // block sizes when callback jitter is enabled.
            let start = self.epoch + Duration::from_millis(200);
            if let Some(wait) = start.checked_duration_since(Instant::now()) {
                thread::sleep(wait);
            }
            let mut position = 0usize;
            // Delay decoded audio, including silence and carrier, before playout.
            // Capture/playout timestamps always remain the actual callback times.
            let mut delay = DelayLine::new(self.delay_samples);
            let mut callback = 0;
            while !thread_stop.load(Ordering::Relaxed) {
                let block = if self.callback_jitter { [480, 960, 240][callback % 3] } else { BLOCK };
                let mut missing = 0;
                let now = Instant::now();
                let mut data = self.data.lock().unwrap();
                for i in 0..block {
                    let at = position + i;
                    let in_marker =
                        at >= WARMUP && (at - WARMUP) / PERIOD < self.count && (at - WARMUP) % PERIOD < MARKER;
                    if self.tone
                        && at >= WARMUP
                        && (at - WARMUP).is_multiple_of(PERIOD)
                        && data.captures.len() < self.count
                    {
                        data.captures.push(Marker {
                            capture_s: (now - self.epoch).as_secs_f64() + i as f64 / RATE as f64,
                            identity: (at - WARMUP) / PERIOD % TONES.len(),
                        });
                    }
                    let frequency = if in_marker { TONES[(at - WARMUP) / PERIOD % TONES.len()] } else { 400.0 };
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
                        missing += 1;
                        0.0
                    });
                    if !self.tone {
                        data.samples
                            .push(((now - self.epoch).as_secs_f64() + i as f64 / RATE as f64, delay.process(value)));
                    }
                }
                drop(data);
                timing.report(block, missing);
                callback += 1;
                position += block;
                let due = start + Duration::from_secs_f64(position as f64 / RATE as f64);
                if let Some(wait) = due.checked_duration_since(Instant::now()) {
                    thread::sleep(wait);
                }
            }
        })?;
        Ok(AudioPorts { mic, speaker, output, _guard: Box::new(Guard { stop, thread: Some(thread) }) })
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
    let settings = Settings::from_env()?;
    let mut tasks = ServerTasks { tasks: Vec::new(), data_dir: None };
    let result = tokio::select! {
        result = measure(&output, count, remote, &mut tasks, settings) => result,
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
    settings: Settings,
) -> anyhow::Result<()> {
    if let Some((url, addr)) = remote {
        return run(output, count, &url, addr, settings).await;
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
    let result = run(output, count, &url, server.media, settings).await;
    drop(server.core);
    drop(server.plane);
    tasks.shutdown().await;
    result
}

async fn run(
    output: &str,
    count: usize,
    url: &str,
    server_addr: std::net::SocketAddr,
    settings: Settings,
) -> anyhow::Result<()> {
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
        Box::new(FakeIo {
            data: sent.clone(),
            tone: true,
            epoch,
            stop: Arc::new(AtomicBool::new(false)),
            count,
            delay_samples: 0,
            callback_jitter: false,
        }),
        server_addr,
    )
    .await?;
    let vb = voice::start(
        &b.connection,
        Box::new(FakeIo {
            data: heard.clone(),
            tone: false,
            epoch,
            stop: Arc::new(AtomicBool::new(false)),
            count,
            delay_samples: settings.extra_delay_ms * RATE / 1000,
            callback_jitter: settings.callback_jitter,
        }),
        server_addr,
    )
    .await?;
    tokio::time::sleep(Duration::from_secs_f64(
        3.0 + count as f64 * PERIOD as f64 / RATE as f64 + MATCH_WINDOW_MS / 1000.0,
    ))
    .await;
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
    let onsets = detect_onsets(&heard.samples);
    anyhow::ensure!(sent.captures.len() == count, "only {} captures", sent.captures.len());
    let matching = match_markers(&sent.captures, &onsets, settings.minimum_ms, MATCH_WINDOW_MS);
    let mut csv = std::fs::File::create(output)?;
    writeln!(csv, "marker,identity,capture_s,playout_s,latency_ms,playout_sample")?;
    let mut latencies = Vec::new();
    for found in &matching.matched {
        let Marker { capture_s, identity } = sent.captures[found.marker];
        let onset = &onsets[found.onset];
        writeln!(
            csv,
            "{},{identity},{capture_s:.9},{:.9},{:.6},{}",
            found.marker, onset.playout_s, found.latency_ms, onset.sample
        )?;
        latencies.push(found.latency_ms);
    }
    let lost = &matching.lost;
    let ambiguous: Vec<_> =
        onsets.iter().enumerate().filter_map(|(i, onset)| onset.identity.is_none().then_some(i)).collect();
    let marker_count = count;
    let count = latencies.len();
    latencies.sort_by(f64::total_cmp);
    let percentile = |q: f64| latencies.get(((q * count as f64).ceil() as usize).saturating_sub(1)).copied();
    println!(
        "n={count} median={:.3} p95={:.3} p99={:.3} ms underflow_samples={} ambiguous_onsets={} rejected_onsets={}",
        percentile(0.5).unwrap_or(f64::NAN),
        percentile(0.95).unwrap_or(f64::NAN),
        percentile(0.99).unwrap_or(f64::NAN),
        heard.underflows,
        ambiguous.len(),
        matching.rejected.len()
    );
    let slots = receiver_stats
        .slots
        .iter()
        .enumerate()
        .map(|(slot, stats)| {
            serde_json::json!({
                "slot": slot,
                "target_samples": stats.target_samples,
                "queued_packets": stats.queued_packets,
                "queued_samples": stats.queued_samples,
                "staged_samples": stats.staged_samples,
                "concealments": stats.concealments,
                "late_drops": stats.late_drops,
                "shrink_samples": stats.shrink_samples,
            })
        })
        .collect::<Vec<_>>();
    let metadata = serde_json::json!({
        "voice_events": voice_events,
        "markers": marker_count,
        "n": count,
        "lost_markers": lost,
        "onsets": onsets.len(),
        "detected_onsets": onsets,
        "ambiguous_onsets": ambiguous,
        "rejected_onsets": matching.rejected,
        "unmatched_onsets": matching.unused,
        "marker_captures": sent.captures,
        "marker_identity_count": TONES.len(),
        "marker_frequencies_hz": TONES,
        "matching_window_ms": MATCH_WINDOW_MS,
        "minimum_latency_ms": settings.minimum_ms,
        "extra_delay_ms": settings.extra_delay_ms,
        "extra_delay_samples": settings.extra_delay_ms * RATE / 1000,
        "identity_window_samples": IDENTITY_WINDOW,
        "minimum_identity_confidence": MIN_IDENTITY_CONFIDENCE,
        "minimum_tone_energy_fraction": MIN_TONE_ENERGY_FRACTION,
        "median_ms": percentile(0.5),
        "p95_ms": percentile(0.95),
        "p99_ms": percentile(0.99),
        "receiver_underflow_samples": heard.underflows,
        "receiver_playout": {
            "speaker_samples": receiver_stats.speaker_samples,
            "callback_samples": receiver_stats.callback_samples,
            "callback_max_samples": receiver_stats.callback_max_samples,
            "headroom_samples": receiver_stats.headroom_samples,
            "underruns": receiver_stats.underruns,
            "underrun_samples": receiver_stats.underrun_samples,
            "slots": slots,
        },
        "receiver_frames_sent": receiver_stats.frames_sent,
        "receiver_frames_received": receiver_stats.frames_received,
        "url": url,
        "server": server_addr.to_string(),
        "sample_rate": RATE,
        "callback_samples": BLOCK,
        "receiver_callback_pattern": if settings.callback_jitter { vec![480, 960, 240] } else { vec![BLOCK] },
        "rms_window_samples": RMS_WINDOW,
        "rms_on_threshold": 0.18,
        "rms_off_threshold": 0.11,
        "marker_samples": MARKER,
        "marker_interval_samples": PERIOD,
        "warmup_samples": WARMUP,
    });
    std::fs::write(format!("{output}.json"), serde_json::to_string_pretty(&metadata)?)?;
    anyhow::ensure!(
        matching.rejected.is_empty(),
        "rejected {} onsets below {:.3} ms; see {output}.json",
        matching.rejected.len(),
        settings.minimum_ms
    );
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

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic(count: usize, latency_ms: f64) -> (Vec<Marker>, Vec<Onset>) {
        let markers: Vec<_> = (0..count)
            .map(|i| Marker { capture_s: 2.0 + i as f64 * PERIOD as f64 / RATE as f64, identity: i % TONES.len() })
            .collect();
        let onsets = markers
            .iter()
            .enumerate()
            .map(|(i, marker)| Onset {
                playout_s: marker.capture_s + latency_ms / 1000.0,
                sample: i,
                identity: Some(marker.identity),
                identity_confidence: 1.0,
                tone_energy_fraction: 1.0,
            })
            .collect();
        (markers, onsets)
    }

    #[test]
    fn latencies_beyond_one_period_do_not_alias() {
        for latency in [350.0, 600.0, 1999.0] {
            let (markers, onsets) = synthetic(100, latency);
            let result = match_markers(&markers, &onsets, 5.0, MATCH_WINDOW_MS);
            assert_eq!(result.matched.len(), 100);
            assert!(result.lost.is_empty());
            for (i, found) in result.matched.iter().enumerate() {
                assert_eq!((found.marker, found.onset), (i, i));
                assert!((found.latency_ms - latency).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn lost_marker_does_not_shift_later_matches() {
        let (markers, mut onsets) = synthetic(100, 600.0);
        onsets.remove(3);
        let result = match_markers(&markers, &onsets, 5.0, MATCH_WINDOW_MS);
        assert_eq!(result.lost, [3]);
        assert_eq!(result.matched.len(), 99);
        for found in result.matched {
            assert_eq!(onsets[found.onset].sample, found.marker);
            assert!((found.latency_ms - 600.0).abs() < 1e-9);
        }
    }

    #[test]
    fn duplicate_and_ambiguous_onsets_are_not_reused() {
        let (markers, mut onsets) = synthetic(100, 350.0);
        onsets[3].identity = None;
        let duplicate = Onset { sample: 100, ..onsets[4] };
        onsets.insert(5, duplicate);
        let result = match_markers(&markers, &onsets, 5.0, MATCH_WINDOW_MS);
        assert_eq!(result.lost, [3]);
        assert_eq!(result.matched.len(), 99);
        assert_eq!(result.unused, [3, 5]);
        let used: std::collections::HashSet<_> = result.matched.iter().map(|found| found.onset).collect();
        assert_eq!(used.len(), result.matched.len());
        for found in result.matched {
            assert_eq!(onsets[found.onset].sample, found.marker);
        }
    }

    #[test]
    fn below_floor_is_rejected_and_consumed() {
        let (mut markers, onsets) = synthetic(1, 1.0);
        markers.push(markers[0]);
        let result = match_markers(&markers, &onsets, 5.0, MATCH_WINDOW_MS);
        assert!(result.matched.is_empty());
        assert_eq!(result.lost, [0, 1]);
        assert_eq!(result.rejected.len(), 1);
        assert_eq!(result.rejected[0].reason, "below_minimum_latency");
        assert!(result.unused.is_empty());
    }

    #[test]
    fn matching_window_is_exclusive() {
        let (markers, onsets) = synthetic(1, MATCH_WINDOW_MS);
        let result = match_markers(&markers, &onsets, 5.0, MATCH_WINDOW_MS);
        assert!(result.matched.is_empty());
        assert_eq!(result.lost, [0]);
    }

    #[test]
    fn mixed_tones_and_carrier_have_no_identity() {
        for frequencies in [[TONES[2], TONES[3]], [400.0, 400.0]] {
            let samples: Vec<_> = (0..IDENTITY_WINDOW)
                .map(|i| {
                    let time = i as f64 / RATE as f64;
                    let value = frequencies.iter().map(|f| (time * f * std::f64::consts::TAU).sin()).sum::<f64>();
                    (time, value as f32 * 0.3)
                })
                .collect();
            assert_eq!(identify_tone(&samples).0, None);
        }
    }

    #[test]
    fn delay_line_holds_the_audio_samples() {
        let mut delay = DelayLine::new(3);
        let output: Vec<_> = [1.0, 2.0, 3.0, 0.0, 0.0, 0.0].into_iter().map(|value| delay.process(value)).collect();
        assert_eq!(output, [0.0, 0.0, 0.0, 1.0, 2.0, 3.0]);
        assert_eq!(DelayLine::new(0).process(0.5), 0.5);
    }
}
