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
        io::{AudioIo, AudioPorts, CaptureSample, OutputTiming},
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
        let output = Arc::new(OutputTiming::default());
        let timing = output.clone();
        thread::spawn(move || {
            let mut t = 0u64;
            loop {
                for _ in 0..480 {
                    let s = if tone { (t as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.3 } else { 0.0 };
                    t += 1;
                    if mic_tx.push(CaptureSample { position: t - 1, value: s }).is_err() {
                        return;
                    }
                }
                let mut out = heard.lock().unwrap();
                let missing = 480usize.saturating_sub(speaker_rx.slots());
                for _ in 0..480 {
                    out.push(speaker_rx.pop().unwrap_or(0.0));
                }
                timing.report(480, missing);
                drop(out);
                thread::sleep(Duration::from_millis(10));
            }
        });
        Ok(AudioPorts { mic, speaker, output, _guard: Box::new(()) })
    }
}

fn options(url: &str, nickname: &str) -> ConnectOptions {
    ConnectOptions {
        url: url.into(),
        nickname: nickname.into(),
        invite: None,
        certificate: None,
        expected_server_key: None,
        trust_on_first_use: true,
        insecure_lan: false,
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
        public_url: None,
        data_dir: None,
        public_ip: None,
        insecure_any_host: true,
        public_endpoints: Vec::new(),
        client_ip_header: None,
        upload_limit: 10 * 1024 * 1024,
        upload_quota: 0,
        storage_limit: 0,
        files_dir: None,
        web_origins: Vec::new(),
        tls: None,
        redirect_http: None,
        connect_url: None,
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
    let vb =
        voice::start(&b.connection, Box::new(FakeIo { heard: heard.clone(), tone: false }), server.http).await.unwrap();

    tokio::time::sleep(Duration::from_secs(3)).await;
    let stats = vb.stats();
    assert_eq!(stats.callback_samples, 480);
    // The generation extension must survive real SDP negotiation and SRTP.
    assert!(stats.slots.iter().any(|slot| slot.generation > 0), "{stats:?}");
    assert!(stats.speaker_samples <= (stats.callback_max_samples + stats.headroom_samples) as u64, "{stats:?}");
    assert!(stats.slots.iter().all(|slot| slot.staged_samples <= 960), "{stats:?}");
    let samples = heard.lock().unwrap().clone();
    let tail = &samples[samples.len().saturating_sub(48_000)..];
    let level = rms(tail);
    assert!(level > 0.05, "B heard rms {level} over the last second");
}
