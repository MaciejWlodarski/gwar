//! Two native engines talk through the real server: a sine wave fed into
//! A's "microphone" must come out of B's "speakers".

use std::{
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use rtrb::RingBuffer;
use vc_client::{
    ConnectOptions, Identity, connect,
    voice::{
        self,
        io::{AudioIo, AudioPorts},
    },
};
use vc_proto::{ClientSoftware, Platform};

struct FakeIo {
    /// Samples played on the speaker side, shared with the test.
    heard: Arc<Mutex<Vec<f32>>>,
    tone: bool,
}

impl AudioIo for FakeIo {
    fn open(self: Box<Self>) -> anyhow::Result<AudioPorts> {
        let (mut mic_tx, mic) = RingBuffer::new(48_000);
        let (speaker, mut speaker_rx) = RingBuffer::new(24_000);
        let heard = self.heard.clone();
        let tone = self.tone;
        // Real-time pacing: 10 ms of samples in, 10 ms out, like a device.
        thread::spawn(move || {
            let mut t = 0u64;
            loop {
                for _ in 0..480 {
                    let s = if tone { (t as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.3 } else { 0.0 };
                    t += 1;
                    if mic_tx.push(s).is_err() {
                        return;
                    }
                }
                let mut out = heard.lock().unwrap();
                for _ in 0..480 {
                    out.push(speaker_rx.pop().unwrap_or(0.0));
                }
                drop(out);
                thread::sleep(Duration::from_millis(10));
            }
        });
        Ok(AudioPorts { mic, speaker, _guard: Box::new(()) })
    }
}

fn options(url: &str, nickname: &str) -> ConnectOptions {
    ConnectOptions {
        url: url.into(),
        nickname: nickname.into(),
        server_password: None,
        identity: Identity::generate(),
        software: ClientSoftware { name: "test".into(), version: "0".into(), platform: Platform::Desktop },
    }
}

fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tone_travels_between_native_clients() {
    let any = "127.0.0.1:0".parse().unwrap();
    let server = vc_server::start(vc_server::Config {
        database: None,
        http_bind: any,
        media_bind: any,
        media_advertise: any,
        max_clients: 8,
        server_password: None,
        web_root: None,
        ice_servers: vec![],
        teamspeak: None,
    })
    .await
    .unwrap();
    let url = format!("ws://{}/ws", server.http);
    let a = connect(options(&url, "a")).await.unwrap();
    let b = connect(options(&url, "b")).await.unwrap();
    for client in [&a, &b] {
        client.connection.join(client.welcome.server.default_channel, None).await.unwrap();
    }

    let silent = Arc::new(Mutex::new(Vec::new()));
    let heard = Arc::new(Mutex::new(Vec::new()));
    let _va = voice::start(&a.connection, Box::new(FakeIo { heard: silent, tone: true }), server.http).await.unwrap();
    let _vb =
        voice::start(&b.connection, Box::new(FakeIo { heard: heard.clone(), tone: false }), server.http).await.unwrap();

    tokio::time::sleep(Duration::from_secs(3)).await;
    let samples = heard.lock().unwrap().clone();
    let tail = &samples[samples.len().saturating_sub(48_000)..];
    let level = rms(tail);
    assert!(level > 0.05, "B heard rms {level} over the last second");
}
