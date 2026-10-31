//! A viewer's watch transport in simulated time, behind an emulated downlink. The SFU side runs
//! the screen task's own egress path (`enqueue`, `release`, `drive`, `allocate`) against a str0m
//! viewer; synthetic shares stand in for publishers. Every datagram is recorded at the SFU's
//! socket send, originals against their arrival from the publisher (docs/screen-share.md,
//! "Queues and repairs").
//!
//! What it leaves out: the publish transports and ingress (3a's tests cover them), and the
//! per-viewer keyframe budget; a share answers keyframe requests at most every 500 ms, as the
//! per-share throttle does.

use std::collections::VecDeque;

use str0m::{change::SdpAnswer, media::Direction};

use super::*;

pub const MS: Duration = Duration::from_millis(1);
/// RTP payload bytes per packet.
const PACKET: usize = 1100;
/// One-way propagation delay of either direction.
const DELAY: Duration = Duration::from_millis(10);
/// Per-datagram overhead below RTP on the wire (IPv4 and UDP).
const WIRE: usize = 28;

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// Picks datagrams for a link to drop.
pub type Filter = Box<dyn FnMut(&[u8]) -> bool>;

/// One direction of a path: a FIFO bottleneck at `rate` (none: unconstrained) whose queue holds
/// at most `queue` of sending time, drop-tail; random loss; then the propagation delay.
pub struct Link {
    pub rate: Option<u64>,
    pub delay: Duration,
    pub queue: Duration,
    pub loss: f64,
    busy: Instant,
    flight: VecDeque<(Instant, Vec<u8>)>,
    seed: u64,
    pub dropped: u64,
    /// Drops the datagrams it picks.
    pub filter: Option<Filter>,
}

impl Link {
    fn new(rate: Option<u64>, now: Instant) -> Self {
        Self {
            rate,
            delay: DELAY,
            queue: Duration::from_millis(100),
            loss: 0.0,
            busy: now,
            flight: VecDeque::new(),
            seed: 0x9e37_79b9_7f4a_7c15,
            dropped: 0,
            filter: None,
        }
    }

    fn random(&mut self) -> f64 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        (self.seed >> 11) as f64 / (1u64 << 53) as f64
    }

    fn send(&mut self, now: Instant, datagram: &[u8]) {
        if self.filter.as_mut().is_some_and(|f| f(datagram)) || (self.loss > 0.0 && self.random() < self.loss) {
            self.dropped += 1;
            return;
        }
        let start = self.busy.max(now);
        if let Some(rate) = self.rate {
            if start - now > self.queue {
                self.dropped += 1;
                return;
            }
            self.busy = start + Duration::from_secs_f64((datagram.len() + WIRE) as f64 * 8.0 / rate as f64);
        } else {
            self.busy = now;
        }
        self.flight.push_back((self.busy + self.delay, datagram.to_vec()));
    }

    fn due(&mut self, now: Instant) -> Vec<Vec<u8>> {
        let mut due = Vec::new();
        while self.flight.front().is_some_and(|(at, _)| *at <= now) {
            due.push(self.flight.pop_front().unwrap().1);
        }
        due
    }
}

/// What a datagram the SFU sent was, by its clear header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// On a slot's main SSRC; `arrival` from the publisher.
    Original { slot: usize, seq: u16, arrival: Instant },
    /// On a slot's RTX SSRC: a repair, or `blank` padding.
    Repair { slot: usize, blank: bool },
    /// On str0m's probe SSRC.
    Probe,
    /// RTCP, STUN, DTLS.
    Control,
}

#[derive(Debug, Clone, Copy)]
pub struct Sent {
    pub at: Instant,
    pub len: usize,
    pub kind: Kind,
}

/// A synthetic share: frames of `bitrate / fps`, keyframes of `keyframe` bytes on the first
/// frame, every `every`, and when a viewer's slot asks.
pub struct Share {
    pub origin: Arc<ScreenOrigin>,
    pub slot: usize,
    pub bitrate: u64,
    pub fps: u32,
    pub keyframe: usize,
    pub every: Option<Duration>,
    /// Sends nothing while false.
    pub on: bool,
    source: u64,
    timestamp: u32,
    next: Instant,
    last_key: Option<Instant>,
    wants_key: bool,
    meter: ShareMeter,
    /// When each keyframe was produced, its size, and the viewer's `B` then (bits per second).
    pub keyframes: Vec<(Instant, usize, f64)>,
    /// Packets its slot refused.
    pub refused: u64,
}

/// A payload whose bytes follow from its source number, so the viewer's copy can be checked.
pub fn payload(source: u64, first: bool, len: usize) -> Arc<[u8]> {
    let mut bytes = vec![if first { 0x08 } else { 0x00 }];
    bytes.extend_from_slice(&source.to_be_bytes());
    bytes
        .extend((0..len.saturating_sub(bytes.len())).map(|i| (source as usize).wrapping_mul(31).wrapping_add(i) as u8));
    Arc::from(bytes)
}

/// The source number a payload was made from.
pub fn source_of(payload: &[u8]) -> Option<u64> {
    Some(u64::from_be_bytes(payload.get(1..9)?.try_into().ok()?))
}

pub struct Sim {
    pub w: Watcher,
    pub client: Rtc,
    pub now: Instant,
    pub start: Instant,
    pub down: Link,
    pub up: Link,
    pub shares: Vec<Share>,
    pub sent: Vec<Sent>,
    /// The viewer's RTP: when, the SSRC, the extended sequence number and the payload.
    pub received: Vec<(Instant, u32, u64, Arc<[u8]>)>,
    pub changes: Vec<(Instant, usize, Change)>,
    /// Each climb of a retry in place: when, and the estimate it stepped to.
    pub climbs: Vec<(Instant, u64)>,
    /// Holds the viewer's datagrams back instead of sending them.
    pub hold: bool,
    pub held: Vec<Vec<u8>>,
    /// Every non-STUN datagram the viewer sent, and when.
    pub uplink: Vec<(Instant, Vec<u8>)>,
    /// The allocation runs every 100 ms, as in the screen task.
    pub allocation: bool,
    next_allocation: Instant,
    /// SFU scheduling cadence; the independent receiver continues to timestamp arrivals every ms.
    pub server_interval: Duration,
    next_server: Instant,
    arrivals: HashMap<(usize, u16), Instant>,
    /// Queue residence seen by independent 100 kbps voice on the same bottleneck.
    voice: bool,
    /// Synthetic screen sound uses the production audio socket bucket, then the same FIFO.
    share_audio: usize,
    voice_queue: Vec<Duration>,
    /// Worst queue voice met within `JOIN` of the start, and from then to 15 s.
    voice_join_queue_max: Duration,
    voice_startup_queue_max: Duration,
    /// Every voice sample: when, and the queue it met.
    voice_trace: Vec<(Instant, Duration)>,
}

/// An optimistic start may overshoot a link slower than the share's start estimate until the
/// estimator backs off on its delay and loss feedback: this long after joining. On the 5 Mbps
/// voice-headroom link with voice flowing, the backoff came within 3.75 s, and voice met 50 ms
/// of queue or more for at most 540 ms of it.
pub const JOIN: Duration = Duration::from_secs(5);

impl Sim {
    /// A connected viewer behind a downlink of `rate` (none: unconstrained).
    pub fn new(rate: Option<u64>) -> Self {
        Self::with_seed(rate, 1)
    }

    /// Seed the thread-local transport IDs before creating either peer. Link loss has
    /// separate, explicit per-direction seeds, so handshake randomness cannot consume it.
    pub fn with_seed(rate: Option<u64>, seed: u64) -> Self {
        let now = str0m::_gwar_sim::start(seed);
        let mut client = Rtc::builder().set_rtp_mode(true).clear_codecs().enable_vp9(true).build(now);
        client.add_local_candidate(Candidate::host(addr(5000), "udp").unwrap());
        // str0m's random mids can collide within one offer: redraw.
        let (offer, pending) = loop {
            let mut api = client.sdp_api();
            let mids: HashSet<Mid> = (0..SCREEN_SLOTS)
                .map(|_| api.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None))
                .collect();
            if mids.len() == SCREEN_SLOTS {
                break api.apply().unwrap();
            }
        };
        let (w, answer) = watcher(
            1,
            7,
            1,
            Lease::new(),
            SCREEN_SLOTS,
            &offer.to_sdp_string(),
            Caps { browser: false, h264: &[], av1: None },
            addr(6000),
            now,
        )
        .unwrap();
        client.sdp_api().accept_answer(pending, SdpAnswer::from_sdp_string(&answer).unwrap()).unwrap();
        let mut sim = Self {
            w,
            client,
            now,
            start: now,
            down: Link::new(rate, now),
            up: Link::new(None, now),
            shares: Vec::new(),
            sent: Vec::new(),
            received: Vec::new(),
            changes: Vec::new(),
            climbs: Vec::new(),
            hold: false,
            held: Vec::new(),
            uplink: Vec::new(),
            allocation: true,
            next_allocation: now,
            server_interval: MS,
            next_server: now,
            arrivals: HashMap::new(),
            voice: false,
            share_audio: 0,
            voice_queue: Vec::new(),
            voice_join_queue_max: Duration::ZERO,
            voice_startup_queue_max: Duration::ZERO,
            voice_trace: Vec::new(),
        };
        for _ in 0..2000 {
            if sim.w.connected && sim.w.slots.len() == SCREEN_SLOTS {
                break;
            }
            sim.tick();
        }
        assert!(sim.w.connected && sim.w.slots.len() == SCREEN_SLOTS, "the viewer connects");
        sim.start = sim.now;
        sim
    }

    /// Starts watching a share in the next free slot, after the ones already watched.
    pub fn watch(&mut self, name: &str, bitrate: u64, fps: u32, keyframe: usize, every: Option<Duration>) -> usize {
        self.watch_capped(name, bitrate, fps, keyframe, every, 8_000_000)
    }

    fn watch_capped(
        &mut self,
        name: &str,
        bitrate: u64,
        fps: u32,
        keyframe: usize,
        every: Option<Duration>,
        cap: u64,
    ) -> usize {
        let slot = self.shares.len();
        let origin = ScreenOrigin::new(1 + slot as SessionId, name.into(), 1, false, false, cap);
        let grant = WatchGrant::new(origin.clone());
        assign(&mut self.w, slot, grant, slot as u64 + 1, self.now);
        self.shares.push(Share {
            origin,
            slot,
            bitrate,
            fps,
            keyframe,
            every,
            on: true,
            source: 1000,
            timestamp: 0,
            next: self.now,
            last_key: None,
            wants_key: true,
            meter: ShareMeter::default(),
            keyframes: Vec::new(),
            refused: 0,
        });
        slot
    }

    /// Joins a share that has been running for a while: its 2 s average and keyframe size are
    /// already measured, as the screen task's allocation tick has them, and it answers the
    /// join's keyframe request with its next frame.
    pub fn join(&mut self, name: &str, bitrate: u64, fps: u32, keyframe: usize, cap: u64) -> usize {
        let frame = Duration::from_secs(1) / fps;
        let mut meter = ShareMeter::default();
        let (mut source, mut timestamp) = (1000, 0u32);
        let mut at = self.now - egress::AVERAGE_WINDOW;
        let mut key = true;
        while at < self.now {
            let size = if key { keyframe } else { (bitrate / 8 / fps as u64) as usize };
            for n in 0..size.div_ceil(PACKET).max(1) {
                let len = if (n + 1) * PACKET >= size { size - n * PACKET } else { PACKET }.max(16);
                meter.record(source, timestamp, len, key && n == 0, at);
                source += 1;
            }
            timestamp = timestamp.wrapping_add(90_000 / fps);
            at += frame;
            key = false;
        }
        let slot = self.shares.len();
        let origin = ScreenOrigin::new(1 + slot as SessionId, name.into(), 1, false, false, cap);
        origin.measured(meter.rate(self.now));
        let grant = WatchGrant::new(origin.clone());
        assign(&mut self.w, slot, grant, slot as u64 + 1, self.now);
        self.shares.push(Share {
            origin,
            slot,
            bitrate,
            fps,
            keyframe,
            every: None,
            on: true,
            source,
            timestamp,
            next: self.now,
            last_key: None,
            wants_key: true,
            meter,
            keyframes: Vec::new(),
            refused: 0,
        });
        slot
    }

    /// `screen.focus` on a share.
    pub fn focus(&mut self, share: usize) {
        let slot = self.shares[share].slot;
        self.w.egress.focus(Some(slot));
        self.w.focus = Some(self.shares[share].origin.share.clone());
    }

    pub fn elapsed(&self) -> Duration {
        self.now - self.start
    }

    pub fn run(&mut self, time: Duration) {
        let end = self.now + time;
        while self.now < end {
            self.tick();
        }
    }

    /// Slot `slot`'s main and RTX SSRCs.
    pub fn ssrcs(&mut self, slot: usize) -> (u32, Option<u32>) {
        let mid = self.w.slots[slot].mid;
        let mut api = self.w.rtc.direct_api();
        let stream = api.stream_tx_by_mid(mid, None).unwrap();
        (*stream.ssrc(), stream.rtx().map(|r| *r))
    }

    fn frames(&mut self) {
        let now = self.now;
        let plan = self.w.egress.plan();
        for share in &mut self.shares {
            if !share.on || now < share.next {
                continue;
            }
            share.next += Duration::from_secs(1) / share.fps;
            share.timestamp = share.timestamp.wrapping_add(90_000 / share.fps);
            let key = share.last_key.is_none()
                || share.every.is_some_and(|every| now - share.last_key.unwrap() >= every)
                || (share.wants_key && now - share.last_key.unwrap() >= Duration::from_millis(500));
            let size = if key { share.keyframe } else { (share.bitrate / 8 / share.fps as u64) as usize };
            if key {
                share.last_key = Some(now);
                share.wants_key = false;
                share.keyframes.push((now, size, plan));
            }
            let packets = size.div_ceil(PACKET).max(1);
            for n in 0..packets {
                let len = if n + 1 == packets { size - n * PACKET } else { PACKET }.max(16);
                let forward = Forward {
                    h264: None,
                    av1: None,
                    dd: None,
                    source: share.source,
                    timestamp: share.timestamp,
                    marker: n + 1 == packets,
                    keyframe: key && n == 0,
                    codec: VideoCodec::Vp9,
                    arrival: now,
                    wallclock: now,
                    payload: payload(share.source, n == 0, len),
                };
                share.source += 1;
                share.meter.record(forward.source, forward.timestamp, len, forward.keyframe, now);
                if !enqueue(&mut self.w, share.slot, &share.origin, &forward, now) {
                    share.refused += 1;
                    let slot = &mut self.w.slots[share.slot];
                    if self.w.egress.paused(share.slot).is_none()
                        && slot.grant.is_some()
                        && slot.segment.waiting()
                        && slot.asked.is_none_or(|at| now.saturating_duration_since(at) >= WAITING_RETRY)
                    {
                        slot.asked = Some(now);
                        share.wants_key = true;
                    }
                }
            }
        }
    }

    fn classify(&mut self, contents: &[u8]) -> Kind {
        if !is_rtp(contents) {
            return Kind::Control;
        }
        let ssrc = u32::from_be_bytes(contents[8..12].try_into().unwrap());
        let seq = u16::from_be_bytes([contents[2], contents[3]]);
        for slot in 0..self.w.slots.len() {
            let mid = self.w.slots[slot].mid;
            let mut api = self.w.rtc.direct_api();
            let Some(stream) = api.stream_tx_by_mid(mid, None) else { continue };
            if *stream.ssrc() == ssrc {
                let arrival = self.arrivals.get(&(slot, seq)).copied().expect("an original was queued");
                return Kind::Original { slot, seq, arrival };
            }
            if stream.rtx().is_some_and(|rtx| *rtx == ssrc) {
                return Kind::Repair { slot, blank: contents[0] & 0x20 != 0 };
            }
        }
        Kind::Probe
    }

    /// One millisecond of both sides.
    pub fn tick(&mut self) {
        let now = self.now;
        self.frames();
        let server_due = now >= self.next_server;
        if server_due {
            self.next_server = now + self.server_interval;
            for (lane, out, arrival) in self.w.egress.pending() {
                self.arrivals.insert((lane, out as u16), arrival);
            }
            let mut requests = std::mem::take(&mut self.w.dropped_lanes);
            let mut out = Vec::new();
            drive(&mut self.w, now, &mut requests, &mut |c: &[u8], _| out.push(c.to_vec()));
            for datagram in out {
                let kind = self.classify(&datagram);
                self.sent.push(Sent { at: now, len: datagram.len(), kind });
                self.down.send(now, &datagram);
            }
            for slot in requests {
                if let Some(share) = self.shares.iter_mut().find(|s| s.slot == slot) {
                    share.wants_key = true;
                }
            }
            if self.allocation && now >= self.next_allocation {
                self.next_allocation = now + ALLOCATION_TICK;
                let rates: HashMap<String, ShareRate> = self
                    .shares
                    .iter_mut()
                    .map(|s| {
                        let rate = s.meter.rate(now);
                        s.origin.measured(rate);
                        (s.origin.share.clone(), rate)
                    })
                    .collect();
                let attempt = self.w.attempt;
                let change = allocate(&mut self.w, now, &rates, true);
                if change.is_none()
                    && let (Some(before), Some(after)) = (attempt, self.w.attempt)
                    && after.start > before.start
                {
                    self.climbs.push((now, after.start));
                }
                if let Some((slot, change)) = change {
                    self.changes.push((now, slot, change));
                    if change == Change::Resume
                        && let Some(share) = self.shares.iter_mut().find(|s| s.slot == slot)
                    {
                        share.wants_key = true;
                    }
                }
            }
        }
        if self.share_audio > 0 && self.elapsed().as_millis().is_multiple_of(20) {
            for _ in 0..self.share_audio {
                // Saturate each 128 kb/s wire allowance, not merely typical 64 kb/s Opus.
                if self.w.egress.charge(egress::Class::Audio, 320, now)
                    && let Some(rate) = self.down.rate
                {
                    self.down.busy =
                        self.down.busy.max(now) + Duration::from_secs_f64((320 + WIRE) as f64 * 8.0 / rate as f64);
                }
            }
        }
        if self.voice && self.elapsed().as_millis().is_multiple_of(10) {
            let queued = self.down.busy.saturating_duration_since(now);
            self.voice_trace.push((now, queued));
            if self.elapsed() >= Duration::from_secs(15) {
                self.voice_queue.push(queued);
            } else if self.elapsed() < JOIN {
                self.voice_join_queue_max = self.voice_join_queue_max.max(queued);
            } else {
                self.voice_startup_queue_max = self.voice_startup_queue_max.max(queued);
            }
            // Voice bypasses the SFU screen queues but serializes on the physical link.
            if let Some(rate) = self.down.rate {
                self.down.busy = self.down.busy.max(now) + Duration::from_secs_f64(125.0 * 8.0 / rate as f64);
            }
        }
        for datagram in self.down.due(now) {
            let receive = Receive {
                proto: Protocol::Udp,
                source: addr(6000),
                destination: addr(5000),
                contents: datagram.as_slice().try_into().unwrap(),
            };
            self.client.handle_input(Input::Receive(now, receive)).unwrap();
            // RTP mode has one pending packet event: drain before the next input.
            self.drive_client();
        }
        self.drive_client();
        if server_due {
            for datagram in self.up.due(now) {
                self.deliver(&datagram);
            }
        }
        self.now += MS;
        if server_due {
            self.w.rtc.handle_input(Input::Timeout(self.now)).unwrap();
        }
        self.client.handle_input(Input::Timeout(self.now)).unwrap();
    }

    fn drive_client(&mut self) {
        let now = self.now;
        let mut due = 0;
        loop {
            match self.client.poll_output().unwrap() {
                Output::Timeout(at) if at <= now && due < DUE_TIMEOUTS => {
                    due += 1;
                    self.client.handle_input(Input::Timeout(now)).unwrap();
                }
                Output::Timeout(_) => break,
                Output::Transmit(t) => {
                    if !matches!(t.contents.first(), Some(0..=3)) {
                        self.uplink.push((now, t.contents.to_vec()));
                    }
                    if self.hold {
                        self.held.push(t.contents.to_vec());
                    } else {
                        self.up.send(now, &t.contents);
                    }
                }
                Output::Event(RtcEvent::RtpPacket(p)) => {
                    self.received.push((now, *p.header.ssrc, *p.seq_no, p.payload.clone()));
                }
                Output::Event(_) => {}
            }
        }
    }

    /// Hands the SFU a datagram from the viewer.
    pub fn deliver(&mut self, datagram: &[u8]) {
        let receive = Receive {
            proto: Protocol::Udp,
            source: addr(5000),
            destination: addr(6000),
            contents: datagram.try_into().unwrap(),
        };
        self.w.rtc.handle_input(Input::Receive(self.now, receive)).unwrap();
    }

    /// Bytes the SFU sent in `[from, to)`.
    pub fn bytes(&self, from: Instant, to: Instant, kinds: impl Fn(&Kind) -> bool) -> usize {
        self.sent.iter().filter(|s| s.at >= from && s.at < to && kinds(&s.kind)).map(|s| s.len).sum()
    }

    /// The latest original of each slot left at most this long after its arrival.
    pub fn oldest_original(&self, slot: usize) -> Duration {
        self.sent
            .iter()
            .filter_map(|s| match s.kind {
                Kind::Original { slot: sl, arrival, .. } if sl == slot => Some(s.at - arrival),
                _ => None,
            })
            .max()
            .unwrap_or_default()
    }

    pub fn paused(&self, slot: usize) -> Option<ScreenPauseReason> {
        self.w.egress.paused(slot)
    }

    /// The viewer's Try again on `share` (`screen.watch` of a share it already watches).
    pub fn try_again(&mut self, share: usize) {
        let now = self.now;
        let rates: HashMap<String, ShareRate> =
            self.shares.iter_mut().map(|s| (s.origin.share.clone(), s.meter.rate(now))).collect();
        let slot = self.shares[share].slot;
        if ask_retry(&mut self.w, slot, &rates, now) {
            self.changes.push((now, slot, Change::Resume));
            self.shares[share].wants_key = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::egress::{DEADLINE, EXTRAS, PLAN, RAMP, RELEASE_BURST, RELEASE_BURST_FLOOR, extras_burst};
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);
    const KEYFRAME: usize = 204_800;

    fn summary(sim: &Sim) -> String {
        format!(
            "t={:?} estimate={} paused={:?} changes={:?} counters={:?}",
            sim.elapsed(),
            sim.w.egress.estimate(),
            (0..sim.shares.len()).map(|s| sim.paused(s)).collect::<Vec<_>>(),
            sim.changes.iter().map(|(at, slot, c)| (*at - sim.start, *slot, *c)).collect::<Vec<_>>(),
            sim.w.egress.counters
        )
    }

    /// A Text-preset share, 0.4 Mbps at 5 fps, opened by a small keyframe; then `keyframes`
    /// switches it to 204 KB keyframes every 2 s.
    fn text(sim: &mut Sim) -> usize {
        sim.watch("text", 400_000, 5, 20_000, None)
    }

    fn keyframes(sim: &mut Sim, share: usize) {
        sim.shares[share].keyframe = KEYFRAME;
        sim.shares[share].every = Some(2 * SECOND);
    }

    fn pauses(sim: &Sim, slot: usize) -> Vec<Duration> {
        sim.changes
            .iter()
            .filter(|(_, s, c)| *s == slot && matches!(c, Change::Pause(_)))
            .map(|(at, _, _)| *at - sim.start)
            .collect()
    }

    /// Voice queue at least this long hurts a call: about what a jitter buffer absorbs.
    const HURT: Duration = Duration::from_millis(50);

    /// How long voice met at least `HURT` of queue in `[from, to)`, and the most it met.
    fn voice_hurt(sim: &Sim, from: Instant, to: Instant) -> (Duration, Duration) {
        let samples = || sim.voice_trace.iter().filter(|(at, _)| *at >= from && *at < to).map(|(_, q)| *q);
        (samples().filter(|q| *q >= HURT).count() as u32 * 10 * MS, samples().max().unwrap_or_default())
    }

    /// Voice around each resume of slot 0 after its first pause: from the resume to the next
    /// pause, at least its ramp and a little more.
    fn retries_voice(sim: &Sim) -> Vec<(Duration, Duration, Duration)> {
        let changes: Vec<_> = sim.changes.iter().filter(|(_, s, _)| *s == 0).collect();
        changes
            .iter()
            .enumerate()
            .filter(|(_, (_, _, c))| *c == Change::Resume)
            .map(|(n, (at, _, _))| {
                let next = changes.get(n + 1).map_or(sim.now, |(t, _, _)| *t);
                let (hurt, max) = voice_hurt(sim, *at, next.max(*at + RAMP + 2 * SECOND).min(sim.now));
                (*at - sim.start, hurt, max)
            })
            .collect()
    }

    fn resumes(sim: &Sim, slot: usize) -> usize {
        sim.changes.iter().filter(|(_, s, c)| *s == slot && *c == Change::Resume).count()
    }

    /// Drops the next original of `ssrc` on the downlink.
    fn lose_next(sim: &mut Sim, ssrc: u32) {
        let mut done = false;
        sim.down.filter = Some(Box::new(move |d: &[u8]| {
            let hit = !done && is_rtp(d) && d[0] & 0x20 == 0 && d[8..12] == ssrc.to_be_bytes();
            done |= hit;
            hit
        }));
    }

    /// Original or repair media of `slot` the SFU sent from `from` on (blank padding excluded).
    fn media_of(sim: &Sim, slot: usize, from: Instant) -> usize {
        sim.sent
            .iter()
            .filter(|s| s.at >= from)
            .filter(|s| match s.kind {
                Kind::Original { slot: sl, .. } => sl == slot,
                Kind::Repair { slot: sl, blank } => sl == slot && !blank,
                _ => false,
            })
            .count()
    }

    /// The viewer received this source number.
    fn received(sim: &Sim, source: u64) -> bool {
        sim.received.iter().any(|(_, _, _, p)| source_of(p) == Some(source))
    }

    #[test]
    fn seeded_simulations_replay_the_same_transport_and_allocation_trace() {
        use std::hash::{DefaultHasher, Hash, Hasher};

        fn replay(seed: u64) -> u64 {
            let mut sim = Sim::with_seed(Some(20_000_000), seed);
            sim.down.loss = 0.02;
            sim.watch("game", 8_000_000, 60, 200_000, None);
            let mut trace = DefaultHasher::new();
            while sim.elapsed() < 90 * SECOND {
                let elapsed = sim.elapsed();
                sim.down.rate =
                    Some(if (COLLAPSE..COLLAPSE + 30 * SECOND).contains(&elapsed) { 1_000_000 } else { 20_000_000 });
                sim.server_interval = if elapsed.as_millis() % 3500 < 150 { 150 * MS } else { 20 * MS };
                sim.tick();
                sim.w.egress.estimate().hash(&mut trace);
            }
            for sent in &sim.sent {
                let kind = match sent.kind {
                    Kind::Original { slot, seq, arrival } => (0, slot, seq, arrival - sim.start),
                    Kind::Repair { slot, blank } => (1 + u8::from(blank), slot, 0, Duration::ZERO),
                    Kind::Probe => (3, 0, 0, Duration::ZERO),
                    // DTLS stays cryptographically random; it is established before
                    // media starts and its ciphertext is not a simulation input.
                    Kind::Control => continue,
                };
                (sent.at - sim.start, sent.len, kind).hash(&mut trace);
            }
            for (at, ssrc, seq, payload) in &sim.received {
                (*at - sim.start, ssrc, seq, source_of(payload)).hash(&mut trace);
            }
            for (at, slot, change) in &sim.changes {
                (*at - sim.start, slot, format!("{change:?}")).hash(&mut trace);
            }
            let c = sim.w.egress.counters;
            // Certificate/signature lengths can vary during the secure handshake.
            // Compare all media counters; control ciphertext is deliberately unseeded.
            (
                c.original_bytes,
                c.extra_bytes,
                c.extras_dropped,
                c.originals_dropped,
                c.evicted,
                c.rejected,
                c.failed_keyframes,
                c.pauses,
                c.escalations,
            )
                .hash(&mut trace);
            trace.finish()
        }

        let first = replay(1);
        assert_eq!(first, replay(1), "same seed, packet ordering, clock, estimates and allocation");
        assert_ne!(first, replay(2), "the transport seed is exercised");
        println!("deterministic replay fingerprint={first:016x}");
    }

    #[test]
    fn keyframes_have_room_on_unconstrained_and_20_mbps_viewers() {
        for rate in [None, Some(20_000_000)] {
            let mut sim = Sim::new(rate);
            let t = text(&mut sim);
            // Warm-up: the estimate grows from its initial 2.5 Mbps by probing.
            sim.run(RAMP);
            keyframes(&mut sim, t);
            sim.run(20 * SECOND);
            let context = summary(&sim);
            assert!(pauses(&sim, 0).is_empty(), "{rate:?} {context}");
            let c = sim.w.egress.counters;
            assert_eq!((c.evicted, c.rejected, c.failed_keyframes), (0, 0, 0), "{rate:?} {context}");
            assert_eq!((sim.shares[0].refused, sim.down.dropped), (0, 0), "{rate:?} {context}");
            let big: Vec<_> = sim.shares[0].keyframes.iter().filter(|k| k.1 == KEYFRAME).copied().collect();
            assert_eq!(big.len(), 10, "{rate:?}");
            for (at, size, plan) in big {
                let left = sim
                    .sent
                    .iter()
                    .filter(|s| matches!(s.kind, Kind::Original { arrival, .. } if arrival == at))
                    .map(|s| s.at - at)
                    .max()
                    .unwrap();
                let wire = size + size.div_ceil(PACKET) * egress::ORIGINAL_OVERHEAD;
                let bound = Duration::from_secs_f64(wire as f64 * 8.0 / plan) + 10 * MS;
                assert!(left <= bound, "{rate:?}: a keyframe took {left:?}, more than K/B + 10 ms = {bound:?}");
            }
        }
    }

    #[test]
    fn keyframes_overflowing_admission_pause_a_3_mbps_viewer_and_its_retries_back_off() {
        let mut sim = Sim::new(Some(3_000_000));
        sim.voice = true;
        let t = text(&mut sim);
        // Failures count once the slot's ramp is over.
        sim.run(RAMP);
        keyframes(&mut sim, t);
        sim.run(140 * SECOND);
        let context = summary(&sim);
        let voice = retries_voice(&sim);
        println!("3 Mbps text retries voice (at, hurt, max): {voice:?}");
        let c = sim.w.egress.counters;
        assert!(c.rejected > 0 && c.failed_keyframes >= 2, "{context}");
        let paused = pauses(&sim, 0);
        let second = sim.shares[0].keyframes.iter().filter(|k| k.1 == KEYFRAME).nth(1).unwrap().0 - sim.start;
        assert!(paused[0] <= second + ALLOCATION_TICK, "paused within two keyframes: {context}");
        assert_eq!(sim.paused(0), Some(ScreenPauseReason::Bandwidth));
        // While the link stays at 3 Mbps, each retry fails within its ramp. One from 2.5 Mbps
        // cannot admit the keyframe, and the link keeps its estimate: inconclusive, so the next
        // one starts `RETRY_FIRST` after it with room for the keyframe, which fails. The
        // retries from 2.5 Mbps back off twice as long each time: 15 s, 30 s.
        let resumed: Vec<Duration> = sim
            .changes
            .iter()
            .filter(|(_, s, c)| *s == 0 && *c == Change::Resume)
            .map(|(at, _, _)| *at - sim.start)
            .collect();
        assert_eq!(resumed.len(), 4, "{context}");
        assert_eq!(paused.len(), 5, "{context}");
        for (n, at) in resumed.iter().enumerate() {
            let gap = *at - paused[n];
            if n % 2 == 0 {
                let backoff = RETRY_FIRST * (1 << (n / 2));
                assert!(gap >= backoff && gap <= backoff + SECOND, "retry {n} after {gap:?}: {context}");
            } else {
                let spacing = *at - resumed[n - 1];
                assert!(spacing >= RETRY_FIRST && spacing <= RETRY_FIRST + SECOND, "retry {n}: {context}");
            }
            assert!(paused[n + 1] - *at <= RAMP + 2 * SECOND, "retry {n} pauses again: {context}");
        }
        // The retries from 2.5 Mbps never queue voice for long; those with room for the
        // keyframe do, briefly, as the keyframe meets the link.
        for (n, (_, hurt, _)) in voice.iter().enumerate() {
            assert!(if n % 2 == 0 { hurt.is_zero() } else { *hurt < 500 * MS }, "{voice:?}: {context}");
        }
    }

    #[test]
    fn keyframes_missing_the_deadline_pause_the_low_priority_slot_and_spare_the_stage() {
        let mut sim = Sim::new(Some(6_000_000));
        let stage = sim.watch("stage", 3_000_000, 30, 60_000, None);
        sim.focus(stage);
        sim.run(8 * SECOND);
        assert_eq!(sim.changes, [], "the stage share alone fits: {}", summary(&sim));
        let t = text(&mut sim);
        // Past the new slot's ramp, when its failures start to count. Initial BWE probes
        // can temporarily undershoot the stage's wire rate; recovery from those drops is
        // separate from the forced-keyframe priority/deadline interval checked here.
        sim.run(RAMP);
        let measured_from = sim.now;
        let stage_refused = sim.shares[stage].refused;
        keyframes(&mut sim, t);
        sim.run(12 * SECOND);
        let context = summary(&sim);
        let c = sim.w.egress.counters;
        assert!(c.evicted > 0 && c.failed_keyframes >= 2, "keyframes expire: {context}");
        let paused = pauses(&sim, 1);
        assert_eq!(paused.len(), 1, "{context}");
        let second = sim.shares[1].keyframes.iter().filter(|k| k.1 == KEYFRAME).nth(1).unwrap().0 - sim.start;
        // The second keyframe fails at its deadline.
        assert!(paused[0] <= second + DEADLINE + ALLOCATION_TICK, "paused within two keyframes: {context}");
        assert!(pauses(&sim, 0).is_empty(), "{context}");
        assert_eq!(
            sim.shares[0].refused, stage_refused,
            "the stage share lost nothing during forced keyframes: {context}"
        );
        let stage_late = sim
            .sent
            .iter()
            .filter_map(|s| match s.kind {
                Kind::Original { slot: 0, arrival, .. } if arrival >= measured_from => Some(s.at - arrival),
                _ => None,
            })
            .max()
            .unwrap();
        assert!(stage_late < Duration::from_millis(100), "{stage_late:?}: {context}");
    }

    #[test]
    fn two_shares_on_a_constrained_viewer_pause_the_lower_priority_one() {
        for focused in [Some(1), None] {
            let mut sim = Sim::new(Some(6_000_000));
            sim.watch("a", 3_000_000, 30, 60_000, None);
            sim.watch("b", 3_000_000, 30, 60_000, None);
            if let Some(share) = focused {
                sim.focus(share);
            }
            sim.run(30 * SECOND);
            let context = summary(&sim);
            // The stage share comes first; otherwise watch order decides.
            let (kept, paused) = if focused.is_some() { (1, 0) } else { (0, 1) };
            assert_eq!(sim.paused(paused), Some(ScreenPauseReason::Bandwidth), "{context}");
            assert!(pauses(&sim, kept).is_empty(), "{context}");
        }
    }

    #[test]
    fn downlink_keeps_voice_headroom_and_one_share_at_one_percent_loss() {
        voice_headroom(1..=1);
    }

    #[test]
    #[ignore = "full seeded matrix runs in the rust-screen-sim CI job"]
    fn full_voice_headroom_matrix() {
        voice_headroom(1..=8);
    }

    #[test]
    fn silent_audio_reservation_preserves_live_bwe_startup() {
        for cadence in [10, 20] {
            let mut sim = Sim::new(Some(20_000_000));
            sim.server_interval = cadence * MS;
            sim.watch("game", 7_872_000, 60, 200_000, None);
            sim.w.egress.reserve_audio(1, sim.now);
            assert!(sim.w.frozen.is_none(), "capacity discovery must use actual TWCC feedback");
            sim.run(10 * SECOND);
            assert!(!sim.received.is_empty(), "{cadence} ms: video must start in the first ten seconds");
            sim.run(20 * SECOND);
            let payload: usize =
                sim.received.iter().filter(|(at, ..)| *at >= sim.now - 5 * SECOND).map(|(_, _, _, p)| p.len()).sum();
            eprintln!(
                "silent audio: {cadence} ms, estimate={}, video={} bps, changes={:?}",
                sim.w.egress.estimate(),
                payload * 8 / 5,
                sim.changes
            );
            assert!(payload * 8 / 5 >= 7_500_000, "{cadence} ms: sustain the in-budget source");
            assert!(
                !sim.changes.iter().any(|(_, _, change)| matches!(change, Change::Pause(_))),
                "{cadence} ms: a lossless open link must not pause"
            );
        }
    }

    #[test]
    fn share_audio_keeps_voice_inside_existing_steady_and_startup_bounds() {
        for interval in [MS, 5 * MS, 10 * MS] {
            for loss in [0.0, 0.01] {
                let mut sim = Sim::new(Some(5_000_000));
                sim.server_interval = interval;
                sim.down.loss = loss;
                sim.up.loss = loss;
                sim.voice = true;
                sim.share_audio = 2;
                sim.join("a", 3_000_000, 30, 150_000, 8_000_000);
                sim.join("b", 3_000_000, 30, 150_000, 8_000_000);
                sim.w.egress.reserve_audio(2, sim.now);
                sim.run(60 * SECOND);
                sim.voice_queue.sort();
                let p99 = sim.voice_queue[sim.voice_queue.len() * 99 / 100];
                let steady = (10 * MS).max(interval + 2 * MS);
                let startup = (10 * MS).max(2 * interval + 2 * MS);
                assert!(p99 <= steady, "{interval:?} loss={loss} p99={p99:?}");
                assert!(
                    sim.voice_startup_queue_max <= startup,
                    "{interval:?} loss={loss} startup={:?}",
                    sim.voice_startup_queue_max
                );
                assert!(voice_hurt(&sim, sim.start, sim.start + JOIN).0 < SECOND);
            }
        }
    }

    fn voice_headroom(seeds: std::ops::RangeInclusive<u64>) {
        let mut failures = Vec::new();
        for interval in [MS, 5 * MS, 10 * MS] {
            for delay in [Duration::ZERO, DELAY] {
                for loss in [0.0, 0.01] {
                    for seed in seeds.clone() {
                        let mut sim = Sim::new(Some(5_000_000));
                        sim.server_interval = interval;
                        sim.down.delay = delay;
                        sim.up.delay = delay;
                        sim.down.seed = seed;
                        sim.up.seed = seed + 100;
                        sim.down.loss = loss;
                        sim.up.loss = loss;
                        sim.voice = true;
                        // Both shares already run: the first starts optimistically from its
                        // measured rate, above this link; the second joins next to it.
                        sim.join("a", 3_000_000, 30, 150_000, 8_000_000);
                        sim.join("b", 3_000_000, 30, 150_000, 8_000_000);
                        sim.run(60 * SECOND);
                        let context =
                            format!("interval={interval:?} delay={delay:?} loss={loss} seed={seed}: {}", summary(&sim));
                        sim.voice_queue.sort();
                        let p99 = sim.voice_queue[sim.voice_queue.len() * 99 / 100];
                        let ssrc = sim.ssrcs(0).0;
                        let bytes: usize = sim
                            .received
                            .iter()
                            .filter(|(at, source, _, _)| *at >= sim.now - 30 * SECOND && *source == ssrc)
                            .map(|(_, _, _, payload)| payload.len())
                            .sum();
                        let mbps = bytes as f64 * 8.0 / 30.0 / 1e6;
                        println!(
                            "downlink join={:?} hurt={:?} startup={:?} p99={p99:?} mbps={mbps:.3} {context}",
                            sim.voice_join_queue_max,
                            voice_hurt(&sim, sim.start, sim.start + JOIN).0,
                            sim.voice_startup_queue_max
                        );
                        // Keep the original 1 ms case's 10 ms bounds. For the added
                        // coarse-clock cases, steady traffic allows one wake interval
                        // plus a fixed 2 ms wire-packet margin; startup probes span two
                        // wakes plus that margin. Thus 5/10 ms cadences allow at most
                        // 10/12 ms steady and 12/22 ms startup, independent of results.
                        // The calibrated VM gate remains unchanged and authoritative.
                        let steady_limit = (10 * MS).max(interval + 2 * MS);
                        let startup_limit = (10 * MS).max(2 * interval + 2 * MS);
                        // Until the estimator backs off from the optimistic start, voice can
                        // meet the bottleneck's own queue, but only within `JOIN`, and for
                        // less than a second of it. Retries no longer start there (`RETRY_STEP`),
                        // so this is the join's alone; it starts as #190 made it.
                        let join_limit = sim.down.queue + 2 * MS;
                        let join_hurt = voice_hurt(&sim, sim.start, sim.start + JOIN).0;
                        if !pauses(&sim, 0).is_empty()
                            || mbps < 2.7
                            || p99 > steady_limit
                            || sim.voice_join_queue_max > join_limit
                            || join_hurt >= SECOND
                            || sim.voice_startup_queue_max > startup_limit
                        {
                            failures.push(format!(
                                "join={:?} hurt={join_hurt:?} startup={:?} p99={p99:?} mbps={mbps} {context}",
                                sim.voice_join_queue_max, sim.voice_startup_queue_max
                            ));
                        }
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn quality_1440p60_uses_a_12mbit_share_without_an_8mbit_tier_clamp() {
        let mut sim = Sim::with_seed(Some(40_000_000), 144060);
        sim.watch_capped("1440p60", 12_000_000, 60, 200_000, None, 12_000_000);
        while sim.elapsed() < 45 * SECOND {
            sim.tick();
        }
        assert!(sim.changes.is_empty(), "{}", summary(&sim));
        let bytes = sim.bytes(sim.now - 10 * SECOND, sim.now, |k| matches!(k, Kind::Original { .. }));
        assert!(bytes as f64 * 8.0 / 10.0 > 8_000_000.0, "{}", summary(&sim));
    }

    #[test]
    fn game_startup_with_scheduler_jitter_does_not_pause_a_healthy_viewer() {
        for step in [1, 2, 5, 10] {
            let mut sim = Sim::new(None);
            sim.server_interval = step * MS;
            sim.watch("game", 8_000_000, 60, 200_000, None);
            while sim.elapsed() < 45 * SECOND {
                sim.tick();
                // Stall the SFU; the receiver's clock and socket work stay independent.
                if sim.elapsed().as_millis().is_multiple_of(100) {
                    sim.next_server += 10 * MS;
                }
            }
            assert!(sim.changes.is_empty(), "step={step}: {}", summary(&sim));
            let bytes = sim.bytes(sim.now - 10 * SECOND, sim.now, |k| matches!(k, Kind::Original { .. }));
            assert!(bytes as f64 * 8.0 / 10.0 >= 0.6 * 8_000_000.0, "steady media, step={step}: {}", summary(&sim));
        }
    }

    #[test]
    fn game_startup_without_outages_never_pauses_across_cadence_and_loss() {
        game_startup(&[1, 5, 10, 15, 20]);
    }

    #[test]
    #[ignore = "full cadence matrix runs in the rust-screen-sim CI job"]
    fn full_startup_matrix() {
        game_startup(&(1..=20).collect::<Vec<_>>());
    }

    fn game_startup(cadences: &[u32]) {
        for loss in [0.01, 0.02, 0.0] {
            for &cadence in cadences {
                let mut sim = Sim::new(Some(20_000_000));
                sim.server_interval = cadence * MS;
                sim.down.loss = loss;
                sim.watch("game", 8_000_000, 60, 200_000, None);
                sim.run(60 * SECOND);
                let context = format!("cadence={cadence} loss={loss}: {}", summary(&sim));
                assert!(sim.changes.is_empty(), "zero startup pauses: {context}");
                let ssrc = sim.ssrcs(0).0;
                for ago in [20, 10, 0] {
                    let to = sim.now - ago * SECOND;
                    let from = to - 10 * SECOND;
                    let bytes: usize = sim
                        .received
                        .iter()
                        .filter(|(at, source, _, _)| *at >= from && *at < to && *source == ssrc)
                        .map(|(_, _, _, payload)| payload.len())
                        .sum();
                    assert!(bytes as f64 * 8.0 / 10.0 >= 7_200_000.0, "steady received video: {context}");
                }
                println!("startup cadence={cadence} loss={loss} pauses=0 estimate={}", sim.w.egress.estimate());
            }
        }
    }

    #[test]
    fn bandwidth_pause_recovers_when_the_link_clears() {
        let (sim, _, _) = collapse(true);
        assert_eq!(sim.paused(0), None, "{}", summary(&sim));
        assert!(resumes(&sim, 0) > 0, "{}", summary(&sim));
        let bytes = sim.bytes(sim.now - 10 * SECOND, sim.now, |k| matches!(k, Kind::Original { .. }));
        assert!(bytes as f64 * 8.0 / 10.0 >= 0.6 * 4_000_000.0, "media flows after resume: {}", summary(&sim));
    }

    #[test]
    fn game_recovers_after_scheduler_delay_and_bandwidth_collapse() {
        for restore_cadence in [true, false] {
            game_recovery(15 * SECOND, 10, 0.0, restore_cadence, Duration::from_secs(240));
        }
    }

    #[test]
    fn game_recovers_after_different_collapse_durations() {
        for collapse in [15 * SECOND, 30 * SECOND + 100 * MS, 120 * SECOND] {
            game_recovery(collapse, 20, 0.01, false, COLLAPSE + collapse + 210 * SECOND);
        }
    }

    #[test]
    #[ignore = "full collapse matrix runs in the rust-screen-sim CI job"]
    fn full_recovery_matrix() {
        // Exercise coarse, lossy scheduling first so recovery failures surface early.
        for loss in [0.01, 0.0] {
            for cadence in [15, 20, 10, 5, 1] {
                for collapse in [
                    120 * SECOND,
                    60 * SECOND,
                    30 * SECOND + 100 * MS,
                    30 * SECOND,
                    29 * SECOND,
                    28 * SECOND,
                    15 * SECOND,
                ] {
                    let end = COLLAPSE + collapse + 360 * SECOND;
                    game_recovery(collapse, cadence, loss, false, end);
                }
            }
        }
    }

    fn game_recovery(collapse: Duration, cadence: u32, loss: f64, restore_cadence: bool, end: Duration) {
        let restore_link = COLLAPSE + collapse;
        let restore_scheduling = restore_link + 40 * SECOND;
        let mut sim = Sim::new(Some(20_000_000));
        sim.server_interval = cadence * MS;
        sim.down.loss = loss;
        sim.watch("game", 8_000_000, 60, 200_000, None);
        sim.run(COLLAPSE);
        assert!(sim.changes.is_empty(), "healthy bootstrap: {}", summary(&sim));
        assert!(sim.w.egress.counters.original_bytes > 0, "{}", summary(&sim));
        sim.down.rate = Some(1_000_000);
        let outage = sim.now;
        sim.run(collapse);
        assert!(paused_since(&sim, outage), "{}", summary(&sim));
        sim.down.rate = Some(20_000_000);
        sim.run(restore_scheduling - restore_link);
        if restore_cadence {
            sim.server_interval = MS;
        }
        sim.run(end - restore_scheduling);

        let restore = sim.start + restore_link;
        assert_game_recovery(
            &mut sim,
            restore,
            &format!("collapse={collapse:?} cadence={cadence} loss={loss} restore_cadence={restore_cadence}"),
        );
    }

    #[test]
    fn game_recovers_with_intermittent_scheduler_stalls() {
        // The first case is the reviewer's exact 12-42 s outage, 15 ms cadence,
        // and 100 ms stall every 2 s, followed through 402 s. Vary both clocks,
        // loss and outage count so historical delays cannot become probe budgets.
        for (cadence, period, stall, loss, outages) in
            [(15, 2000, 100, 0.0, 1), (10, 1300, 60, 0.01, 1), (20, 3500, 150, 0.02, 2), (15, 5000, 250, 0.0, 3)]
        {
            let schedule = format!("cadence={cadence} period={period} stall={stall} loss={loss}");
            let mut sim = Sim::new(Some(20_000_000));
            sim.down.loss = loss;
            sim.watch("game", 8_000_000, 60, 200_000, None);
            let run = |sim: &mut Sim, duration| {
                let end = sim.now + duration;
                while sim.now < end {
                    sim.server_interval = if (sim.now - sim.start).as_millis() % period < stall {
                        stall as u32 * MS
                    } else {
                        cadence * MS
                    };
                    sim.tick();
                }
            };
            run(&mut sim, COLLAPSE);
            assert!(sim.w.egress.counters.original_bytes > 0, "{schedule}: {}", summary(&sim));
            for outage in 0..outages {
                sim.down.rate = Some(1_000_000);
                let start = sim.now;
                run(&mut sim, 30 * SECOND);
                assert!(paused_since(&sim, start), "{schedule}: {}", summary(&sim));
                sim.down.rate = Some(20_000_000);
                let restore = sim.now;
                run(&mut sim, 360 * SECOND);
                assert_game_recovery(&mut sim, restore, &format!("{schedule} outage={outage}"));
            }
        }
    }

    #[test]
    fn game_recovers_after_successive_outages_without_repausing() {
        // Exact review repro first: 12-42 s and 222-251 s, 15 ms cadence,
        // 1% loss, then healthy capacity through 431 s. Each restoration has
        // its own no-repause and three received-throughput-window assertions.
        for (cadence, loss, outages) in [
            (15, 0.01, vec![30 * SECOND, 29 * SECOND]),
            (1, 0.0, vec![15 * SECOND, 28 * SECOND, 30 * SECOND + 100 * MS]),
            (10, 0.02, vec![29 * SECOND, 30 * SECOND, 60 * SECOND]),
            (20, 0.01, vec![60 * SECOND, 15 * SECOND]),
        ] {
            let mut sim = Sim::new(Some(20_000_000));
            sim.server_interval = cadence * MS;
            sim.down.loss = loss;
            sim.watch("game", 8_000_000, 60, 200_000, None);
            sim.run(COLLAPSE);
            assert!(sim.w.egress.counters.original_bytes > 0, "{}", summary(&sim));
            for (outage, duration) in outages.into_iter().enumerate() {
                sim.down.rate = Some(1_000_000);
                let start = sim.now;
                sim.run(duration);
                assert!(paused_since(&sim, start), "{}", summary(&sim));
                sim.down.rate = Some(20_000_000);
                let restore = sim.now;
                sim.run(180 * SECOND);
                assert_game_recovery(&mut sim, restore, &format!("cadence={cadence} loss={loss} outage={outage}"));
            }
        }
    }

    /// Slot 0 was paused at `from` or later.
    fn paused_since(sim: &Sim, from: Instant) -> bool {
        sim.changes.iter().any(|(at, slot, change)| *slot == 0 && *at >= from && matches!(change, Change::Pause(_)))
    }

    fn assert_game_recovery(sim: &mut Sim, restore: Instant, scenario: &str) {
        let context = format!("{scenario}: {}", summary(sim));
        // The resume that lasted: a retry late in the outage may already have made it, or may
        // still fail on its ramp once (its keyframes met the outage), but no more than that.
        let resumed = sim
            .changes
            .iter()
            .rev()
            .find_map(|(at, slot, change)| (*slot == 0 && *change == Change::Resume).then_some(*at))
            .unwrap_or_else(|| panic!("the healthy link must resume: {context}"));
        assert!(resumed.saturating_duration_since(restore) <= 180 * SECOND, "resume within 180 s: {context}");
        assert_eq!(sim.paused(0), None, "{context}");
        let repaused = sim.changes.iter().filter(|(at, _, c)| *at >= restore && matches!(c, Change::Pause(_))).count();
        assert!(repaused <= 1, "video stays resumed: {context}");
        let ssrc = sim.ssrcs(0).0;
        let mut rates = Vec::new();
        for ago in [20, 10, 0] {
            let to = sim.now - ago * SECOND;
            let from = to - 10 * SECOND;
            let bytes: usize = sim
                .received
                .iter()
                .filter(|(at, source, _, _)| *at >= from && *at < to && *source == ssrc)
                .map(|(_, _, _, payload)| payload.len())
                .sum();
            let bps = bytes as f64 * 8.0 / 10.0;
            assert!(bps >= 7_200_000.0, "received video {bps} bps: {context}");
            rates.push(bps);
        }
        println!(
            "{scenario} resume_after={:?} received_bps={rates:?} extras_dropped={} estimate={}",
            resumed.saturating_duration_since(restore),
            sim.w.egress.counters.extras_dropped,
            sim.w.egress.estimate()
        );
    }

    #[test]
    fn small_text_deltas_do_not_create_local_congestion() {
        let mut sim = Sim::new(None);
        sim.watch("static text", 16 * 8 * 5, 5, 20_000, None);
        sim.run(30 * SECOND);
        assert_eq!(sim.w.egress.counters.extras_dropped, 0, "{}", summary(&sim));
        assert_eq!(sim.w.egress.counters.originals_dropped, 0, "{}", summary(&sim));
        assert!(sim.changes.is_empty(), "{}", summary(&sim));
        assert!(sim.received.len() >= 140, "{}", summary(&sim));
    }

    #[test]
    fn focusing_an_already_paused_share_displaces_the_lower_priority_share() {
        let mut sim = Sim::new(Some(6_000_000));
        sim.watch("a", 3_000_000, 30, 60_000, None);
        sim.watch("b", 3_000_000, 30, 60_000, None);
        sim.run(30 * SECOND);
        assert_eq!(sim.paused(1), Some(ScreenPauseReason::Bandwidth), "{}", summary(&sim));
        // Each share has resume headroom on this link, while their combined wire rate
        // exceeds B and the physical link. The 6 Mbps link leaves enough resume
        // headroom for either 3 Mbps share on its own.
        sim.focus(1);
        // Handing the room over says nothing about the link: what it remembers stays.
        sim.w.remembered = Some(9_000_000);
        let step = sim.w.step;
        let focused = sim.now;
        while sim.paused(0).is_none() && sim.now - focused < 15 * SECOND {
            sim.tick();
        }
        assert_eq!(sim.w.egress.trigger, "priority", "{}", summary(&sim));
        assert_eq!((sim.w.remembered, sim.w.step), (Some(9_000_000), step), "{}", summary(&sim));
        sim.run(15 * SECOND - (sim.now - focused));
        assert_eq!(sim.paused(0), Some(ScreenPauseReason::Bandwidth), "{}", summary(&sim));
        assert_eq!(sim.paused(1), None, "{}", summary(&sim));
        assert!(resumes(&sim, 1) > 0);
    }

    /// When `collapse` drops the link: past the slot's ramp.
    const COLLAPSE: Duration = Duration::from_secs(12);

    /// 4 Mbps on a 10 Mbps downlink that drops to 1 Mbps after `COLLAPSE`, and back after 15 s.
    fn collapse(cap: bool) -> (Sim, u64, u64) {
        let mut sim = Sim::new(Some(10_000_000));
        sim.w.egress.cap = cap;
        sim.watch("game", 4_000_000, 30, 60_000, None);
        sim.run(COLLAPSE);
        assert_eq!(sim.changes, [], "{}", summary(&sim));
        sim.down.rate = Some(1_000_000);
        sim.run(15 * SECOND);
        sim.down.rate = Some(10_000_000);
        let (extras, dropped) = (sim.w.egress.counters.extra_bytes, sim.w.egress.counters.extras_dropped);
        sim.run(45 * SECOND);
        (sim, extras, dropped)
    }

    #[test]
    fn a_bandwidth_collapse_sends_no_stale_original_and_pauses_the_viewer() {
        let (sim, _, _) = collapse(true);
        let context = summary(&sim);
        assert!(sim.oldest_original(0) <= DEADLINE, "{context}");
        let paused = pauses(&sim, 0);
        assert!(!paused.is_empty() && paused[0] > COLLAPSE, "{context}");
        assert!(paused[0] < COLLAPSE + 10 * SECOND, "paused soon after the collapse: {context}");
    }

    #[test]
    fn probes_after_a_collapse_fit_the_reserve() {
        let (capped, extras, dropped) = collapse(true);
        let (uncapped, _, _) = collapse(false);
        let context = summary(&capped);
        let c = capped.w.egress.counters;
        // With every slot paused nothing else flows: the viewer's estimator restarts and probes.
        assert!(c.extra_bytes > extras + 100_000, "probes ran: {context}");
        assert!(c.extras_dropped - dropped <= 1, "probes fit the extras burst: {context}");
        let (a, b) = (capped.w.egress.estimate() as f64, uncapped.w.egress.estimate() as f64);
        // Preventing probe loss can improve recovery; only a lower capped estimate
        // would show that the reserve slowed discovery.
        assert!(a >= 0.9 * b, "the cap does not slow recovery: {a} against {b}");
    }

    /// Two 2 Mbps shares; slot 1 loses an original whose NACK is held back while the slot
    /// stops (`stop`), then the NACK arrives and the estimator is made to probe.
    fn stop_with_a_nack_in_flight(stop: impl FnOnce(&mut Watcher, Instant)) -> (Sim, Instant) {
        let mut sim = Sim::new(None);
        sim.watch("a", 2_000_000, 30, 60_000, None);
        sim.watch("b", 2_000_000, 30, 60_000, None);
        sim.run(4 * SECOND);
        let (ssrc, _) = sim.ssrcs(1);
        lose_next(&mut sim, ssrc);
        sim.hold = true;
        sim.run(100 * MS);
        assert!(sim.down.dropped == 1 && !sim.held.is_empty());
        stop(&mut sim.w, sim.now);
        let stopped = sim.now;
        sim.hold = false;
        for datagram in std::mem::take(&mut sim.held) {
            sim.deliver(&datagram);
        }
        sim.allocation = false;
        let estimate = sim.w.egress.estimate();
        sim.w.rtc.bwe().reset(Bitrate::bps(estimate / 3));
        sim.run(3 * SECOND);
        (sim, stopped)
    }

    #[test]
    fn final_unwatch_stops_periodic_and_pending_probes() {
        for pending in [false, true] {
            let mut sim = Sim::new(None);
            sim.watch("only", 4_000_000, 30, 60_000, None);
            sim.run(6 * SECOND);
            assert!(sim.w.desired.is_some());
            assert!(sim.bytes(sim.start, sim.now, |k| !matches!(k, Kind::Control)) > 100_000);
            if pending {
                sim.w.rtc.bwe().reset(Bitrate::bps(100_000));
                sim.w.rtc.handle_input(Input::Timeout(sim.now)).unwrap();
            }
            empty(&mut sim.w, 0, sim.now);
            assert_eq!(sim.w.desired, None);
            let stopped = sim.now;
            sim.run(30 * SECOND);
            assert!(sim.w.rtc.is_alive(), "the retained watch transport remains usable");
            assert_eq!(
                sim.bytes(stopped, sim.now, |k| !matches!(k, Kind::Control)),
                0,
                "no media or probes after final unwatch (pending={pending}): {}",
                summary(&sim)
            );
            assert!(sim.bytes(stopped, sim.now, |k| matches!(k, Kind::Control)) > 0);
        }
    }

    #[test]
    fn pause_and_unwatch_stop_everything_of_the_slot() {
        let (paused, at) = stop_with_a_nack_in_flight(|w, _| apply(w, 1, Change::Pause(ScreenPauseReason::Bandwidth)));
        let (unwatched, at2) = stop_with_a_nack_in_flight(|w, now| empty(w, 1, now));
        for (sim, at) in [(paused, at), (unwatched, at2)] {
            let context = summary(&sim);
            assert_eq!(media_of(&sim, 1, at), 0, "nothing of the slot after it stopped: {context}");
            let probes = sim.bytes(at, sim.now, |k| matches!(k, Kind::Probe | Kind::Repair { .. }));
            assert!(probes > 0, "the estimator probed: {context}");
            assert!(media_of(&sim, 0, at) > 0, "the other slot goes on");
        }
    }

    #[test]
    fn a_late_nack_after_a_reordered_publisher_repair_and_a_pause() {
        let mut sim = Sim::new(None);
        sim.watch("share", 0, 1, 0, None);
        sim.shares[0].on = false;
        let t0 = sim.now;
        let origin = sim.shares[0].origin.clone();
        let write = |sim: &mut Sim, source: u64, arrival: Instant| {
            let forward = Forward {
                h264: None,
                av1: None,
                dd: None,
                source,
                timestamp: source as u32 * 3000,
                marker: true,
                keyframe: source == 100,
                codec: VideoCodec::Vp9,
                arrival,
                wallclock: arrival,
                payload: payload(source, true, 1000),
            };
            assert!(enqueue(&mut sim.w, 0, &origin, &forward, sim.now), "{source} is queued");
        };
        write(&mut sim, 100, t0);
        sim.run(390 * MS);
        // 102 waited 390 ms; the viewer loses it and its NACK is held until 715 ms.
        let (ssrc, _) = sim.ssrcs(0);
        lose_next(&mut sim, ssrc);
        sim.hold = true;
        sim.uplink.clear();
        write(&mut sim, 102, t0);
        let mut next = 103;
        let mut late = Some(t0 + 580 * MS);
        while sim.now < t0 + 715 * MS {
            if sim.now >= t0 + 390 * MS + (next - 102) as u32 * 25 * MS {
                let now = sim.now;
                write(&mut sim, next, now);
                next += 1;
            }
            if late.is_some_and(|at| sim.now >= at) {
                // A publisher repair of 101 that arrived at 570 ms.
                write(&mut sim, 101, t0 + 570 * MS);
                late = None;
            }
            sim.tick();
        }
        assert!(!received(&sim, 102));
        let nacks: Vec<Vec<u8>> = sim.uplink.iter().map(|(_, d)| d.clone()).collect();
        sim.hold = false;
        for datagram in std::mem::take(&mut sim.held) {
            sim.deliver(&datagram);
        }
        while sim.now < t0 + 1200 * MS {
            if sim.now >= t0 + 390 * MS + (next - 102) as u32 * 25 * MS {
                let now = sim.now;
                write(&mut sim, next, now);
                next += 1;
            }
            sim.tick();
        }
        sim.run(50 * MS);
        // str0m 0.24.1 evicts in sequence order and stops at 101, cached at 580 ms: 102, sent at
        // 390 ms, is still served at 715 ms. A change here means str0m's eviction changed.
        // str0m 0.24.1 does not cache 101 at all: its sequence number precedes the cache's
        // eviction position (`rtx_cache_buf.rs`, `push`), so it shields nothing, and 102 was
        // evicted at 200 ms of age. A change here means str0m's cache changed: re-check the
        // "Queues and repairs" reasoning.
        assert!(!received(&sim, 102), "102 is no longer served at 715 ms");
        for source in (100..next).filter(|&s| s != 102) {
            assert!(received(&sim, source), "{source} reached the viewer");
        }
        for (_, _, _, p) in &sim.received {
            let source = source_of(p).unwrap();
            assert_eq!(*p, payload(source, true, 1000), "{source} is byte-identical");
        }
        // A pause wipes it: the same NACKs again get nothing of the slot.
        apply(&mut sim.w, 0, Change::Pause(ScreenPauseReason::Bandwidth));
        let paused = sim.now;
        for datagram in &nacks {
            sim.deliver(datagram);
        }
        sim.run(SECOND);
        assert_eq!(media_of(&sim, 0, paused), 0);
    }

    #[test]
    fn set_rtx_cache_empties_the_cache() {
        for replace in [false, true] {
            let mut sim = Sim::new(None);
            sim.watch("share", 2_000_000, 30, 60_000, None);
            sim.run(2 * SECOND);
            let (ssrc, _) = sim.ssrcs(0);
            lose_next(&mut sim, ssrc);
            sim.hold = true;
            sim.run(50 * MS);
            sim.shares[0].on = false;
            sim.run(100 * MS);
            assert!(sim.w.slots[0].originals.is_empty(), "only already sent packets enter this cache test");
            assert_eq!(sim.w.egress.queued(), 0);
            let mid = sim.w.slots[0].mid;
            if replace {
                let mut api = sim.w.rtc.direct_api();
                api.stream_tx_by_mid(mid, None).unwrap().set_rtx_cache(RTX_PACKETS, RTX_AGE, Some(RTX_RATIO));
            }
            let at = sim.now;
            sim.shares[0].on = false;
            sim.hold = false;
            for datagram in std::mem::take(&mut sim.held) {
                sim.deliver(&datagram);
            }
            sim.run(500 * MS);
            let repairs =
                sim.sent.iter().filter(|s| s.at >= at && matches!(s.kind, Kind::Repair { blank: false, .. })).count();
            assert_eq!(repairs > 0, !replace, "replace: {replace}");
        }
    }

    #[test]
    fn a_nack_storm_stays_within_the_egress_cap_and_pauses_the_lowest_slot() {
        // Media near B, so str0m's own RTX ratio cap (0.15) no longer keeps repairs within the
        // 0.1 x estimate reserve.
        let mut sim = Sim::new(Some(6_000_000));
        sim.watch("a", 2_000_000, 30, 60_000, None);
        sim.watch("b", 2_000_000, 30, 60_000, None);
        sim.focus(0);
        sim.run(6 * SECOND);
        assert!(sim.changes.is_empty(), "{}", summary(&sim));
        // The viewer loses some packets and NACKs them; every 20 ms its NACKs of the last
        // 200 ms arrive again, so it keeps asking for most of what str0m still caches. Generic
        // NACKs (RTPFB, FMT 1) lead their compound packets, which SRTCP leaves clear, and
        // str0m's SRTCP has no replay protection.
        sim.down.loss = 0.05;
        let start = sim.now;
        let mut allowance = 0.0;
        let mut largest = 0;
        while sim.now < start + 10 * SECOND {
            if (sim.now - start).as_millis().is_multiple_of(20) {
                let recent: Vec<Vec<u8>> = sim
                    .uplink
                    .iter()
                    .filter(|(at, d)| sim.now - *at <= 200 * MS && d[0] & 0x1f == 1 && d[1] == 205)
                    .map(|(_, d)| d.clone())
                    .collect();
                for datagram in &recent {
                    sim.deliver(datagram);
                }
            }
            let estimate = sim.w.egress.estimate();
            largest = largest.max(estimate);
            allowance += (PLAN + EXTRAS) * estimate as f64 / 8.0 * MS.as_secs_f64();
            sim.tick();
        }
        let context = summary(&sim);
        let media = sim.bytes(start, sim.now, |k| !matches!(k, Kind::Control));
        let burst =
            extras_burst(largest as f64) + (RELEASE_BURST * PLAN * largest as f64 / 8.0).max(RELEASE_BURST_FLOOR);
        assert!(media as f64 <= allowance + burst, "{media} bytes against {allowance} + {burst}: {context}");
        assert!(sim.w.egress.counters.extras_dropped > 0, "{context}");
        // The lower-priority share goes first; sustained overload may stop the
        // stage too. Shared pacing can avoid sustained extras overflow, so the
        // forced-overflow escalation itself is covered by the egress unit test.
        assert_eq!(
            sim.changes.first().map(|c| (c.1, c.2)),
            Some((1, Change::Pause(ScreenPauseReason::Bandwidth))),
            "{context}"
        );
    }

    /// The viewer got at least 90 % of the share's bytes in every second from `from` for
    /// `seconds`, with no drop at the SFU or on the way and no gap in its sequence numbers.
    fn assert_whole_share(sim: &mut Sim, from: Instant, seconds: u32, bitrate: u64, context: &str) {
        let c = sim.w.egress.counters;
        assert_eq!((c.evicted, c.rejected, c.failed_keyframes, c.originals_dropped), (0, 0, 0, 0), "{context}");
        assert_eq!(sim.down.dropped, 0, "{context}");
        let ssrc = sim.ssrcs(0).0;
        for second in 0..seconds {
            let start = from + second * SECOND;
            let bytes: usize = sim
                .received
                .iter()
                .filter(|(at, source, _, _)| *at >= start && *at < start + SECOND && *source == ssrc)
                .map(|(_, _, _, payload)| payload.len())
                .sum();
            let bps = bytes as f64 * 8.0;
            assert!(bps >= 0.9 * bitrate as f64, "second {second}: {bps} bps: {context}");
        }
        let mut seqs: Vec<u64> = sim.received.iter().filter(|r| r.1 == ssrc).map(|r| r.2).collect();
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(seqs.last().unwrap() - seqs[0] + 1, seqs.len() as u64, "no gap at the viewer: {context}");
    }

    #[test]
    fn an_optimistic_start_gives_a_fast_viewer_the_whole_share_at_once() {
        // The 2026-10-28 incident: a 4-12 Mbps share with large screen keyframes, a fast
        // viewer 35 ms away. Starting at 2.5 Mbps, its first 12 s lost most packets at the SFU.
        for (bitrate, keyframe, cadence) in
            [(4_000_000, 150_000, 1), (8_000_000, 250_000, 5), (12_000_000, 350_000, 10), (12_000_000, 350_000, 1)]
        {
            let mut sim = Sim::new(Some(200_000_000));
            sim.server_interval = cadence * MS;
            sim.down.delay = 17 * MS;
            sim.up.delay = 17 * MS;
            sim.join("game", bitrate, 60, keyframe, 12_000_000);
            let joined = sim.now;
            sim.run(30 * SECOND);
            let context = format!("bitrate={bitrate} keyframe={keyframe} cadence={cadence}: {}", summary(&sim));
            assert!(sim.changes.is_empty(), "{context}");
            assert_whole_share(&mut sim, joined, 30, bitrate, &context);
        }
    }

    #[test]
    fn a_slow_viewer_pauses_cleanly_and_recovers_once_its_link_improves() {
        let mut sim = Sim::new(Some(2_000_000));
        sim.voice = true;
        sim.join("game", 8_000_000, 60, 200_000, 12_000_000);
        sim.run(70 * SECOND);
        let context = summary(&sim);
        let voice = retries_voice(&sim);
        println!(
            "2 Mbps game join voice {:?} retries voice (at, hurt, max): {voice:?}",
            voice_hurt(&sim, sim.start, sim.start + JOIN),
        );
        // Only the join's start overruns the link: the retries step up from what it carried.
        assert!(voice.len() >= 2, "{context}");
        assert!(voice_hurt(&sim, sim.start + JOIN, sim.now).0.is_zero(), "{voice:?}: {context}");
        let paused = pauses(&sim, 0);
        assert!(!paused.is_empty() && paused[0] <= RAMP + SECOND, "paused after its ramp: {context}");
        // Cleanly: nothing of the slot is sent between a pause and the next resume.
        for at in &paused {
            let from = sim.start + *at + ALLOCATION_TICK;
            let to = sim
                .changes
                .iter()
                .find_map(|(t, s, c)| (*s == 0 && *t > from && *c == Change::Resume).then_some(*t))
                .unwrap_or(sim.now);
            let media = sim.sent.iter().filter(|s| s.at >= from && s.at < to).filter(|s| match s.kind {
                Kind::Original { slot, .. } | Kind::Repair { slot, blank: false } => slot == 0,
                _ => false,
            });
            assert_eq!(media.count(), 0, "nothing of a paused slot: {context}");
        }
        // Paused, with nothing else flowing, the estimate fell far below the share: probing
        // alone used to leave such a viewer paused for good.
        sim.down.rate = Some(50_000_000);
        let improved = sim.now;
        sim.run(150 * SECOND);
        assert_game_recovery(&mut sim, improved, "2 Mbps, then 50 Mbps");
    }

    #[test]
    fn a_keyframe_dominated_share_on_a_slow_link_bounds_voice_after_its_join() {
        for rate in [2_000_000, 3_000_000] {
            // A text share: 500 kb/s with 300 KB keyframes, so its start is about 7.5 Mbps.
            let mut sim = Sim::new(Some(rate));
            sim.voice = true;
            sim.join("text", 500_000, 5, 300_000, 8_000_000);
            sim.run(300 * SECOND);
            let context = format!("rate={rate}: {}", summary(&sim));
            let voice = retries_voice(&sim);
            println!(
                "{rate} text join voice {:?} retries voice (at, hurt, max): {voice:?}",
                voice_hurt(&sim, sim.start, sim.start + JOIN),
            );
            // It tries, backing off, until `RETRY_STRIKES` failed; every retry pauses again
            // within its ramp.
            assert!(voice.len() >= RETRY_STRIKES as usize, "{context}");
            assert!(sim.w.struck.is_some_and(|(_, strikes)| strikes >= RETRY_STRIKES), "{context}");
            let changes: Vec<_> = sim.changes.iter().filter(|(_, s, _)| *s == 0).collect();
            for pair in changes.windows(2).filter(|pair| pair[0].2 == Change::Resume) {
                assert!(pair[1].0 - pair[0].0 <= RAMP + 2 * SECOND, "{context}");
            }
            // At 2 Mbps the link pushes back on every retry from 2.5 Mbps. At 3 Mbps it carries
            // one but no keyframe: the retry after it admits the keyframe, and TWCC backs it off
            // within about 60 ms, when the rest of the keyframe goes (it would miss its deadline).
            // Voice never meets a long queue past the join either way.
            assert!(voice_hurt(&sim, sim.start + JOIN, sim.now).0.is_zero(), "{voice:?}: {context}");
            // A link that recovers gets the share back by itself.
            sim.down.rate = Some(50_000_000);
            let improved = sim.now;
            sim.run(180 * SECOND);
            let context = format!("rate={rate}, then 50 Mbps: {}", summary(&sim));
            assert_eq!(sim.paused(0), None, "{context}");
            let resumed = sim.changes.iter().rev().find(|(_, s, c)| *s == 0 && *c == Change::Resume).unwrap().0;
            println!("{rate} text recovered after {:?}", resumed.saturating_duration_since(improved));
            assert!(resumed.saturating_duration_since(improved) <= 150 * SECOND, "{context}");
        }
    }

    /// A bandwidth pause of slot 0 and how it judged the retry it ended: when, the retry's start
    /// and whether it counted, what the transport sent and what the link showed, the estimate, and the failed retries and
    /// the next retry's keyframe room after it.
    #[derive(Debug, Clone, Copy)]
    struct Judged {
        at: Duration,
        attempt: Option<Attempt>,
        sent: u64,
        shown: u64,
        estimate: u64,
        retries: u32,
        untested: Option<u64>,
        failed: Option<u64>,
    }

    /// Runs `time`, recording each bandwidth pause of slot 0.
    fn run_judged(sim: &mut Sim, time: Duration, judged: &mut Vec<Judged>) {
        let end = sim.now + time;
        while sim.now < end {
            let attempt = sim.w.attempt;
            let changes = sim.changes.len();
            sim.tick();
            if let Some((at, 0, Change::Pause(ScreenPauseReason::Bandwidth))) = sim.changes.get(changes).copied() {
                judged.push(Judged {
                    at: at - sim.start,
                    attempt,
                    sent: sent(&sim.w),
                    shown: shown(&sim.w),
                    estimate: sim.w.egress.estimate(),
                    retries: sim.w.retries,
                    untested: sim.w.untested,
                    failed: sim.w.failed,
                });
            }
        }
    }

    /// A share on a 2 Mbps link for `slow`, then repaired to 50 Mbps with the estimator frozen
    /// (as in production on 2026-10-28: TWCC estimates above each restart are ignored, so only
    /// retries can lift it). It must come back within 180 s by itself, keep voice clean on the
    /// way, and stay with the whole share. If its automatic retries had already stopped, a
    /// frozen estimator shows no change: it must stay paused, without a retry, until its viewer's
    /// Try again, and come back within 180 s of that. The bandwidth pauses after the repair (or
    /// the Try again), how long recovery took from then, and whether retries had stopped.
    fn recovers_frozen(
        name: &str,
        bitrate: u64,
        fps: u32,
        keyframe: usize,
        slow: Duration,
    ) -> (Vec<Judged>, Duration, bool) {
        let mut sim = Sim::new(Some(2_000_000));
        sim.voice = true;
        sim.join(name, bitrate, fps, keyframe, 12_000_000);
        let mut judged = Vec::new();
        run_judged(&mut sim, slow, &mut judged);
        let context = format!("{name} {keyframe} slow={slow:?}: {}", summary(&sim));
        let voice = retries_voice(&sim);
        println!("frozen {name} {keyframe} slow={slow:?} judged={judged:#?} retries voice (at, hurt, max): {voice:?}");
        assert_eq!(sim.paused(0), Some(ScreenPauseReason::Bandwidth), "{context}");
        assert!(voice_hurt(&sim, sim.start + JOIN, sim.now).0.is_zero(), "{voice:?}: {context}");
        sim.down.rate = Some(50_000_000);
        sim.w.frozen = Some(sim.w.egress.estimate());
        let mut improved = sim.now;
        let mut after = Vec::new();
        let stopped = sim.w.struck.is_some_and(|(_, strikes)| strikes >= RETRY_STRIKES);
        if stopped {
            run_judged(&mut sim, 60 * SECOND, &mut after);
            assert!(resumes_since(&sim, improved).is_empty(), "no automatic retry once stopped: {}", summary(&sim));
            sim.try_again(0);
            improved = sim.now;
        }
        run_judged(&mut sim, 180 * SECOND, &mut after);
        let context = format!("{name} {keyframe} slow={slow:?}, then 50 Mbps, frozen: {}", summary(&sim));
        println!("frozen {name} {keyframe} slow={slow:?} after repair judged={after:#?}");
        assert_eq!(sim.paused(0), None, "{context}");
        let resumed = sim.changes.iter().rev().find(|(_, s, c)| *s == 0 && *c == Change::Resume).unwrap().0;
        let recovered = resumed.saturating_duration_since(improved);
        let climbs: Vec<_> =
            sim.climbs.iter().filter(|(at, _)| *at > resumed).map(|(at, to)| (*at - improved, *to)).collect();
        println!("frozen {name} {keyframe} slow={slow:?} recovered after {recovered:?}, then climbed {climbs:?}");
        // Resumed, and climbed to the whole share.
        assert!(climbs.last().map_or(recovered, |(at, _)| *at) <= 180 * SECOND, "{climbs:?}: {context}");
        assert!(voice_hurt(&sim, improved, sim.now).0.is_zero(), "{context}");
        // It stays, with the whole share.
        sim.run(30 * SECOND);
        let context = format!("{name} {keyframe} slow={slow:?}, recovered: {}", summary(&sim));
        assert!(!paused_since(&sim, sim.now - 30 * SECOND), "{context}");
        let ssrc = sim.ssrcs(0).0;
        let bytes: usize = sim
            .received
            .iter()
            .filter(|(at, source, _, _)| *at >= sim.now - 20 * SECOND && *source == ssrc)
            .map(|(_, _, _, payload)| payload.len())
            .sum();
        assert!(bytes as f64 * 8.0 / 20.0 >= 0.9 * bitrate as f64, "{bytes} bytes: {context}");
        (after, recovered, stopped)
    }

    #[test]
    fn a_keyframe_dominated_share_recovers_even_if_its_estimator_never_grows() {
        // An 8 Mbps share with 250 KB keyframes: their room within the deadline is 6.25 Mbps.
        for slow in [40 * SECOND, 300 * SECOND] {
            let (after, _, stopped) = recovers_frozen("game", 8_000_000, 60, 250_000, slow);
            // Three retries failed within 300 s at 2 Mbps.
            assert_eq!(stopped, slow > RETRY_BACKOFF[1] + RETRY_BACKOFF[2], "slow={slow:?}");
            if stopped {
                continue;
            }
            // The first retry on the repaired link, from 2.5 Mbps, cannot admit the keyframe: the
            // estimate holds but nothing tests it, so it neither counts nor steps from its probes.
            let first = after.first().copied().unwrap_or_else(|| panic!("slow={slow:?}"));
            let start = first.attempt.unwrap_or_else(|| panic!("{first:?}")).start;
            assert!(first.estimate >= start && first.untested == Some(start), "{first:?}");
        }
    }

    #[test]
    fn a_share_admission_throttles_climbs_past_its_own_start_on_a_frozen_estimator() {
        // Small keyframes: each retry the link carries is throttled by the SFU's own admission
        // to about 0.7 of its start, so stepping from what it sent alone would stall near the
        // start; the step is at least `PLAN` of a start the link carried.
        for keyframe in [20_000, 60_000] {
            for slow in [40 * SECOND, 300 * SECOND] {
                recovers_frozen("game", 8_000_000, 60, keyframe, slow);
            }
        }
    }

    #[test]
    fn a_text_share_recovers_on_a_frozen_estimator() {
        // 500 kb/s with 300 KB keyframes: its start is room for the keyframe, which the
        // continuous-fit resume cannot admit with what is queued beside it at that estimate.
        for slow in [40 * SECOND, 300 * SECOND] {
            recovers_frozen("text", 500_000, 5, 300_000, slow);
        }
    }

    #[test]
    fn a_retry_the_link_carried_in_full_steps_up_without_backing_off() {
        for rate in [4_000_000, 5_000_000, 6_000_000, 7_000_000] {
            let mut sim = Sim::new(Some(rate));
            sim.voice = true;
            sim.join("game", 8_000_000, 60, 20_000, 12_000_000);
            let mut judged = Vec::new();
            let mut stepped = None;
            while sim.elapsed() < 240 * SECOND {
                let n = judged.len();
                run_judged(&mut sim, MS, &mut judged);
                let Some(&carried) = judged.get(n).filter(|j| j.attempt.is_some_and(|a| j.shown >= a.start)) else {
                    continue;
                };
                if stepped.is_some() {
                    continue;
                }
                // Carried in full: no failure, so the next retry comes `RETRY_FIRST` later, a
                // step above what the transport sent.
                let context = format!("rate={rate} {carried:?}: {}", summary(&sim));
                assert_eq!(carried.retries, 0, "{context}");
                let resumed = resumes(&sim, 0);
                while resumes(&sim, 0) == resumed {
                    run_judged(&mut sim, MS, &mut judged);
                }
                let gap = sim.now - MS - sim.start - carried.at;
                assert!(gap >= RETRY_FIRST && gap <= RETRY_FIRST + 2 * ALLOCATION_TICK, "{gap:?}: {context}");
                let start = carried.attempt.unwrap().start;
                let step = ((RETRY_STEP * carried_step(carried.sent, start) as f64) as u64).max(INITIAL_ESTIMATE);
                assert_eq!(sim.w.attempt, Some(Attempt { slot: 0, start: step, counted: true }), "{context}");
                stepped = Some((carried, gap));
            }
            let context = format!("rate={rate}: {}", summary(&sim));
            let voice = retries_voice(&sim);
            println!(
                "carried rate={rate} stepped={stepped:?} judged={judged:#?} retries voice (at, hurt, max): {voice:?}"
            );
            // None of its retries overruns the link enough to queue voice for long.
            assert!(voice.len() >= 2, "{context}");
            assert!(voice_hurt(&sim, sim.start + JOIN, sim.now).0.is_zero(), "{voice:?}: {context}");
            assert!(stepped.is_some() || rate != 5_000_000, "a retry carried in full: {judged:?}: {context}");
        }
    }

    #[test]
    fn a_climb_on_a_still_slow_link_keeps_voice_clean() {
        // A frozen estimator on a link too slow for the share: its retries climb in place as
        // long as the link carries them, and stop at the first step it pushes back on. A
        // later carried retry climbs no higher than that step, so the viewer is not paused
        // at the same overshoot after every retry.
        for rate in [4_000_000, 5_000_000, 6_000_000, 7_000_000] {
            let mut sim = Sim::new(Some(rate));
            sim.voice = true;
            sim.join("game", 8_000_000, 60, 20_000, 12_000_000);
            sim.w.frozen = Some(sim.w.egress.estimate());
            let mut judged = Vec::new();
            run_judged(&mut sim, 240 * SECOND, &mut judged);
            for &(at, to) in &sim.climbs {
                let at = at - sim.start;
                if let Some(last) = judged.iter().rev().find(|j| j.at < at)
                    && let Some(failed) = last.failed
                {
                    assert!(
                        to < failed,
                        "rate={rate} climb at {at:?} to {to} reached {failed} that failed: {judged:#?}"
                    );
                }
            }
            let context = format!("rate={rate}: {}", summary(&sim));
            let voice = retries_voice(&sim);
            let climbs: Vec<_> = sim.climbs.iter().map(|(at, to)| (*at - sim.start, *to)).collect();
            let hurt = voice_hurt(&sim, sim.start + JOIN, sim.now);
            println!(
                "frozen still slow rate={rate} climbs={climbs:?} voice after join {hurt:?} retries voice: {voice:?}"
            );
            assert!(voice.len() >= 2, "{context}");
            assert!(hurt.0.is_zero(), "{climbs:?} {voice:?}: {context}");
        }
    }

    #[test]
    fn a_retry_is_judged_against_its_own_slots_start() {
        // Two shares, and a retry of the first: a pause of the other one (the continuous-fit
        // resume may have resumed it mid-ramp) judges the retry by the retry's slot.
        let mut sim = Sim::new(Some(200_000_000));
        sim.join("game", 8_000_000, 60, 250_000, 12_000_000);
        sim.join("small", 300_000, 30, 10_000, 4_000_000);
        let start = 5_000_000;
        assert!(top(&sim.w, 1) <= start && start < top(&sim.w, 0), "{}", summary(&sim));
        // It held its estimate but sent next to nothing: inconclusive, below its share's start.
        set_estimate(&mut sim.w, start, sim.now);
        sim.w.carried.clear();
        sim.w.retries = 1;
        sim.w.attempt = Some(Attempt { slot: 0, start, counted: true });
        judge(&mut sim.w);
        assert_eq!((sim.w.untested, sim.w.retries), (Some(start), 0), "{}", summary(&sim));
    }

    #[test]
    fn a_remembered_estimate_never_starts_above_the_shares_ingress_and_a_pause_forgets_it() {
        let mut sim = Sim::new(Some(2_000_000));
        sim.w.remembered = Some(100_000_000);
        sim.join("game", 8_000_000, 60, 200_000, 12_000_000);
        assert_eq!(sim.w.egress.estimate(), egress::ingress_ceiling(12_000_000), "{}", summary(&sim));
        while resumes(&sim, 0) == 0 {
            sim.tick();
        }
        let context = summary(&sim);
        assert_eq!(pauses(&sim, 0).len(), 1, "{context}");
        assert_eq!((sim.w.proven, sim.w.remembered), (None, None), "{context}");
        // Its retry steps up from what the link carried, not from the forgotten estimate.
        let step = ((RETRY_STEP * sim.w.step as f64) as u64).max(INITIAL_ESTIMATE);
        assert!(step < 4_000_000, "{context}");
        assert_eq!(sim.w.egress.estimate(), step, "{context}");
    }

    #[test]
    fn a_static_share_proves_only_what_its_viewer_carried() {
        // An optimistic start for 300 KB keyframes, which a 500 kb/s share sends only on request:
        // str0m keeps the start, untested, so it is no measure of the link.
        let mut sim = Sim::new(Some(200_000_000));
        sim.join("text", 500_000, 5, 300_000, 8_000_000);
        sim.run(30 * SECOND);
        let context = summary(&sim);
        assert!(sim.changes.is_empty(), "{context}");
        let sent = sim.bytes(sim.now - RAMP, sim.now, |k| !matches!(k, Kind::Control));
        let carried = sent as f64 * 8.0 / RAMP.as_secs_f64();
        let proven = sim.w.proven.expect("media flowed past its ramp");
        assert!(proven < sim.w.egress.estimate(), "{proven}: {context}");
        assert!(proven as f64 <= 1.5 * carried * 1.1, "{proven} for {carried} bps carried: {context}");
    }

    #[test]
    fn try_again_waits_for_the_spacing_since_the_last_start_and_adds_no_failed_retry() {
        let mut sim = Sim::new(Some(2_000_000));
        sim.join("game", 8_000_000, 60, 200_000, 12_000_000);
        while pauses(&sim, 0).is_empty() {
            sim.tick();
        }
        let joined = sim.start;
        // Paused after its ramp, under `RETRY_FIRST` after the optimistic start.
        assert!(sim.now - joined < RETRY_FIRST, "{}", summary(&sim));
        let rates = HashMap::new();
        assert!(!ask_retry(&mut sim.w, 0, &rates, sim.now));
        assert_eq!(sim.paused(0), Some(ScreenPauseReason::Bandwidth));
        sim.run(RETRY_FIRST);
        let context = summary(&sim);
        let resumed: Vec<_> = sim
            .changes
            .iter()
            .filter(|(_, s, c)| *s == 0 && *c == Change::Resume)
            .map(|(at, ..)| *at - joined)
            .collect();
        assert_eq!(resumed.len(), 1, "{context}");
        assert!(resumed[0] >= RETRY_FIRST && resumed[0] <= RETRY_FIRST + ALLOCATION_TICK, "{context}");
        assert_eq!(sim.w.retries, 0, "{context}");
        // Right after it, another one waits again.
        let at = sim.now;
        while sim.paused(0).is_none() {
            sim.tick();
        }
        assert!(!ask_retry(&mut sim.w, 0, &rates, sim.now) || sim.now - at >= RETRY_FIRST, "{}", summary(&sim));
    }

    /// Voice glitches in `[from, to)`: runs of samples that met `HURT` or more of queue, a run
    /// ending once voice met less for a second. How many, their total time at `HURT` or more,
    /// and the most voice met.
    fn glitches(sim: &Sim, from: Instant, to: Instant) -> (usize, Duration, Duration) {
        let mut count = 0;
        let mut last: Option<Instant> = None;
        for (at, _) in sim.voice_trace.iter().filter(|(at, q)| *at >= from && *at < to && *q >= HURT) {
            if last.is_none_or(|l| *at - l > SECOND) {
                count += 1;
            }
            last = Some(*at);
        }
        let (hurt, max) = voice_hurt(sim, from, to);
        (count, hurt, max)
    }

    /// Automatic and manual resumes of slot 0 from `from` on, as offsets from the sim's start.
    fn resumes_since(sim: &Sim, from: Instant) -> Vec<Duration> {
        sim.changes
            .iter()
            .filter(|(at, s, c)| *at >= from && *s == 0 && *c == Change::Resume)
            .map(|(at, ..)| *at - sim.start)
            .collect()
    }

    const MINUTE: Duration = Duration::from_secs(60);

    #[test]
    fn a_link_too_slow_for_a_text_shares_keyframes_stops_retrying_and_spares_voice() {
        for (rate, keyframe) in [(3_000_000, 300_000), (3_500_000, 300_000), (3_000_000, 204_800)] {
            let mut sim = Sim::new(Some(rate));
            sim.voice = true;
            sim.join("text", 500_000, 5, keyframe, 8_000_000);
            sim.run(30 * MINUTE);
            let context = format!("rate={rate} keyframe={keyframe}: {}", summary(&sim));
            let glitches = glitches(&sim, sim.start + JOIN, sim.now);
            let resumed = resumes_since(&sim, sim.start);
            println!(
                "slow text rate={rate} keyframe={keyframe} 30 min: voice glitches after the join {glitches:?}, \
                 resumes {resumed:?}, retries {} struck {:?}",
                sim.w.retries, sim.w.struck
            );
            // Its keyframe goes as soon as TWCC backs a retry off, and voice never meets a long
            // queue. Until October 29 it met 50 ms or more 7-14 times, for 2.3-2.7 s in all.
            assert_eq!(glitches.0, 0, "{glitches:?}: {context}");
            // The retries stop within 10 minutes, after a few of them; before, 21-28 in 30 minutes.
            assert!(sim.w.struck.is_some_and(|(_, strikes)| strikes >= RETRY_STRIKES), "{context}");
            assert!(resumed.len() <= 8 && resumed.last().is_some_and(|at| *at < 10 * MINUTE), "{resumed:?}: {context}");
        }
    }

    #[test]
    fn a_link_that_improves_after_retries_stopped_gets_its_picture_back() {
        for (name, rate, bitrate, fps, keyframe) in
            [("text", 3_000_000, 500_000, 5, 300_000), ("game", 2_000_000, 8_000_000, 60, 200_000)]
        {
            let mut sim = Sim::new(Some(rate));
            sim.voice = true;
            sim.join(name, bitrate, fps, keyframe, 12_000_000);
            sim.run(10 * MINUTE);
            let slow = glitches(&sim, sim.start + JOIN, sim.now);
            let before = format!("{:?} struck {:?}", resumes_since(&sim, sim.start), sim.w.struck);
            let stopped = sim.w.struck.is_some_and(|(_, strikes)| strikes >= RETRY_STRIKES);
            sim.down.rate = Some(50_000_000);
            let improved = sim.now;
            sim.run(5 * MINUTE);
            let context = format!("{name} rate={rate}, then 50 Mbps: {}", summary(&sim));
            let back = sim.changes.iter().rev().find(|(_, s, c)| *s == 0 && *c == Change::Resume).map(|c| c.0);
            println!(
                "improves {name} rate={rate}: slow voice {slow:?}, resumes {before}; after the repair paused={:?} \
                 last resume {:?} after the repair, voice {:?}",
                sim.paused(0),
                back.map(|b| b.saturating_duration_since(improved)),
                glitches(&sim, improved, sim.now)
            );
            assert!(stopped, "retries stopped before the repair: {before}: {context}");
            assert_eq!(slow.0, 0, "{slow:?}: {context}");
            assert_eq!(sim.paused(0), None, "{context}");
            assert!(back.is_some_and(|at| at.saturating_duration_since(improved) <= MINUTE), "{context}");
            assert_eq!(glitches(&sim, improved, sim.now).0, 0, "{context}");
        }
    }

    #[test]
    fn stopped_retries_resume_on_a_new_path_at_most_once_per_backoff_step() {
        let mut sim = Sim::new(Some(2_000_000));
        let (w, now) = (&mut sim.w, sim.now);
        w.egress.set_estimate(1_000_000, now);
        (w.struck, w.retries) = (Some((2_000_000, RETRY_STRIKES)), 3);
        saw_path(w, addr(5000));
        assert!(!w.moved, "the first address is no move");
        saw_path(w, addr(5001));
        assert!(std::mem::take(&mut w.moved));
        // Flipping between addresses the transport already used is no new path.
        saw_path(w, addr(5000));
        saw_path(w, addr(5001));
        assert!(!w.moved);
        assert_eq!(resume_on_change(w, false, now), None);
        assert!(stopped(w));
        assert_eq!(resume_on_change(w, true, now), Some("path"));
        assert!(!stopped(w));
        // The retry it allows fails from a lower start: stopped again at once.
        strike(w, 500_000);
        assert_eq!(w.struck, Some((2_000_000, RETRY_STRIKES)));
        // Another new path within the backoff step does not resume them; one after it does.
        let backoff = RETRY_BACKOFF[3];
        assert_eq!(resume_on_change(w, true, now + backoff - SECOND), None);
        assert!(stopped(w));
        assert_eq!(resume_on_change(w, true, now + backoff), Some("path"));
        // And one failing from a new height stops them there.
        strike(w, 4_000_000);
        assert_eq!(w.struck, Some((4_000_000, RETRY_STRIKES)));
    }

    #[test]
    fn stopped_retries_resume_once_the_estimate_held_above_the_failed_start_for_the_ramp() {
        let mut sim = Sim::new(Some(2_000_000));
        let (w, now) = (&mut sim.w, sim.now);
        w.struck = Some((2_000_000, RETRY_STRIKES));
        // Below `RETRY_LEVEL` times the failed start: never.
        w.egress.set_estimate(2_400_000, now);
        assert_eq!(resume_on_change(w, false, now), None);
        assert_eq!(resume_on_change(w, false, now + 2 * RAMP), None);
        let above = now + 2 * RAMP;
        w.egress.set_estimate(2_500_000, above);
        assert_eq!(resume_on_change(w, false, above), None);
        assert_eq!(resume_on_change(w, false, above + RAMP - SECOND), None);
        // A dip below starts the clock over.
        w.egress.set_estimate(2_000_000, above + RAMP - SECOND);
        assert_eq!(resume_on_change(w, false, above + RAMP - SECOND), None);
        let again = above + RAMP;
        w.egress.set_estimate(2_500_000, again);
        assert_eq!(resume_on_change(w, false, again), None);
        assert_eq!(resume_on_change(w, false, again + RAMP - MS), None);
        assert!(stopped(w));
        assert_eq!(resume_on_change(w, false, again + RAMP), Some("estimate"));
        assert!(!stopped(w));
        // The next failure, from any start, stops them again.
        strike(w, 1_000_000);
        assert_eq!(w.struck, Some((2_000_000, RETRY_STRIKES)));
    }

    #[test]
    fn a_retry_from_the_shares_whole_start_that_carried_next_to_nothing_strikes() {
        let mut sim = Sim::new(None);
        sim.join("game", 8_000_000, 60, 200_000, 12_000_000);
        sim.run(2 * RAMP);
        let w = &mut sim.w;
        let start = top(w, 0);
        // The estimate held at the start, but the link was not shown to carry it: with no
        // higher start to test it from, it failed.
        w.egress.set_estimate(start, sim.now);
        for _ in 0..RETRY_STRIKES {
            w.carried.clear();
            w.attempt = Some(Attempt { slot: 0, start, counted: true });
            judge(w);
            assert_eq!(w.failed, Some(start));
        }
        assert!(stopped(w), "struck {:?}", w.struck);
    }

    #[test]
    fn a_retry_that_climbs_in_place_clears_the_strikes_but_keeps_the_failed_cap() {
        let mut sim = Sim::new(None);
        sim.join("game", 8_000_000, 60, 200_000, 12_000_000);
        sim.run(2 * RAMP);
        let (w, now) = (&mut sim.w, sim.now);
        let start = 1_000_000;
        w.egress.set_estimate(start, now);
        // The link carried the retry's start in full over its ramp; the share needs more.
        w.carried = VecDeque::from([(now - RAMP, 0), (now, start * RAMP.as_secs() / 8)]);
        w.started = Some(now - RAMP);
        w.attempt = Some(Attempt { slot: 0, start, counted: true });
        (w.struck, w.last_chance, w.failed) = (Some((2_000_000, RETRY_STRIKES - 1)), true, Some(4_000_000));
        let lanes = [ShareRate { average: 8_000_000.0, ..ShareRate::default() }; SCREEN_SLOTS];
        assert!(climb(w, &lanes, &HashMap::new(), now));
        assert_eq!((w.struck, w.last_chance, w.failed), (None, false, Some(4_000_000)));
    }

    #[test]
    fn a_new_path_retries_a_stopped_viewer_no_sooner_than_the_spacing_since_its_last_start() {
        let mut sim = Sim::new(Some(2_000_000));
        sim.join("game", 8_000_000, 60, 200_000, 12_000_000);
        while pauses(&sim, 0).is_empty() {
            sim.tick();
        }
        let (paused, started) = (sim.now, sim.w.started.expect("the share started"));
        assert!(paused - started < RETRY_FIRST, "{}", summary(&sim));
        sim.w.struck = Some((2 * sim.w.egress.estimate().max(INITIAL_ESTIMATE), RETRY_STRIKES));
        saw_path(&mut sim.w, addr(5000));
        saw_path(&mut sim.w, addr(5001));
        sim.run(RETRY_FIRST);
        let context = summary(&sim);
        let resumed = resumes_since(&sim, paused);
        assert_eq!(resumed.len(), 1, "{resumed:?}: {context}");
        let at = sim.start + resumed[0];
        assert!(at >= started + RETRY_FIRST && at <= started + RETRY_FIRST + SECOND, "{resumed:?}: {context}");
        // The link has not changed: the retry fails and they stop again.
        let end = sim.now + 2 * MINUTE;
        while !stopped(&sim.w) && sim.now < end {
            sim.tick();
        }
        let context = summary(&sim);
        assert!(stopped(&sim.w), "struck {:?}: {context}", sim.w.struck);
        assert!(resumes_since(&sim, paused).len() <= 2, "{context}");
    }

    #[test]
    fn a_link_that_degrades_pauses_and_stops_retrying() {
        for (name, rate, bitrate, fps, keyframe) in
            [("game", 3_000_000, 8_000_000, 60, 200_000), ("text", 3_000_000, 500_000, 5, 300_000)]
        {
            let mut sim = Sim::new(Some(50_000_000));
            sim.voice = true;
            sim.join(name, bitrate, fps, keyframe, 12_000_000);
            sim.run(MINUTE);
            assert_eq!(sim.paused(0), None, "{}", summary(&sim));
            sim.down.rate = Some(rate);
            let degraded = sim.now;
            sim.run(20 * MINUTE);
            println!(
                "degrades {name} to {rate}: voice right after {:?}, then {:?}; resumes {:?} struck {:?}",
                glitches(&sim, degraded, degraded + 15 * SECOND),
                glitches(&sim, degraded + 15 * SECOND, sim.now),
                resumes_since(&sim, degraded),
                sim.w.struck
            );
            println!("degrades {name} retries voice {:?}", retries_voice(&sim));
            let context = format!("{name} degraded to {rate}: {}", summary(&sim));
            // Once TWCC caught up with the slower link, a retry meets 50 ms of queue only briefly,
            // on a few of them; before, the first retry started from the healthy link's traffic
            // and met it for 600 ms, and they never stopped.
            let (count, hurt, max) = glitches(&sim, degraded + 15 * SECOND, sim.now);
            assert!(count <= 2 && hurt <= 100 * MS && max < 60 * MS, "{:?}: {context}", (count, hurt, max));
            assert!(resumes_since(&sim, degraded).len() <= 8, "{context}");
        }
    }

    /// Two viewers of the same share on their own links, behind one publisher: `b`'s share
    /// mirrors `a`'s frame for frame, and either viewer's keyframe request gives both the
    /// keyframe, as the per-share throttle does.
    struct Duo {
        a: Sim,
        b: Sim,
    }

    impl Duo {
        fn new(a: Option<u64>, b: Option<u64>) -> Self {
            let mut a = Sim::with_seed(a, 1);
            let mut b = Sim::with_seed(b, 2);
            while a.now < b.now {
                a.tick();
            }
            while b.now < a.now {
                b.tick();
            }
            (a.start, b.start) = (a.now, b.now);
            Self { a, b }
        }

        fn join(&mut self, bitrate: u64, keyframe: usize) {
            self.a.join("game", bitrate, 60, keyframe, 12_000_000);
            self.b.join("game", bitrate, 60, keyframe, 12_000_000);
        }

        fn run(&mut self, time: Duration) {
            let end = self.a.now + time;
            while self.a.now < end {
                let wants = self.a.shares[0].wants_key || self.b.shares[0].wants_key;
                self.a.shares[0].wants_key = wants;
                self.b.shares[0].wants_key = wants;
                self.a.tick();
                self.b.tick();
            }
        }
    }

    #[test]
    fn viewers_on_different_links_do_not_affect_each_other() {
        let mut duo = Duo::new(Some(200_000_000), Some(2_000_000));
        duo.join(8_000_000, 200_000);
        let joined = duo.a.now;
        duo.run(90 * SECOND);
        let (fast, slow) = (&mut duo.a, &duo.b);
        assert_eq!(fast.shares[0].keyframes.len(), slow.shares[0].keyframes.len(), "one publisher");
        let context = format!("fast: {} slow: {}", summary(fast), summary(slow));
        assert!(!pauses(slow, 0).is_empty(), "{context}");
        assert!(
            fast.shares[0].keyframes.len() > 1,
            "the slow viewer's requests reached the publisher, and the fast one carried them: {context}"
        );
        assert!(fast.changes.is_empty(), "{context}");
        assert_whole_share(fast, joined, 90, 8_000_000, &context);
    }
}
