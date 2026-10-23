//! Probe cluster data structures for bandwidth estimation.
//!
//! Probe clusters are short bursts of packets sent at specific bitrates to test
//! network capacity.

use std::cmp;
use std::time::{Duration, Instant};

use crate::rtp_::{Bitrate, DataSize, TwccClusterId};
use crate::util::{already_happened, not_happening};

const MAX_PADDING_PACKET_SIZE: DataSize = DataSize::bytes(240);

/// Configuration for a probe cluster (the plan).
///
/// This represents the immutable blueprint for a bandwidth probe: what bitrate
/// to test, for how long, and with what constraints.
#[derive(Debug, Clone, Copy)]
pub struct ProbeClusterConfig {
    /// Unique identifier for this probe cluster
    cluster: TwccClusterId,

    /// Target bitrate to probe at (e.g., 3 Mbps)
    target_bitrate: Bitrate,

    /// How long to sustain the target bitrate (e.g., 15ms)
    target_duration: Duration,

    /// Minimum number of packets to send (e.g., 5)
    /// This ensures statistical validity even for short bursts.
    min_packet_count: usize,

    /// Delta time between sent bursts of packets during probe.
    ///
    /// Mirrors WebRTC's `ProbeClusterConfig.min_probe_delta`. The pacer/probe sender must not
    /// schedule probe packets more frequently than this.
    min_probe_delta: Duration,

    /// The kind of probe this is.
    kind: ProbeKind,

    /// Application payload-byte budget, before RTP/SRTP serialization.
    max_bytes: DataSize,
    pub(crate) max_send_gap: Option<Duration>,
}

/// Kind of probe
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProbeKind {
    Initial,
    Exponential,
    IncreaseAlr,
    PeriodicAlr,
    LargeDrop,
    Stagnant,
}

impl ProbeKind {
    pub(crate) fn is_alr(&self) -> bool {
        matches!(self, ProbeKind::PeriodicAlr | ProbeKind::IncreaseAlr)
    }
}

impl ProbeClusterConfig {
    /// Create a new probe cluster configuration with standard defaults:
    /// - 15ms duration (enough to get meaningful feedback without excessive delay)
    /// - 5 minimum packets (statistical significance for BWE analysis)
    pub fn new(cluster: TwccClusterId, target_bitrate: Bitrate, kind: ProbeKind) -> Self {
        Self {
            cluster,
            target_bitrate,
            // WebRTC defaults
            target_duration: Duration::from_millis(15),
            min_packet_count: 5,
            // WebRTC default for general probing (not initial/network-state): 2ms.
            min_probe_delta: Duration::from_millis(2),
            kind,
            max_bytes: DataSize::bytes(i64::MAX),
            max_send_gap: None,
        }
    }

    /// Bound the cluster payload target; allow one packet of rounding at the socket.
    pub(crate) fn with_max_bytes(mut self, max_bytes: DataSize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    pub(crate) fn with_max_send_gap(mut self, gap: Duration) -> Self {
        self.max_send_gap = Some(gap);
        self
    }

    pub(crate) fn with_max_bitrate(mut self, bitrate: Bitrate) -> Self {
        self.target_bitrate = self.target_bitrate.min(bitrate);
        self
    }

    pub(crate) fn is_byte_limited(&self) -> bool {
        self.max_bytes < DataSize::bytes(i64::MAX)
    }

    /// Set a custom target duration for this probe.
    /// WebRTC uses 100ms for initial probes to allow time for media to start.
    pub fn with_duration(mut self, duration: Duration) -> Self {
        self.target_duration = duration;
        self
    }

    /// Set a custom minimum packet count for this probe.
    pub fn with_min_packet_count(mut self, min_packet_count: usize) -> Self {
        self.min_packet_count = min_packet_count;
        self
    }

    /// Set a custom minimum probe delta (spacing constraint) for this probe.
    pub fn with_min_probe_delta(mut self, min_probe_delta: Duration) -> Self {
        self.min_probe_delta = min_probe_delta;
        self
    }

    /// Check if this probe was created during ALR.
    pub fn is_alr_probe(&self) -> bool {
        self.kind.is_alr()
    }

    /// Get the probe cluster ID.
    pub fn cluster(&self) -> TwccClusterId {
        self.cluster
    }

    /// Get the target bitrate.
    pub fn target_bitrate(&self) -> Bitrate {
        self.target_bitrate
    }

    /// Get the minimum packet count required for a valid probe.
    pub fn min_packet_count(&self) -> usize {
        self.min_packet_count
    }

    /// Get the minimum probe delta (spacing constraint).
    pub fn min_probe_delta(&self) -> Duration {
        self.min_probe_delta
    }

    /// Calculate the target bytes for this probe.
    /// This is how much data we expect to send at target_bitrate for target_duration.
    pub fn target_bytes(&self) -> DataSize {
        self.target_bitrate * self.target_duration
    }
}

/// Runtime state of an active probe cluster (the execution).
///
/// This tracks what's actually happening as we send probe packets: how much
/// we've sent, how many packets, and when we started.
#[derive(Debug)]
pub struct ProbeClusterState {
    /// The immutable plan
    config: ProbeClusterConfig,

    /// Total bytes sent so far in this probe
    bytes_sent: DataSize,
    /// Runtime byte target, including coarse scheduling and minimum packet count.
    byte_target: DataSize,

    /// Total packets sent so far in this probe
    packets_sent: usize,

    /// When the first packet was sent
    /// None = probe hasn't started yet (still queued)
    started_at: Option<Instant>,

    /// When the last packet was sent
    /// Used to calculate actual send duration (first to last packet)
    last_packet_at: Option<Instant>,

    /// When we last created a padding request
    /// Used to prevent creating multiple padding packets for the same instant
    last_padding_at: Instant,
    /// A requested burst has not reached its first actual socket send yet.
    padding_pending: bool,
    paced_target_set: bool,
}

impl ProbeClusterState {
    /// Create a new probe cluster state from a config.
    pub fn new(config: ProbeClusterConfig) -> Self {
        Self {
            config,
            bytes_sent: DataSize::ZERO,
            byte_target: config.target_bytes().min(config.max_bytes),
            packets_sent: 0,
            started_at: None,
            last_packet_at: None,
            last_padding_at: already_happened(),
            padding_pending: false,
            paced_target_set: false,
        }
    }

    /// Get the probe configuration.
    pub fn config(&self) -> &ProbeClusterConfig {
        &self.config
    }

    /// Calculates the next probe time based on total bytes sent and target bitrate.
    /// This naturally handles variable packet sizes (media packets can be much larger
    /// than padding packets). Next packet can be sent when:
    /// `now >= start_time + (bytes_sent / target_bitrate)`
    pub fn next_probe_time(&self) -> Instant {
        let Some(started_at) = self.started_at else {
            return already_happened();
        };

        if self.config.target_bitrate == Bitrate::ZERO {
            return not_happening();
        }

        // Packet-level pacing: schedule based on bytes sent at the target bitrate.
        // next_time = start_time + (bytes_sent / target_bitrate)
        let send_duration = self.bytes_sent / self.config.target_bitrate;
        let probe_time = started_at + send_duration;

        let min_burst_time = self.last_padding_at + self.config.min_probe_delta();
        probe_time.max(min_burst_time)
    }

    /// Check if the probe cluster is complete.
    ///
    /// A probe is complete when BOTH conditions are met (matching WebRTC):
    /// 1. Sent bytes >= runtime target (nominal duration, or the selected unpaced burst interval)
    /// 2. Sent packets >= min_packet_count
    ///
    /// Note: WebRTC does NOT check duration for completion. Duration is only used
    /// to calculate the target bytes threshold. This ensures probes complete even
    /// when all packets are sent at the same instant (e.g., in tests or when the
    /// pacer generates bursts faster than wall-clock time advances).
    /// Explicitly byte-limited probes allow one regular wake (up to 20 ms) for
    /// a usable second timestamp before ending an otherwise complete cluster.
    pub fn is_complete(&self, now: Instant) -> bool {
        if self.started_at.is_none() {
            return false; // Not started yet
        }

        // Match WebRTC's BitrateProber::ProbeSent() logic:
        // if (sent_bytes >= probe_cluster_min_bytes && sent_probes >= probe_cluster_min_probes)
        let bytes_met = self.bytes_sent >= self.byte_target;
        let packets_met = self.packets_sent >= self.config.min_packet_count;

        // Application-paced media can consume the entire padding tail in the
        // first burst. A byte-limited cluster still needs a later timestamp,
        // otherwise the probe estimator rejects it with a zero send interval.
        // If the next wake misses the supported regular cadence, finish without
        // adding a timing packet: the zero-duration result is discarded rather
        // than measuring the scheduler stall as a loss of network capacity.
        let timing_met = !self.config.is_byte_limited()
            || self.last_packet_at > self.started_at
            || now.saturating_duration_since(self.started_at.unwrap()) > Duration::from_millis(20);
        bytes_met && packets_met && timing_met
    }

    /// Check if it's time to send the next probe packet.
    ///
    /// Returns `true` if `now >= next_probe_time()`, meaning we should send a packet now.
    pub fn should_send_now(&self, now: Instant) -> bool {
        let next_time = self.next_probe_time();
        let min_burst_time = self.last_padding_at + self.config.min_probe_delta();
        now >= next_time && now >= min_burst_time
    }

    /// Queue a complete probe for an external wire-byte pacer. The pacer, rather
    /// than padding generation, owns send spacing. Coarse clocks need several
    /// samples; they must not turn a single burst into a low capacity estimate.
    pub(crate) fn next_paced_packet(&mut self, cadence: Duration) -> Option<DataSize> {
        if self.padding_pending {
            return None;
        }
        if !self.paced_target_set {
            let duration = self
                .config
                .target_duration
                .max(cadence * 2 + self.config.min_probe_delta);
            self.byte_target = (self.config.target_bitrate * duration).min(self.config.max_bytes);
            self.paced_target_set = true;
        }
        let remaining = self.byte_target.saturating_sub(self.bytes_sent);
        self.padding_pending = true;
        Some(remaining.max(MAX_PADDING_PACKET_SIZE))
    }

    /// Calculate how much padding should be generated for the next probe packet.
    ///
    /// WebRTC probing uses **bursts** of packets, with a minimum delta between bursts
    /// (`min_probe_delta`). In str0m, a `PaddingRequest` is expressed as a number of bytes
    /// to queue (which may result in multiple RTP padding packets), so we request a burst
    /// sized approximately as:
    ///
    /// `burst_bytes = target_bitrate * min_probe_delta`
    ///
    /// This avoids unintentionally capping probe throughput at `MAX_PADDING_PACKET_SIZE /
    /// min_probe_delta` (e.g. 240 bytes / 2ms = 960 kbit/s), which would be far below the
    /// intended probe bitrate.
    ///
    /// `wake_interval` is the application's selected burst interval for unpaced
    /// streams, or zero to retain the default first-burst size for paced streams.
    /// Returns `None` if it's not time to send yet, or if we've already created
    /// a padding packet for this instant.
    pub fn next_packet(&mut self, now: Instant, wake_interval: Duration) -> Option<DataSize> {
        if self.started_at.is_none() {
            self.started_at = Some(now);
        }

        // Enforce min burst spacing (`min_probe_delta`) between successive padding requests.
        // (The very first burst is not gated because `last_padding_at` starts at already_happened()).
        if now < self.last_padding_at + self.config.min_probe_delta() {
            return None;
        }

        let first_burst = self.last_padding_at < self.started_at.unwrap();
        let top_up = self.config.is_byte_limited() && !wake_interval.is_zero();
        if now < self.next_probe_time() && !(first_burst && top_up) {
            return None;
        }

        // Check if we've already created a padding packet for this exact instant
        // This prevents creating multiple padding packets when handle_timeout() is
        // called multiple times with the same `now` value
        if self.last_padding_at >= now {
            return None;
        }

        let coarse = wake_interval > self.config.min_probe_delta();
        // A cluster must span the selected burst interval. Keeping only 15 ms worth
        // of bytes at a 20 ms cadence systematically undermeasures its target rate.
        // Reserve a final minimum interval so actual sends have distinct timestamps.
        let burst_duration = if first_burst {
            let duration = self
                .config
                .target_duration
                .max(wake_interval + self.config.min_probe_delta());
            self.byte_target = (self.config.target_bitrate * duration).min(self.config.max_bytes);
            if coarse {
                self.byte_target = self
                    .byte_target
                    .max(MAX_PADDING_PACKET_SIZE * self.config.min_packet_count() as u64)
                    .min(self.config.max_bytes);
            }
            wake_interval
                .min(duration.saturating_sub(self.config.min_probe_delta()))
                .max(self.config.min_probe_delta())
        } else {
            now.saturating_duration_since(self.last_padding_at)
        };
        self.last_padding_at = now;
        // At low estimates, batch the already-required minimum packet count over
        // two wakes rather than capping feedback at one small packet per wake.
        let burst_floor = if coarse {
            MAX_PADDING_PACKET_SIZE * self.config.min_packet_count().div_ceil(2) as u64
        } else {
            MAX_PADDING_PACKET_SIZE
        };
        let mut burst_bytes = self.config.target_bitrate * burst_duration;
        if first_burst && top_up {
            // Media can start an unpaced probe before padding is requested. Top
            // up that same burst instead of waiting a wake and undermeasuring it.
            burst_bytes = burst_bytes.saturating_sub(self.bytes_sent);
        }
        let mut recommended_probe_size = burst_bytes.max(burst_floor);
        if first_burst && coarse && top_up {
            // Include initial media when leaving a tail for a distinct timestamp.
            recommended_probe_size = recommended_probe_size.min(
                self.byte_target
                    .saturating_sub(self.bytes_sent)
                    .saturating_sub(MAX_PADDING_PACKET_SIZE),
            );
        }
        let bytes_remaining = self.byte_target.saturating_sub(self.bytes_sent);

        // Like BitrateProber::ProbeSent, completion requires both the byte and
        // packet thresholds. At low rates the byte target can fit in one packet;
        // keep sending bounded padding until the minimum packet count is met.
        // If media spent the byte target at one clock, one later blank packet
        // supplies usable timing within the application's packet-rounding reserve.
        let needs_timestamp = self.config.is_byte_limited()
            && self.last_packet_at == self.started_at
            && self.started_at.is_some_and(|start| now > start);
        let request_bytes = if bytes_remaining == DataSize::ZERO
            && (self.packets_sent < self.config.min_packet_count || needs_timestamp)
        {
            MAX_PADDING_PACKET_SIZE
        } else {
            cmp::min(bytes_remaining, recommended_probe_size)
        };

        if request_bytes == DataSize::ZERO {
            None
        } else {
            self.padding_pending = true;
            Some(request_bytes)
        }
    }

    /// Record a packet that was sent as part of this probe (media or padding).
    ///
    /// This updates the probe's tracking state so that `next_probe_time()` correctly
    /// calculates when the next packet should be sent based on the target bitrate.
    ///
    /// Should be called by the pacer whenever ANY packet is sent during an active probe,
    /// not just padding packets generated by `next_packet()`.
    pub fn record_packet(&mut self, now: Instant, size: DataSize) {
        // If a probe is active and we observe a media packet before any probe-generated padding,
        // treat that as the start of the probe. This keeps timing semantics consistent and ensures
        // `next_probe_time()` enforces `min_probe_delta` from the first packet onwards.
        if self.packets_sent == 0 {
            self.started_at = Some(now);
        }
        if self.padding_pending {
            // Padding can be prepared on a timeout and polled only on a later wake.
            // Space from the actual send; otherwise the whole next burst could also
            // drain at this clock, leaving the estimator a zero-duration cluster.
            self.last_padding_at = now;
            self.padding_pending = false;
        }

        self.bytes_sent += size;
        self.packets_sent += 1;
        self.last_packet_at = Some(now);
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn byte_limited_probe_keeps_a_tail_and_stays_bounded_after_a_stall() {
        let now = Instant::now();
        let interval = Duration::from_millis(20);
        for media in [0, 500] {
            let config = ProbeClusterConfig::new(0.into(), Bitrate::mbps(30), ProbeKind::Initial)
                .with_max_bytes(DataSize::bytes(2000));
            let mut state = ProbeClusterState::new(config);
            if media > 0 {
                state.record_packet(now, DataSize::bytes(media));
            }
            let padding = state.next_packet(now, interval).unwrap();
            let first = DataSize::bytes(media) + padding;
            assert!(
                first < DataSize::bytes(2000),
                "leave a distinct send timestamp"
            );
            for at in (0..padding.as_bytes_usize()).step_by(240) {
                state.record_packet(now, (padding.as_bytes_usize() - at).min(240).into());
            }
            assert!(!state.is_complete(now));
            let after_stall = now + Duration::from_millis(250);
            let tail = state.next_packet(after_stall, interval).unwrap();
            assert_eq!(first + tail, DataSize::bytes(2000));
            state.record_packet(after_stall, tail);
            assert!(state.is_complete(after_stall));
        }
    }

    #[test]
    fn byte_limited_media_cannot_complete_a_probe_at_one_timestamp() {
        let now = Instant::now();
        let interval = Duration::from_millis(20);
        let config = ProbeClusterConfig::new(0.into(), Bitrate::mbps(10), ProbeKind::Initial)
            .with_max_bytes(DataSize::bytes(2000));
        for padding_first in [false, true] {
            let mut state = ProbeClusterState::new(config);
            if padding_first {
                let padding = state.next_packet(now, interval).unwrap();
                state.record_packet(now, padding);
            }
            // Released originals can consume the reserved tail before another wake.
            for _ in 0..5 {
                state.record_packet(now, DataSize::bytes(400));
            }
            assert!(
                !state.is_complete(now),
                "zero-duration feedback is unusable"
            );
            assert!(state.next_packet(now, interval).is_none());
            assert!(
                state.is_complete(now + Duration::from_millis(150)),
                "end without tagging a late packet after a scheduler stall"
            );
            let later = now + interval;
            let padding = state.next_packet(later, interval).unwrap();
            assert_eq!(
                padding, MAX_PADDING_PACKET_SIZE,
                "one bounded timestamp packet"
            );
            state.record_packet(later, padding);
            assert!(state.is_complete(later));
        }
    }

    #[test]
    fn late_probe_wakes_catch_up_without_exceeding_the_cluster_budget() {
        let now = Instant::now();
        let config = ProbeClusterConfig::new(0.into(), Bitrate::mbps(10), ProbeKind::Initial);
        let mut state = ProbeClusterState::new(config);
        let first = state.next_packet(now, Duration::ZERO).unwrap();
        state.record_packet(now, first);
        let late = now + Duration::from_millis(10);
        let burst = state.next_packet(late, Duration::ZERO).unwrap();
        assert_eq!(burst, Bitrate::mbps(10) * Duration::from_millis(10));
        state.record_packet(late, burst);
        let later = late + Duration::from_millis(10);
        let last = state.next_packet(later, Duration::ZERO).unwrap();
        assert_eq!(first + burst + last, config.target_bytes());
        assert!(
            state.next_packet(later, Duration::ZERO).is_none(),
            "one burst per wake"
        );
    }

    #[test]
    fn delayed_poll_spaces_bursts_from_actual_send_time() {
        let now = Instant::now();
        let interval = Duration::from_millis(20);
        let config = ProbeClusterConfig::new(0.into(), Bitrate::mbps(10), ProbeKind::Initial);
        let mut state = ProbeClusterState::new(config);
        let first = state.next_packet(now, interval).unwrap();
        let sent = now + interval;
        state.record_packet(sent, first);
        assert!(state.next_packet(sent, interval).is_none());
        let second = state.next_packet(sent + interval, interval).unwrap();
        assert_eq!(
            first + second,
            config.target_bitrate() * (interval + config.min_probe_delta())
        );
        assert_eq!(state.started_at, Some(sent));
    }

    #[test]
    fn coarse_wakes_balance_the_bursts_within_the_cluster_budget() {
        let now = Instant::now();
        for cadence in [15, 20] {
            let interval = Duration::from_millis(cadence);
            let config = ProbeClusterConfig::new(0.into(), Bitrate::mbps(10), ProbeKind::Initial);
            let mut state = ProbeClusterState::new(config);
            let first = state.next_packet(now, interval).unwrap();
            assert_eq!(first, config.target_bitrate() * interval);
            state.record_packet(now, first);
            let second = state.next_packet(now + interval, interval).unwrap();
            assert_eq!(
                first + second,
                config.target_bitrate() * (interval + config.min_probe_delta())
            );
            assert!(state.next_packet(now + interval, interval).is_none());
        }
    }

    #[test]
    fn coarse_low_rate_probe_batches_the_minimum_packets() {
        let now = Instant::now();
        let interval = Duration::from_millis(20);
        let config = ProbeClusterConfig::new(0.into(), Bitrate::kbps(200), ProbeKind::Initial);
        let mut state = ProbeClusterState::new(config);
        let first = state.next_packet(now, interval).unwrap();
        assert_eq!(first, MAX_PADDING_PACKET_SIZE * 3u64);
        for _ in 0..3 {
            state.record_packet(now, MAX_PADDING_PACKET_SIZE);
        }
        let later = now + interval * 2;
        let second = state.next_packet(later, interval).unwrap();
        assert_eq!(second, MAX_PADDING_PACKET_SIZE * 2u64);
        for _ in 0..2 {
            state.record_packet(later, MAX_PADDING_PACKET_SIZE);
        }
        assert!(state.is_complete(later));
        assert_eq!(
            first + second,
            MAX_PADDING_PACKET_SIZE * config.min_packet_count() as u64
        );
    }

    #[test]
    fn low_rate_padding_reaches_minimum_packet_count() {
        let now = Instant::now();
        let config = ProbeClusterConfig::new(0.into(), Bitrate::kbps(10), ProbeKind::Initial);
        let mut state = ProbeClusterState::new(config);
        for i in 0..config.min_packet_count() {
            let at = if i == 0 { now } else { state.next_probe_time() };
            let size = state
                .next_packet(at, Duration::ZERO)
                .expect("minimum packets still needed");
            assert!(size <= MAX_PADDING_PACKET_SIZE);
            state.record_packet(at, size);
        }
        assert!(state.is_complete(state.next_probe_time()));
    }

    // Test helper to directly set state for testing
    impl ProbeClusterState {
        fn test_set_state(&mut self, bytes_sent: DataSize, packets_sent: usize, now: Instant) {
            self.bytes_sent = bytes_sent;
            self.packets_sent = packets_sent;
            if self.started_at.is_none() {
                self.started_at = Some(now);
            }
            self.last_packet_at = Some(now);
        }
    }

    #[test]
    fn probe_cluster_not_complete_before_start() {
        let now = Instant::now();
        let config = ProbeClusterConfig::new(1.into(), Bitrate::mbps(3), ProbeKind::Initial);
        let state = ProbeClusterState::new(config);

        assert!(!state.is_complete(now));
    }

    #[test]
    fn probe_cluster_not_complete_after_packets_but_not_bytes() {
        let now = Instant::now();
        let config = ProbeClusterConfig::new(1.into(), Bitrate::mbps(3), ProbeKind::Initial);
        // target_bytes = 3 Mbps * 15ms = 45,000 bits = 5,625 bytes
        let mut state = ProbeClusterState::new(config);

        // Send 5 packets (meets packet count) but only small packets (doesn't meet bytes)
        // 5 * 100 = 500 bytes, which is < 5,625 bytes
        state.test_set_state(DataSize::bytes(500), 5, now);

        // Not complete: packets met but bytes not met
        assert!(!state.is_complete(now));
    }

    #[test]
    fn probe_cluster_not_complete_after_bytes_but_not_packets() {
        let now = Instant::now();
        let config = ProbeClusterConfig::new(1.into(), Bitrate::mbps(3), ProbeKind::Initial);
        // target_bytes = 3 Mbps * 15ms = 45,000 bits = 5,625 bytes
        let mut state = ProbeClusterState::new(config);

        // Send 2 large packets (meets bytes) but not enough packets
        // 2 * 3000 = 6000 bytes > 5,625 bytes, but only 2 packets < 5 packets
        state.test_set_state(DataSize::bytes(6000), 2, now);

        // Not complete: bytes met but packets not met
        assert!(!state.is_complete(now));
    }

    #[test]
    fn probe_cluster_complete_when_both_criteria_met() {
        let now = Instant::now();
        let config = ProbeClusterConfig::new(1.into(), Bitrate::mbps(3), ProbeKind::Initial);
        // target_bytes = 3 Mbps * 15ms = 45,000 bits = 5,625 bytes
        let mut state = ProbeClusterState::new(config);

        // Send 5 packets of 1200 bytes each = 6000 bytes
        // Meets both: 6000 >= 5625 bytes AND 5 >= 5 packets
        state.test_set_state(DataSize::bytes(6000), 5, now);

        // Complete: both bytes and packets met, even at same instant
        assert!(state.is_complete(now));
    }

    #[test]
    fn probe_cluster_complete_even_with_zero_duration() {
        let now = Instant::now();
        let config = ProbeClusterConfig::new(1.into(), Bitrate::mbps(3), ProbeKind::Initial);
        let mut state = ProbeClusterState::new(config);

        // Send all packets at exactly the same instant (duration = 0)
        state.test_set_state(DataSize::bytes(6000), 5, now);

        // Should still complete (this is the bug fix - no duration requirement)
        assert!(state.is_complete(now));
    }

    #[test]
    fn probe_cluster_tracks_bytes_and_packets() {
        let now = Instant::now();
        let config = ProbeClusterConfig::new(1.into(), Bitrate::mbps(3), ProbeKind::Initial);
        let mut state = ProbeClusterState::new(config);

        state.test_set_state(DataSize::bytes(1200), 1, now);
        assert_eq!(state.bytes_sent, DataSize::bytes(1200));
        assert_eq!(state.packets_sent, 1);

        state.test_set_state(DataSize::bytes(2200), 2, now + Duration::from_millis(1));
        assert_eq!(state.bytes_sent, DataSize::bytes(2200));
        assert_eq!(state.packets_sent, 2);
    }

    #[test]
    fn min_probe_delta_is_enforced_in_next_probe_time() {
        let now = Instant::now();
        let config = ProbeClusterConfig::new(1.into(), Bitrate::mbps(3), ProbeKind::Initial)
            .with_min_probe_delta(Duration::from_millis(20));
        let mut state = ProbeClusterState::new(config);
        assert_eq!(state.config().min_probe_delta(), Duration::from_millis(20));

        // Start a burst at `now`, then verify that a second burst is blocked until >= now+20ms.
        assert!(state.next_packet(now, Duration::ZERO).is_some());
        assert!(
            state
                .next_packet(now + Duration::from_millis(19), Duration::ZERO)
                .is_none()
        );
        assert!(
            state
                .next_packet(now + Duration::from_millis(20), Duration::ZERO)
                .is_some()
        );
        let next = state.last_padding_at;
        assert!(
            next >= now + Duration::from_millis(20),
            "next_probe_time {next:?} must be >= now + min_probe_delta"
        );
    }
}
