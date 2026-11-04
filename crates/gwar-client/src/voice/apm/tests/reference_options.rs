//! Paired native-reference options. All samples stay in memory; no audio devices.
use super::*;

#[derive(Clone, Copy)]
struct NativeOption {
    name: &'static str,
    warm: bool,
    legacy: bool,
    profile: u8,
}

const OPTIONS: [NativeOption; 7] = [
    NativeOption { name: "left-warm-p0", warm: true, legacy: true, profile: 0 },
    NativeOption { name: "left-cold-p0", warm: false, legacy: true, profile: 0 },
    NativeOption { name: "left-warm-p7", warm: true, legacy: true, profile: 7 },
    NativeOption { name: "two-warm-p7", warm: true, legacy: false, profile: 7 },
    NativeOption { name: "two-cold-p7", warm: false, legacy: false, profile: 7 },
    NativeOption { name: "two-warm-p1", warm: true, legacy: false, profile: 1 },
    NativeOption { name: "two-cold-p1", warm: false, legacy: false, profile: 1 },
];

fn option_output(option: NativeOption, far: &[[f32; 2]], mic: &[f32], pin: usize) -> Vec<f32> {
    let _scope = measurement_scope();
    AEC_NATIVE_PROFILE.store(option.profile, Ordering::Relaxed);
    AEC_NATIVE_CHANNELS.store(!option.legacy, Ordering::Relaxed);
    AEC_LEGACY_REFERENCE.store(option.legacy, Ordering::Relaxed);
    let out = if option.warm {
        warm_through(&mut warm_session(), far, mic, Some(pin))
    } else {
        through(&mut session(LEARNING_FRAMES), far, mic, Some(pin))
    };
    AEC_NATIVE_PROFILE.store(SHIPPED_AEC_PROFILE, Ordering::Relaxed);
    AEC_NATIVE_CHANNELS.store(false, Ordering::Relaxed);
    AEC_LEGACY_REFERENCE.store(false, Ordering::Relaxed);
    out
}

fn emit_option_window(option: NativeOption, case: &str, from: usize, to: usize, samples: &[f32]) {
    let window = &samples[from..to];
    let energy = window.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / RATE as f64;
    eprintln!("native-option-echo\t{}\t{case}\t{}\t{:.9}\t{energy:.15e}", option.name, to - from, db(window));
}

/// Same 396 mono + 153 wide window observations as the historical paired table.
/// The near-pitch rows are repeated pure-echo controls, not independent rooms.
#[test]
#[ignore = "seven arms on all 549 paired echo windows; release, alone, single-threaded"]
fn native_reference_options_full_echo() {
    let _scope = measurement_scope();
    let mono_windows: Vec<_> = (0..8).map(|at| (at, at + 1)).chain([(8, 12), (12, 16), (16, 24)]).collect();
    for before in [6, 20] {
        let pin = before * RATE;
        let total = pin + 24 * RATE;
        for pitch in [100.0, 150.0, 230.0] {
            let far = pairs(&voice(total, pitch, -20.0));
            for delay in [20, 60, 150] {
                let mic = room(&left_channel(&far), delay);
                for option in OPTIONS {
                    let out = option_output(option, &far, &mic, pin);
                    for near in [180, 280] {
                        for (a, b) in &mono_windows {
                            emit_option_window(
                                option,
                                &format!("mono,pin={before},far={pitch},delay={delay},near={near},s={a}-{b}"),
                                pin + a * RATE,
                                pin + b * RATE,
                                &out,
                            );
                        }
                    }
                }
            }
        }
    }
    let wide_windows = [(0, 1), (1, 2), (2, 4), (4, 6), (6, 8), (8, 12), (12, 16), (16, 24), (32, 48)];
    let run_wide = |name: &str, pin: usize, far: &[[f32; 2]], mic: &[f32]| {
        for option in OPTIONS {
            let out = option_output(option, far, mic, pin);
            for (a, b) in wide_windows {
                emit_option_window(
                    option,
                    &format!("wide,{name},q={a}-{b}"),
                    pin + a * RATE / 4,
                    pin + b * RATE / 4,
                    &out,
                );
            }
        }
    };
    for before in [6, 20, 60, 310] {
        let pin = before * RATE;
        for (name, first, second) in [("same", 120.0, 120.0), ("same", 190.0, 190.0), ("new", 150.0, 120.0)] {
            let mut far = pairs(&long_voice(pin, first, -18.0));
            far.extend(wide(&long_voice(12 * RATE, second, -18.0)));
            let mic = room(&left_channel(&far), 60);
            run_wide(&format!("{name} {first}->{second} pin {before}"), pin, &far, &mic);
        }
        let talk = long_voice(pin + 12 * RATE, 150.0, -18.0);
        let mid = long_voice(pin + 12 * RATE, 120.0, -20.0);
        let side = long_voice(pin + 12 * RATE, 170.0, -28.0);
        let far: Vec<_> = (0..talk.len())
            .map(|at| if at < pin { [talk[at]; 2] } else { [mid[at] + side[at], mid[at] - side[at]] })
            .collect();
        let right: Vec<_> = far.iter().map(|s| s[1]).collect();
        let mic: Vec<_> =
            room(&left_channel(&far), 60).iter().zip(room(&right, 75)).map(|(l, r)| l + 0.7 * r).collect();
        run_wide(&format!("speakers pin {before}"), pin, &far, &mic);
    }
    let pin = 310 * RATE;
    let mut far = pairs(&voice(pin, 150.0, -18.0));
    far.extend(wide(&long_voice(12 * RATE, 120.0, -18.0)));
    let mic = room(&left_channel(&far), 60);
    run_wide("steady pin 310", pin, &far, &mic);
}

#[test]
#[ignore = "right-only and independent wide acoustic paths; release, alone, single-threaded"]
fn native_reference_options_right_echo() {
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
            for option in OPTIONS {
                let out = option_output(option, &far, &mic, pin);
                for (a, b) in windows {
                    emit_option_window(
                        option,
                        &format!("right,{shape},pin={before},q={a}-{b}"),
                        pin + a * RATE / 4,
                        pin + b * RATE / 4,
                        &out,
                    );
                }
            }
        }
    }
}
