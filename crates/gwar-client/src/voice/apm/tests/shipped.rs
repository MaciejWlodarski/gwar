//! Owner-approved P7 on the unchanged warm, left-reference production path.
use super::*;

fn accepted_p7_windows() -> &'static std::collections::BTreeMap<String, (f32, f32)> {
    static APPROVED: std::sync::OnceLock<std::collections::BTreeMap<String, (f32, f32)>> = std::sync::OnceLock::new();
    APPROVED.get_or_init(|| {
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/measurements/aec-paired-echo-cases.csv"))
            .lines()
            .filter_map(|line| {
                let line = line.strip_prefix("7,\"")?;
                let (case, levels) = line.split_once("\",").unwrap();
                let mut levels = levels.split(',');
                Some((
                    case.to_owned(),
                    (levels.next().unwrap().parse().unwrap(), levels.next().unwrap().parse().unwrap()),
                ))
            })
            .collect()
    })
}

pub(super) fn check_approved_echo(case: &str, current: f32, shipped: f32) {
    let &(main, approved) = accepted_p7_windows().get(case).expect("an owner-reviewed window");
    assert_eq!(current.to_bits(), main.to_bits(), "main fixture changed in {case}");
    assert!(shipped <= approved, "P7 exceeds its approved per-window echo: {case}, {shipped} > {approved}");
}

#[test]
fn production_stereo_keeps_one_render_reference() {
    let stage = Stage::stereo();
    assert_eq!(stage.apm.measurement().render_channels, 1, "the two-reference fix requires a separate owner decision");
}

#[test]
#[ignore = "all 549 windows against the owner-approved P7 envelope; release alone"]
fn shipped_p7_echo_tables() {
    AEC_NATIVE_PROFILE.store(0, Ordering::Relaxed);
    AEC_ECHO_COMPARE_PROFILE.store(SHIPPED_AEC_PROFILE, Ordering::Relaxed);
    AEC_APPROVED_ECHO.store(true, Ordering::Relaxed);
    AEC_ECHO_REGRESSIONS.store(0, Ordering::Relaxed);
    AEC_ECHO_WORST_BITS.store(0, Ordering::Relaxed);
    the_warmed_switch_against_cold_and_staying_mono();
    the_wide_leak_cold_and_warm();
    AEC_APPROVED_ECHO.store(false, Ordering::Relaxed);
    AEC_ECHO_COMPARE_PROFILE.store(0, Ordering::Relaxed);
    AEC_NATIVE_PROFILE.store(SHIPPED_AEC_PROFILE, Ordering::Relaxed);
    eprintln!(
        "shipped echo: over +1 dB {}, worst {:.9} dB; every window meets the approved envelope",
        AEC_ECHO_REGRESSIONS.load(Ordering::Relaxed),
        f32::from_bits(AEC_ECHO_WORST_BITS.load(Ordering::Relaxed))
    );
}

fn route_arm(
    mode: u8,
    profile: u8,
    (pin, moved, told): (usize, usize, bool),
    far: &[[f32; 2]],
    before: &[f32],
    after: &[f32],
) -> Vec<f32> {
    AEC_NATIVE_PROFILE.store(profile, Ordering::Relaxed);
    let mut p = if mode == 2 { warm_session() } else { session(LEARNING_FRAMES) };
    let mut out = Vec::with_capacity(far.len());
    for at in (0..far.len()).step_by(FRAME) {
        if mode == 2 && (at / FRAME).is_multiple_of(WARMING_RING / 4) {
            p.caught_up();
        }
        if mode > 0 && at == pin {
            p.caught_up();
            p.pin_stereo();
            p.collected();
        }
        if told && at == moved {
            p.route_changed();
        }
        let mic = if at >= moved { after } else { before };
        out.extend(tick(&mut p, &far[at..at + FRAME], &mic[at..at + FRAME]));
        if mode == 2 {
            p.ready_for_the_next_frame();
        }
    }
    AEC_NATIVE_PROFILE.store(SHIPPED_AEC_PROFILE, Ordering::Relaxed);
    out
}

#[test]
#[ignore = "main/P7 route gaps, 27 rooms; release alone"]
fn what_an_unreported_device_costs() {
    let total = 36 * RATE;
    let windows = [(0usize, 3usize), (3, 5), (5, 8), (8, 16)];
    let arms = [
        ("mono", 0, 0, false),
        ("cold", 1, 0, false),
        ("main-unreported", 2, 0, false),
        ("main-reported", 2, 0, true),
        ("P7-unreported", 2, SHIPPED_AEC_PROFILE, false),
        ("P7-reported", 2, SHIPPED_AEC_PROFILE, true),
    ];
    for (name, pin, moved, echo) in [
        ("in charge, then headphones", 8, 16, false),
        ("warming, headphones 8 s before the share", 16, 8, false),
        ("warming, headphones 3 s before the share", 16, 13, false),
        ("in charge, then another room", 8, 16, true),
    ] {
        let (pin, moved) = (pin * RATE, moved * RATE);
        let from = pin.max(moved);
        let mut sums = [[0.0f64; 9]; 6];
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
                    let after = if echo {
                        room(&left, delay + 60).iter().zip(&said).map(|(e, s)| 0.7 * e + s).collect()
                    } else {
                        said.clone()
                    };
                    for (index, (arm, mode, profile, told)) in arms.iter().enumerate() {
                        let out = route_arm(*mode, *profile, (pin, moved, *told), &far, &before, &after);
                        let count = if echo { 9 } else { 4 };
                        for w in 0..count {
                            if echo && w % 2 != 0 {
                                continue;
                            }
                            let value = if echo {
                                db(&out[from + w * RATE..from + (w + 1) * RATE])
                            } else {
                                let (a, b) = windows[w];
                                let span = from + a * RATE..from + b * RATE;
                                let (mut speaking, mut kept) = (0usize, 0usize);
                                for (o, s) in out[span.clone()]
                                    .as_chunks::<FRAME>()
                                    .0
                                    .iter()
                                    .zip(said[span].as_chunks::<FRAME>().0)
                                {
                                    if db(&s[..]) > -40.0 {
                                        speaking += 1;
                                        kept += usize::from(db(&o[..]) > db(&s[..]) - 10.0);
                                    }
                                }
                                kept as f32 / speaking as f32
                            };
                            sums[index][w] += f64::from(value);
                            eprintln!("shipped-route\t{name}\t{far_pitch}\t{delay}\t{pitch}\t{arm}\t{w}\t{value:.9}");
                        }
                    }
                }
            }
        }
        for (index, (arm, _, _, _)) in arms.iter().enumerate() {
            let count = if echo { 9 } else { 4 };
            for w in 0..count {
                if echo && w % 2 != 0 {
                    continue;
                }
                eprintln!("shipped-route-mean\t{name}\t{arm}\t{w}\t{:.9}", sums[index][w] / 27.0);
            }
        }
    }
}
