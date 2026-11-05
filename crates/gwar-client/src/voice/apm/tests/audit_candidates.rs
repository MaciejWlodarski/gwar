//! Candidates from the post-merge voice audit, against the shipped path. Measurement only:
//! production selects neither.
//!
//! - F1: `[mid, mid]` for every stereo canceller instead of the left reference (the known defect
//!   in vendor/sonora/GWAR-PATCH.md, until the two-reference follow-up is decided).
//! - F5: a warmed canceller takes over only if it found an echo recently, instead of ever.
use super::*;

/// All 549 windows of the approved P7 envelope with the candidate in place of shipped P7. Each
/// `native-echo-pair` line's `candidate` is the candidate; compare it with the approved P7 level
/// of the same window in `docs/measurements/aec-paired-echo-cases.csv` (profile 7 rows).
#[test]
#[ignore = "F1 candidate on all 549 echo windows; release, alone, single-threaded"]
fn mid_reference_echo_tables() {
    let _scope = measurement_scope();
    AEC_NATIVE_PROFILE.store(0, Ordering::Relaxed);
    AEC_ECHO_COMPARE_PROFILE.store(SHIPPED_AEC_PROFILE, Ordering::Relaxed);
    AEC_MID_REFERENCE.store(true, Ordering::Relaxed);
    the_warmed_switch_against_cold_and_staying_mono();
    the_wide_leak_cold_and_warm();
}

/// The right-only and independent wide paths of `native_reference_options_right_echo`, shipped
/// against the candidate. `right-only`: after the pin only the right speaker plays and only it
/// reaches the microphone.
#[test]
#[ignore = "F1 candidate on right-only and independent wide paths; release, alone, single-threaded"]
fn mid_reference_right_paths() {
    let _scope = measurement_scope();
    let windows = [(0, 1), (1, 2), (2, 4), (4, 6), (6, 8), (8, 12), (12, 16), (16, 24), (32, 48)];
    for before in [6, 20, 60, 310] {
        let pin = before * RATE;
        let total = pin + 12 * RATE;
        let cue = long_voice(total, 150.0, -18.0);
        let left = long_voice(total, 120.0, -18.0);
        let right = talker(total, 190.0, 3.1, 3.3, -18.0);
        for shape in ["right-only", "wide-right", "wide-both"] {
            let far: Vec<_> = (0..total)
                .map(|at| {
                    if at < pin {
                        [cue[at]; 2]
                    } else if shape == "right-only" {
                        [0.0, right[at]]
                    } else {
                        [left[at], right[at]]
                    }
                })
                .collect();
            let l = room(&left_channel(&far), 60);
            let r = room(&far.iter().map(|s| s[1]).collect::<Vec<_>>(), 75);
            let mic: Vec<_> =
                l.iter().zip(r).map(|(l, r)| if shape == "wide-both" { l + 0.7 * r } else { r }).collect();
            for (arm, mid) in [("shipped", false), ("mid", true)] {
                AEC_MID_REFERENCE.store(mid, Ordering::Relaxed);
                let out = warm_through(&mut warm_session(), &far, &mic, Some(pin));
                for (a, b) in windows {
                    let window = &out[pin + a * RATE / 4..pin + b * RATE / 4];
                    let energy = window.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / RATE as f64;
                    eprintln!("mid-right\t{arm}\t{shape},pin={before},q={a}-{b}\t{:.9}\t{energy:.15e}", db(window));
                }
            }
            AEC_MID_REFERENCE.store(false, Ordering::Relaxed);
        }
    }
}

/// F5's candidate window: three seconds of sound.
pub(super) const RECENT_ECHO_FRAMES: usize = 300;

/// All 549 windows with the F5 candidate in place of shipped P7, as [`mid_reference_echo_tables`].
#[test]
#[ignore = "F5 candidate on all 549 echo windows; release, alone, single-threaded"]
fn recent_echo_tables() {
    let _scope = measurement_scope();
    AEC_NATIVE_PROFILE.store(0, Ordering::Relaxed);
    AEC_ECHO_COMPARE_PROFILE.store(SHIPPED_AEC_PROFILE, Ordering::Relaxed);
    AEC_RECENT_ECHO.store(RECENT_ECHO_FRAMES, Ordering::Relaxed);
    the_warmed_switch_against_cold_and_staying_mono();
    the_wide_leak_cold_and_warm();
}

/// The route gaps F5 is about: headphones on the same device, so no route change is reported,
/// before the first share sound. Same rooms and windows as `what_an_unreported_device_costs`.
#[test]
#[ignore = "F5 candidate on the unreported headphone route gaps; release, alone, single-threaded"]
fn recent_echo_route_gaps() {
    let _scope = measurement_scope();
    let total = 36 * RATE;
    let windows = [(0usize, 3usize), (3, 5), (5, 8), (8, 16)];
    for (name, pin, moved) in
        [("warming, headphones 8 s before the share", 16, 8), ("warming, headphones 3 s before the share", 16, 13)]
    {
        let (pin, moved) = (pin * RATE, moved * RATE);
        let from = pin.max(moved);
        let mut sums = [[0.0f64; 4]; 3];
        for far_pitch in [100.0, 150.0, 230.0] {
            for delay in [20, 60, 150] {
                for (pitch, vibrato, syllables) in [(213.0, 3.1, 3.3), (176.0, 2.3, 4.1), (251.0, 3.7, 2.9)] {
                    let far = pairs(&long_voice(total, far_pitch, -20.0));
                    let left = left_channel(&far);
                    let said: Vec<f32> = talker(total, pitch, vibrato, syllables, -24.0)
                        .iter()
                        .enumerate()
                        .map(|(i, s)| {
                            let hiss = ((i as u32).wrapping_mul(2_654_435_761) as f32 / u32::MAX as f32 - 0.5) * 0.002;
                            hiss + if (i / RATE).is_multiple_of(2) { 0.0 } else { *s }
                        })
                        .collect();
                    let before: Vec<_> = room(&left, delay).iter().zip(&said).map(|(e, s)| e + s).collect();
                    for (index, (arm, mode, recent)) in
                        [("cold", 1, 0), ("P7-unreported", 2, 0), ("recent-unreported", 2, RECENT_ECHO_FRAMES)]
                            .into_iter()
                            .enumerate()
                    {
                        let _arm_scope = measurement_scope();
                        AEC_RECENT_ECHO.store(recent, Ordering::Relaxed);
                        let out = super::shipped::route_arm(
                            mode,
                            SHIPPED_AEC_PROFILE,
                            (pin, moved, false),
                            &far,
                            &before,
                            &said,
                        );
                        AEC_RECENT_ECHO.store(0, Ordering::Relaxed);
                        for (w, (a, b)) in windows.iter().enumerate() {
                            let span = from + a * RATE..from + b * RATE;
                            let (mut speaking, mut kept) = (0usize, 0usize);
                            for (o, s) in
                                out[span.clone()].as_chunks::<FRAME>().0.iter().zip(said[span].as_chunks::<FRAME>().0)
                            {
                                if db(&s[..]) > -40.0 {
                                    speaking += 1;
                                    kept += usize::from(db(&o[..]) > db(&s[..]) - 10.0);
                                }
                            }
                            let value = kept as f64 / speaking as f64;
                            sums[index][w] += value;
                            eprintln!("recent-route\t{name}\t{far_pitch}\t{delay}\t{pitch}\t{arm}\t{w}\t{value:.9}");
                        }
                    }
                }
            }
        }
        for (index, arm) in ["cold", "P7-unreported", "recent-unreported"].iter().enumerate() {
            let means: Vec<String> = sums[index].iter().map(|sum| format!("{:.3}", sum / 27.0)).collect();
            eprintln!("recent-route-mean\t{name}\t{arm}\t{}", means.join("\t"));
        }
    }
}

/// F1: the CI regressions of the stereo canceller, run with the candidate. Their fixtures play
/// wide sound as `[s, -s]`, whose mid is silence, and let the left speaker reach the microphone.
#[test]
#[ignore = "F1 candidate on the CI wide-sound regressions; reports which fail"]
fn mid_reference_wide_regressions() {
    let _scope = measurement_scope();
    AEC_MID_REFERENCE.store(true, Ordering::Relaxed);
    let tests: [(&str, fn()); 4] = [
        ("stereo_bursts_never_leak_once_pinned", stereo_bursts_never_leak_once_pinned),
        (
            "wide_sound_after_voice_is_cancelled_from_when_it_is_learned",
            wide_sound_after_voice_is_cancelled_from_when_it_is_learned,
        ),
        ("wide_sound_after_five_minutes_of_voice", wide_sound_after_five_minutes_of_voice),
        (
            "wide_sound_from_the_first_moment_after_voice_does_not_leak",
            wide_sound_from_the_first_moment_after_voice_does_not_leak,
        ),
    ];
    for (name, test) in tests {
        let outcome = std::panic::catch_unwind(test);
        eprintln!("mid-ci\t{name}\t{}", if outcome.is_ok() { "pass" } else { "FAIL" });
    }
}
