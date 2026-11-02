//! Google Congestion Control (GoogCC) Bandwidth Estimation based on TWCC feedback.
//!
//! This implementation is ported from libWebRTC's GoogCC and goes beyond the simplified
//! IETF draft (<https://datatracker.ietf.org/doc/html/draft-ietf-rmcat-gcc-02>) to include
//! WebRTC's production features:
//!
//! - Delay-based control (trendline estimator with AIMD rate control)
//! - Loss-based control (with inherent loss rate estimation)
//! - Probe controller with state machine and multi-stage probing strategy
//! - ALR (Application Limited Region) detection and periodic probing
//! - Link capacity estimation from ALR probes
//!
//! The probe controller in particular closely matches WebRTC's `ProbeController` behavior
//! and default constants, enabling compatible bandwidth discovery with WebRTC endpoints.

use std::cmp::Ordering;
use std::fmt;
use std::time::{Duration, Instant};

use crate::Reason;
use crate::rtp_::{Bitrate, DataSize, TwccClusterId, TwccSendRecord, TwccSeq};
use crate::util::Soonest;

mod acked_bitrate_estimator;
mod alr_detector;
pub(crate) mod api;
mod delay;
mod loss_controller;
mod macros;
mod probe;
mod smoother;
mod time;

use acked_bitrate_estimator::AckedBitrateEstimator;
use alr_detector::AlrDetector;
use delay::DelayController;
use loss_controller::{LossController, LossControllerState};
use macros::log_loss;
use smoother::EstimateSmoother;

pub(crate) use macros::{log_pacer_media_debt, log_pacer_padding_debt};
#[cfg(test)]
pub(crate) use probe::ProbeKind;
pub(crate) use probe::{BandwidthLimitedCause, ProbeEstimator};
pub(crate) use probe::{ProbeClusterState, ProbeControl};

#[cfg(feature = "_internal_test_exports")]
pub use probe::ProbeClusterConfig;
#[cfg(not(feature = "_internal_test_exports"))]
pub(crate) use probe::ProbeClusterConfig;

const INITIAL_BITRATE_WINDOW: Duration = Duration::from_millis(500);
const BITRATE_WINDOW: Duration = Duration::from_millis(150);
const STARTUP_PHASE: Duration = Duration::from_secs(2);

pub struct Bwe {
    bwe: SendSideBandwidthEstimator,
    desired_bitrate: Bitrate,
    smoother: EstimateSmoother,
    min_probe_interval: Duration,
    probe_byte_limit: DataSize,
    probe_bitrate_limit: Option<Bitrate>,
}

impl Bwe {
    pub fn new(initial: Bitrate) -> Self {
        let send_side_bwe = SendSideBandwidthEstimator::new(initial);
        Bwe {
            bwe: send_side_bwe,
            desired_bitrate: Bitrate::ZERO,
            smoother: EstimateSmoother::new(),
            min_probe_interval: Duration::ZERO,
            probe_byte_limit: DataSize::bytes(i64::MAX),
            probe_bitrate_limit: None,
        }
    }

    pub fn handle_timeout(&mut self, now: Instant, do_probe: bool) -> Option<ProbeClusterConfig> {
        let result = self.bwe.handle_timeout(self.desired_bitrate, do_probe, now);
        if let Some(estimate) = self.bwe.last_estimate() {
            self.smoother.record(now, estimate);
        }
        result.map(|config| config.with_max_bytes(self.probe_byte_limit))
    }

    pub fn set_egress_pacing(&mut self, rate: Bitrate) {
        self.probe_bitrate_limit = Some(rate);
        self.bwe.probe_control.set_bitrate_limit(rate);
        self.bwe.application_paced = true;
    }

    pub fn set_probe_byte_limit(&mut self, bytes: usize) {
        self.probe_byte_limit = DataSize::bytes(bytes.min(i64::MAX as usize) as i64);
    }

    pub fn start_probe(&mut self, config: ProbeClusterConfig, now: Instant) -> bool {
        self.bwe.start_probe(config, now)
    }

    pub fn end_probe(&mut self, now: Instant, cluster_id: TwccClusterId) {
        self.bwe.end_probe(now, cluster_id);
    }

    pub fn set_min_probe_interval(&mut self, interval: Duration) {
        if self.min_probe_interval == interval {
            return;
        }
        self.min_probe_interval = interval;
        self.bwe.probe_control.set_min_probe_interval(interval);
    }

    pub fn reset(&mut self, init_bitrate: Bitrate) {
        self.bwe.reset(init_bitrate);
        if let Some(limit) = self.probe_bitrate_limit {
            self.bwe.probe_control.set_bitrate_limit(limit);
            self.bwe.application_paced = true;
        }
        self.smoother = EstimateSmoother::new();
        self.bwe
            .probe_control
            .set_min_probe_interval(self.min_probe_interval);
    }

    pub fn update<'t>(
        &mut self,
        records: impl Iterator<Item = &'t crate::rtp_::TwccSendRecord>,
        now: Instant,
    ) {
        self.bwe.update(records, now);
    }

    pub fn poll_estimate(&mut self) -> Option<Bitrate> {
        self.smoother.poll()
    }

    pub fn poll_timeout(&self) -> (Option<Instant>, Reason) {
        self.bwe.poll_timeout()
    }

    pub fn last_estimate(&self) -> Option<Bitrate> {
        self.bwe.last_estimate()
    }

    pub fn on_media_sent(&mut self, payload_size: DataSize, is_padding: bool, now: Instant) {
        if !is_padding {
            // Update ALR detector with media bytes sent
            self.bwe.on_media_sent(payload_size, now);
        }
    }

    pub fn is_overusing(&self) -> bool {
        self.bwe.is_overusing()
    }

    pub fn set_desired_bitrate(&mut self, v: Bitrate) {
        self.desired_bitrate = v;
    }
}

struct SendSideBandwidthEstimator {
    delay_controller: DelayController,
    loss_controller: LossController,
    acked_bitrate_estimator: AckedBitrateEstimator,
    probe_control: ProbeControl,
    probe_estimator: ProbeEstimator,
    started_at: Option<Instant>,
    alr_detector: AlrDetector,
    last_updated_estimate: Option<Bitrate>,
    application_paced: bool,
}

impl SendSideBandwidthEstimator {
    pub fn new(initial_bitrate: Bitrate) -> Self {
        let mut alr_detector = AlrDetector::new();
        alr_detector.set_estimated_bitrate(initial_bitrate);

        let mut loss_controller = LossController::new();
        loss_controller.set_bandwidth_estimate(initial_bitrate);

        Self {
            delay_controller: DelayController::new(initial_bitrate),
            loss_controller,
            acked_bitrate_estimator: AckedBitrateEstimator::new(
                INITIAL_BITRATE_WINDOW,
                BITRATE_WINDOW,
            ),
            probe_control: ProbeControl::new(),
            probe_estimator: ProbeEstimator::new(),
            started_at: None,
            alr_detector,
            last_updated_estimate: None,
            application_paced: false,
        }
    }

    /// Whether the delay-based detector currently signals overuse.
    ///
    /// This is useful for gating behaviors (like padding/probing) that would otherwise
    /// re-excite the system while we're already congested.
    pub fn is_overusing(&self) -> bool {
        self.delay_controller.is_overusing()
    }

    /// Update ALR detector with actual bytes sent.
    ///
    /// Should be called for media packets (not padding/probes).
    /// This is typically called from the session's packet sending logic.
    pub fn on_media_sent(&mut self, bytes: DataSize, now: Instant) {
        self.alr_detector.on_bytes_sent(bytes, now);
    }

    /// Record a packet from a TWCC report.
    pub fn update<'t>(&mut self, records: impl Iterator<Item = &'t TwccSendRecord>, now: Instant) {
        let _ = self.started_at.get_or_insert(now);

        let send_records: Vec<_> = records.collect();

        // Feed records to probe estimator for analysis and process any new probe results
        let mut latest_probe_result = None;
        let mut latest_probe_capacity = None;
        let previous_delay_estimate = self.delay_controller.last_estimate();
        let previous_estimate = self.last_estimate();
        for (config, bitrate, saturated) in
            self.probe_estimator.update(send_records.iter().copied())
        {
            // A paced probe whose receive rate matches its actual send rate only
            // demonstrates a capacity floor. Local underfill cannot lower a prior
            // estimate; ordinary delay/loss feedback still detects congestion.
            let delay_probe = if config.max_send_gap.is_some() && !saturated {
                if previous_estimate.is_some_and(|previous| bitrate < previous) {
                    // No new capacity above the combined controller result was
                    // demonstrated. Preserve genuine loss-limited backoff.
                    continue;
                }
                // A floor can recover a lower loss estimate without inventing
                // evidence for the higher delay ceiling, or lowering that ceiling.
                bitrate.max(previous_delay_estimate.unwrap_or(bitrate))
            } else {
                bitrate
            };
            latest_probe_result = Some(delay_probe);
            latest_probe_capacity = Some(bitrate);
        }

        let mut acked_packets = vec![];

        let mut max_rtt = None;
        let mut count = 0;
        let mut lost = 0;
        for record in send_records.iter() {
            count += 1;
            let Ok(acked_packet) = (*record).try_into() else {
                lost += 1;
                continue;
            };
            acked_packets.push(acked_packet);
            max_rtt = max_rtt.max(record.rtt());
        }
        acked_packets.sort_by(AckedPacket::order_by_receive_time);

        for acked_packet in acked_packets.iter() {
            self.acked_bitrate_estimator
                .update(acked_packet.remote_recv_time, acked_packet.size);
        }

        let acked_bitrate = self.acked_bitrate_estimator.current_estimate();

        // Use the latest probe result from this update, if any
        let probe_result = latest_probe_result;

        // Update delay controller with the latest probe result
        let maybe_estimate =
            self.delay_controller
                .update(&acked_packets, acked_bitrate, probe_result, now);

        let Some(delay_estimate) = maybe_estimate else {
            return;
        };

        let loss = if count == 0 {
            0.0
        } else {
            lost as f64 / count as f64
        };
        log_loss!(loss);

        // A probe can recover loss-limited capacity, but an unsaturated floor
        // must not expose a higher delay ceiling before a loss observation completes.
        if let Some(capacity) = latest_probe_capacity {
            self.loss_controller
                .set_probe_estimate(capacity.min(delay_estimate), delay_estimate);
        }

        // Startup may follow delay directly only while loss is not limiting it.
        // Otherwise even a later loss-free partial report could erase a probe cap.
        if in_startup_phase(self.started_at, now)
            && loss <= 0.001
            && self.loss_controller.loss_based_result().state == LossControllerState::DelayBased
        {
            self.loss_controller.set_bandwidth_estimate(delay_estimate);
            return;
        }

        if let Some(acked_bitrate) = acked_bitrate {
            self.loss_controller.set_acknowledged_bitrate(acked_bitrate);
        }

        // This corresponds to UpdateLossBasedEstimator + UpdateEstimate
        self.loss_controller
            .update_bandwidth_estimate(&send_records, delay_estimate);

        // Loss-based result is capped by delay_based_limit
        let loss_result = self.loss_controller.loss_based_result();
        if let Some(loss_estimate) = loss_result.bandwidth_estimate {
            if loss_estimate > delay_estimate {
                // Loss controller produced higher estimate than delay controller
                // Cap it at delay estimate (delay controller is the upper limit)
                self.loss_controller.set_bandwidth_estimate(delay_estimate);
            }
        }

        // Feed the (possibly combined) estimate into subcomponents wanting it.
        self.propagate_estimate();
    }

    pub fn poll_timeout(&self) -> (Option<Instant>, Reason) {
        let delay_timeout = Some(self.delay_controller.poll_timeout());
        let probe_timeout = Some(self.probe_control.poll_timeout());
        let probe_estimator_timeout = Some(self.probe_estimator.poll_timeout());
        (delay_timeout, Reason::BweDelayControl)
            .soonest((probe_timeout, Reason::BweProbeControl))
            .soonest((probe_estimator_timeout, Reason::BweProbeEstimator))
    }

    /// Handle periodic timeout for BWE components.
    pub fn handle_timeout(
        &mut self,
        desired_bitrate: Bitrate,
        do_probe: bool,
        now: Instant,
    ) -> Option<ProbeClusterConfig> {
        self.delay_controller
            .handle_timeout(self.acked_bitrate_estimator.current_estimate(), now);

        // Update probe control with desired bitrate.
        self.probe_control.set_desired_bitrate(desired_bitrate);

        // Empty media queues are application-limited too. Advance the budget
        // without charging probe/padding bytes, including before the first media.
        if do_probe {
            self.alr_detector.handle_timeout(now);
        }

        // Get ALR state and forward to both probe control and loss controller
        // An explicit application ceiling is limited by policy even above the
        // utilization detector's 65% threshold. Its ACK rate cannot be used as
        // evidence that the network itself has lost capacity.
        let alr_start_time = self
            .alr_detector
            .alr_start_time()
            .or_else(|| self.application_paced.then_some(now));
        if let Some(t) = alr_start_time {
            self.probe_control.set_alr_start_time(t);
        } else {
            self.probe_control.set_alr_stop_time(now);
        }

        self.loss_controller.set_alr_start_time(alr_start_time);

        // Clean up expired probe cluster state
        self.probe_estimator.handle_timeout(now);

        // Feed the current estimate into subcontrollers, if it changed.
        self.propagate_estimate();

        // If we can't probe, clear any pending/active probes
        if !do_probe {
            self.probe_estimator.clear_probes();
        }

        self.probe_control.enable(do_probe);

        // Timeout-driven probe logic (WebRTC `Process()` equivalent).
        self.probe_control.handle_timeout(now)
    }

    fn propagate_estimate(&mut self) {
        // Do we have a value?
        let Some(estimate) = self.last_estimate() else {
            return;
        };
        // Did it change?
        if self.last_updated_estimate == Some(estimate) {
            return;
        }

        let cause = self.bandwidth_limited_cause();

        self.probe_control.set_estimated_bitrate(estimate, cause);
        self.alr_detector.set_estimated_bitrate(estimate);

        // Don't update until this changes.
        self.last_updated_estimate = Some(estimate);
    }

    fn bandwidth_limited_cause(&self) -> BandwidthLimitedCause {
        if self.delay_controller.is_overusing() {
            return BandwidthLimitedCause::DelayBasedLimitedDelayIncreased;
        }

        match self.loss_controller.loss_based_result().state {
            LossControllerState::DelayBased => BandwidthLimitedCause::DelayBasedLimited,
            LossControllerState::Increasing => BandwidthLimitedCause::LossLimitedBweIncreasing,
            LossControllerState::Decreasing => BandwidthLimitedCause::LossLimitedBwe,
        }
    }

    /// Get the latest estimate.
    pub fn last_estimate(&self) -> Option<Bitrate> {
        let delay_estimate = self.delay_controller.last_estimate();

        let loss_result = self.loss_controller.loss_based_result();

        // Only apply loss-based limiting when actively in a loss-limiting state
        match loss_result.state {
            LossControllerState::DelayBased => {
                // Loss controller defers to delay-based estimate
                delay_estimate
            }
            LossControllerState::Decreasing | LossControllerState::Increasing => {
                // Loss controller is actively limiting or recovering
                match (delay_estimate, loss_result.bandwidth_estimate) {
                    (Some(de), Some(le)) => Some(de.min(le)),
                    (None, le @ Some(_)) => le,
                    (de @ Some(_), None) => de,
                    (None, None) => None,
                }
            }
        }
    }

    /// Start analyzing a probe cluster.
    ///
    /// This should be called when the pacer starts sending a probe cluster,
    /// to tell the estimator which cluster to watch for in TWCC feedback.
    /// Returns `true` if the probe was started, `false` if rejected.
    pub fn start_probe(&mut self, config: ProbeClusterConfig, now: Instant) -> bool {
        self.probe_estimator.probe_start(config, now)
    }

    /// End a probe cluster and mark it for cleanup.
    ///
    /// This should be called when the pacer finishes sending a probe cluster.
    /// The estimator will continue collecting feedback for a cluster history period
    /// to allow late-arriving TWCC reports to refine the estimate.
    pub fn end_probe(&mut self, now: Instant, cluster_id: TwccClusterId) {
        self.probe_estimator.end_probe(now, cluster_id);
    }

    pub fn reset(&mut self, init_bitrate: Bitrate) {
        *self = Self::new(init_bitrate);
    }
}

/// A RTP packet that has been sent and acknowledged by the receiver in a TWCC report.
#[derive(Debug, Copy, Clone)]
pub struct AckedPacket {
    /// The TWCC sequence number
    seq_no: TwccSeq,
    /// The size of the packets in bytes.
    size: DataSize,
    /// When we sent the packet
    local_send_time: Instant,
    /// When the packet was received at the remote, note this Instant is only usable with other
    /// instants of the same type i.e. those that represent a TWCC reported receive time for this
    /// session.
    remote_recv_time: Instant,
    /// The local time when received confirmation that the other side received the seq i.e. when we
    /// received the TWCC report for this packet.
    local_recv_time: Instant,
}

impl AckedPacket {
    fn rtt(&self) -> Duration {
        self.local_recv_time - self.local_send_time
    }

    fn order_by_receive_time(lhs: &Self, rhs: &Self) -> Ordering {
        if lhs.remote_recv_time != rhs.remote_recv_time {
            lhs.remote_recv_time.cmp(&rhs.remote_recv_time)
        } else if lhs.local_send_time != rhs.local_send_time {
            lhs.local_send_time.cmp(&rhs.local_send_time)
        } else {
            lhs.seq_no.cmp(&rhs.seq_no)
        }
    }
}

// NB: Extracted for lifetime reasons
fn in_startup_phase(started_at: Option<Instant>, now: Instant) -> bool {
    started_at
        .map(|s| now.duration_since(s) <= STARTUP_PHASE)
        .unwrap_or(false)
}

impl TryFrom<&TwccSendRecord> for AckedPacket {
    type Error = ();

    fn try_from(value: &TwccSendRecord) -> Result<Self, Self::Error> {
        let Some(remote_recv_time) = value.remote_recv_time() else {
            return Err(());
        };
        let Some(local_recv_time) = value.local_recv_time() else {
            return Err(());
        };

        Ok(Self {
            seq_no: value.seq(),
            size: value.size().into(),
            local_send_time: value.local_send_time(),
            remote_recv_time,
            local_recv_time,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BandwidthUsage {
    Overuse,
    Normal,
    Underuse,
}

impl fmt::Display for BandwidthUsage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BandwidthUsage::Overuse => write!(f, "overuse"),
            BandwidthUsage::Normal => write!(f, "normal"),
            BandwidthUsage::Underuse => write!(f, "underuse"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtp_::{TwccClusterId, TwccPacketId};

    #[test]
    fn explicit_application_pacing_keeps_periodic_discovery_above_the_alr_threshold() {
        for reset in [false, true] {
            let base = Instant::now();
            let mut controller = Bwe::new(Bitrate::mbps(5));
            controller.set_egress_pacing(Bitrate::kbps(6250));
            if reset {
                controller.reset(Bitrate::mbps(5));
            }
            let bwe = &mut controller.bwe;
            let mut periodic = false;
            for tick in 0..=600 {
                let at = base + Duration::from_millis(tick * 10);
                // 4 Mbps is policy-limited to 80%, above automatic ALR's 65%.
                bwe.on_media_sent(DataSize::bytes(5000), at);
                if let Some(config) = bwe.handle_timeout(Bitrate::mbps(50), true, at) {
                    periodic |= config.is_alr_probe();
                }
            }
            assert!(
                periodic,
                "the policy ceiling cannot suppress capacity discovery"
            );
        }
    }

    #[test]
    fn probe_floor_caps_recovery_through_partial_loss_observations() {
        for startup in [false, true] {
            let base = Instant::now();
            let mut controller = Bwe::new(Bitrate::mbps(5));
            let bwe = &mut controller.bwe;
            bwe.started_at = Some(if startup { base } else { base - STARTUP_PHASE });
            bwe.loss_controller
                .set_bandwidth_estimate(Bitrate::kbps(500));
            let loss_packets: Vec<_> = (0..26)
                .map(|i| {
                    TwccSendRecord::test_new(
                        TwccPacketId::new(i + 1),
                        base + Duration::from_millis(i * 10),
                        1200,
                        base + Duration::from_millis(300),
                        (i % 2 == 0).then_some(base + Duration::from_millis(i * 10 + 2)),
                    )
                })
                .collect();
            // Establish a loss-limited result below a still-valid 5 Mbps delay ceiling.
            bwe.loss_controller.update_bandwidth_estimate(
                &loss_packets.iter().collect::<Vec<_>>(),
                Bitrate::mbps(5),
            );
            let next_loss_packets: Vec<_> = (0..26)
                .map(|i| {
                    TwccSendRecord::test_new(
                        TwccPacketId::new(100 + i),
                        base + Duration::from_millis(260 + i * 10),
                        1200,
                        base + Duration::from_millis(550),
                        None,
                    )
                })
                .collect();
            bwe.loss_controller.update_bandwidth_estimate(
                &next_loss_packets.iter().collect::<Vec<_>>(),
                Bitrate::mbps(5),
            );
            assert!(bwe.last_estimate().unwrap() <= Bitrate::kbps(500));
            assert_ne!(
                bwe.loss_controller.loss_based_result().state,
                LossControllerState::DelayBased
            );

            let cluster: TwccClusterId = 7.into();
            let config = ProbeClusterConfig::new(cluster, Bitrate::mbps(10), ProbeKind::Initial)
                .with_max_send_gap(Duration::from_millis(20));
            let config = config.with_duration(Duration::from_millis(10));
            let probe_start = base + Duration::from_millis(520);
            assert!(bwe.start_probe(config, probe_start));
            let packets: Vec<_> = (0..12)
                .map(|i| {
                    TwccSendRecord::test_new(
                        TwccPacketId::with_cluster(1000 + i, cluster),
                        probe_start + Duration::from_millis(i * 16),
                        1200,
                        base + Duration::from_millis(710),
                        Some(probe_start + Duration::from_millis(i * 16 + 2)),
                    )
                })
                .collect();
            // Twelve 1200-byte packets spaced 16 ms apart demonstrate only 600 kbps.
            bwe.update(packets.iter(), base + Duration::from_millis(710));
            assert_eq!(bwe.delay_controller.last_estimate(), Some(Bitrate::mbps(5)));
            assert_eq!(bwe.last_estimate(), Some(Bitrate::kbps(600)));
            bwe.end_probe(base + Duration::from_millis(710), cluster);

            // More feedback arrives before the 250 ms loss observation can complete.
            for i in 0..3 {
                let at = base + Duration::from_millis(710 + i * 10);
                let record = TwccSendRecord::test_new(
                    TwccPacketId::new(2000 + i),
                    at,
                    1200,
                    at + Duration::from_millis(20),
                    Some(at + Duration::from_millis(2)),
                );
                bwe.update([&record].into_iter(), at + Duration::from_millis(20));
                assert_eq!(bwe.last_estimate(), Some(Bitrate::kbps(600)));
            }
            controller.handle_timeout(base + Duration::from_millis(750), true);
            assert!(controller.poll_estimate().unwrap() <= Bitrate::kbps(600));
        }
    }

    #[test]
    fn paced_probe_underfill_is_a_floor_but_network_queueing_still_reduces_capacity() {
        for queued in [false, true] {
            let base = Instant::now();
            let mut bwe = SendSideBandwidthEstimator::new(Bitrate::mbps(5));
            let cluster: TwccClusterId = 7.into();
            let config = ProbeClusterConfig::new(cluster, Bitrate::mbps(10), ProbeKind::Initial)
                .with_max_send_gap(Duration::from_millis(20));
            assert!(bwe.probe_estimator.probe_start(config, base));
            let packets: Vec<_> = (0..20)
                .map(|i| {
                    TwccSendRecord::test_new(
                        TwccPacketId::with_cluster(1000 + i, cluster),
                        base + Duration::from_millis(i * 4),
                        1200,
                        base + Duration::from_millis(300),
                        Some(base + Duration::from_millis(i * if queued { 8 } else { 4 } + 2)),
                    )
                })
                .collect();
            bwe.update(packets.iter(), base + Duration::from_millis(300));
            let estimate = bwe.last_estimate().unwrap();
            if queued {
                assert!(
                    estimate < Bitrate::mbps(5),
                    "real congestion must still reduce capacity"
                );
            } else {
                assert!(
                    estimate >= Bitrate::mbps(5),
                    "local probe underfill manufactured a capacity drop"
                );
            }
        }
    }
}
