//! A desktop-shell stand-in for apps/web/e2e/native-voice.mjs: the built web
//! app drives this process through `NativeVoiceEngine`'s command contract
//! (apps/desktop/src-tauri/VOICE-IPC.md) instead of Tauri. The real web MLS
//! controller installs keys; this process runs the real native engine with
//! virtual 10 ms devices: a sine microphone and a recorded speaker.
//!
//! Usage: native_bridge <microphone Hz>
//! stdin:  {"id", "command", "args"} per line
//! stdout: {"id", "ok"} | {"id", "error"} replies and {"event", "payload"} events
//! Test-only command `snapshot {frequency}` reports the speaker's recent tone.

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
use gwar_client::voice::{
    LinkState, VoiceHandle,
    io::{AudioIo, AudioPorts, CaptureSample, OutputTiming},
    keys::EncryptedStart,
    sframe::{Binding, ControlReply},
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

struct ToneIo {
    frequency: f64,
    played: Arc<Mutex<VecDeque<f32>>>,
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
                    block.push(speaker_rx.pop().unwrap_or_else(|_| {
                        missing += 1;
                        0.0
                    }));
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
}

struct Bridge {
    frequency: f64,
    played: Arc<Mutex<VecDeque<f32>>>,
    state: Mutex<State>,
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
            "snapshot" => {
                let frequency = args["frequency"].as_f64().context("frequency")?;
                Ok(json!({ "tone": tone(&self.played.lock().unwrap(), frequency) }))
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
        let io = Box::new(ToneIo { frequency: self.frequency, played: self.played.clone() });
        let server: IpAddr = "127.0.0.1".parse()?;
        let handle = match crypto {
            Some(crypto) => start_encrypted(io, server, signal, crypto).await?,
            None => start_with(io, server, signal).await?,
        };
        let events = handle.take_crypto_events();
        {
            let mut state = self.state.lock().unwrap();
            anyhow::ensure!(state.owner == owner && state.run == run, "superseded");
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
    let bridge = Arc::new(Bridge { frequency, played: Arc::default(), state: Mutex::default() });
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
