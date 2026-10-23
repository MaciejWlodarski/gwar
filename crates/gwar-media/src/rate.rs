//! Deterministic, per-encoder quality policy. No clocks, adapters or transports are owned here.

use std::time::Duration;

use crate::{Codec, CodecCaps, EncoderConfig, MediaError, RateControl, Result, Size};

const PRESSURE_HOLD: Duration = Duration::from_secs(2);
const RECOVERY_HOLD: Duration = Duration::from_secs(10);
const MAX_SAMPLE_GAP: Duration = Duration::from_secs(1);

/// Screen-content policy, using the plan's existing preset names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// Preserve physical resolution; reduce 15 to 10 to 5 fps.
    Text,
    /// Reduce 30 to 20 to 15 fps, then reduce resolution at 15 fps.
    Smooth,
    /// Preserve frame rate and reduce resolution, one tier at a time.
    Game,
}

/// Physical resolution cap. The 720p tier is also the degradation floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResolutionTier {
    /// Up to 1280 by 720 pixels.
    P720,
    /// Up to 1920 by 1080 pixels.
    P1080,
    /// Up to 2560 by 1440 pixels.
    P1440,
    /// Up to 3840 by 2160 pixels; Game is capped at 30 fps.
    P2160,
}

impl ResolutionTier {
    /// Landscape bounds; portrait sources use transposed bounds.
    pub fn size(self) -> Size {
        let (width, height) = match self {
            Self::P720 => (1280, 720),
            Self::P1080 => (1920, 1080),
            Self::P1440 => (2560, 1440),
            Self::P2160 => (3840, 2160),
        };
        Size { width, height }
    }
}

/// Fixed policy for one primary, compatibility or simulcast encoder instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControllerConfig {
    /// Content policy.
    pub preset: Preset,
    /// Requested maximum resolution tier, already admitted by the publisher/server.
    pub tier: ResolutionTier,
    /// Physical source dimensions. Outputs preserve aspect ratio, never upscale, and are even.
    pub source_size: Size,
    /// Administrative encoded-video budget allocated to this layer, excluding overhead.
    pub bitrate_cap_bps: u32,
}

/// Observations over the interval ending at the explicit sample time.
///
/// Call at least once a second. A longer gap breaks both continuity timers. Values must describe
/// this encoder's work, including capture/submission/crypto/packetization CPU, not other layers.
/// The publisher reserves voice, transport overhead and other encoders before assigning bandwidth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControllerFeedback {
    /// This layer's available encoded-video bits per second; zero pauses immediately.
    pub bandwidth_bps: u32,
    /// Observed encode time per frame over this interval (use a conservative percentile).
    pub encode_time: Duration,
    /// Attributable CPU in thousandths of one core (100 means 10 percent of a core).
    pub cpu_millicores: u32,
    /// Encoder reported late frames during the interval.
    pub encoder_late: bool,
    /// Encoder reported dropped frames during the interval.
    pub encoder_dropped: bool,
    /// Capture reported dropped frames during the interval.
    pub capture_dropped: bool,
    /// One-shot failure of the current encoder session; permanently excludes that candidate.
    pub encoder_failed: bool,
}

/// Complete requested settings, reusing B1's encoder and rate configuration types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QualityTarget {
    /// Index into the constructor's capability slice, identifying the exact encoder candidate.
    pub encoder_index: usize,
    /// Current resolution cap; actual size can be smaller for a small or non-16:9 source.
    pub tier: ResolutionTier,
    /// Codec/profile, physical size, frame rate and bitrate to apply.
    pub encoder: EncoderConfig,
    /// Verified hardware path, copied from B1 capabilities; never inferred from codec names.
    pub hardware: bool,
}

/// Requested action for one encoder; applying it and keyframe transitions belong to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateDecision {
    /// Encode at these settings. Size/profile changes require reconfiguration and a keyframe.
    Encode(QualityTarget),
    /// Stop submitting frames until sustained headroom permits resumption.
    Paused,
    /// No remaining usable, negotiated encoder. Fail only this layer/watch.
    Unavailable,
}

#[derive(Debug)]
struct EncoderPlan {
    steps: Vec<QualityTarget>,
}

impl EncoderPlan {
    fn finalize(&mut self) {
        // Tier pixel densities and inherited fps can invert nominal bitrate needs.
        // Clamp downwards so pressure/recovery never face a costlier lower rung.
        // Keep equal-bitrate rungs: smaller frames or lower cadence can still
        // relieve CPU/encode/drop pressure while preserving the preset's floor.
        let mut ceiling = u32::MAX;
        for target in &mut self.steps {
            let bitrate = &mut target.encoder.rate.bitrate_bps;
            *bitrate = (*bitrate).min(ceiling);
            ceiling = *bitrate;
        }
    }
}

/// Pure state machine for exactly one layer, with caller-provided monotonic time.
///
/// Two seconds of continuous pressure lower one quality step. Ten seconds of continuous
/// headroom recover one step (or resume). Headroom means 20 percent spare bandwidth, CPU at
/// most 80 percent of budget, encode time at most 60 percent of the prospective frame interval
/// and 80 percent of the hardware latency target. Intermediate observations reset the matching timers.
/// Bitrate cuts take effect immediately; restoration requires headroom for the current step
/// for the same ten-second hold, independently of quality promotion, avoiding BWE chatter.
/// At the quality floor, sustained pressure pauses instead of violating Text/Game invariants.
///
/// Capabilities must already be filtered for viewer/profile/platform eligibility and available
/// sessions (including the OpenH264 release policy). Preference is hardware AV1, H.264, VP9,
/// then VP8, with hardware before software within a codec. Software AV1 is always discarded.
/// Failed candidates are never retried or automatically promoted by this controller; membership
/// holds, aggregate allocation, SVC metadata and encoder probing belong to A3/B11/B11a.
#[derive(Debug)]
pub struct RateController {
    preset: Preset,
    plans: Vec<EncoderPlan>,
    active: usize,
    step: usize,
    bitrate_bps: u32,
    paused: bool,
    last_sample: Option<Duration>,
    pressure_since: Option<Duration>,
    headroom_since: Option<Duration>,
    bitrate_headroom_since: Option<Duration>,
}

impl RateController {
    /// Build a layer-local ladder from verified B1 capabilities.
    ///
    /// Rejects zero/tiny sources, unadmitted preset/tier bitrates, and an empty usable ladder.
    /// Software paths never exceed 1080p30. Text refuses a software or hardware fallback that
    /// cannot retain its requested physical resolution; other presets may start at a lower tier.
    /// Capability fps limits apply at construction/failover; Game then preserves that frame rate.
    pub fn new(config: ControllerConfig, encoders: &[CodecCaps]) -> Result<Self> {
        if config.source_size.width < 2 || config.source_size.height < 2 {
            return Err(MediaError::Invalid("rate controller source must be at least 2 by 2"));
        }
        let fps = preset_fps(config.preset, config.tier);
        let size = fit_size(config.source_size, config.tier);
        if bitrate(config.preset, config.tier, size, fps) > config.bitrate_cap_bps {
            return Err(MediaError::Invalid("preset tier exceeds the allocated bitrate cap"));
        }
        let mut candidates: Vec<_> = encoders.iter().enumerate().collect();
        candidates.sort_by_key(|(_, caps)| {
            let codec = match caps.profile.codec() {
                Codec::Av1 => 0,
                Codec::H264 => 1,
                Codec::Vp9 => 2,
                Codec::Vp8 => 3,
            };
            (codec, !caps.hardware)
        });
        let plans: Vec<_> = candidates.into_iter().filter_map(|(index, caps)| make_plan(config, caps, index)).collect();
        let Some(first) = plans.first() else {
            return Err(MediaError::Invalid("no usable negotiated encoder for the preset"));
        };
        let bitrate_bps = first.steps[0].encoder.rate.bitrate_bps;
        Ok(Self {
            preset: config.preset,
            plans,
            active: 0,
            step: 0,
            bitrate_bps,
            paused: false,
            last_sample: None,
            pressure_since: None,
            headroom_since: None,
            bitrate_headroom_since: None,
        })
    }

    /// Read the current decision without advancing time or touching adapters.
    pub fn decision(&self) -> RateDecision {
        let Some(plan) = self.plans.get(self.active) else {
            return RateDecision::Unavailable;
        };
        if self.paused {
            return RateDecision::Paused;
        }
        let mut target = plan.steps[self.step];
        target.encoder.rate.bitrate_bps = self.bitrate_bps;
        RateDecision::Encode(target)
    }

    /// Apply an interval's observations at a strictly increasing monotonic timestamp.
    ///
    /// Invalid/repeated/backward time leaves all state unchanged. A gap over one second resets
    /// timers, so missing observations never count as sustained pressure or recovery. No method
    /// reads a clock. Independent instances cannot change another layer's codec or quality.
    pub fn update(&mut self, now: Duration, feedback: ControllerFeedback) -> Result<RateDecision> {
        if let Some(last) = self.last_sample {
            if now <= last {
                return Err(MediaError::Invalid("rate controller sample time must increase"));
            }
            if now - last > MAX_SAMPLE_GAP {
                self.reset_timers();
            }
        }
        self.last_sample = Some(now);
        if self.active == self.plans.len() {
            return Ok(RateDecision::Unavailable);
        }
        if feedback.encoder_failed {
            self.fail_encoder();
            self.reset_timers();
            if self.active == self.plans.len() {
                return Ok(RateDecision::Unavailable);
            }
            // Timing/drop observations came from the failed encoder, not the replacement.
            self.bitrate_bps = self.bitrate_bps.min(feedback.bandwidth_bps.max(1));
            self.paused |= feedback.bandwidth_bps == 0;
            return Ok(self.decision());
        }
        if feedback.bandwidth_bps == 0 {
            self.paused = true;
            self.reset_timers();
            return Ok(self.decision());
        }

        let plan = &self.plans[self.active];
        let current = plan.steps[self.step];
        self.bitrate_bps = self.bitrate_bps.min(feedback.bandwidth_bps);
        let drops = feedback.encoder_late || feedback.encoder_dropped || feedback.capture_dropped;
        let pressure = drops
            || feedback.bandwidth_bps < current.encoder.rate.bitrate_bps
            || feedback.cpu_millicores > cpu_budget(current)
            || feedback.encode_time.as_nanos() > encode_budget(current, 80);
        let next = plan.steps[if self.paused { self.step } else { self.step.saturating_sub(1) }];
        let has_headroom = |target: QualityTarget| {
            !drops
                && u64::from(feedback.bandwidth_bps) * 5 >= u64::from(target.encoder.rate.bitrate_bps) * 6
                && feedback.cpu_millicores <= cpu_budget(target) * 4 / 5
                && feedback.encode_time.as_nanos() <= encode_budget(target, 60).min(hardware_latency(target) / 5 * 4)
        };
        let bitrate_headroom =
            !self.paused && self.bitrate_bps < current.encoder.rate.bitrate_bps && has_headroom(current);

        if pressure {
            self.headroom_since = None;
            self.bitrate_headroom_since = None;
            let since = *self.pressure_since.get_or_insert(now);
            if now - since >= PRESSURE_HOLD {
                if self.step + 1 < plan.steps.len() {
                    self.step += 1;
                    self.bitrate_bps = self.bitrate_bps.min(plan.steps[self.step].encoder.rate.bitrate_bps);
                } else {
                    self.paused = true;
                }
                self.reset_timers();
            }
        } else {
            self.pressure_since = None;
            if has_headroom(next) {
                let since = *self.headroom_since.get_or_insert(now);
                if now - since >= RECOVERY_HOLD {
                    if !self.paused {
                        self.step = self.step.saturating_sub(1);
                    }
                    self.paused = false;
                    self.bitrate_bps = self.plans[self.active].steps[self.step].encoder.rate.bitrate_bps;
                    self.reset_timers();
                    return Ok(self.decision());
                }
            } else {
                self.headroom_since = None;
            }
            if bitrate_headroom {
                let since = *self.bitrate_headroom_since.get_or_insert(now);
                if now - since >= RECOVERY_HOLD {
                    self.bitrate_bps = current.encoder.rate.bitrate_bps;
                    self.bitrate_headroom_since = None;
                }
            } else {
                self.bitrate_headroom_since = None;
            }
        }
        Ok(self.decision())
    }

    fn reset_timers(&mut self) {
        self.pressure_since = None;
        self.headroom_since = None;
        self.bitrate_headroom_since = None;
    }

    fn fail_encoder(&mut self) {
        let prior = self.plans[self.active].steps[self.step];
        self.active += 1;
        while let Some(plan) = self.plans.get_mut(self.active) {
            // Only Game preserves cadence permanently. Text/Smooth must retain their
            // higher-fps recovery steps, even when failover starts on a degraded rung.
            if self.preset == Preset::Game {
                for target in &mut plan.steps {
                    limit_fps(target, prior.encoder.rate.max_fps);
                }
                plan.steps.dedup_by_key(|target| (target.encoder.size, target.encoder.rate.max_fps));
            }
            let step = plan.steps.iter().position(|target| {
                target.encoder.size.width <= prior.encoder.size.width
                    && target.encoder.size.height <= prior.encoder.size.height
                    && target.encoder.rate.max_fps <= prior.encoder.rate.max_fps
            });
            let step = step.or_else(|| {
                // An unusually low capability fps can fall below every recovery rung.
                // Keep the highest supported resolution within the previous dimensions,
                // inserting its inherited cadence after the higher-fps recovery steps.
                let first = plan.steps.iter().position(|target| {
                    target.encoder.size.width <= prior.encoder.size.width
                        && target.encoder.size.height <= prior.encoder.size.height
                })?;
                let size = plan.steps[first].encoder.size;
                let step = plan.steps.iter().rposition(|target| target.encoder.size == size)? + 1;
                let mut start = plan.steps[step - 1];
                limit_fps(&mut start, prior.encoder.rate.max_fps);
                plan.steps.insert(step, start);
                // Further pressure may reduce resolution, but must not increase fps.
                for target in &mut plan.steps[step + 1..] {
                    limit_fps(target, prior.encoder.rate.max_fps);
                }
                Some(step)
            });
            if let Some(step) = step {
                // Independent capability limits can select a smaller frame with
                // a denser tier's nominal rate. Inherit the prior nominal ceiling
                // before finalizing lower rungs; keep higher-fps recovery intact.
                let bitrate = &mut plan.steps[step].encoder.rate.bitrate_bps;
                *bitrate = (*bitrate).min(prior.encoder.rate.bitrate_bps);
            }
            plan.finalize();
            if let Some(step) = step {
                self.step = step;
                self.bitrate_bps = self.bitrate_bps.min(plan.steps[step].encoder.rate.bitrate_bps);
                return;
            }
            self.active += 1;
        }
    }
}

fn limit_fps(target: &mut QualityTarget, max_fps: u32) {
    let rate = &mut target.encoder.rate;
    let fps = rate.max_fps.min(max_fps);
    rate.bitrate_bps = ((u64::from(rate.bitrate_bps) * u64::from(fps) / u64::from(rate.max_fps)) as u32).max(100_000);
    rate.max_fps = fps;
}

fn make_plan(config: ControllerConfig, caps: &CodecCaps, encoder_index: usize) -> Option<EncoderPlan> {
    if caps.max_fps == 0 || (caps.profile.codec() == Codec::Av1 && !caps.hardware) {
        return None;
    }
    let fps = preset_fps(config.preset, config.tier).min(caps.max_fps).min(if caps.hardware { 60 } else { 30 });
    let mut steps: Vec<QualityTarget> = Vec::new();
    for tier in [ResolutionTier::P2160, ResolutionTier::P1440, ResolutionTier::P1080, ResolutionTier::P720] {
        if tier > config.tier || (config.preset == Preset::Text && tier != config.tier) {
            continue;
        }
        let size = fit_size(config.source_size, tier);
        if size.width > caps.max_size.width
            || size.height > caps.max_size.height
            || (!caps.hardware && size.width.max(size.height) > 1920)
            || (!caps.hardware && size.width.min(size.height) > 1080)
        {
            continue;
        }
        // Small sources can fit several tiers without changing physical resolution.
        if steps.last().is_some_and(|last| last.encoder.size == size) {
            continue;
        }
        let rates: Vec<_> = match config.preset {
            Preset::Text => [fps, fps.min(10), fps.min(5)].into_iter().collect(),
            Preset::Smooth if steps.is_empty() => [fps, fps.min(20), fps.min(15)].into_iter().collect(),
            Preset::Smooth => vec![fps.min(15)],
            Preset::Game => vec![fps],
        };
        for max_fps in rates {
            if steps.last().is_some_and(|last| last.encoder.size == size && last.encoder.rate.max_fps == max_fps) {
                continue;
            }
            // Tier reference rates and even-pixel rounding can make a lower tier's
            // nominal need higher than the admitted request, especially for odd aspects.
            let bitrate_bps = bitrate(config.preset, tier, size, max_fps).min(config.bitrate_cap_bps);
            steps.push(QualityTarget {
                encoder_index,
                tier,
                encoder: EncoderConfig { profile: caps.profile, size, rate: RateControl { bitrate_bps, max_fps } },
                hardware: caps.hardware,
            });
        }
    }
    let mut plan = EncoderPlan { steps };
    plan.finalize();
    (!plan.steps.is_empty()).then_some(plan)
}

fn preset_fps(preset: Preset, tier: ResolutionTier) -> u32 {
    match preset {
        Preset::Text => 15,
        Preset::Smooth => 30,
        Preset::Game if tier == ResolutionTier::P2160 => 30,
        Preset::Game => 60,
    }
}

fn fit_size(source: Size, tier: ResolutionTier) -> Size {
    let mut bounds = tier.size();
    if source.height > source.width {
        bounds = Size { width: bounds.height, height: bounds.width };
    }
    let (mut width, mut height) = (u64::from(source.width), u64::from(source.height));
    if width > u64::from(bounds.width) {
        height = height * u64::from(bounds.width) / width;
        width = u64::from(bounds.width);
    }
    if height > u64::from(bounds.height) {
        width = width * u64::from(bounds.height) / height;
        height = u64::from(bounds.height);
    }
    Size { width: (width as u32 & !1).max(2), height: (height as u32 & !1).max(2) }
}

fn bitrate(preset: Preset, tier: ResolutionTier, size: Size, fps: u32) -> u32 {
    // Provisional H.264 reference rates, also used for VPx/AV1 until F3 measures them.
    let rates = match preset {
        Preset::Text => [1_250_000_u64, 2_500_000, 3_500_000, 6_000_000],
        Preset::Smooth => [2_000_000, 4_000_000, 7_000_000, 12_000_000],
        Preset::Game => [4_000_000, 8_000_000, 14_000_000, 20_000_000],
    };
    let reference = tier.size();
    let pixels = u64::from(size.width) * u64::from(size.height);
    let reference_pixels = u64::from(reference.width) * u64::from(reference.height);
    // Game 2160p30 remains 30 fps even after downscaling to lower tiers.
    let value =
        rates[tier as usize] * pixels * u64::from(fps) / (reference_pixels * u64::from(preset_fps(preset, tier)));
    (value as u32).max(100_000)
}

fn cpu_budget(target: QualityTarget) -> u32 {
    if !target.hardware {
        1500
    } else if target.encoder.size.width.max(target.encoder.size.height) > 1920
        || target.encoder.size.width.min(target.encoder.size.height) > 1080
    {
        150
    } else {
        100
    }
}

fn hardware_latency(target: QualityTarget) -> u128 {
    if !target.hardware {
        u128::MAX
    } else if cpu_budget(target) == 150 {
        12_000_000
    } else {
        8_000_000
    }
}

fn encode_budget(target: QualityTarget, percent: u128) -> u128 {
    (1_000_000_000_u128 * percent / (100 * u128::from(target.encoder.rate.max_fps))).min(hardware_latency(target))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CodecProfile;

    fn assert_ladders(controller: &RateController, config: ControllerConfig) {
        for plan in &controller.plans {
            for target in &plan.steps {
                assert!(
                    (100_000..=config.bitrate_cap_bps).contains(&target.encoder.rate.bitrate_bps),
                    "{config:?}: {plan:?}"
                );
                if config.preset == Preset::Text {
                    assert_eq!(target.encoder.size, fit_size(config.source_size, config.tier));
                }
            }
            for pair in plan.steps.windows(2) {
                let [upper, lower] = pair else { unreachable!() };
                assert!(lower.encoder.rate.bitrate_bps <= upper.encoder.rate.bitrate_bps, "{config:?}: {plan:?}");
                assert!(lower.encoder.size.width <= upper.encoder.size.width);
                assert!(lower.encoder.size.height <= upper.encoder.size.height);
                assert!(lower.encoder.rate.max_fps <= upper.encoder.rate.max_fps);
                if config.preset == Preset::Game {
                    assert_eq!(lower.encoder.rate.max_fps, upper.encoder.rate.max_fps);
                }
            }
        }
    }

    fn assert_failover_ladders(config: ControllerConfig, encoders: &[CodecCaps]) {
        let Ok(initial) = RateController::new(config, encoders) else { return };
        assert_ladders(&initial, config);
        // Fail at every initial quality rung, then walk every remaining candidate.
        // This includes inherited Game cadence and inserted Text/Smooth fps rungs.
        for step in 0..initial.plans[0].steps.len() {
            let mut controller = RateController::new(config, encoders).unwrap();
            controller.step = step;
            controller.bitrate_bps = controller.plans[0].steps[step].encoder.rate.bitrate_bps;
            while controller.active < controller.plans.len() {
                let prior = controller.plans[controller.active].steps[controller.step];
                controller.fail_encoder();
                assert_ladders(&controller, config);
                if let Some(plan) = controller.plans.get(controller.active) {
                    // Inspect the nominal rung, not decision()'s transient send rate:
                    // a cut send rate can hide an inverted pressure/recovery threshold.
                    let start = plan.steps[controller.step];
                    assert!(
                        start.encoder.rate.bitrate_bps <= prior.encoder.rate.bitrate_bps,
                        "{config:?}: {prior:?} -> {start:?}"
                    );
                    assert!(start.encoder.size.width <= prior.encoder.size.width);
                    assert!(start.encoder.size.height <= prior.encoder.size.height);
                    assert!(start.encoder.rate.max_fps <= prior.encoder.rate.max_fps);
                }
            }
        }
    }

    #[test]
    fn finalized_ladder_bitrates_are_monotonic_across_policy_and_capability_sweep() {
        let tiers = [ResolutionTier::P720, ResolutionTier::P1080, ResolutionTier::P1440, ResolutionTier::P2160];
        let sources = [
            Size { width: 2, height: 2 },
            Size { width: 640, height: 360 },
            ResolutionTier::P720.size(),
            ResolutionTier::P1080.size(),
            Size { width: 1930, height: 1080 },
            Size { width: 1080, height: 1930 },
            Size { width: 2559, height: 1439 },
            ResolutionTier::P1440.size(),
            ResolutionTier::P2160.size(),
            Size { width: 2160, height: 3840 },
            Size { width: 3840, height: 1080 },
            Size { width: 1440, height: 1440 },
            Size { width: 4000, height: 3000 },
        ];
        let h264 = CodecProfile::H264 { profile_idc: 100, profile_iop: 0, level_idc: 52 };
        let capabilities: Vec<_> = [false, true]
            .into_iter()
            .flat_map(|hardware| {
                tiers.into_iter().flat_map(move |tier| {
                    [0, 1, 3, 5, 10, 15, 20, 30, 60].into_iter().map(move |max_fps| CodecCaps {
                        profile: h264,
                        hardware,
                        max_size: tier.size(),
                        max_fps,
                    })
                })
            })
            .collect();
        for preset in [Preset::Text, Preset::Smooth, Preset::Game] {
            for tier in tiers {
                for source_size in sources {
                    let admitted = bitrate(preset, tier, fit_size(source_size, tier), preset_fps(preset, tier));
                    for bitrate_cap_bps in [100_000, admitted, admitted + 100_000, 50_000_000] {
                        let config = ControllerConfig { preset, tier, source_size, bitrate_cap_bps };
                        // AV1 and H.264 limits vary independently: sharing one
                        // capability ceiling misses cross-ladder bitrate inversions.
                        for primary in &capabilities {
                            for replacement in &capabilities {
                                let encoders = [
                                    CodecCaps {
                                        profile: CodecProfile::Av1 {
                                            profile: 0,
                                            level_idx: 13,
                                            high_tier: false,
                                            bit_depth: 8,
                                        },
                                        ..primary.clone()
                                    },
                                    replacement.clone(),
                                    CodecCaps {
                                        profile: CodecProfile::Vp9 { profile: 0 },
                                        hardware: true,
                                        max_size: ResolutionTier::P2160.size(),
                                        max_fps: 60,
                                    },
                                    CodecCaps {
                                        profile: CodecProfile::Vp8,
                                        hardware: false,
                                        max_size: ResolutionTier::P1080.size(),
                                        max_fps: 30,
                                    },
                                ];
                                assert_failover_ladders(config, &encoders);
                            }
                        }
                    }
                }
            }
        }
    }
}
