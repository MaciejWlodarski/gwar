//! Bandwidth estimation.

use std::time::Duration;

use crate::{Rtc, rtp_::Mid};

pub use crate::rtp_::Bitrate;

#[derive(Debug, PartialEq)]
#[non_exhaustive]
/// Bandwidth estimation kind.
pub enum BweKind {
    /// Transport wide congestion control.
    ///
    /// Emits the current estimate with `can_probe: true` when probing becomes
    /// available, initially using the configured starting bitrate. Estimate
    /// updates follow while probing is available. On loss of probing capability,
    /// emits the last reported estimate once with `can_probe: false`, then
    /// suppresses updates until probing becomes available again.
    Twcc {
        /// Estimated available bitrate. Initially this is the configured starting bitrate.
        /// When probing becomes unavailable, this retains the last estimate.
        estimate: Bitrate,
        /// Whether the local BWE can currently probe for capacity.
        ///
        /// Requires BWE to be enabled, SRTP keys, and a sending media section
        /// supporting TWCC feedback and the transport sequence extension.
        /// This does not guarantee that feedback is arriving.
        can_probe: bool,
    },
    /// REMB (Receiver Estimated Maximum Bitrate)
    Remb {
        /// Bitrate reported by the remote receiver.
        estimate: Bitrate,
        /// Media section associated with the report.
        mid: Mid,
    },
}

/// Access to the Bandwidth Estimate subsystem.
pub struct Bwe<'a>(pub(crate) &'a mut Rtc);

impl<'a> Bwe<'a> {
    /// Configure the desired bitrate.
    ///
    /// Configure the bandwidth estimation system with the desired bitrate.
    /// **Note:** This only has an effect if BWE has been enabled via
    /// [`RtcConfig::enable_bwe`][crate::RtcConfig::enable_bwe].
    ///
    /// * `desired_bitrate` The bitrate you would like to eventually send at. The BWE system will try
    ///   to reach this bitrate by probing with padding packets. You should allocate your media bitrate
    ///   based on the estimated the BWE system produces via
    ///   [`Event::EgressBitrateEstimate`][crate::Event::EgressBitrateEstimate]. This rate might not
    ///   be reached if the network link cannot sustain the desired bitrate.
    ///
    /// ## Example
    ///
    /// Say you have three simulcast video tracks each with a high layer configured at 1.5Mbit/s.
    /// You should then set the desired bitrate to 4.5Mbit/s (or slightly higher). If the network
    /// link can sustain 4.5Mbit/s there will eventually be an
    /// [`Event::EgressBitrateEstimate`][crate::Event::EgressBitrateEstimate] with this estimate.
    pub fn set_desired_bitrate(&mut self, desired_bitrate: Bitrate) {
        self.0.session.set_bwe_desired_bitrate(desired_bitrate);
    }

    /// Limit the bitrate of regular background padding, without limiting probe clusters.
    ///
    /// `Some(Bitrate::ZERO)` disables regular padding; `None` removes the limit.
    /// The limit persists across estimate updates and BWE resets. Applications with
    /// a separate probe reserve can disable background padding to let it refill.
    pub fn set_padding_bitrate_limit(&mut self, limit: Option<Bitrate>) {
        self.0.session.set_bwe_padding_bitrate_limit(limit);
    }

    /// Bound probe payload bytes to an application's separate socket reserve.
    ///
    /// Allow for RTP/SRTP headers and one packet of rounding when choosing the
    /// limit. Minimum packet counts still apply. Byte-limited unpaced probes
    /// combine media and padding in their first burst and learn only regular
    /// cadences up to 20 ms. Persists across BWE resets.
    pub fn set_probe_byte_limit(&mut self, bytes: usize) {
        self.0.session.set_bwe_probe_byte_limit(bytes);
    }

    /// Set the minimum interval between dispatched probe clusters.
    ///
    /// Defaults to zero. Applications with a separate extras budget can allow its
    /// reserve to refill between clusters. This setting survives estimator resets.
    pub fn set_min_probe_interval(&mut self, interval: Duration) {
        self.0.session.set_bwe_min_probe_interval(interval);
    }

    /// Pace all RTP, including unpaced media, RTX and padding, before assigning TWCC send times.
    ///
    /// `media` bounds the combined ordinary egress; `probe` bounds short capacity probes.
    /// Queue pressure cannot raise either rate. Control packets bypass this pacer.
    /// The setting survives BWE resets. Use a dedicated video transport: audio on this
    /// transport would share the limit too.
    pub fn set_egress_pacing(&mut self, media: Bitrate, probe: Bitrate) {
        self.0.session.set_bwe_egress_pacing(media, probe);
    }

    /// Reset the BWE with a new init_bitrate
    ///
    /// This discards estimator/smoothing state, queued probes, pacer debt and
    /// in-flight TWCC send records, then starts estimation from `init_bitrate`.
    /// Transport sequence numbers and SRTP state are preserved. Normal transitions from audio-only to video sending
    /// do not require a reset: with TWCC negotiated, probing can discover
    /// capacity before video starts. The desired bitrate controls the capacity
    /// the estimator attempts to discover.
    pub fn reset(&mut self, init_bitrate: Bitrate) {
        self.0.session.reset_bwe(init_bitrate);
    }
}
