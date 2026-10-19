//! A plain TeamSpeak client on the bridged official TeamSpeak server and a
//! native WebRTC client of ours hear each other.

mod support;

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use audiopus::{Application, Channels, SampleRate, coder::Encoder};
use futures::StreamExt;
use gwar_client::{
    ConnectOptions, Identity, connect,
    voice::{
        self,
        io::{AudioIo, AudioPorts, CaptureSample, OutputTiming},
    },
};
use gwar_proto::{ClientSoftware, Platform};
use rtrb::RingBuffer;
use tokio::time::{Instant, timeout};
use tsclientlib::{Connection, Identity as TsIdentity, StreamItem};
use tsproto_packets::packets::{AudioData, CodecType, OutAudio};

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
        let output = Arc::new(OutputTiming::default());
        let timing = output.clone();
        thread::spawn(move || {
            let mut t = 0u64;
            loop {
                for _ in 0..480 {
                    let s = if with_tone { tone(t) } else { 0.0 };
                    t += 1;
                    if mic_tx
                        .push(CaptureSample { captured_at: std::time::Instant::now(), position: t - 1, value: s })
                        .is_err()
                    {
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

fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn teamspeak_and_webrtc_clients_hear_each_other() {
    let Some(bridged) = support::start().await else { return };
    // Wait for the TeamSpeak server (started in the background).
    drop(support::ts_client(bridged.ts_voice, "probe").await);

    // Native WebRTC participant: speaks a tone and records what it hears.
    let native = native(&bridged, "native").await;
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

async fn native(bridged: &support::Bridged, nickname: &str) -> gwar_client::Connected {
    connect(ConnectOptions {
        url: format!("ws://{}/ws", bridged.server.http),
        nickname: nickname.into(),
        invite: None,
        certificate: None,
        expected_server_key: None,
        trust_on_first_use: true,
        insecure_lan: false,
        server_password: None,
        identity: Identity::generate(),
        software: ClientSoftware { name: "test".into(), version: "0".into(), platform: Platform::Desktop },
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn teamspeak_whisper_reaches_only_its_target() {
    let Some(bridged) = support::start().await else { return };
    drop(support::ts_client(bridged.ts_voice, "probe").await);

    // Two silent native listeners in the default channel; each gets a puppet
    // once the TeamSpeak user joins them.
    let mut listeners = Vec::new();
    for nickname in ["target", "bystander"] {
        let client = native(&bridged, nickname).await;
        client.connection.join(client.welcome.server.default_channel, None).await.unwrap();
        let heard = Arc::new(Mutex::new(Vec::new()));
        let voice =
            voice::start(&client.connection, Box::new(FakeIo { heard: heard.clone(), tone: false }), bridged.media)
                .await
                .unwrap();
        listeners.push((client, voice, heard));
    }
    let mut ts = Connection::build(bridged.ts_voice.to_string())
        .name("whisperer".to_string())
        .identity(TsIdentity::create())
        .connect()
        .unwrap();
    let puppet = |ts: &Connection, name: &str| {
        let state = ts.get_state().ok()?;
        state.clients.values().find(|c| c.name == name).map(|c| c.id.0)
    };
    let target = timeout(Duration::from_secs(20), async {
        loop {
            if let Some(clid) = puppet(&ts, "target")
                && puppet(&ts, "bystander").is_some()
            {
                return clid;
            }
            let _ = timeout(Duration::from_millis(100), ts.events().next()).await;
        }
    })
    .await
    .expect("both puppets on TeamSpeak");

    let level = |heard: &Arc<Mutex<Vec<f32>>>| {
        let samples = heard.lock().unwrap().clone();
        rms(&samples[samples.len().saturating_sub(96_000)..])
    };
    // Plain speech reaches both, so both can hear at all.
    speak(&mut ts, None).await;
    for (name, (_, _, heard)) in ["target", "bystander"].iter().zip(&listeners) {
        let heard_level = level(heard);
        assert!(heard_level > 0.05, "{name} heard plain speech at rms {heard_level}");
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    for (_, _, heard) in &listeners {
        heard.lock().unwrap().clear();
    }

    speak(&mut ts, Some(target)).await;
    let (target_level, bystander_level) = (level(&listeners[0].2), level(&listeners[1].2));
    assert!(target_level > 0.05, "the whisper target heard rms {target_level}");
    assert!(bystander_level < 0.005, "the bystander overheard the whisper at rms {bystander_level}");
}

/// Sends a tone from a TeamSpeak client for two seconds, to its channel or
/// whispered to one client.
async fn speak(ts: &mut Connection, whisper_to: Option<u16>) {
    let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
    let mut frame = [0f32; 960];
    let mut packet = [0u8; 1275];
    let mut t = 0u64;
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end {
        for s in frame.iter_mut() {
            *s = tone(t);
            t += 1;
        }
        let n = encoder.encode_float(&frame, &mut packet).unwrap();
        let data = &packet[..n];
        let audio = match whisper_to {
            Some(client) => AudioData::C2SWhisper {
                id: 0,
                codec: CodecType::OpusVoice,
                channels: Vec::new(),
                clients: vec![client],
                data,
            },
            None => AudioData::C2S { id: 0, codec: CodecType::OpusVoice, data },
        };
        ts.send_audio(OutAudio::new(&audio)).unwrap();
        let _ = timeout(Duration::from_millis(20), async {
            let mut events = ts.events();
            while events.next().await.is_some() {}
        })
        .await;
    }
}

/// The bridge orders each TeamSpeak speaker's frames by the voice id its
/// puppets receive. A listener joining mid-spurt must see the same id for the
/// same frame as one that heard the spurt from its start.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn teamspeak_voice_ids_are_the_same_for_every_listener() {
    let Some(bridged) = support::start().await else { return };
    drop(support::ts_client(bridged.ts_voice, "probe").await);
    let address = bridged.ts_voice.to_string();
    let speaker = joined(&address, "speaker").await;
    let from = speaker.get_state().unwrap().own_client.0;
    let stop = Arc::new(AtomicBool::new(false));
    let early = tokio::spawn(listen(joined(&address, "early").await, from, stop.clone()));
    let speaking = tokio::spawn(speak_noise(speaker, stop.clone()));
    tokio::time::sleep(Duration::from_secs(1)).await;
    let late = tokio::spawn(listen(joined(&address, "late").await, from, stop.clone()));
    tokio::time::sleep(Duration::from_secs(2)).await;
    stop.store(true, Ordering::Relaxed);
    speaking.await.unwrap();
    let (early, late) = (early.await.unwrap(), late.await.unwrap());

    let late_frames: HashSet<u64> = late.iter().map(|(_, frame)| *frame).collect();
    let prefix = early.iter().position(|(_, frame)| late_frames.contains(frame)).expect("frames heard by both");
    assert!(prefix >= 20, "the late listener did not join mid-spurt ({prefix} early-only frames)");
    let early_ids: HashMap<u64, u16> = early.iter().map(|(id, frame)| (*frame, *id)).collect();
    let pairs: Vec<_> = late.iter().filter_map(|(id, frame)| Some((*id, *early_ids.get(frame)?))).collect();
    assert!(pairs.len() > 50, "only {} frames heard by both", pairs.len());
    assert!(pairs.iter().all(|(late, early)| late == early), "TeamSpeak renumbers voice per listener: {pairs:?}");
}

async fn joined(address: &str, name: &str) -> Connection {
    let mut ts =
        Connection::build(address.to_string()).name(name.to_string()).identity(TsIdentity::create()).connect().unwrap();
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
    ts
}

/// Speaks distinct frames (noise) every 20 ms until stopped.
async fn speak_noise(mut ts: Connection, stop: Arc<AtomicBool>) {
    let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
    let mut seed = 1u32;
    while !stop.load(Ordering::Relaxed) {
        let mut frame = [0f32; 960];
        for s in frame.iter_mut() {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *s = (seed >> 8) as f32 / (1u32 << 24) as f32 * 0.4 - 0.2;
        }
        let mut packet = [0u8; 1275];
        let n = encoder.encode_float(&frame, &mut packet).unwrap();
        ts.send_audio(OutAudio::new(&AudioData::C2S { id: 0, codec: CodecType::OpusVoice, data: &packet[..n] }))
            .unwrap();
        let _ = timeout(Duration::from_millis(20), async {
            let mut events = ts.events();
            while events.next().await.is_some() {}
        })
        .await;
    }
}

/// Collects (voice id, payload digest) of `from`'s frames until shortly after `stop`.
async fn listen(mut ts: Connection, from: u16, stop: Arc<AtomicBool>) -> Vec<(u16, u64)> {
    let mut heard = Vec::new();
    let mut until = None;
    while until.is_none_or(|end| Instant::now() < end) {
        if until.is_none() && stop.load(Ordering::Relaxed) {
            until = Some(Instant::now() + Duration::from_millis(300));
        }
        let _ = timeout(Duration::from_millis(20), async {
            let mut events = ts.events();
            while let Some(Ok(item)) = events.next().await {
                if let StreamItem::Audio(audio) = item
                    && let AudioData::S2C { id, from: sender, data, .. } = audio.data().data()
                    && *sender == from
                    && !data.is_empty()
                {
                    heard.push((
                        *id,
                        std::hash::BuildHasher::hash_one(
                            &std::hash::BuildHasherDefault::<std::hash::DefaultHasher>::default(),
                            data,
                        ),
                    ));
                }
            }
        })
        .await;
    }
    heard
}
