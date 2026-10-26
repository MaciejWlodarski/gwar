//! Per-packet ingress enforcement for one share (docs/screen-share.md, "Ingress enforcement"):
//! two token buckets, frame bounds and the overrun rule. Nothing is buffered: a packet is either
//! within the limits when it arrives or it is dropped, and so is the rest of the share until the
//! next keyframe starts.
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

/// One RTP timestamp may carry at most this many bytes (two 1080p keyframes) …
pub const FRAME_BYTES: usize = 512 * 1024;
/// … this many packets …
pub const FRAME_PACKETS: usize = 600;
/// Bounds the lifetime of current-timestamp accounting, not a frame buffer. Allow
/// publisher pacing on slow uplinks; bytes, packets and buckets stay bounded.
pub const FRAME_SPAN: Duration = Duration::from_secs(2);
/// Packets per second a share may send, and its packet burst (`FRAME_PACKETS`).
const PACKET_RATE: f64 = 2_000.0;
/// The byte bucket's rate is this times the share's cap.
const BYTE_HEADROOM: f64 = 1.25;
/// Overruns that stop a share, within `OVERRUN_WINDOW`.
const OVERRUNS: usize = 3;
const OVERRUN_WINDOW: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    Vp8,
    Vp9,
    /// Profile/constraint/level bytes, from SDP or the stream SPS.
    H264(u32),
    /// Profile 0 (Main), the only profile negotiated.
    Av1,
}

/// What the clear payload descriptor says about a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Descriptor {
    /// The first packet of a frame.
    pub frame_start: bool,
    /// The first packet of a keyframe.
    pub keyframe: bool,
}

/// Reads the start-of-frame and keyframe bits, and nothing else of the codec. `None` for a
/// payload whose descriptor is truncated or leaves no payload.
pub fn describe(codec: VideoCodec, payload: &[u8]) -> Option<Descriptor> {
    let first = *payload.first()?;
    match codec {
        // H.264 and AV1 need timestamp tracking and ordered prefix confirmation by their
        // transport.
        VideoCodec::H264(_) | VideoCodec::Av1 => None,
        // RFC 9628: |I|P|L|F|B|E|V|Z|. B starts a frame; P = 0 is not inter-predicted.
        VideoCodec::Vp9 => {
            vp9_descriptor_len(payload).filter(|&len| len < payload.len())?;
            let frame_start = first & 0x08 != 0;
            Some(Descriptor { frame_start, keyframe: frame_start && first & 0x40 == 0 })
        }
        // RFC 7741: |X|R|N|S|R|PID|, optional extensions, then the VP8 payload header whose
        // lowest bit is 0 for a keyframe. S with partition 0 starts a frame.
        VideoCodec::Vp8 => {
            let frame_start = first & 0x10 != 0 && first & 0x07 == 0;
            let mut at = 1;
            if first & 0x80 != 0 {
                let extensions = *payload.get(1)?;
                at = 2;
                if extensions & 0x80 != 0 {
                    at += if *payload.get(at)? & 0x80 != 0 { 2 } else { 1 };
                }
                if extensions & 0x40 != 0 {
                    at += 1;
                }
                if extensions & 0x30 != 0 {
                    at += 1;
                }
            }
            let header = *payload.get(at)?;
            Some(Descriptor { frame_start, keyframe: frame_start && header & 0x01 == 0 })
        }
    }
}

/// The length of a VP9 payload descriptor (RFC 9628 §4.2), with its optional fields.
fn vp9_descriptor_len(payload: &[u8]) -> Option<usize> {
    let first = *payload.first()?;
    let (picture_id, inter, layers, flexible, scalability) =
        (first & 0x80 != 0, first & 0x40 != 0, first & 0x20 != 0, first & 0x10 != 0, first & 0x02 != 0);
    let mut at = 1;
    if picture_id {
        at += if *payload.get(at)? & 0x80 != 0 { 2 } else { 1 };
    }
    if layers {
        // Layer indices, and TL0PICIDX in non-flexible mode.
        at += if flexible { 1 } else { 2 };
    }
    if flexible && inter {
        // Up to three reference indices, each with a "next" bit.
        for _ in 0..3 {
            let reference = *payload.get(at)?;
            at += 1;
            if reference & 0x01 == 0 {
                break;
            }
        }
    }
    if scalability {
        let structure = *payload.get(at)?;
        at += 1;
        let spatial = usize::from(structure >> 5) + 1;
        if structure & 0x10 != 0 {
            at += 4 * spatial;
        }
        if structure & 0x08 != 0 {
            let groups = *payload.get(at)?;
            at += 1;
            for _ in 0..groups {
                let group = *payload.get(at)?;
                at += 1 + usize::from((group >> 2) & 0x03);
            }
        }
    }
    payload.get(at.checked_sub(1)?)?;
    Some(at)
}

/// A token bucket refilled continuously.
#[derive(Debug, Clone)]
pub struct Bucket {
    rate: f64,
    burst: f64,
    tokens: f64,
    at: Instant,
}

impl Bucket {
    pub fn new(rate: f64, burst: f64, now: Instant) -> Self {
        Self { rate, burst, tokens: burst, at: now }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate).min(self.burst);
        self.at = now;
    }

    /// New rate and burst from now on; tokens above the new burst are lost.
    pub fn reconfigure(&mut self, rate: f64, burst: f64, now: Instant) {
        self.refill(now);
        self.rate = rate;
        self.burst = burst;
        self.tokens = self.tokens.min(burst);
    }

    /// Current credit after accounting for elapsed time.
    pub fn available(&mut self, now: Instant) -> f64 {
        self.refill(now);
        self.tokens
    }

    /// Whether it has refilled completely, so that dropping it loses nothing.
    pub fn full(&mut self, now: Instant) -> bool {
        self.refill(now);
        self.tokens >= self.burst
    }

    /// Takes `amount` if it is all there; otherwise takes nothing.
    pub fn take(&mut self, amount: f64, now: Instant) -> bool {
        self.refill(now);
        if self.tokens < amount {
            return false;
        }
        self.tokens -= amount;
        true
    }
}

struct Frame {
    timestamp: u32,
    first: Instant,
    bytes: usize,
    packets: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Forward it; `keyframe` when it starts one.
    Forward { keyframe: bool },
    /// Dropped while the share waits for a keyframe, or a stray packet.
    Drop,
    /// The packet broke a limit: it and the rest of the share drop until the next keyframe,
    /// every slot waits for it and the publisher is asked for one.
    Overrun,
    /// The third overrun within 10 s: the share ends with `limit`.
    Stop,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct IngressCounters {
    pub forwarded: u64,
    pub dropped: u64,
    pub overruns: u64,
    pub frame_span: u64,
    pub frame_bytes: u64,
    pub frame_packets: u64,
    pub bucket: u64,
    /// Datagrams the publish transport's meter dropped before str0m.
    pub transport_dropped: u64,
}

pub struct Ingress {
    bytes: Bucket,
    packets: Bucket,
    frame: Option<Frame>,
    /// The highest source sequence number seen; lower ones are repairs or reordered.
    highest: Option<u64>,
    /// Dropping everything until a keyframe starts.
    blocked: bool,
    overruns: VecDeque<Instant>,
    pub counters: IngressCounters,
}

impl Ingress {
    /// `cap_bps`: the share's bitrate cap. A share starts waiting for its first keyframe.
    pub fn new(cap_bps: u64, now: Instant) -> Self {
        Self {
            bytes: Bucket::new(BYTE_HEADROOM * cap_bps as f64 / 8.0, FRAME_BYTES as f64, now),
            packets: Bucket::new(PACKET_RATE, FRAME_PACKETS as f64, now),
            frame: None,
            highest: None,
            blocked: true,
            overruns: VecDeque::new(),
            counters: IngressCounters::default(),
        }
    }

    pub fn waiting(&self) -> bool {
        self.blocked
    }

    /// Judges one packet of the share's main stream (RTX already unwrapped), on arrival.
    pub fn admit(&mut self, source: u64, timestamp: u32, size: usize, describe: Descriptor, now: Instant) -> Verdict {
        let newest = self.highest.is_none_or(|highest| source > highest);
        if newest {
            self.highest = Some(source);
        }
        if self.blocked {
            if !(newest && describe.keyframe) {
                self.counters.dropped += 1;
                return Verdict::Drop;
            }
            self.blocked = false;
        }
        if let Some(verdict) = self.account(source, timestamp, size, now) {
            return verdict;
        }
        self.counters.forwarded += 1;
        Verdict::Forward { keyframe: newest && describe.keyframe }
    }

    /// Charge an H.264 or AV1 packet on arrival, including a held prefix. Releasing it never charges
    /// it again. This also bounds traffic while ingress waits for a confirmed IDR.
    pub fn account(&mut self, source: u64, timestamp: u32, size: usize, now: Instant) -> Option<Verdict> {
        let newest = self.highest.is_none_or(|highest| source >= highest);
        if newest {
            self.highest = Some(source);
        }
        let mut within = self.packets.take(1.0, now) && self.bytes.take(size as f64, now);
        if !within {
            self.counters.bucket += 1;
        }
        // Frame bounds follow the newest frame, counting each of its packets in whatever order
        // they arrive; a late repair of an older frame only counts against the buckets.
        let current = self.frame.as_ref().is_some_and(|frame| frame.timestamp == timestamp);
        if newest || current {
            let frame = match &mut self.frame {
                Some(frame) if frame.timestamp == timestamp => frame,
                frame => frame.insert(Frame { timestamp, first: now, bytes: 0, packets: 0 }),
            };
            frame.bytes += size;
            frame.packets += 1;
            let size_ok = frame.bytes <= FRAME_BYTES && frame.packets <= FRAME_PACKETS;
            let span_ok = now.saturating_duration_since(frame.first) <= FRAME_SPAN;
            self.counters.frame_bytes += u64::from(frame.bytes > FRAME_BYTES);
            self.counters.frame_packets += u64::from(frame.packets > FRAME_PACKETS);
            self.counters.frame_span += u64::from(!span_ok);
            within &= size_ok && span_ok;
        }
        if within {
            return None;
        }
        self.counters.dropped += 1;
        Some(self.overrun(now))
    }

    /// A complete, bounded SPS/PPS/IDR (H.264) or sequence/key frame header (AV1) prefix
    /// opens blocked ingress.
    pub fn block_prefix(&mut self) {
        self.blocked = true;
    }

    pub fn confirmed_prefix(&mut self) {
        self.blocked = false;
    }

    /// Records an overrun: the share drops until its next keyframe, or stops on the third
    /// within `OVERRUN_WINDOW`.
    pub fn overrun(&mut self, now: Instant) -> Verdict {
        self.counters.overruns += 1;
        self.blocked = true;
        self.frame = None;
        while self.overruns.front().is_some_and(|at| now.saturating_duration_since(*at) > OVERRUN_WINDOW) {
            self.overruns.pop_front();
        }
        self.overruns.push_back(now);
        if self.overruns.len() >= OVERRUNS { Verdict::Stop } else { Verdict::Overrun }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: Descriptor = Descriptor { frame_start: true, keyframe: true };
    const START: Descriptor = Descriptor { frame_start: true, keyframe: false };
    const MORE: Descriptor = Descriptor { frame_start: false, keyframe: false };

    #[test]
    fn descriptors_find_frame_and_keyframe_starts() {
        // VP9: B set, P clear → keyframe start; P set → delta; B clear → continuation.
        assert_eq!(describe(VideoCodec::Vp9, &[0x08, 0x00]), Some(KEY));
        assert_eq!(describe(VideoCodec::Vp9, &[0x48, 0x00]), Some(START));
        assert_eq!(describe(VideoCodec::Vp9, &[0x00, 0x00]), Some(MORE));
        assert_eq!(describe(VideoCodec::Vp9, &[]), None);
        // A descriptor with no payload after it.
        assert_eq!(describe(VideoCodec::Vp9, &[0x08]), None);
        // I (15-bit picture id) and L in non-flexible mode: four descriptor bytes.
        assert_eq!(describe(VideoCodec::Vp9, &[0xa8, 0x81, 0x02, 0x00, 0x07, 0x00]), Some(KEY));
        assert_eq!(describe(VideoCodec::Vp9, &[0xa8, 0x81, 0x02, 0x00, 0x07]), None);
        // Flexible, inter-predicted, with two reference indices.
        assert_eq!(describe(VideoCodec::Vp9, &[0x58, 0x03, 0x04, 0x00]), Some(START));
        assert_eq!(describe(VideoCodec::Vp9, &[0x58, 0x03, 0x04]), None);
        // A scalability structure with one spatial layer's size and one group of one reference.
        assert_eq!(describe(VideoCodec::Vp9, &[0x0a, 0x18, 0, 1, 0, 1, 0x01, 0x04, 0x02, 0x00]), Some(KEY));
        assert_eq!(describe(VideoCodec::Vp9, &[0x0a, 0x18, 0, 1, 0, 1, 0x01, 0x04, 0x02]), None);
        // VP8 without extensions: S, PID 0, payload header bit 0 clear → keyframe.
        assert_eq!(describe(VideoCodec::Vp8, &[0x10, 0x00]), Some(KEY));
        assert_eq!(describe(VideoCodec::Vp8, &[0x10, 0x01]), Some(START));
        // Partition 1 is never a frame start.
        assert_eq!(describe(VideoCodec::Vp8, &[0x11, 0x00]), Some(MORE));
        // X with a 15-bit picture id, TL0PICIDX and TID: the header byte follows them.
        assert_eq!(describe(VideoCodec::Vp8, &[0x90, 0xe0, 0x80, 0x01, 0x07, 0x20, 0x00]), Some(KEY));
        assert_eq!(describe(VideoCodec::Vp8, &[0x90, 0xe0, 0x80, 0x01, 0x07, 0x20, 0x01]), Some(START));
        // A 7-bit picture id only.
        assert_eq!(describe(VideoCodec::Vp8, &[0x90, 0x80, 0x05, 0x00]), Some(KEY));
        // Truncated descriptors are not guessed at.
        assert_eq!(describe(VideoCodec::Vp8, &[0x90, 0x80, 0x85]), None);
        assert_eq!(describe(VideoCodec::Vp8, &[0x10]), None);
    }

    #[test]
    fn a_share_waits_for_its_first_keyframe() {
        let now = Instant::now();
        let mut ingress = Ingress::new(8_000_000, now);
        assert_eq!(ingress.admit(1, 100, 1000, START, now), Verdict::Drop);
        assert_eq!(ingress.admit(2, 100, 1000, MORE, now), Verdict::Drop);
        assert_eq!(ingress.admit(3, 200, 1000, KEY, now), Verdict::Forward { keyframe: true });
        assert_eq!(ingress.admit(4, 200, 1000, MORE, now), Verdict::Forward { keyframe: false });
        // A repair of a dropped packet, older than the keyframe, still passes ingress: the
        // slots drop it against their resume point.
        assert_eq!(ingress.admit(2, 100, 1000, MORE, now), Verdict::Forward { keyframe: false });
    }

    #[test]
    fn a_bucket_emptying_mid_frame_cuts_the_rest_until_a_keyframe() {
        let now = Instant::now();
        // 80 kbps: 12.5 KB/s of refill, 512 KB of burst.
        let mut ingress = Ingress::new(80_000, now);
        assert_eq!(ingress.admit(0, 0, 1000, KEY, now), Verdict::Forward { keyframe: true });
        let mut sequence = 1;
        let mut forwarded = 1;
        let mut timestamp = 0;
        // Frames of 200 packets, 1100 bytes each, until the byte bucket runs out mid-frame.
        let cut = loop {
            if sequence % 200 == 0 {
                timestamp += 3000;
            }
            match ingress.admit(sequence, timestamp, 1100, if sequence % 200 == 0 { START } else { MORE }, now) {
                Verdict::Forward { .. } => forwarded += 1,
                Verdict::Overrun => break sequence,
                other => panic!("{other:?} at {sequence}"),
            }
            sequence += 1;
        };
        assert_eq!(forwarded, 1 + (FRAME_BYTES - 1000) / 1100, "burst exhausted exactly");
        assert_ne!(cut % 200, 0, "cut mid-frame");
        // The rest of that frame and the next delta frames drop, even after a refill.
        let later = now + Duration::from_secs(60);
        assert_eq!(ingress.admit(cut + 1, timestamp, 1100, MORE, later), Verdict::Drop);
        assert_eq!(ingress.admit(cut + 2, timestamp + 3000, 1100, START, later), Verdict::Drop);
        assert!(ingress.waiting());
        assert_eq!(ingress.admit(cut + 3, timestamp + 6000, 1100, KEY, later), Verdict::Forward { keyframe: true });
    }

    #[test]
    fn paced_large_keyframes_fit_without_weakening_abuse_limits() {
        let now = Instant::now();
        let mut ingress = Ingress::new(3_000_000, now);
        // 400 KB over 1.2 s, repeated within the old ten-second Stop window.
        for frame in 0..4u64 {
            for packet in 0..400u64 {
                let at = now + Duration::from_millis(frame * 1500 + packet * 3);
                assert!(matches!(
                    ingress.admit(frame * 400 + packet, frame as u32, 1024, if packet == 0 { KEY } else { MORE }, at),
                    Verdict::Forward { .. }
                ));
            }
        }
        assert_eq!(ingress.counters.overruns, 0);
        // An oversized timestamp still overruns, and repeated abuse still stops.
        let mut abuse = Ingress::new(1_000_000_000, now);
        for attempt in 0..3u64 {
            let at = now + Duration::from_secs(attempt);
            assert_eq!(
                abuse.admit(attempt * 2, attempt as u32, FRAME_BYTES, KEY, at),
                Verdict::Forward { keyframe: true }
            );
            assert_eq!(
                abuse.admit(attempt * 2 + 1, attempt as u32, 1, MORE, at),
                if attempt == 2 { Verdict::Stop } else { Verdict::Overrun }
            );
        }
    }

    #[test]
    fn frame_bounds_cut_frames_without_a_boundary() {
        let now = Instant::now();
        // Spaced so the buckets keep refilling: only the frame bound can cut. `reversed` sends
        // each pair of packets in reverse sequence order.
        for (case, size, packets, step, reversed) in [
            ("bytes", 1200, FRAME_BYTES / 1200 + 1, Duration::from_micros(100), false),
            ("packets", 10, FRAME_PACKETS + 1, Duration::from_micros(500), false),
            ("reordered packets", 10, FRAME_PACKETS + 1, Duration::from_micros(500), true),
            ("span", 10, 42, Duration::from_millis(50), false),
        ] {
            let mut ingress = Ingress::new(1_000_000_000, now);
            let mut at = now;
            let mut verdicts = Vec::new();
            for n in 0..packets {
                let sequence = match n {
                    0 => 0,
                    n if reversed && n % 2 == 1 => n + 1,
                    n if reversed => n - 1,
                    n => n,
                } as u64;
                let describe = if n == 0 { KEY } else { MORE };
                verdicts.push(ingress.admit(sequence, 7, size, describe, at));
                at += step;
            }
            let last = *verdicts.last().unwrap();
            assert_eq!(last, Verdict::Overrun, "{case}");
            assert!(verdicts[..verdicts.len() - 1].iter().all(|v| matches!(v, Verdict::Forward { .. })), "{case}");
        }
    }

    #[test]
    fn three_overruns_in_ten_seconds_stop_the_share_and_old_ones_expire() {
        let now = Instant::now();
        let mut ingress = Ingress::new(1_000_000_000, now);
        let mut sequence = 0;
        let mut overrun = |ingress: &mut Ingress, at: Instant| {
            assert!(matches!(ingress.admit(sequence, sequence as u32, 10, KEY, at), Verdict::Forward { .. }));
            sequence += 1;
            // A continuation just beyond the allowance breaks the span bound.
            let verdict =
                ingress.admit(sequence, (sequence - 1) as u32, 10, MORE, at + FRAME_SPAN + Duration::from_millis(100));
            sequence += 1;
            verdict
        };
        assert_eq!(overrun(&mut ingress, now), Verdict::Overrun);
        assert_eq!(overrun(&mut ingress, now + Duration::from_secs(4)), Verdict::Overrun);
        // The first one is more than 10 s old by now.
        assert_eq!(overrun(&mut ingress, now + Duration::from_secs(11)), Verdict::Overrun);
        assert_eq!(overrun(&mut ingress, now + Duration::from_secs(14)), Verdict::Stop);
    }
}
