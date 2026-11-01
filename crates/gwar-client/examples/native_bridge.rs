//! A desktop-shell stand-in for apps/web/e2e/native-voice.mjs: the built web
//! app drives this process through `NativeVoiceEngine`'s command contract
//! (apps/desktop/src-tauri/VOICE-IPC.md) instead of Tauri. The real web MLS
//! controller installs keys; this process runs the real native engine with
//! virtual 10 ms devices: a sine microphone and a recorded speaker.
//!
//! Usage: native_bridge <microphone Hz>
//! stdin:  {"id", "command", "args"} per line
//! stdout: {"id", "ok"} | {"id", "error"} replies and {"event", "payload"} events
//! Test-only command `snapshot {frequency}` reports the speaker's recent tone. Share sound uses
//! the real playout and capture queue; the "computer" plays a 997 Hz tone, `share_stats` counts.

use std::{
    collections::{HashMap, VecDeque},
    net::IpAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::Context;
use base64::{Engine, prelude::BASE64_STANDARD};
use gwar_client::voice::{
    LinkState, VoiceHandle,
    io::{AudioIo, AudioPorts, CaptureSample, OutputTiming, mono},
    keys::EncryptedStart,
    sframe::{Binding, ControlReply},
    share::{CaptureQueue, SharePlayout, parse_frames},
    start_encrypted, start_with,
};
use rtrb::RingBuffer;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::oneshot,
};

const RATE: usize = 48_000;
const BLOCK: usize = 480;
const WINDOW: usize = 4096;
/// The tone the fixture's "computer" plays while a share captures its sound.
const COMPUTER_TONE: f64 = 997.0;

struct ToneIo {
    frequency: f64,
    played: Arc<Mutex<VecDeque<f32>>>,
    computer: ComputerLog,
}

/// The computer tone's strength at the speaker, one value per block, for as long as a run asks.
type ComputerLog = Arc<Mutex<Option<Vec<f32>>>>;

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
        let (speaker, mut speaker_rx) = RingBuffer::<[f32; 2]>::new(RATE / 2);
        let output = Arc::new(OutputTiming::default());
        let timing = output.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = thread::Builder::new().name("native-bridge-audio".into()).spawn(move || {
            let epoch = Instant::now();
            let mut position = 0u64;
            while !thread_stop.load(Ordering::Relaxed) {
                let mut missing = 0;
                let mut block = Vec::with_capacity(BLOCK);
                for i in 0..BLOCK {
                    let phase = (position + i as u64) as f64 * std::f64::consts::TAU / RATE as f64;
                    let value = (phase * self.frequency).sin() as f32 * 0.25;
                    let _ = mic_tx.push(CaptureSample {
                        captured_at: Instant::now(),
                        position: position + i as u64,
                        value,
                    });
                    block.push(speaker_rx.pop().map_or_else(
                        |_| {
                            missing += 1;
                            0.0
                        },
                        mono,
                    ));
                }
                if let Some(log) = self.computer.lock().unwrap().as_mut() {
                    log.push(strength(&block, COMPUTER_TONE));
                }
                {
                    let mut played = self.played.lock().unwrap();
                    played.extend(block);
                    while played.len() > WINDOW {
                        played.pop_front();
                    }
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

/// One tone's amplitude in a block, windowed so that a voice beside it does not leak in.
fn strength(block: &[f32], frequency: f64) -> f32 {
    let (mut sine, mut cosine, mut weight) = (0.0, 0.0, 0.0);
    for (i, s) in block.iter().enumerate() {
        let hann = 0.5 - 0.5 * (i as f64 * std::f64::consts::TAU / block.len() as f64).cos();
        let phase = i as f64 * frequency * std::f64::consts::TAU / RATE as f64;
        sine += f64::from(*s) * hann * phase.sin();
        cosine += f64::from(*s) * hann * phase.cos();
        weight += hann;
    }
    (2.0 * (sine * sine + cosine * cosine).sqrt() / weight) as f32
}

/// The tone's share of the recent speaker energy, 0 when silent (the browser test's measure).
fn tone(played: &VecDeque<f32>, frequency: f64) -> f64 {
    let (mut energy, mut sine, mut cosine) = (0.0, 0.0, 0.0);
    for (i, s) in played.iter().enumerate() {
        let s = f64::from(*s);
        let phase = i as f64 * frequency * std::f64::consts::TAU / RATE as f64;
        energy += s * s;
        sine += s * phase.sin();
        cosine += s * phase.cos();
    }
    let rms = (energy / played.len().max(1) as f64).sqrt();
    if rms < 0.002 { 0.0 } else { 2.0 * (sine * sine + cosine * cosine) / (played.len() as f64 * energy) }
}

#[derive(Default)]
struct State {
    owner: u64,
    run: u64,
    handle: Option<VoiceHandle>,
    answers: HashMap<u64, oneshot::Sender<Result<String, String>>>,
    playout: Option<Arc<SharePlayout>>,
    capture: Option<(u64, Arc<CaptureQueue>)>,
}

struct Bridge {
    frequency: f64,
    played: Arc<Mutex<VecDeque<f32>>>,
    computer: ComputerLog,
    state: Mutex<State>,
    /// When the fake computer started, and how many samples it has written.
    capture_clock: Arc<Mutex<(Option<Instant>, u64)>>,
}

fn emit(event: &str, payload: Value) {
    println!("{}", json!({ "event": event, "payload": payload }));
}

fn number(args: &Value, key: &str) -> anyhow::Result<u64> {
    args[key].as_u64().with_context(|| format!("missing {key}"))
}

impl Bridge {
    /// The current run's key channel, cloned so no lock is held across awaits.
    fn keys(&self, args: &Value) -> anyhow::Result<Option<gwar_client::voice::keys::KeyChannel>> {
        let state = self.state.lock().unwrap();
        anyhow::ensure!(number(args, "owner")? == state.owner, "superseded owner");
        let current = number(args, "run")? == state.run;
        Ok(state.handle.as_ref().filter(|_| current).and_then(|h| h.keys().cloned()))
    }

    fn playout(&self) -> anyhow::Result<Arc<SharePlayout>> {
        let mut state = self.state.lock().unwrap();
        if state.playout.is_none() {
            let playout = Arc::new(SharePlayout::new()?);
            if let Some(handle) = &state.handle {
                handle.set_share_bus(playout.bus());
            }
            state.playout = Some(playout);
        }
        Ok(state.playout.clone().expect("made above"))
    }

    async fn command(self: Arc<Self>, command: &str, args: Value) -> anyhow::Result<Value> {
        match command {
            "voice_bootstrap" => {
                let mut state = self.state.lock().unwrap();
                state.owner += 1;
                state.run = 0;
                state.handle = None;
                Ok(json!(state.owner))
            }
            "voice_start" => self.start(args).await.map(|()| Value::Null),
            "voice_answer" => {
                let sender = self.state.lock().unwrap().answers.remove(&number(&args, "run")?);
                if let Some(sender) = sender {
                    let answer = match args["sdp"].as_str() {
                        Some(sdp) => Ok(sdp.to_owned()),
                        None => Err(args["error"].as_str().unwrap_or("no answer").to_owned()),
                    };
                    let _ = sender.send(answer);
                }
                Ok(Value::Null)
            }
            "voice_stop" => {
                let mut state = self.state.lock().unwrap();
                if number(&args, "run")? >= state.run {
                    state.run = 0;
                    state.handle = None;
                    self.played.lock().unwrap().clear();
                }
                Ok(Value::Null)
            }
            "voice_set_keys" => {
                let body = args["body"].clone();
                let keys = self.keys(&args)?;
                Ok(serde_json::to_value(match keys {
                    Some(keys) => keys.control(body).await,
                    None => ControlReply::ignored(&body),
                })?)
            }
            "voice_crypto_counters" => {
                let keys = self.keys(&args)?;
                Ok(keys.map_or(Value::Null, |k| serde_json::to_value(k.counters()).unwrap()))
            }
            "audio_devices" => Ok(json!({ "inputs": [], "outputs": [] })),
            "share_capture_support" => Ok(json!("ok")),
            "share_capture_start" => {
                let id = number(&args, "id")?;
                let queue = Arc::<CaptureQueue>::default();
                self.state.lock().unwrap().capture = Some((id, queue.clone()));
                let started = Instant::now();
                self.capture_clock.lock().unwrap().0 = Some(started);
                let written = self.capture_clock.clone();
                tokio::spawn(async move {
                    let mut position = 0u64;
                    while !queue.is_closed() {
                        let block: Vec<f32> = (0..BLOCK as u64)
                            .flat_map(|i| {
                                let phase = (position + i) as f64 * COMPUTER_TONE * std::f64::consts::TAU / RATE as f64;
                                [phase.sin() as f32 * 0.2; 2]
                            })
                            .collect();
                        queue.write(&block);
                        position += BLOCK as u64;
                        written.lock().unwrap().1 = position;
                        // By the clock, as a sound device runs: a plain 10 ms sleep runs 13% slow.
                        let due = started + Duration::from_secs_f64(position as f64 / RATE as f64);
                        tokio::time::sleep_until(due.into()).await;
                    }
                });
                Ok(Value::Null)
            }
            "share_capture_read" => {
                let id = number(&args, "id")?;
                let queue = self.state.lock().unwrap().capture.clone().filter(|(running, _)| *running == id);
                let (_, queue) = queue.context("closed")?;
                let samples = queue.read(Duration::from_millis(100)).await;
                anyhow::ensure!(!samples.is_empty() || !queue.is_closed(), "closed");
                Ok(json!(BASE64_STANDARD.encode(samples.iter().flat_map(|s| s.to_le_bytes()).collect::<Vec<u8>>())))
            }
            "share_capture_stats" => {
                let (started, written) = *self.capture_clock.lock().unwrap();
                let dropped = self.state.lock().unwrap().capture.as_ref().map_or(0, |(_, queue)| queue.dropped());
                let elapsed = started.map_or(0.0, |started| started.elapsed().as_secs_f64());
                Ok(
                    json!({ "writtenSeconds": written as f64 / RATE as f64, "elapsedSeconds": elapsed, "dropped": dropped }),
                )
            }
            "share_capture_stop" => {
                let id = number(&args, "id")?;
                let mut state = self.state.lock().unwrap();
                if state.capture.as_ref().is_some_and(|(running, _)| *running == id) {
                    state.capture.take().unwrap().1.close();
                }
                Ok(Value::Null)
            }
            "share_playback_open" => {
                let (slot, run) = (number(&args, "slot")? as usize, number(&args, "run")?);
                let gain = args["gain"].as_f64().context("gain")? as f32;
                let opened = self.playout()?.open(slot, run, gain);
                Ok(json!(opened && self.state.lock().unwrap().handle.is_some()))
            }
            "share_playback_close" => {
                self.playout()?.close(number(&args, "slot")? as usize, number(&args, "run")?);
                Ok(Value::Null)
            }
            "share_playback_gain" => {
                let (slot, run) = (number(&args, "slot")? as usize, number(&args, "run")?);
                self.playout()?.set_gain(slot, run, args["gain"].as_f64().context("gain")? as f32);
                Ok(Value::Null)
            }
            "share_playback_frames" => {
                let body: Vec<u8> = serde_json::from_value(args["body"].clone())?;
                let (_, frames) = parse_frames(&body).context("malformed share frames")?;
                let playout = self.playout()?;
                for frame in frames {
                    playout.push(frame);
                }
                Ok(Value::Null)
            }
            "share_stats" => {
                let stats = self.playout()?.stats();
                Ok(
                    json!({ "frames": stats.frames, "decoded": stats.decoded, "concealed": stats.concealed, "late": stats.late_drops, "backlog": stats.backlog_drops, "input": stats.input_drops, "underruns": stats.ring_underruns }),
                )
            }
            "computer_log_start" => {
                *self.computer.lock().unwrap() = Some(Vec::new());
                Ok(Value::Null)
            }
            "computer_log_stop" => Ok(json!(self.computer.lock().unwrap().take().unwrap_or_default())),
            "snapshot" => {
                let frequency = args["frequency"].as_f64().context("frequency")?;
                let played = self.played.lock().unwrap();
                let energy = played.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>();
                let rms = (energy / played.len().max(1) as f64).sqrt();
                Ok(json!({ "tone": tone(&played, frequency), "rms": rms }))
            }
            // Device, gain and gate setters do not change what this fixture tests.
            _ => Ok(Value::Null),
        }
    }

    async fn start(self: &Arc<Self>, args: Value) -> anyhow::Result<()> {
        let (owner, run) = (number(&args, "owner")?, number(&args, "run")?);
        let binding = &args["binding"];
        let crypto = match args["mode"].as_str() {
            Some("plaintext") => None,
            Some("encrypted") => {
                let mut full = binding.clone();
                full["run"] = json!(run);
                let resume_after = args["resumeAfter"].as_str().map(str::parse).transpose()?;
                Some(EncryptedStart { binding: Binding::parse(&full).context("invalid voice binding")?, resume_after })
            }
            _ => anyhow::bail!("invalid voice binding"),
        };
        {
            let mut state = self.state.lock().unwrap();
            anyhow::ensure!(owner == state.owner && run > state.run, "superseded");
            state.run = run;
            state.handle = None;
            // Only this run's playout counts: an earlier run's tone cannot pass a later check.
            self.played.lock().unwrap().clear();
        }
        let (answer, answered) = oneshot::channel();
        self.state.lock().unwrap().answers.insert(run, answer);
        let signal = move |sdp: String| async move {
            emit("voice://offer", json!({ "owner": owner, "run": run, "sdp": sdp }));
            answered.await?.map_err(anyhow::Error::msg)
        };
        let io = Box::new(ToneIo {
            frequency: self.frequency,
            played: self.played.clone(),
            computer: self.computer.clone(),
        });
        let server: IpAddr = "127.0.0.1".parse()?;
        let handle = match crypto {
            Some(crypto) => start_encrypted(io, server, signal, crypto).await?,
            None => start_with(io, server, signal).await?,
        };
        let events = handle.take_crypto_events();
        {
            let mut state = self.state.lock().unwrap();
            anyhow::ensure!(state.owner == owner && state.run == run, "superseded");
            if let Some(playout) = &state.playout {
                handle.set_share_bus(playout.bus());
            }
            state.handle = Some(handle);
        }
        if let Some(mut events) = events {
            let bridge = self.clone();
            tokio::spawn(async move {
                while let Some(status) = events.recv().await {
                    let failed = status.state == gwar_client::voice::sframe::CryptoState::Failed;
                    let mut payload = serde_json::to_value(status).unwrap();
                    payload["owner"] = json!(owner);
                    payload["run"] = json!(run);
                    emit("voice://crypto", payload);
                    // Like the desktop shell: a failed run is retired at once.
                    let mut state = bridge.state.lock().unwrap();
                    if failed && state.owner == owner && state.run == run {
                        state.run = 0;
                        state.handle = None;
                        bridge.played.lock().unwrap().clear();
                    }
                }
            });
        }
        let bridge = self.clone();
        tokio::spawn(async move {
            let mut last = None;
            loop {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let link = {
                    let state = bridge.state.lock().unwrap();
                    match &state.handle {
                        Some(h) if state.run == run => h.stats().link,
                        _ => return,
                    }
                };
                if last != Some(link) {
                    last = Some(link);
                    let state = match link {
                        LinkState::Connecting => continue,
                        LinkState::Connected => "connected",
                        LinkState::Lost => "failed",
                    };
                    emit("voice://state", json!({ "owner": owner, "run": run, "state": state }));
                }
            }
        });
        Ok(())
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    let frequency: f64 = std::env::args().nth(1).context("usage: native_bridge <microphone Hz>")?.parse()?;
    let bridge = Arc::new(Bridge {
        frequency,
        played: Arc::default(),
        computer: Arc::default(),
        state: Mutex::default(),
        capture_clock: Arc::default(),
    });
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let request: Value = serde_json::from_str(&line)?;
        let bridge = bridge.clone();
        // Commands run concurrently, like Tauri's async commands: voice_start waits for voice_answer.
        tokio::spawn(async move {
            let id = request["id"].clone();
            let command = request["command"].as_str().unwrap_or_default().to_owned();
            match bridge.command(&command, request["args"].clone()).await {
                Ok(ok) => println!("{}", json!({ "id": id, "ok": ok })),
                Err(error) => println!("{}", json!({ "id": id, "error": format!("{error:#}") })),
            }
        });
    }
    Ok(())
}
