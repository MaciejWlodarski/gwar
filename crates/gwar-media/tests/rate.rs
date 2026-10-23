use std::time::Duration;

use gwar_media::*;

const H264: CodecProfile = CodecProfile::H264 { profile_idc: 100, profile_iop: 0, level_idc: 52 };
const AV1: CodecProfile = CodecProfile::Av1 { profile: 0, level_idx: 13, high_tier: false, bit_depth: 8 };

fn caps(profile: CodecProfile, hardware: bool) -> CodecCaps {
    CodecCaps { profile, hardware, max_size: ResolutionTier::P2160.size(), max_fps: 60 }
}

fn config(preset: Preset, tier: ResolutionTier) -> ControllerConfig {
    ControllerConfig { preset, tier, source_size: ResolutionTier::P2160.size(), bitrate_cap_bps: 50_000_000 }
}

fn controller(preset: Preset, tier: ResolutionTier) -> RateController {
    RateController::new(config(preset, tier), &[caps(H264, true)]).unwrap()
}

fn healthy() -> ControllerFeedback {
    ControllerFeedback {
        bandwidth_bps: 50_000_000,
        encode_time: Duration::from_millis(1),
        cpu_millicores: 20,
        encoder_late: false,
        encoder_dropped: false,
        capture_dropped: false,
        encoder_failed: false,
    }
}

fn encode(decision: RateDecision) -> QualityTarget {
    let RateDecision::Encode(target) = decision else { panic!("expected encoder, got {decision:?}") };
    target
}

fn sample(controller: &mut RateController, seconds: u64, feedback: ControllerFeedback) -> RateDecision {
    controller.update(Duration::from_secs(seconds), feedback).unwrap()
}

#[test]
fn preset_table_matches_the_plan_without_assuming_an_av1_discount() {
    for (preset, tier, fps, bitrate_bps) in [
        (Preset::Text, ResolutionTier::P1080, 15, 2_500_000),
        (Preset::Text, ResolutionTier::P1440, 15, 3_500_000),
        (Preset::Text, ResolutionTier::P2160, 15, 6_000_000),
        (Preset::Smooth, ResolutionTier::P1080, 30, 4_000_000),
        (Preset::Smooth, ResolutionTier::P1440, 30, 7_000_000),
        (Preset::Smooth, ResolutionTier::P2160, 30, 12_000_000),
        (Preset::Game, ResolutionTier::P1080, 60, 8_000_000),
        (Preset::Game, ResolutionTier::P1440, 60, 14_000_000),
        (Preset::Game, ResolutionTier::P2160, 30, 20_000_000),
    ] {
        for profile in [H264, AV1] {
            let c = RateController::new(config(preset, tier), &[caps(profile, true)]).unwrap();
            let target = encode(c.decision());
            assert_eq!(target.encoder.size, tier.size());
            assert_eq!(target.encoder.rate, RateControl { max_fps: fps, bitrate_bps });
            assert_eq!(target.encoder.profile, profile);
        }
    }
}

#[test]
fn sustained_pressure_preserves_text_resolution_and_game_frame_rate() {
    for (preset, tier, expected_tier, expected_fps) in [
        (Preset::Text, ResolutionTier::P2160, ResolutionTier::P2160, 10),
        (Preset::Game, ResolutionTier::P1440, ResolutionTier::P1080, 60),
        (Preset::Game, ResolutionTier::P2160, ResolutionTier::P1440, 30),
        (Preset::Smooth, ResolutionTier::P1080, ResolutionTier::P1080, 20),
    ] {
        for pressure in [
            ControllerFeedback { encoder_late: true, ..healthy() },
            ControllerFeedback { encoder_dropped: true, ..healthy() },
            ControllerFeedback { capture_dropped: true, ..healthy() },
            ControllerFeedback { encode_time: Duration::from_millis(60), ..healthy() },
            ControllerFeedback { cpu_millicores: 151, ..healthy() },
            ControllerFeedback { bandwidth_bps: 500_000, ..healthy() },
        ] {
            let mut c = controller(preset, tier);
            let initial = encode(c.decision());
            for seconds in 0..2 {
                let target = encode(sample(&mut c, seconds, pressure));
                assert_eq!(target.tier, tier);
                assert_eq!(target.encoder.rate.max_fps, initial.encoder.rate.max_fps);
            }
            let target = encode(sample(&mut c, 2, pressure));
            assert_eq!(target.tier, expected_tier, "{preset:?}, {pressure:?}");
            assert_eq!(target.encoder.rate.max_fps, expected_fps, "{preset:?}, {pressure:?}");
            assert!(target.encoder.rate.bitrate_bps <= initial.encoder.rate.bitrate_bps);
        }
    }
}

#[test]
fn alternating_every_second_does_not_oscillate_in_quality_or_bitrate() {
    for pressure in [
        ControllerFeedback { encoder_dropped: true, ..healthy() },
        ControllerFeedback { capture_dropped: true, ..healthy() },
        ControllerFeedback { encode_time: Duration::from_millis(70), ..healthy() },
        ControllerFeedback { cpu_millicores: 1501, ..healthy() },
        ControllerFeedback { bandwidth_bps: 500_000, ..healthy() },
    ] {
        for preset in [Preset::Text, Preset::Smooth, Preset::Game] {
            let mut c = controller(preset, ResolutionTier::P1440);
            let first = sample(&mut c, 0, pressure);
            for second in 1..60 {
                let feedback = if second % 2 == 0 { pressure } else { healthy() };
                assert_eq!(sample(&mut c, second, feedback), first);
            }
        }
    }
}

#[test]
fn recovery_requires_ten_continuous_seconds_and_only_advances_one_step() {
    for preset in [Preset::Text, Preset::Smooth, Preset::Game] {
        let mut c = controller(preset, ResolutionTier::P1440);
        let bad = ControllerFeedback { encoder_dropped: true, ..healthy() };
        for second in 0..=5 {
            sample(&mut c, second, bad);
        }
        let degraded = c.decision();
        // Nine seconds of headroom are insufficient; pressure restarts the whole hold.
        for second in 6..=15 {
            assert_eq!(sample(&mut c, second, healthy()), degraded);
        }
        sample(&mut c, 16, bad);
        for second in 17..=26 {
            assert_eq!(sample(&mut c, second, healthy()), degraded);
        }
        let recovered = encode(sample(&mut c, 27, healthy()));
        assert_ne!(RateDecision::Encode(recovered), degraded);
        let one_step = match preset {
            Preset::Text => (ResolutionTier::P1440, 10),
            Preset::Smooth => (ResolutionTier::P1440, 20),
            Preset::Game => (ResolutionTier::P1080, 60),
        };
        assert_eq!((recovered.tier, recovered.encoder.rate.max_fps), one_step);
        for second in 28..=37 {
            assert_eq!(sample(&mut c, second, healthy()), RateDecision::Encode(recovered));
        }
        assert_eq!(encode(sample(&mut c, 38, healthy())).tier, ResolutionTier::P1440);
    }
}

#[test]
fn software_vp9_compatibility_pressure_never_changes_the_primary() {
    let mut primary = RateController::new(config(Preset::Game, ResolutionTier::P1440), &[caps(AV1, true)]).unwrap();
    let mut compat = RateController::new(
        config(Preset::Game, ResolutionTier::P1440),
        &[caps(CodecProfile::Vp9 { profile: 0 }, false)],
    )
    .unwrap();
    let primary_start = primary.decision();
    let compat_start = encode(compat.decision());
    assert_eq!((compat_start.tier, compat_start.encoder.rate.max_fps), (ResolutionTier::P1080, 30));
    for second in 0..=2 {
        assert_eq!(sample(&mut primary, second, healthy()), primary_start);
        sample(&mut compat, second, ControllerFeedback { cpu_millicores: 1501, ..healthy() });
    }
    let target = encode(compat.decision());
    assert_eq!((target.tier, target.encoder.rate.max_fps), (ResolutionTier::P720, 30));
    assert_eq!(primary.decision(), primary_start);
}

#[test]
fn failure_walks_the_codec_ladder_without_software_av1_or_quality_promotion() {
    let encoders = [
        caps(CodecProfile::Vp8, false),
        caps(AV1, false),
        caps(H264, false),
        caps(AV1, true),
        caps(CodecProfile::Vp9 { profile: 0 }, false),
        caps(H264, true),
    ];
    let mut c = RateController::new(config(Preset::Game, ResolutionTier::P1080), &encoders).unwrap();
    assert_eq!(encode(c.decision()).encoder.profile.codec(), Codec::Av1);
    for (second, codec, hardware, fps) in [
        (0, Codec::H264, true, 60),
        (1, Codec::H264, false, 30),
        (2, Codec::Vp9, false, 30),
        (3, Codec::Vp8, false, 30),
    ] {
        let target = encode(sample(&mut c, second, ControllerFeedback { encoder_failed: true, ..healthy() }));
        assert_eq!(
            (target.encoder.profile.codec(), target.hardware, target.encoder.rate.max_fps),
            (codec, hardware, fps)
        );
    }
    assert_eq!(sample(&mut c, 4, ControllerFeedback { encoder_failed: true, ..healthy() }), RateDecision::Unavailable);
    assert_eq!(sample(&mut c, 5, healthy()), RateDecision::Unavailable);
    assert!(RateController::new(config(Preset::Game, ResolutionTier::P1080), &[caps(AV1, false)]).is_err());

    let mut c = RateController::new(config(Preset::Game, ResolutionTier::P1440), &encoders).unwrap();
    for second in 0..=2 {
        sample(&mut c, second, ControllerFeedback { capture_dropped: true, ..healthy() });
    }
    let before = encode(c.decision());
    let after = encode(sample(&mut c, 3, ControllerFeedback { encoder_failed: true, ..healthy() }));
    assert_eq!(after.encoder.size, before.encoder.size);
    assert_eq!(after.encoder.rate.max_fps, before.encoder.rate.max_fps);
}

#[test]
fn unavailable_text_fallback_refuses_only_that_layer() {
    let mut c = RateController::new(
        config(Preset::Text, ResolutionTier::P2160),
        &[caps(AV1, true), caps(CodecProfile::Vp9 { profile: 0 }, false)],
    )
    .unwrap();
    assert_eq!(sample(&mut c, 0, ControllerFeedback { encoder_failed: true, ..healthy() }), RateDecision::Unavailable);
}

#[test]
fn zero_bandwidth_pauses_and_recovery_needs_headroom_for_the_next_setting() {
    let mut c = controller(Preset::Text, ResolutionTier::P1080);
    assert_eq!(sample(&mut c, 0, ControllerFeedback { bandwidth_bps: 0, ..healthy() }), RateDecision::Paused);
    for second in 1..=10 {
        assert_eq!(sample(&mut c, second, healthy()), RateDecision::Paused);
    }
    let full = sample(&mut c, 11, healthy());
    assert!(matches!(full, RateDecision::Encode(_)));
    sample(&mut c, 12, ControllerFeedback { encoder_dropped: true, ..healthy() });
    sample(&mut c, 13, ControllerFeedback { encoder_dropped: true, ..healthy() });
    let lower = sample(&mut c, 14, ControllerFeedback { encoder_dropped: true, ..healthy() });
    // Enough for 10 fps, but less than 20 percent above the prospective 15 fps target.
    for second in 15..40 {
        assert_eq!(sample(&mut c, second, ControllerFeedback { bandwidth_bps: 2_999_999, ..healthy() }), lower);
    }
    for second in 40..=49 {
        assert_eq!(sample(&mut c, second, ControllerFeedback { bandwidth_bps: 3_000_000, ..healthy() }), lower);
    }
    assert_eq!(sample(&mut c, 50, ControllerFeedback { bandwidth_bps: 3_000_000, ..healthy() }), full);
}

#[test]
fn time_gaps_break_continuity_and_invalid_time_is_transactional() {
    let mut c = controller(Preset::Text, ResolutionTier::P1080);
    let bad = ControllerFeedback { capture_dropped: true, ..healthy() };
    let initial = sample(&mut c, 0, bad);
    assert_eq!(sample(&mut c, 2, bad), initial);
    assert_eq!(sample(&mut c, 3, bad), initial);
    assert_ne!(sample(&mut c, 4, bad), initial);
    let degraded = c.decision();
    for second in 5..=14 {
        sample(&mut c, second, healthy());
    }
    assert!(c.update(Duration::from_secs(14), ControllerFeedback { encoder_failed: true, ..healthy() }).is_err());
    assert!(c.update(Duration::from_secs(13), bad).is_err());
    assert_eq!(c.decision(), degraded);
    assert_eq!(sample(&mut c, 16, healthy()), degraded);
    for second in 17..=25 {
        assert_eq!(sample(&mut c, second, healthy()), degraded);
    }
    assert_eq!(sample(&mut c, 26, healthy()), initial);
}

#[test]
fn exhausted_ladder_pauses_instead_of_breaking_preset_invariants() {
    for preset in [Preset::Text, Preset::Smooth, Preset::Game] {
        let mut c = controller(preset, ResolutionTier::P720);
        let bad = ControllerFeedback { encoder_dropped: true, ..healthy() };
        for second in 0..=12 {
            sample(&mut c, second, bad);
        }
        assert_eq!(c.decision(), RateDecision::Paused);
        for second in 13..=22 {
            assert_eq!(sample(&mut c, second, healthy()), RateDecision::Paused);
        }
        let resumed = encode(sample(&mut c, 23, healthy()));
        assert_eq!(resumed.tier, ResolutionTier::P720);
        if preset == Preset::Text {
            assert_eq!(resumed.encoder.rate.max_fps, 5);
        }
        if preset == Preset::Game {
            assert_eq!(resumed.encoder.rate.max_fps, 60);
        }
    }
}

#[test]
fn limits_aspect_ratio_and_software_caps_are_enforced() {
    let mut cfg = config(Preset::Game, ResolutionTier::P1440);
    cfg.bitrate_cap_bps = 8_000_000;
    assert!(RateController::new(cfg, &[caps(H264, true)]).is_err());
    cfg = config(Preset::Text, ResolutionTier::P2160);
    assert!(RateController::new(cfg, &[caps(H264, false)]).is_err());
    cfg.source_size = Size { width: 1081, height: 1921 };
    let target = encode(RateController::new(cfg, &[caps(H264, false)]).unwrap().decision());
    assert_eq!(target.encoder.size, Size { width: 1080, height: 1920 });
    assert!(target.encoder.rate.max_fps <= 30);
    cfg.source_size = Size { width: 3440, height: 1440 };
    let target = encode(RateController::new(cfg, &[caps(H264, true)]).unwrap().decision());
    assert_eq!(target.encoder.size, cfg.source_size);
    cfg.source_size = Size { width: 1280, height: 720 };
    let target = encode(RateController::new(cfg, &[caps(H264, true)]).unwrap().decision());
    assert_eq!(target.encoder.size, cfg.source_size);
    cfg.source_size.width = 0;
    assert!(RateController::new(cfg, &[caps(H264, true)]).is_err());
    cfg.source_size = ResolutionTier::P1080.size();
    assert!(RateController::new(cfg, &[CodecCaps { max_fps: 0, ..caps(H264, true) }]).is_err());
    assert!(RateController::new(cfg, &[]).is_err());
}

#[test]
fn eighty_percent_interval_and_cpu_boundaries_are_exact() {
    // Software has no hardware latency target: 30 fps, 80 percent interval = 26,666,666 ns.
    for (nanos, cpu, degrades) in [(26_666_666, 1500, false), (26_666_667, 1500, true), (1_000_000, 1501, true)] {
        let mut c = RateController::new(
            config(Preset::Game, ResolutionTier::P1080),
            &[caps(CodecProfile::Vp9 { profile: 0 }, false)],
        )
        .unwrap();
        let before = c.decision();
        for second in 0..=2 {
            sample(
                &mut c,
                second,
                ControllerFeedback { encode_time: Duration::from_nanos(nanos), cpu_millicores: cpu, ..healthy() },
            );
        }
        assert_eq!(c.decision() != before, degrades);
    }
    // High-resolution hardware is allowed 15 percent of one core; 1080p gets 10 percent.
    for (tier, cpu, degrades) in [
        (ResolutionTier::P1080, 100, false),
        (ResolutionTier::P1080, 101, true),
        (ResolutionTier::P1440, 150, false),
        (ResolutionTier::P1440, 151, true),
    ] {
        let mut c = controller(Preset::Game, tier);
        let before = c.decision();
        for second in 0..=2 {
            sample(&mut c, second, ControllerFeedback { cpu_millicores: cpu, ..healthy() });
        }
        assert_eq!(c.decision() != before, degrades);
    }
}

#[test]
fn same_profile_failover_identifies_the_exact_candidate_and_resets_timers() {
    let encoders = [caps(H264, true), caps(H264, true), caps(AV1, false)];
    let mut c = RateController::new(config(Preset::Game, ResolutionTier::P1080), &encoders).unwrap();
    assert_eq!(encode(c.decision()).encoder_index, 0);
    let bad = ControllerFeedback { encoder_dropped: true, ..healthy() };
    sample(&mut c, 0, bad);
    sample(&mut c, 1, bad);
    let target = encode(sample(&mut c, 2, ControllerFeedback { encoder_failed: true, ..bad }));
    assert_eq!(target.encoder_index, 1);
    assert_eq!(target.tier, ResolutionTier::P1080);
    assert_eq!(encode(sample(&mut c, 3, bad)).tier, ResolutionTier::P1080);
    assert_eq!(encode(sample(&mut c, 4, bad)).tier, ResolutionTier::P1080);
    assert_eq!(encode(sample(&mut c, 5, bad)).tier, ResolutionTier::P720);
}

#[test]
fn recovery_cannot_count_alternating_feedback_after_a_real_downscale() {
    for preset in [Preset::Text, Preset::Smooth, Preset::Game] {
        let mut c = controller(preset, ResolutionTier::P1440);
        let bad = ControllerFeedback { capture_dropped: true, ..healthy() };
        for second in 0..=2 {
            sample(&mut c, second, bad);
        }
        let degraded = c.decision();
        for second in 3..=60 {
            assert_eq!(sample(&mut c, second, if second % 2 == 0 { bad } else { healthy() }), degraded);
        }
    }
}

#[test]
fn faster_replacement_hardware_retains_the_failed_encoders_cadence() {
    let encoders = [CodecCaps { max_fps: 30, ..caps(AV1, true) }, caps(H264, true)];
    let mut c = RateController::new(config(Preset::Game, ResolutionTier::P1080), &encoders).unwrap();
    let before = encode(c.decision());
    assert_eq!(before.encoder.rate.max_fps, 30);
    let after = encode(sample(&mut c, 0, ControllerFeedback { encoder_failed: true, ..healthy() }));
    assert_eq!(after.encoder_index, 1);
    assert_eq!(after.encoder.profile.codec(), Codec::H264);
    assert!(after.hardware);
    assert_eq!(after.encoder.rate, before.encoder.rate);
    for second in 1..=20 {
        assert_eq!(sample(&mut c, second, healthy()), RateDecision::Encode(after));
    }
}

fn assert_degraded_failover_recovers(preset: Preset, degraded_fps: u32, recovered_fps: u32) {
    let mut c =
        RateController::new(config(preset, ResolutionTier::P1080), &[caps(AV1, true), caps(H264, true)]).unwrap();
    for second in 0..=2 {
        sample(&mut c, second, ControllerFeedback { encoder_dropped: true, ..healthy() });
    }
    let degraded = encode(c.decision());
    assert_eq!(degraded.encoder.rate.max_fps, degraded_fps);
    let replacement = encode(sample(&mut c, 3, ControllerFeedback { encoder_failed: true, ..healthy() }));
    assert_eq!(replacement.encoder_index, 1);
    assert_eq!(replacement.encoder.size, degraded.encoder.size);
    assert_eq!(replacement.encoder.rate, degraded.encoder.rate);
    for second in 4..=13 {
        assert_eq!(sample(&mut c, second, healthy()), RateDecision::Encode(replacement));
    }
    let recovered = encode(sample(&mut c, 14, healthy()));
    assert_eq!(recovered.encoder_index, 1);
    assert_eq!(recovered.encoder.size, degraded.encoder.size);
    assert_eq!(recovered.encoder.rate.max_fps, recovered_fps);
}

#[test]
fn degraded_text_recovers_full_frame_rate_after_failover() {
    assert_degraded_failover_recovers(Preset::Text, 10, 15);
}

#[test]
fn degraded_smooth_recovers_full_frame_rate_after_failover() {
    assert_degraded_failover_recovers(Preset::Smooth, 20, 30);
}

#[test]
fn low_fps_smooth_failover_keeps_the_highest_supported_resolution() {
    for (tier, replacement_tier) in [
        (ResolutionTier::P1440, ResolutionTier::P1440),
        (ResolutionTier::P2160, ResolutionTier::P2160),
        (ResolutionTier::P2160, ResolutionTier::P1440),
    ] {
        let encoders = [
            CodecCaps { max_fps: 10, ..caps(AV1, true) },
            CodecCaps { max_size: replacement_tier.size(), max_fps: 30, ..caps(H264, true) },
        ];
        let mut c = RateController::new(config(Preset::Smooth, tier), &encoders).unwrap();
        let before = encode(c.decision());
        assert_eq!(before.encoder.size, tier.size());
        assert_eq!(before.encoder.rate.max_fps, 10);
        let replacement = encode(sample(&mut c, 0, ControllerFeedback { encoder_failed: true, ..healthy() }));
        assert_eq!(replacement.encoder_index, 1);
        assert_eq!(replacement.encoder.profile, H264);
        assert_eq!(replacement.tier, replacement_tier);
        assert_eq!(replacement.encoder.size, replacement_tier.size());
        assert_eq!(replacement.encoder.rate.max_fps, 10);
    }
}

#[test]
fn low_fps_smooth_failover_keeps_1440p_at_three_megabits() {
    let encoders = [
        CodecCaps { max_fps: 10, ..caps(AV1, true) },
        CodecCaps { max_size: ResolutionTier::P1440.size(), max_fps: 30, ..caps(H264, true) },
    ];
    let mut c = RateController::new(config(Preset::Smooth, ResolutionTier::P1440), &encoders).unwrap();
    let feedback = ControllerFeedback { bandwidth_bps: 3_000_000, ..healthy() };
    let replacement = encode(sample(&mut c, 0, ControllerFeedback { encoder_failed: true, ..feedback }));
    assert_eq!(replacement.encoder.profile, H264);
    assert_eq!(replacement.encoder.size, ResolutionTier::P1440.size());
    assert_eq!(replacement.encoder.rate, RateControl { max_fps: 10, bitrate_bps: 2_333_333 });
    for second in 1..=90 {
        assert_eq!(sample(&mut c, second, feedback), RateDecision::Encode(replacement));
    }
}

#[test]
fn low_fps_smooth_failover_retains_degradation_and_full_cadence_recovery() {
    let encoders = [
        CodecCaps { max_fps: 10, ..caps(AV1, true) },
        CodecCaps { max_size: ResolutionTier::P1440.size(), max_fps: 30, ..caps(H264, true) },
    ];
    let mut c = RateController::new(config(Preset::Smooth, ResolutionTier::P1440), &encoders).unwrap();
    sample(&mut c, 0, ControllerFeedback { encoder_failed: true, ..healthy() });
    for (start, tier) in [(1, ResolutionTier::P1080), (4, ResolutionTier::P720)] {
        let before = c.decision();
        let pressure = ControllerFeedback { encoder_dropped: true, ..healthy() };
        for second in start..start + 2 {
            assert_eq!(sample(&mut c, second, pressure), before);
        }
        let degraded = encode(sample(&mut c, start + 2, pressure));
        assert_eq!(degraded.tier, tier);
        assert_eq!(degraded.encoder.size, tier.size());
        assert_eq!(degraded.encoder.rate.max_fps, 10);
    }
    for (start, tier, fps) in [
        (7, ResolutionTier::P1080, 10),
        (18, ResolutionTier::P1440, 10),
        (29, ResolutionTier::P1440, 15),
        (40, ResolutionTier::P1440, 20),
        (51, ResolutionTier::P1440, 30),
    ] {
        let before = c.decision();
        for second in start..start + 10 {
            assert_eq!(sample(&mut c, second, healthy()), before);
        }
        let recovered = encode(sample(&mut c, start + 10, healthy()));
        assert_eq!(recovered.encoder_index, 1);
        assert_eq!(recovered.tier, tier);
        assert_eq!(recovered.encoder.size, tier.size());
        assert_eq!(recovered.encoder.rate.max_fps, fps);
    }
}

#[test]
fn transient_bitrate_cut_recovers_at_the_current_rung_before_quality_promotion() {
    let mut c = controller(Preset::Text, ResolutionTier::P1080);
    for second in 0..=2 {
        sample(&mut c, second, ControllerFeedback { encoder_dropped: true, ..healthy() });
    }
    let nominal = encode(c.decision());
    assert_eq!(nominal.encoder.rate, RateControl { max_fps: 10, bitrate_bps: 1_666_666 });
    let cut = encode(sample(&mut c, 3, ControllerFeedback { bandwidth_bps: 100_000, ..healthy() }));
    assert_eq!(cut.encoder.rate, RateControl { max_fps: 10, bitrate_bps: 100_000 });
    let current_headroom = ControllerFeedback { bandwidth_bps: 2_000_000, ..healthy() };
    for second in 4..=13 {
        assert_eq!(sample(&mut c, second, current_headroom), RateDecision::Encode(cut));
    }
    for second in 14..=25 {
        assert_eq!(sample(&mut c, second, current_headroom), RateDecision::Encode(nominal));
    }
    let promotion_headroom = ControllerFeedback { bandwidth_bps: 3_000_000, ..healthy() };
    for second in 26..=35 {
        assert_eq!(sample(&mut c, second, promotion_headroom), RateDecision::Encode(nominal));
    }
    let promoted = encode(sample(&mut c, 36, promotion_headroom));
    assert_eq!(promoted.encoder.rate, RateControl { max_fps: 15, bitrate_bps: 2_500_000 });
}

#[test]
fn bitrate_restoration_does_not_restart_quality_promotion_hold() {
    let mut c = controller(Preset::Text, ResolutionTier::P1080);
    for second in 0..=2 {
        sample(&mut c, second, ControllerFeedback { encoder_dropped: true, ..healthy() });
    }
    sample(&mut c, 3, ControllerFeedback { bandwidth_bps: 100_000, ..healthy() });
    for second in 4..=8 {
        sample(&mut c, second, ControllerFeedback { bandwidth_bps: 2_000_000, ..healthy() });
    }
    for second in 9..=13 {
        let target = encode(sample(&mut c, second, healthy()));
        assert_eq!(target.encoder.rate, RateControl { max_fps: 10, bitrate_bps: 100_000 });
    }
    for second in 14..=18 {
        let target = encode(sample(&mut c, second, healthy()));
        assert_eq!(target.encoder.rate, RateControl { max_fps: 10, bitrate_bps: 1_666_666 });
    }
    assert_eq!(encode(sample(&mut c, 19, healthy())).encoder.rate.max_fps, 15);
}

#[test]
fn nonstandard_source_targets_stay_within_cap_at_construction_failover_and_recovery() {
    let cfg = ControllerConfig {
        source_size: Size { width: 1930, height: 1080 },
        bitrate_cap_bps: 7_920_000,
        ..config(Preset::Game, ResolutionTier::P1440)
    };
    let encoders = [
        CodecCaps { max_size: ResolutionTier::P1080.size(), ..caps(AV1, true) },
        CodecCaps { max_size: ResolutionTier::P1080.size(), ..caps(H264, true) },
    ];
    let mut c = RateController::new(cfg, &encoders).unwrap();
    let initial = encode(c.decision());
    assert_eq!(initial.encoder.size, Size { width: 1920, height: 1074 });
    assert_eq!(initial.encoder.rate.bitrate_bps, cfg.bitrate_cap_bps);
    for second in 0..=2 {
        let target = encode(sample(&mut c, second, ControllerFeedback { encoder_dropped: true, ..healthy() }));
        assert!(target.encoder.rate.bitrate_bps <= cfg.bitrate_cap_bps);
    }
    assert_eq!(encode(c.decision()).tier, ResolutionTier::P720);
    for second in 3..=13 {
        let target = encode(sample(&mut c, second, healthy()));
        assert!(target.encoder.rate.bitrate_bps <= cfg.bitrate_cap_bps);
    }
    assert_eq!(c.decision(), RateDecision::Encode(initial));
    let replacement = encode(sample(&mut c, 14, ControllerFeedback { encoder_failed: true, ..healthy() }));
    assert_eq!(replacement.encoder_index, 1);
    assert_eq!(replacement.encoder.rate.bitrate_bps, cfg.bitrate_cap_bps);
    sample(&mut c, 15, ControllerFeedback { bandwidth_bps: 100_000, ..healthy() });
    for second in 16..=26 {
        let target = encode(sample(&mut c, second, healthy()));
        assert!(target.encoder.rate.bitrate_bps <= cfg.bitrate_cap_bps);
    }
    assert_eq!(c.decision(), RateDecision::Encode(replacement));
}

#[test]
fn failover_below_the_replacement_fps_floor_inserts_a_recoverable_rung() {
    for (preset, recovery_fps) in [(Preset::Text, [5, 10, 15]), (Preset::Smooth, [15, 20, 30])] {
        let encoders = [CodecCaps { max_fps: 3, ..caps(AV1, true) }, caps(H264, true)];
        let mut c = RateController::new(config(preset, ResolutionTier::P720), &encoders).unwrap();
        let replacement = encode(sample(&mut c, 0, ControllerFeedback { encoder_failed: true, ..healthy() }));
        assert_eq!(replacement.encoder_index, 1);
        assert_eq!(replacement.encoder.rate.max_fps, 3);
        for (index, fps) in recovery_fps.into_iter().enumerate() {
            let start = index as u64 * 11 + 1;
            let before = c.decision();
            for second in start..start + 10 {
                assert_eq!(sample(&mut c, second, healthy()), before);
            }
            let recovered = encode(sample(&mut c, start + 10, healthy()));
            assert_eq!(recovered.encoder.rate.max_fps, fps);
        }
    }
}

#[test]
fn nonstandard_smooth_failover_recovers_after_drops_at_steady_bandwidth() {
    let cfg = ControllerConfig {
        source_size: Size { width: 1930, height: 1080 },
        ..config(Preset::Smooth, ResolutionTier::P2160)
    };
    let encoders = [CodecCaps { max_fps: 10, ..caps(AV1, true) }, CodecCaps { max_fps: 30, ..caps(H264, true) }];
    let mut c = RateController::new(cfg, &encoders).unwrap();
    let feedback = ControllerFeedback { bandwidth_bps: 1_250_000, ..healthy() };
    let replacement = encode(sample(&mut c, 0, ControllerFeedback { encoder_failed: true, ..feedback }));
    assert_eq!(replacement.encoder_index, 1);
    assert_eq!(replacement.encoder.size, cfg.source_size);
    assert_eq!(replacement.encoder.rate, RateControl { max_fps: 10, bitrate_bps: 1_005_208 });
    for second in 1..=3 {
        sample(&mut c, second, ControllerFeedback { encoder_dropped: true, ..feedback });
    }
    let degraded = encode(c.decision());
    assert_eq!(degraded.encoder.size, Size { width: 1920, height: 1074 });
    assert_eq!(degraded.encoder.rate, replacement.encoder.rate);
    // Smaller frames remain useful for drop/CPU pressure even at the same bitrate.
    // Healthy feedback must neither force another downscale nor block recovery.
    for second in 4..=13 {
        assert_eq!(sample(&mut c, second, feedback), RateDecision::Encode(degraded));
    }
    for second in 14..=90 {
        assert_eq!(sample(&mut c, second, feedback), RateDecision::Encode(replacement));
    }
}

#[test]
fn resolution_limited_smooth_failover_stays_at_steady_bandwidth_and_recovers() {
    let cfg = ControllerConfig {
        source_size: Size { width: 1930, height: 1080 },
        ..config(Preset::Smooth, ResolutionTier::P2160)
    };
    let encoders = [
        CodecCaps { max_fps: 10, ..caps(AV1, true) },
        CodecCaps { max_size: ResolutionTier::P1080.size(), max_fps: 30, ..caps(H264, true) },
    ];
    let mut c = RateController::new(cfg, &encoders).unwrap();
    let prior = encode(c.decision());
    assert_eq!(prior.encoder.size, cfg.source_size);
    assert_eq!(prior.encoder.rate, RateControl { max_fps: 10, bitrate_bps: 1_005_208 });
    let feedback = ControllerFeedback { bandwidth_bps: 1_250_000, ..healthy() };
    let replacement = encode(sample(&mut c, 0, ControllerFeedback { encoder_failed: true, ..feedback }));
    assert_eq!(replacement.encoder_index, 1);
    assert_eq!(replacement.encoder.profile, H264);
    assert_eq!(replacement.encoder.size, Size { width: 1920, height: 1074 });
    assert_eq!(replacement.encoder.rate, prior.encoder.rate);
    // The limited replacement's own nominal need used to be 1,325,925 bps,
    // causing an unnecessary 720p downscale at 3 s with no recovery by 90 s.
    for second in 1..=90 {
        assert_eq!(sample(&mut c, second, feedback), RateDecision::Encode(replacement));
    }
    for second in 91..=93 {
        sample(&mut c, second, ControllerFeedback { encoder_dropped: true, ..feedback });
    }
    let degraded = encode(c.decision());
    assert_eq!(degraded.encoder.size, Size { width: 1280, height: 716 });
    assert_eq!(degraded.encoder.rate.max_fps, 10);
    assert!(degraded.encoder.rate.bitrate_bps <= replacement.encoder.rate.bitrate_bps);
    for second in 94..=103 {
        assert_eq!(sample(&mut c, second, feedback), RateDecision::Encode(degraded));
    }
    assert_eq!(sample(&mut c, 104, feedback), RateDecision::Encode(replacement));
    // The inherited bitrate ceiling must leave the higher-fps recovery rungs intact.
    for (start, fps) in [(105, 15), (116, 20), (127, 30)] {
        let before = c.decision();
        for second in start..start + 10 {
            assert_eq!(sample(&mut c, second, healthy()), before);
        }
        let recovered = encode(sample(&mut c, start + 10, healthy()));
        assert_eq!(recovered.encoder_index, 1);
        assert_eq!(recovered.encoder.size, replacement.encoder.size);
        assert_eq!(recovered.encoder.rate.max_fps, fps);
    }
}
