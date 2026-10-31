//! Share sound in the native engine (docs/share-audio.md): playout of what viewers receive,
//! and the bounded queue that carries captured system sound towards the webview's sender.
//!
//! Voice comes first. Share sound runs on its own ordinary-priority thread, which decodes
//! ahead into one bounded stereo ring. The voice pipeline only takes what that ring holds
//! and never waits: a late, slow or dead share thread is silence in the share, nothing else.
//! The mid signal joins voice before the echo canceller's reference, so a viewer's microphone
//! does not send the share back into the call. The side signal is only the difference
//! between the two speakers.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;
use audiopus::{Channels, MutSignals, SampleRate, coder::Decoder, packet::Packet};
use gwar_proto::SCREEN_SLOTS;
use rtrb::{Consumer, Producer, RingBuffer};

use super::{io::RATE, opus_samples};

const FRAME_MS: u32 = 20;
const FRAME: usize = (RATE * FRAME_MS / 1000) as usize;
/// Opus's longest packet, per channel; other durations are refused before decoding.
const MAX_PACKET_SAMPLES: usize = 5760;
/// Ingress admits at most 1,200 payload bytes; this is Opus's own limit.
const MAX_OPUS_BYTES: usize = 1275;

/// Decoded stereo held ahead of the pipeline: enough for an ordinary thread's wake-up jitter.
const RING_TARGET: usize = (RATE / 25) as usize; // 40 ms
const RING_CAPACITY: usize = (RATE / 5) as usize; // 200 ms

const MIN_DELAY: Duration = Duration::from_millis(40);
const MAX_DELAY: Duration = Duration::from_millis(160);
/// Packets queued per stream: at most 640 ms at the normal 20 ms packet.
const MAX_QUEUED: usize = 32;
/// Queued frames beyond the delay that are played slightly faster to absorb clock drift.
const DRIFT_MARGIN: Duration = Duration::from_millis(10);
/// 0.2 % stays inaudible and covers any sound card's clock error.
const DRIFT_RATE: f64 = 0.002;
/// Anything further ahead is a stall's backlog: dropped, never replayed.
const BACKLOG_MARGIN: Duration = Duration::from_millis(80);
/// A stream that has nothing for this long stops concealing and starts over.
const CONCEAL_LIMIT: u32 = 5;

/// Commands wait here while the share thread runs; a full queue drops share sound only.
const INPUT_QUEUE: usize = 256;

/// Encoded share sound from one watch slot, in arrival order.
#[derive(Debug, Clone)]
pub struct ShareFrame {
    pub slot: usize,
    /// The watch transport it arrived on: a newer run retires everything older.
    pub run: u64,
    pub rtp_time: u32,
    pub opus: Vec<u8>,
}

enum Input {
    Frame(ShareFrame, Instant),
    Open { slot: usize, run: u64 },
    Close { slot: usize },
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ShareStats {
    pub frames: u64,
    pub decoded: u64,
    pub concealed: u64,
    pub late_drops: u64,
    pub backlog_drops: u64,
    pub input_drops: u64,
    /// Samples the voice pipeline wanted while a stream was open and the ring was empty.
    pub ring_underruns: u64,
}

#[derive(Default)]
struct Counters {
    frames: AtomicU64,
    decoded: AtomicU64,
    concealed: AtomicU64,
    late_drops: AtomicU64,
    backlog_drops: AtomicU64,
    input_drops: AtomicU64,
    ring_underruns: AtomicU64,
}

/// What the voice pipeline reads: decoded share sound, independent of any one voice run.
pub struct ShareBus {
    ring: Mutex<Consumer<[f32; 2]>>,
    /// Any open stream; without one the pipeline's only cost is this load.
    active: AtomicBool,
    counters: Arc<Counters>,
}

impl ShareBus {
    pub(super) fn active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    /// Adds share sound to `mid`, the mono mix that also feeds the echo canceller, and writes the
    /// stereo difference to `side`. Never waits. Deafened consumes the sound without playing it,
    /// so the picture and the sound stay together. Share sound only uses the room voice leaves:
    /// a loud share can clip, the voice under it cannot.
    pub(super) fn mix(&self, mid: &mut [f32], side: &mut [f32], deafened: bool) {
        side.fill(0.0);
        let Ok(mut ring) = self.ring.try_lock() else { return };
        let mut missing = 0u64;
        for (voice, side) in mid.iter_mut().zip(side) {
            let Ok([left, right]) = ring.pop() else {
                missing += 1;
                continue;
            };
            if deafened {
                continue;
            }
            let room = 1.0 - voice.abs();
            let added = (0.5 * (left + right)).clamp(-room, room);
            *voice += added;
            *side = (0.5 * (left - right)).clamp(-room, room);
        }
        if missing > 0 && self.active() {
            self.counters.ring_underruns.fetch_add(missing, Ordering::Relaxed);
        }
    }
}

/// Ends the share thread when the last owner goes.
struct Stop(Arc<AtomicBool>);
impl Drop for Stop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Native playout of every watched share. It outlives voice runs: a reconnect replaces the
/// pipeline reading [`ShareBus`], not the streams.
pub struct SharePlayout {
    input: SyncSender<Input>,
    bus: Arc<ShareBus>,
    counters: Arc<Counters>,
    gain: Arc<[AtomicU32; SCREEN_SLOTS]>,
    _stop: Stop,
}

impl SharePlayout {
    pub fn new() -> Result<Self> {
        let (input, inbox) = sync_channel(INPUT_QUEUE);
        let (tx, rx) = RingBuffer::new(RING_CAPACITY);
        let counters = Arc::<Counters>::default();
        let bus =
            Arc::new(ShareBus { ring: Mutex::new(rx), active: AtomicBool::new(false), counters: counters.clone() });
        let gain = Arc::new(std::array::from_fn(|_| AtomicU32::new(1.0f32.to_bits())));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = Worker {
            inbox,
            ring: tx,
            streams: (0..SCREEN_SLOTS).map(|_| None).collect(),
            bus: bus.clone(),
            counters: counters.clone(),
            gain: Arc::clone(&gain),
            stop: stop.clone(),
            block: vec![[0.0; 2]; RING_TARGET],
        };
        // Deliberately not promoted: voice's realtime thread is the one that must not lose time.
        thread::Builder::new().name("gwar-share-sound".into()).spawn(move || worker.run())?;
        Ok(Self { input, bus, counters, gain, _stop: Stop(stop) })
    }

    pub fn bus(&self) -> Arc<ShareBus> {
        self.bus.clone()
    }

    /// Prepares a slot for `run`'s frames; a newer run forgets the older run's decoder and queue.
    pub fn open(&self, slot: usize, run: u64) {
        if slot < SCREEN_SLOTS {
            self.send(Input::Open { slot, run });
        }
    }

    pub fn close(&self, slot: usize) {
        if slot < SCREEN_SLOTS {
            self.send(Input::Close { slot });
        }
    }

    /// Volume of one slot, 0 to 1; muting is 0.
    pub fn set_gain(&self, slot: usize, gain: f32) {
        if let Some(value) = self.gain.get(slot) {
            let gain = if gain.is_finite() { gain.clamp(0.0, 1.0) } else { 0.0 };
            value.store(gain.to_bits(), Ordering::Relaxed);
        }
    }

    /// Never blocks; a full queue costs share sound, not the caller.
    pub fn push(&self, frame: ShareFrame) {
        self.counters.frames.fetch_add(1, Ordering::Relaxed);
        self.send(Input::Frame(frame, Instant::now()));
    }

    fn send(&self, input: Input) {
        if let Err(TrySendError::Full(_)) = self.input.try_send(input) {
            self.counters.input_drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn stats(&self) -> ShareStats {
        let read = |value: &AtomicU64| value.load(Ordering::Relaxed);
        let c = &self.counters;
        ShareStats {
            frames: read(&c.frames),
            decoded: read(&c.decoded),
            concealed: read(&c.concealed),
            late_drops: read(&c.late_drops),
            backlog_drops: read(&c.backlog_drops),
            input_drops: read(&c.input_drops),
            ring_underruns: read(&c.ring_underruns),
        }
    }
}

struct Queued {
    rtp_time: u32,
    opus: Vec<u8>,
}

/// Ordered queue of one stream, played a little behind arrival. Unlike voice's buffer it never
/// edits the signal: music has no silences to hide a cut, so it keeps a slightly larger delay.
struct Jitter {
    queue: VecDeque<Queued>,
    /// The RTP time of the next packet to play.
    next: Option<u32>,
    started: bool,
    last_arrival: Option<(Instant, u32)>,
    /// Largest recent arrival error in seconds, decaying.
    deviation: f64,
}

/// Wrapping RTP order.
fn ahead(a: u32, b: u32) -> i32 {
    a.wrapping_sub(b) as i32
}

impl Jitter {
    fn new() -> Self {
        Self { queue: VecDeque::new(), next: None, started: false, last_arrival: None, deviation: 0.0 }
    }

    fn delay(&self) -> Duration {
        Duration::from_secs_f64(
            (FRAME_MS as f64 / 1000.0 + 2.0 * self.deviation).clamp(MIN_DELAY.as_secs_f64(), MAX_DELAY.as_secs_f64()),
        )
    }

    fn observe(&mut self, rtp_time: u32, now: Instant) {
        if let Some((at, time)) = self.last_arrival
            && ahead(rtp_time, time) > 0
        {
            let sent = f64::from(ahead(rtp_time, time)) / f64::from(RATE);
            let error = (now.saturating_duration_since(at).as_secs_f64() - sent).abs();
            self.deviation = error.max(self.deviation * 0.995);
        }
        if self.last_arrival.is_none_or(|(_, time)| ahead(rtp_time, time) > 0) {
            self.last_arrival = Some((now, rtp_time));
        }
    }

    /// False for a packet that can no longer play or a duplicate.
    fn push(&mut self, packet: Queued, now: Instant) -> bool {
        self.observe(packet.rtp_time, now);
        if self.next.is_some_and(|next| ahead(packet.rtp_time, next) < 0) {
            return false;
        }
        let at = self.queue.iter().rposition(|q| ahead(q.rtp_time, packet.rtp_time) <= 0);
        if let Some(index) = at
            && self.queue[index].rtp_time == packet.rtp_time
        {
            return false;
        }
        self.queue.insert(at.map_or(0, |index| index + 1), packet);
        true
    }

    fn queued(&self) -> Duration {
        Duration::from_millis(u64::from(FRAME_MS) * self.queue.len() as u64)
    }

    /// Whether playback has started, after the first packets waited out the delay.
    fn ready(&mut self) -> bool {
        self.started |= !self.queue.is_empty() && self.queued() >= self.delay();
        self.started
    }

    fn restart(&mut self) {
        self.started = false;
        self.next = None;
    }

    /// A stall's backlog goes, oldest first; returns how many packets.
    fn trim(&mut self) -> u64 {
        let mut dropped = 0;
        while self.started && self.queued() > self.delay() + BACKLOG_MARGIN {
            self.queue.pop_front();
            self.next = None;
            dropped += 1;
        }
        dropped
    }

    /// The next packet in order, or `None` when its turn has come and it did not arrive.
    fn pop(&mut self) -> Option<Queued> {
        let front = self.queue.front()?;
        match self.next {
            Some(next) if ahead(front.rtp_time, next) > 0 => None,
            _ => self.queue.pop_front(),
        }
    }

    fn skip(&mut self) {
        self.next = self.next.map(|next| next.wrapping_add(FRAME as u32));
    }
}

struct Stream {
    run: u64,
    jitter: Jitter,
    decoder: Decoder,
    pcm: VecDeque<[f32; 2]>,
    /// Read position between `pcm[0]` and `pcm[1]`.
    fraction: f64,
    concealing: u32,
    scratch: Vec<f32>,
}

impl Stream {
    fn new(run: u64) -> Result<Self> {
        Ok(Self {
            run,
            jitter: Jitter::new(),
            decoder: Decoder::new(SampleRate::Hz48000, Channels::Stereo)?,
            pcm: VecDeque::with_capacity(2 * MAX_PACKET_SAMPLES),
            fraction: 0.0,
            concealing: 0,
            scratch: vec![0.0; 2 * MAX_PACKET_SAMPLES],
        })
    }

    fn decode(&mut self, packet: Option<&[u8]>, counters: &Counters) {
        let input = packet.and_then(|data| Packet::try_from(data).ok());
        let is_loss = input.is_none();
        let end = if is_loss { 2 * FRAME } else { self.scratch.len() };
        let Ok(output) = MutSignals::try_from(&mut self.scratch[..end]) else { return };
        let Ok(samples) = self.decoder.decode_float(input, output, false) else { return };
        let (count, source) = if is_loss {
            self.concealing += 1;
            (&counters.concealed, samples)
        } else {
            self.concealing = 0;
            (&counters.decoded, samples)
        };
        count.fetch_add(1, Ordering::Relaxed);
        self.pcm.extend(self.scratch[..2 * source].as_chunks::<2>().0.iter().copied());
    }

    /// Decodes until `frames` stereo samples are ready, or the stream has nothing to give yet.
    fn fill(&mut self, frames: usize, counters: &Counters) {
        counters.backlog_drops.fetch_add(self.jitter.trim(), Ordering::Relaxed);
        while self.pcm.len() < frames + 2 {
            if !self.jitter.ready() {
                return;
            }
            match self.jitter.pop() {
                Some(packet) => {
                    let samples = opus_samples(&packet.opus).unwrap_or(FRAME as u32);
                    self.jitter.next = Some(packet.rtp_time.wrapping_add(samples));
                    self.decode(Some(&packet.opus), counters);
                }
                None if self.jitter.queue.is_empty() && self.concealing >= CONCEAL_LIMIT => {
                    self.jitter.restart();
                    self.concealing = 0;
                    return;
                }
                None => {
                    self.decode(None, counters);
                    self.jitter.skip();
                }
            }
        }
    }

    /// Adds `gain` times this stream to `mix`. Drift is taken up by reading very slightly faster.
    fn mix(&mut self, mix: &mut [[f32; 2]], gain: f32) {
        let step = 1.0
            + if self.jitter.started && self.jitter.queued() > self.jitter.delay() + DRIFT_MARGIN {
                DRIFT_RATE
            } else {
                0.0
            };
        for out in mix {
            let (Some(&now), Some(&next)) = (self.pcm.front(), self.pcm.get(1)) else { return };
            let blend = self.fraction as f32;
            out[0] += gain * (now[0] + (next[0] - now[0]) * blend);
            out[1] += gain * (now[1] + (next[1] - now[1]) * blend);
            self.fraction += step;
            while self.fraction >= 1.0 {
                self.fraction -= 1.0;
                self.pcm.pop_front();
            }
        }
    }
}

struct Worker {
    inbox: Receiver<Input>,
    ring: Producer<[f32; 2]>,
    streams: Vec<Option<Stream>>,
    bus: Arc<ShareBus>,
    counters: Arc<Counters>,
    gain: Arc<[AtomicU32; SCREEN_SLOTS]>,
    stop: Arc<AtomicBool>,
    block: Vec<[f32; 2]>,
}

impl Worker {
    fn run(mut self) {
        while !self.stop.load(Ordering::Relaxed) {
            match self.inbox.recv_timeout(Duration::from_millis(5)) {
                Ok(input) => {
                    self.handle(input);
                    while let Ok(input) = self.inbox.try_recv() {
                        self.handle(input);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            self.produce();
        }
    }

    fn handle(&mut self, input: Input) {
        match input {
            Input::Open { slot, run } => {
                if self.streams[slot].as_ref().is_none_or(|stream| run > stream.run)
                    && let Ok(stream) = Stream::new(run)
                {
                    self.streams[slot] = Some(stream);
                }
            }
            Input::Close { slot } => self.streams[slot] = None,
            Input::Frame(frame, arrived) => {
                let Some(stream) = self.streams.get_mut(frame.slot).and_then(Option::as_mut) else { return };
                let duration = opus_samples(&frame.opus).map_or(0, |samples| samples as usize);
                if stream.run != frame.run
                    || frame.opus.len() > MAX_OPUS_BYTES
                    || duration == 0
                    || duration > MAX_PACKET_SAMPLES
                    || stream.jitter.queue.len() >= MAX_QUEUED
                    || !stream.jitter.push(Queued { rtp_time: frame.rtp_time, opus: frame.opus }, arrived)
                {
                    self.counters.late_drops.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        self.bus.active.store(self.streams.iter().any(Option::is_some), Ordering::Relaxed);
    }

    /// Tops the ring up to its target. Silence is also written, so one quiet or stalled stream
    /// cannot hold the others back; an idle engine writes nothing.
    fn produce(&mut self) {
        if !self.bus.active() {
            return;
        }
        let held = self.ring.buffer().capacity() - self.ring.slots();
        let wanted = RING_TARGET.saturating_sub(held);
        if wanted == 0 {
            return;
        }
        let block = &mut self.block[..wanted];
        block.fill([0.0; 2]);
        for (slot, stream) in self.streams.iter_mut().enumerate() {
            let Some(stream) = stream else { continue };
            stream.fill(wanted, &self.counters);
            stream.mix(block, f32::from_bits(self.gain[slot].load(Ordering::Relaxed)));
        }
        for frame in block.iter() {
            let _ = self.ring.push(*frame);
        }
    }
}

/// How much captured stereo may wait for the webview: anything older is gone, not delayed.
const CAPTURE_LIMIT: usize = (RATE / 5) as usize * 2;

/// Captured system sound on its way to the webview's sender, as interleaved 48 kHz stereo.
/// One capture thread writes and one bridge call reads. A stalled reader loses the oldest sound;
/// it can neither grow this queue nor make the capture thread wait.
pub struct CaptureQueue {
    samples: Mutex<CaptureState>,
    ready: tokio::sync::Notify,
    closed: AtomicBool,
}

#[derive(Default)]
struct CaptureState {
    samples: VecDeque<f32>,
    dropped: u64,
}

impl Default for CaptureQueue {
    fn default() -> Self {
        Self { samples: Mutex::default(), ready: tokio::sync::Notify::new(), closed: AtomicBool::new(false) }
    }
}

impl CaptureQueue {
    /// `stereo` is interleaved. Called from the capture thread; the lock is only held to copy.
    pub fn write(&self, stereo: &[f32]) {
        let mut state = self.samples.lock().unwrap_or_else(|e| e.into_inner());
        state.samples.extend(stereo.iter().copied());
        let excess = state.samples.len().saturating_sub(CAPTURE_LIMIT);
        if excess > 0 {
            // Whole frames: the channels must not swap.
            let excess = (excess.div_ceil(2) * 2).min(state.samples.len());
            state.samples.drain(..excess);
            state.dropped += excess as u64 / 2;
        }
        drop(state);
        self.ready.notify_one();
    }

    /// Ends the bridge's wait for sound: the capture stopped or failed.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.ready.notify_one();
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Stereo frames lost to a stalled reader.
    pub fn dropped(&self) -> u64 {
        self.samples.lock().unwrap_or_else(|e| e.into_inner()).dropped
    }

    fn take(&self) -> Vec<f32> {
        self.samples.lock().unwrap_or_else(|e| e.into_inner()).samples.drain(..).collect()
    }

    /// What has accumulated, waiting up to `timeout` for the first sound. Empty when there was
    /// none or the capture closed. A promise, not a timer, wakes the webview: a hidden window's
    /// timers are throttled, and this must keep flowing while another app is in front.
    pub async fn read(&self, timeout: Duration) -> Vec<f32> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.ready.notified();
            let samples = self.take();
            if !samples.is_empty() || self.is_closed() {
                return samples;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Vec::new();
            }
        }
    }
}

/// The largest batch a webview may send: 4 slots × 16 packets × Opus's largest.
const MAX_BATCH: usize = 16 + 4 * 16 * (7 + 1275);

/// `owner: u64, run: u64`, then per frame `slot: u8, rtp_time: u32, length: u16, opus`, little-endian.
pub fn parse_frames(body: &[u8]) -> Option<(u64, Vec<ShareFrame>)> {
    if body.len() > MAX_BATCH {
        return None;
    }
    let (header, mut rest) = body.split_at_checked(16)?;
    let owner = u64::from_le_bytes(header[..8].try_into().ok()?);
    let run = u64::from_le_bytes(header[8..].try_into().ok()?);
    let mut frames = Vec::new();
    while !rest.is_empty() {
        let (head, tail) = rest.split_at_checked(7)?;
        let slot = usize::from(head[0]);
        let rtp_time = u32::from_le_bytes(head[1..5].try_into().ok()?);
        let length = usize::from(u16::from_le_bytes(head[5..7].try_into().ok()?));
        let (opus, tail) = tail.split_at_checked(length)?;
        frames.push(ShareFrame { slot, run, rtp_time, opus: opus.to_vec() });
        rest = tail;
    }
    Some((owner, frames))
}

#[cfg(test)]
mod tests;
