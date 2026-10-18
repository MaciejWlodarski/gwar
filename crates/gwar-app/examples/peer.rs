//! Someone to talk to while trying the mobile app against a local server:
//! joins the default channel, says hello and answers every channel message.
//! With `voice`, it also joins voice with a quiet 440 Hz tone as its microphone
//! and prints once a second what reaches its virtual speaker.
//!
//! cargo run -p gwar-app --example peer -- ws://127.0.0.1:8790 Ola [voice]

use std::{
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use rtrb::RingBuffer;
use vc_client::{
    ConnectOptions, Identity, connect,
    voice::{
        self,
        io::{AudioIo, AudioPorts, CaptureSample, OutputTiming},
    },
};
use vc_proto::{ChatTarget, ClientSoftware, Event, Platform};

/// A 10 ms virtual device: a quiet tone in, the speaker's samples kept for the meter.
struct ToneIo {
    heard: Arc<Mutex<Vec<f32>>>,
}

impl AudioIo for ToneIo {
    fn open(self: Box<Self>) -> anyhow::Result<AudioPorts> {
        let (mut mic_tx, mic) = RingBuffer::new(48_000);
        let (speaker, mut speaker_rx) = RingBuffer::new(24_000);
        let output = Arc::new(OutputTiming::default());
        let timing = output.clone();
        let heard = self.heard;
        thread::spawn(move || {
            let start = Instant::now();
            let mut t = 0u64;
            loop {
                for _ in 0..480 {
                    let value = (t as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.05;
                    if mic_tx.push(CaptureSample { captured_at: Instant::now(), position: t, value }).is_err() {
                        return;
                    }
                    t += 1;
                }
                let missing = 480usize.saturating_sub(speaker_rx.slots());
                let mut out = heard.lock().unwrap();
                out.extend((0..480).map(|_| speaker_rx.pop().unwrap_or(0.0)));
                let excess = out.len().saturating_sub(48_000);
                out.drain(..excess);
                drop(out);
                timing.report(480, missing);
                let due = start + Duration::from_secs_f64(t as f64 / 48_000.0);
                thread::sleep(due.saturating_duration_since(Instant::now()));
            }
        });
        Ok(AudioPorts { mic, speaker, output, _guard: Box::new(()) })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let address = args.next().unwrap_or_else(|| "ws://127.0.0.1:8790".into());
    let nickname = args.next().unwrap_or_else(|| "Ola".into());
    let with_voice = args.next().as_deref() == Some("voice");
    let software = ClientSoftware { name: "Gwar peer".into(), version: "0".into(), platform: Platform::Desktop };
    let url = format!("{}/ws", address.trim_end_matches('/'));
    let mut options = ConnectOptions::new(url, Identity::generate(), software);
    options.nickname = nickname;
    options.trust_on_first_use = true;
    options.insecure_lan = true;
    let mut connected = connect(options).await?;
    let lobby = connected.welcome.server.default_channel;
    connected.connection.join(lobby, None).await?;
    connected.connection.chat(ChatTarget::Channel(lobby), "Hello from the peer").await?;
    println!("in channel {lobby} as {}; Ctrl-C to quit", connected.welcome.uid);

    let heard = Arc::new(Mutex::new(Vec::new()));
    let voice = if with_voice {
        let host = url::Url::parse(&address)?.host_str().unwrap_or("127.0.0.1").to_owned();
        let server = tokio::net::lookup_host((host.as_str(), 0)).await?.next().ok_or("no address")?;
        Some(voice::start(&connected.connection, Box::new(ToneIo { heard: heard.clone() }), server).await?)
    } else {
        None
    };

    let mut meter = tokio::time::interval(Duration::from_secs(1));
    // The server drops a connection that says nothing for a minute.
    let mut keepalive = tokio::time::interval(Duration::from_secs(20));
    loop {
        tokio::select! {
            _ = keepalive.tick() => {
                connected.connection.request(vc_proto::Request::Ping {}).await?;
            }
            event = connected.events.recv() => {
                let Some(event) = event else { break };
                if let Event::ChatMessage(m) = event
                    && m.author_uid != connected.welcome.uid
                    && matches!(m.target, ChatTarget::Channel(_))
                {
                    connected.connection.chat(m.target, format!("Got it: {}", m.text)).await?;
                }
            }
            _ = meter.tick(), if voice.is_some() => {
                let s = voice.as_ref().map(|v| v.stats()).expect("voice");
                let samples = heard.lock().unwrap().clone();
                let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt();
                println!("voice {:?}: sent {} received {} heard rms {rms:.4}", s.link, s.frames_sent, s.frames_received);
            }
        }
    }
    Ok(())
}
