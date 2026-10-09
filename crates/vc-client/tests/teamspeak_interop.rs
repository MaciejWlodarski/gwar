//! A plain TeamSpeak client on the bridged official TeamSpeak server and a
//! native WebRTC client of ours hear each other.

mod support;

use std::{
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use audiopus::{Application, Channels, SampleRate, coder::Encoder};
use futures::StreamExt;
use rtrb::RingBuffer;
use tokio::time::{Instant, timeout};
use tsclientlib::{Connection, Identity as TsIdentity, StreamItem};
use tsproto_packets::packets::{AudioData, CodecType, OutAudio};
use vc_client::{
    ConnectOptions, Identity, connect,
    voice::{
        self,
        io::{AudioIo, AudioPorts},
    },
};
use vc_proto::{ClientSoftware, Platform};

fn tone(t: u64) -> f32 {
    (t as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.3
}

/// Synthetic devices: optional tone into the mic, speaker output recorded.
struct FakeIo {
    heard: Arc<Mutex<Vec<f32>>>,
    tone: bool,
}

impl AudioIo for FakeIo {
    fn open(self: Box<Self>) -> anyhow::Result<AudioPorts> {
        let (mut mic_tx, mic) = RingBuffer::new(48_000);
        let (speaker, mut speaker_rx) = RingBuffer::new(24_000);
        let (heard, with_tone) = (self.heard.clone(), self.tone);
        thread::spawn(move || {
            let mut t = 0u64;
            loop {
                for _ in 0..480 {
                    let s = if with_tone { tone(t) } else { 0.0 };
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

fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn teamspeak_and_webrtc_clients_hear_each_other() {
    let Some(bridged) = support::start().await else { return };
    // Wait for the TeamSpeak server (started in the background).
    drop(support::ts_client(bridged.ts_voice, "probe").await);

    // Native WebRTC participant: speaks a tone and records what it hears.
    let url = format!("ws://{}/ws", bridged.server.http);
    let native = connect(ConnectOptions {
        url,
        nickname: "native".into(),
        server_password: None,
        identity: Identity::generate(),
        software: ClientSoftware { name: "test".into(), version: "0".into(), platform: Platform::Desktop },
    })
    .await
    .unwrap();
    // Plain TeamSpeak participant.
    let mut ts = Connection::build(bridged.ts_voice.to_string())
        .name("ts-user".to_string())
        .identity(TsIdentity::create())
        .connect()
        .unwrap();
    timeout(Duration::from_secs(8), async {
        let mut events = ts.events();
        while let Some(item) = events.next().await {
            if let StreamItem::MessageEvent(tsclientlib::messages::s2c::InMessage::ChannelListFinished(_)) =
                item.unwrap()
            {
                return;
            }
        }
    })
    .await
    .expect("TS client connected");

    native.connection.join(native.welcome.server.default_channel, None).await.unwrap();
    let heard = Arc::new(Mutex::new(Vec::new()));
    let _voice = voice::start(&native.connection, Box::new(FakeIo { heard: heard.clone(), tone: true }), bridged.media)
        .await
        .unwrap();
    let own = ts.get_state().unwrap().own_client.0;

    // TS → WebRTC: the TS client speaks a tone while we pump its connection;
    // the native client's voice arrives through its puppet.
    let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
    let mut frame = [0f32; 960];
    let mut packet = [0u8; 1275];
    let mut t = 0u64;
    let mut native_frames_at_ts = 0;
    let end = Instant::now() + Duration::from_secs(6);
    while Instant::now() < end {
        for s in frame.iter_mut() {
            *s = tone(t);
            t += 1;
        }
        let n = encoder.encode_float(&frame, &mut packet).unwrap();
        ts.send_audio(OutAudio::new(&AudioData::C2S { id: 0, codec: CodecType::OpusVoice, data: &packet[..n] }))
            .unwrap();
        // Service the TS connection for one frame time and count WebRTC → TS audio.
        let _ = timeout(Duration::from_millis(20), async {
            let mut events = ts.events();
            while let Some(Ok(item)) = events.next().await {
                if let StreamItem::Audio(audio) = item
                    && let AudioData::S2C { from, .. } = audio.data().data()
                    && *from != own
                {
                    native_frames_at_ts += 1;
                }
            }
        })
        .await;
    }

    let samples = heard.lock().unwrap().clone();
    let level = rms(&samples[samples.len().saturating_sub(48_000)..]);
    assert!(level > 0.05, "native client heard rms {level} from the TS speaker");
    assert!(native_frames_at_ts > 20, "TS client received only {native_frames_at_ts} frames from the native speaker");
}
