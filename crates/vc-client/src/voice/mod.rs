//! Native voice engine: microphone → gate → Opus → WebRTC, and WebRTC →
//! per-slot jitter buffer → Opus decode → mix → speakers.
//!
//! Threads: a tokio task drives the str0m WebRTC session; one realtime-ish
//! "pipeline" thread does all audio DSP and coding; device callbacks only
//! move samples through lock-free rings (see [`io`]).

pub mod io;
pub mod jitter;
pub mod permission;
mod rtc;

use std::{
    net::{IpAddr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use audiopus::{
    Application, Bitrate, Channels, MutSignals, SampleRate,
    coder::{Decoder, Encoder},
    packet::Packet,
};
use tokio::sync::mpsc;
use vc_proto::AUDIO_SLOTS;

use self::{
    io::{AudioIo, AudioPorts},
    jitter::{JitterBuffer, Playout},
};
use crate::Connection;

pub const FRAME: usize = 960; // 20 ms at 48 kHz

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputMode {
    /// Transmit while the input level exceeds `threshold_db` (dBFS).
    VoiceActivity { threshold_db: f32 },
    /// Transmit while [`VoiceHandle::set_ptt`] is held.
    PushToTalk,
}

/// Lock-free controls shared between the UI and the pipeline thread.
pub struct Controls {
    muted: AtomicBool,
    deafened: AtomicBool,
    ptt: AtomicBool,
    vad_threshold: AtomicU32,
    push_to_talk: AtomicBool,
    master: AtomicU32,
    slot_gain: [AtomicU32; AUDIO_SLOTS],
    /// Latest input level in dBFS (for the meter).
    input_level: AtomicU32,
    transmitting: AtomicBool,
    stop: AtomicBool,
}

fn f(v: &AtomicU32) -> f32 {
    f32::from_bits(v.load(Ordering::Relaxed))
}

fn set_f(v: &AtomicU32, x: f32) {
    v.store(x.to_bits(), Ordering::Relaxed);
}

impl Default for Controls {
    fn default() -> Self {
        Self {
            muted: AtomicBool::new(false),
            deafened: AtomicBool::new(false),
            ptt: AtomicBool::new(false),
            vad_threshold: AtomicU32::new((-50.0f32).to_bits()),
            push_to_talk: AtomicBool::new(false),
            master: AtomicU32::new(1.0f32.to_bits()),
            slot_gain: std::array::from_fn(|_| AtomicU32::new(1.0f32.to_bits())),
            input_level: AtomicU32::new((-100.0f32).to_bits()),
            transmitting: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        }
    }
}

/// State of the WebRTC link to the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// Offer/answer done, ICE not connected yet.
    Connecting,
    Connected,
    /// The transport died (or the server closed it); start a new session.
    Lost,
}

/// Counters written by the transport/pipeline, read through [`VoiceHandle::stats`].
#[derive(Default)]
pub(crate) struct Counters {
    frames_sent: AtomicU64,
    frames_received: AtomicU64,
    /// 0 connecting, 1 connected, 2 lost.
    link: AtomicU8,
}

impl Counters {
    pub(crate) fn set_link(&self, state: LinkState) {
        let v = match state {
            LinkState::Connecting => 0,
            LinkState::Connected => 1,
            LinkState::Lost => 2,
        };
        // `Lost` is final.
        let _ = self.link.try_update(Ordering::Relaxed, Ordering::Relaxed, |old| (old != 2).then_some(v));
    }
    pub(crate) fn frame_received(&self) {
        self.frames_received.fetch_add(1, Ordering::Relaxed);
    }
}

/// Snapshot of transport counters, for diagnostics and the connection indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceStats {
    /// Opus frames handed to the transport.
    pub frames_sent: u64,
    /// Opus frames received from the server (all slots).
    pub frames_received: u64,
    pub link: LinkState,
}

/// Handle owned by the app; dropping it stops the engine.
pub struct VoiceHandle {
    controls: Arc<Controls>,
    counters: Arc<Counters>,
    _ports_guard: Box<dyn Send>,
}

impl VoiceHandle {
    pub fn set_muted(&self, muted: bool) {
        self.controls.muted.store(muted, Ordering::Relaxed);
    }
    pub fn set_deafened(&self, deafened: bool) {
        self.controls.deafened.store(deafened, Ordering::Relaxed);
    }
    pub fn set_ptt(&self, down: bool) {
        self.controls.ptt.store(down, Ordering::Relaxed);
    }
    pub fn set_input_mode(&self, mode: InputMode) {
        match mode {
            InputMode::VoiceActivity { threshold_db } => {
                set_f(&self.controls.vad_threshold, threshold_db);
                self.controls.push_to_talk.store(false, Ordering::Relaxed);
            }
            InputMode::PushToTalk => self.controls.push_to_talk.store(true, Ordering::Relaxed),
        }
    }
    /// Per-slot volume, 0.0–2.0. The app maps slots to users via `voice.slot`.
    pub fn set_slot_gain(&self, slot: usize, gain: f32) {
        if let Some(g) = self.controls.slot_gain.get(slot) {
            set_f(g, gain.clamp(0.0, 2.0));
        }
    }
    pub fn set_master_gain(&self, gain: f32) {
        set_f(&self.controls.master, gain.clamp(0.0, 2.0));
    }
    pub fn input_level_db(&self) -> f32 {
        f(&self.controls.input_level)
    }
    pub fn transmitting(&self) -> bool {
        self.controls.transmitting.load(Ordering::Relaxed)
    }
    pub fn stats(&self) -> VoiceStats {
        let c = &self.counters;
        VoiceStats {
            frames_sent: c.frames_sent.load(Ordering::Relaxed),
            frames_received: c.frames_received.load(Ordering::Relaxed),
            link: match c.link.load(Ordering::Relaxed) {
                0 => LinkState::Connecting,
                1 => LinkState::Connected,
                _ => LinkState::Lost,
            },
        }
    }
}

impl Drop for VoiceHandle {
    fn drop(&mut self) {
        self.controls.stop.store(true, Ordering::Relaxed);
    }
}

/// An encoded microphone frame on its way to the network.
pub(crate) struct Outgoing {
    pub opus: Vec<u8>,
    /// RFC 6464 level, negative dBov.
    pub level: i8,
    pub speech: bool,
}

/// A received frame for one slot.
pub(crate) struct Incoming {
    pub slot: usize,
    pub rtp_time: u64,
    pub opus: Vec<u8>,
}

/// Negotiates voice with the server and starts audio.
///
/// `local_ip` is the address the server should send media to; `None` uses the
/// interface that routes to the server.
pub async fn start(connection: &Connection, io: Box<dyn AudioIo>, server: SocketAddr) -> Result<VoiceHandle> {
    let connection = connection.clone();
    start_with(io, server.ip(), move |offer| async move { Ok(connection.voice_offer(offer).await?) }).await
}

/// Like [`start`], but the SDP offer/answer exchange goes through `signal`
/// (e.g. a WebSocket owned by the UI layer). `server` is the server's IP,
/// used to pick the local interface for media.
pub async fn start_with<F, Fut>(io: Box<dyn AudioIo>, server: IpAddr, signal: F) -> Result<VoiceHandle>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    let local_ip = route_ip(server).context("no route to server")?;
    let ports = tokio::task::spawn_blocking(move || io.open()).await??;
    let (out_tx, out_rx) = mpsc::channel::<Outgoing>(64);
    let (in_tx, in_rx) = std::sync::mpsc::sync_channel::<Incoming>(512);
    let counters = Arc::new(Counters::default());
    rtc::start(signal, local_ip, out_rx, in_tx, counters.clone()).await?;
    let controls = Arc::new(Controls::default());
    let AudioPorts { mic, speaker, _guard } = ports;
    let pipeline_controls = controls.clone();
    let pipeline_counters = counters.clone();
    thread::Builder::new().name("vc-voice-pipeline".into()).spawn(move || {
        if let Err(e) = pipeline(pipeline_controls, pipeline_counters, mic, speaker, out_tx, in_rx) {
            tracing::warn!("voice pipeline stopped: {e:#}");
        }
    })?;
    Ok(VoiceHandle { controls, counters, _ports_guard: _guard })
}

/// Voice on a TeamSpeak server (see [`crate::teamspeak`]): frames travel
/// over the TS connection instead of WebRTC; DSP and mixing are shared.
pub async fn start_teamspeak(
    io: Box<dyn AudioIo>,
    mut audio_in: mpsc::Receiver<crate::teamspeak::TsAudio>,
    audio_out: mpsc::Sender<Vec<u8>>,
) -> Result<VoiceHandle> {
    let ports = tokio::task::spawn_blocking(move || io.open()).await??;
    let (out_tx, mut out_rx) = mpsc::channel::<Outgoing>(64);
    let (in_tx, in_rx) = std::sync::mpsc::sync_channel::<Incoming>(512);
    let counters = Arc::new(Counters::default());
    counters.set_link(LinkState::Connected);
    let link = counters.clone();
    tokio::spawn(async move {
        // TS ends a talk spurt with an empty frame; send one after a short silence.
        let mut talking = false;
        loop {
            tokio::select! {
                frame = audio_in.recv() => {
                    let Some(frame) = frame else { break };
                    link.frame_received();
                    let _ = in_tx.try_send(Incoming { slot: frame.slot, rtp_time: frame.rtp_time, opus: frame.opus });
                }
                outgoing = async {
                    if talking {
                        tokio::time::timeout(Duration::from_millis(80), out_rx.recv()).await
                    } else {
                        Ok(out_rx.recv().await)
                    }
                } => match outgoing {
                    Ok(Some(frame)) => {
                        talking = true;
                        if audio_out.send(frame.opus).await.is_err() { break; }
                    }
                    Ok(None) => break,
                    Err(_) => {
                        talking = false;
                        let _ = audio_out.send(Vec::new()).await;
                    }
                },
            }
        }
        link.set_link(LinkState::Lost);
    });
    let controls = Arc::new(Controls::default());
    let AudioPorts { mic, speaker, _guard } = ports;
    let pipeline_controls = controls.clone();
    let pipeline_counters = counters.clone();
    thread::Builder::new().name("vc-voice-pipeline".into()).spawn(move || {
        if let Err(e) = pipeline(pipeline_controls, pipeline_counters, mic, speaker, out_tx, in_rx) {
            tracing::warn!("voice pipeline stopped: {e:#}");
        }
    })?;
    Ok(VoiceHandle { controls, counters, _ports_guard: _guard })
}

/// The local address the OS uses to reach `server` (no packets are sent).
fn route_ip(server: IpAddr) -> Option<IpAddr> {
    let bind: SocketAddr = if server.is_ipv4() { "0.0.0.0:0".parse().ok()? } else { "[::]:0".parse().ok()? };
    let socket = std::net::UdpSocket::bind(bind).ok()?;
    socket.connect((server, 9)).ok()?;
    Some(socket.local_addr().ok()?.ip())
}

fn level_db(frame: &[f32]) -> f32 {
    let rms = (frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32).sqrt();
    20.0 * rms.max(1e-5).log10()
}

const HANGOVER: Duration = Duration::from_millis(300);
/// Playout target in the speaker ring, in samples (~60 ms).
const SPEAKER_TARGET: usize = FRAME * 3;

fn pipeline(
    controls: Arc<Controls>,
    counters: Arc<Counters>,
    mut mic: rtrb::Consumer<f32>,
    mut speaker: rtrb::Producer<f32>,
    out: mpsc::Sender<Outgoing>,
    incoming: std::sync::mpsc::Receiver<Incoming>,
) -> Result<()> {
    let mut encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)?;
    encoder.set_bitrate(Bitrate::BitsPerSecond(48_000))?;
    encoder.set_inband_fec(true)?;
    encoder.set_packet_loss_perc(5)?;
    let mut decoders =
        (0..AUDIO_SLOTS).map(|_| Decoder::new(SampleRate::Hz48000, Channels::Mono)).collect::<Result<Vec<_>, _>>()?;
    let mut buffers: Vec<_> = (0..AUDIO_SLOTS).map(|_| JitterBuffer::new(3)).collect();
    let mut frame = vec![0f32; FRAME];
    let mut decoded = vec![0f32; FRAME * 6];
    let mut mix = vec![0f32; FRAME];
    let mut packet = vec![0u8; 1275];
    let mut last_voice = Instant::now() - HANGOVER;

    while !controls.stop.load(Ordering::Relaxed) {
        // ---- capture
        while mic.slots() >= FRAME {
            for s in frame.iter_mut() {
                *s = mic.pop().unwrap_or(0.0);
            }
            let db = level_db(&frame);
            set_f(&controls.input_level, db);
            let blocked = controls.muted.load(Ordering::Relaxed) || controls.deafened.load(Ordering::Relaxed);
            let wants = if controls.push_to_talk.load(Ordering::Relaxed) {
                controls.ptt.load(Ordering::Relaxed)
            } else {
                db >= f(&controls.vad_threshold)
            };
            if wants && !blocked {
                last_voice = Instant::now();
            }
            let send = !blocked && last_voice.elapsed() < HANGOVER;
            controls.transmitting.store(send, Ordering::Relaxed);
            if send {
                let n = encoder.encode_float(&frame, &mut packet[..])?;
                let level = (-db.clamp(-127.0, 0.0)).round() as i8;
                // Tell the server's speech gate the truth about voice activity.
                if out.try_send(Outgoing { opus: packet[..n].to_vec(), level: -level, speech: true }).is_ok() {
                    counters.frames_sent.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        // ---- receive
        while let Ok(item) = incoming.try_recv() {
            let samples = crate::voice::opus_samples(&item.opus).unwrap_or(FRAME as u32);
            if let Some(buffer) = buffers.get_mut(item.slot) {
                buffer.push(item.rtp_time, u64::from(samples), item.opus);
            }
        }

        // ---- playout: keep the device ring topped up to the target
        while speaker.slots() > 0 && (speaker.buffer().capacity() - speaker.slots()) < SPEAKER_TARGET {
            mix.fill(0.0);
            let deafened = controls.deafened.load(Ordering::Relaxed);
            for (slot, buffer) in buffers.iter_mut().enumerate() {
                let n = match buffer.pop() {
                    Playout::Packet(data) => {
                        let input = Packet::try_from(data.as_slice()).ok();
                        let output = MutSignals::try_from(&mut decoded[..])?;
                        decoders[slot].decode_float(input, output, false).unwrap_or(0)
                    }
                    Playout::Conceal => {
                        let output = MutSignals::try_from(&mut decoded[..FRAME])?;
                        decoders[slot].decode_float(None, output, false).unwrap_or(0)
                    }
                    Playout::Idle => 0,
                };
                if n == 0 || deafened {
                    continue;
                }
                let gain = f(&controls.slot_gain[slot]);
                for (m, s) in mix.iter_mut().zip(&decoded[..n.min(FRAME)]) {
                    *m += s * gain;
                }
            }
            let master = if deafened { 0.0 } else { f(&controls.master) };
            for s in &mix {
                if speaker.push((s * master).clamp(-1.0, 1.0)).is_err() {
                    break;
                }
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

/// Samples per channel at 48 kHz in an Opus packet (RFC 6716 §3.1).
pub fn opus_samples(packet: &[u8]) -> Option<u32> {
    let toc = *packet.first()?;
    let config = toc >> 3;
    let frame = match config {
        0..=11 => [480, 960, 1920, 2880][(config & 3) as usize],
        12..=15 => [480, 960][(config & 1) as usize],
        _ => [120, 240, 480, 960][(config & 3) as usize],
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => u32::from(*packet.get(1)? & 0x3f),
    };
    (frames > 0 && frame * frames <= 5760).then_some(frame * frames)
}
