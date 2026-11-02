//! Offline DSP path: real Opus render/capture, AEC, the production input gate and
//! the production DeepFilterNet model. No devices, sleeps or model-worker timing.
use super::*;
use audiopus::{
    Application, Bitrate, Channels as OpusChannels, MutSignals, SampleRate,
    coder::{Decoder, Encoder},
    packet::Packet,
};
use std::time::{Duration, Instant};

fn human_speech(len: usize) -> Vec<f32> {
    let pcm = include_bytes!("fixtures/librispeech-1688-142285-0007.f32le");
    let samples: Vec<f32> = pcm.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
    let mut out: Vec<f32> = (0..len)
        .map(|i| {
            let at = (i / 3) % samples.len();
            let next = (at + 1) % samples.len();
            let phase = (i % 3) as f32 / 3.0;
            samples[at] * (1.0 - phase) + samples[next] * phase
        })
        .collect();
    let gain = 10f32.powf(-24.0 / 20.0) / (out.iter().map(|x| x * x).sum::<f32>() / len as f32).sqrt();
    out.iter_mut().for_each(|s| *s *= gain);
    out
}

fn near_signal(name: &str, len: usize) -> Vec<f32> {
    if matches!(name, "silence" | "noise") {
        return vec![0.0; len];
    }
    if name.starts_with("human") {
        return human_speech(len);
    }
    if name == "talker" {
        return talker(len, 213.0, 3.1, 3.3, -24.0);
    }
    let talk = talker(len, 213.0, 3.1, 3.3, -24.0);
    let mut phase = 0.0f64;
    let mut out: Vec<f32> = (0..len)
        .map(|i| {
            let t = i as f64 / RATE as f64;
            let f = if name == "sung vibrato" {
                220.0 * (1.0 + 0.025 * (std::f64::consts::TAU * 5.2 * t).sin())
            } else {
                440.0
            };
            phase = (phase + std::f64::consts::TAU * f / RATE as f64) % std::f64::consts::TAU;
            match name {
                "sung vibrato" => {
                    (phase.sin() + 0.4 * (2.0 * phase + 0.7).sin() + 0.2 * (3.0 * phase + 1.2).sin()) as f32
                }
                "instrument" => {
                    ((1..=9).map(|h| (f64::from(h) * phase + f64::from(h * h)).sin() / f64::from(h * h)).sum::<f64>()
                        * (-3.0 * (t % 0.7)).exp()) as f32
                }
                "hum plus talker" => talk[i] + 0.03 * (std::f64::consts::TAU * 60.0 * t).sin() as f32,
                _ => phase.sin() as f32,
            }
        })
        .collect();
    let gain = 10f32.powf(-24.0 / 20.0) / (out.iter().map(|x| x * x).sum::<f32>() / len as f32).sqrt();
    out.iter_mut().for_each(|s| *s *= gain);
    out
}

fn coded_render(lead: usize, total: usize) -> Vec<[f32; 2]> {
    let cue = long_voice(total, 150.0, -20.0);
    let mut encoder = Encoder::new(SampleRate::Hz48000, OpusChannels::Stereo, Application::Audio).unwrap();
    encoder.set_bitrate(Bitrate::BitsPerSecond(64_000)).unwrap();
    let mut decoder = Decoder::new(SampleRate::Hz48000, OpusChannels::Stereo).unwrap();
    let (mut packet, mut pcm) = (vec![0; 1275], vec![0.0f32; 2 * 960]);
    let mut out = Vec::with_capacity(total);
    for at in (0..total).step_by(960) {
        for i in 0..960 {
            let t = (at + i) as f64 / RATE as f64;
            let pair = if at + i < lead * RATE {
                [cue[at + i]; 2]
            } else {
                [
                    (0.2 * (std::f64::consts::TAU * 330.0 * t).sin()) as f32,
                    (0.2 * (std::f64::consts::TAU * 523.0 * t).sin()) as f32,
                ]
            };
            (pcm[2 * i], pcm[2 * i + 1]) = (pair[0], pair[1]);
        }
        let n = encoder.encode_float(&pcm, &mut packet).unwrap();
        let count = decoder
            .decode_float(
                Some(Packet::try_from(&packet[..n]).unwrap()),
                MutSignals::try_from(&mut pcm[..]).unwrap(),
                false,
            )
            .unwrap();
        assert_eq!(count, 960);
        let gain = if at < lead * RATE { 1.0 } else { 4.0 };
        out.extend(
            pcm.as_chunks::<2>().0.iter().map(|p| [(gain * p[0]).clamp(-1.0, 1.0), (gain * p[1]).clamp(-1.0, 1.0)]),
        );
    }
    out
}

fn projection(out: &[f32], near: &[f32], from: usize, to: usize) -> [f32; 2] {
    let end = to - 2400;
    let dot = |lag: usize, step: usize| {
        (from..end).step_by(step).map(|i| f64::from(near[i]) * f64::from(out[i + lag])).sum::<f64>()
    };
    let coarse = (0usize..2400).step_by(16).max_by(|a, b| dot(*a, 8).abs().total_cmp(&dot(*b, 8).abs())).unwrap();
    let lag = (coarse.saturating_sub(16)..(coarse + 17).min(2400))
        .max_by(|a, b| dot(*a, 1).abs().total_cmp(&dot(*b, 1).abs()))
        .unwrap();
    let d = dot(lag, 1);
    let n = (from..end).map(|i| f64::from(near[i]).powi(2)).sum::<f64>();
    let o = (from..end).map(|i| f64::from(out[i + lag]).powi(2)).sum::<f64>();
    [(20.0 * (d.abs() / n.max(1e-30) + 1e-30).log10()) as f32, (d.abs() / (n * o).sqrt().max(1e-30)) as f32]
}

#[derive(Debug)]
struct Holds {
    maximum: [usize; 4],
    kept: f32,
    sent: Vec<usize>,
    levels: [f32; 3],
    projection: [[[f32; 2]; 3]; 2],
    aec_split: [[[f32; 2]; 3]; 2],
    echo_levels: Vec<f32>,
}

/// Stable diagnostic tags, represented as independent controls so adding a
/// comparison cannot silently enable another experiment.
struct ArmOptions {
    warm: bool,
    aec: bool,
    gain: bool,
    reset_model: bool,
    minimum_snr: Option<f32>,
    stages: Option<(f32, bool)>,
    normalize: bool,
    mask_after_stereo: bool,
    native_at_share: bool,
    nonlinear_restart: u8,
    native_channels: bool,
    warm_only_two: bool,
    native_profile: u8,
}
impl ArmOptions {
    fn tagged(tag: u8) -> Self {
        let mut o = Self {
            warm: false,
            aec: true,
            gain: false,
            reset_model: false,
            minimum_snr: None,
            stages: None,
            normalize: false,
            mask_after_stereo: false,
            native_at_share: false,
            nonlinear_restart: 0,
            native_channels: false,
            warm_only_two: false,
            native_profile: 0,
        };
        match tag {
            0 => {}
            1 => o.warm = true,
            2 => o.aec = false,
            3 => {
                o.warm = true;
                o.reset_model = true;
            }
            4 => o.reset_model = true,
            5 => {
                o.warm = true;
                o.minimum_snr = Some(-100.0);
            }
            6 => o.minimum_snr = Some(-100.0),
            7 => {
                o.warm = true;
                o.gain = true;
            }
            8 => o.gain = true,
            9 => {
                o.aec = false;
                o.gain = true;
            }
            10 => {
                o.warm = true;
                o.normalize = true;
            }
            11 => o.normalize = true,
            12 => {
                o.aec = false;
                o.normalize = true;
            }
            13 => {
                o.warm = true;
                o.stages = Some((-100.0, true));
            }
            14 => {
                o.warm = true;
                o.stages = Some((20.0, true));
            }
            15 => {
                o.warm = true;
                o.stages = Some((35.0, false));
            }
            16 => {
                o.warm = true;
                o.mask_after_stereo = true;
            }
            17 => {
                o.warm = true;
                o.gain = true;
                o.mask_after_stereo = true;
            }
            18 => {
                o.gain = true;
                o.mask_after_stereo = true;
            }
            20 => {
                o.warm = true;
                o.gain = true;
                o.native_at_share = true;
            }
            21..=24 => {
                o.warm = true;
                o.gain = true;
                o.nonlinear_restart = tag - 20;
            }
            25..=28 => {
                o.gain = true;
                o.nonlinear_restart = tag - 24;
            }
            30 => {
                o.warm = true;
                o.gain = true;
                o.native_channels = true;
            }
            31 => {
                o.gain = true;
                o.native_channels = true;
            }
            32..=35 => {
                o.warm = true;
                o.gain = true;
                o.native_channels = true;
                o.nonlinear_restart = tag - 31;
            }
            36..=38 => {
                o.warm = true;
                o.gain = true;
                o.nonlinear_restart = tag - 31;
            }
            40..=45 => {
                o.warm = true;
                o.gain = true;
                o.native_profile = tag - 39;
            }
            50..=55 => {
                o.warm = true;
                o.gain = true;
                o.native_channels = true;
                o.native_profile = tag - 49;
            }
            80 => {
                o.gain = true;
                o.native_profile = 1;
            }
            81 => {
                o.gain = true;
                o.native_profile = 2;
            }
            90..=92 => {
                o.warm = true;
                o.gain = true;
                o.native_profile = tag - 83;
            }
            86 => {
                o.warm = true;
                o.gain = true;
                o.warm_only_two = true;
                o.nonlinear_restart = 3;
            }
            87 => {
                o.warm = true;
                o.gain = true;
                o.warm_only_two = true;
                o.nonlinear_restart = 4;
            }
            88 => {
                o.warm = true;
                o.gain = true;
                o.warm_only_two = true;
                o.nonlinear_restart = 3;
                o.native_profile = 5;
            }
            89 => {
                o.warm = true;
                o.gain = true;
                o.warm_only_two = true;
                o.nonlinear_restart = 3;
                o.native_profile = 7;
            }
            93 => {
                o.gain = true;
                o.native_profile = 7;
            }
            94 => {
                o.gain = true;
                o.native_profile = 9;
            }
            95 => {
                o.warm = true;
                o.gain = true;
                o.native_profile = 10;
            }
            96 => {
                o.warm = true;
                o.gain = true;
                o.native_profile = 11;
            }
            97 | 98 => {
                o.warm = true;
                o.gain = true;
                o.native_profile = tag - 85;
            }
            _ => panic!("unknown measurement arm {tag}"),
        }
        o
    }
}

#[cfg(feature = "deep-filter")]
fn arm(name: &str, lead: usize, echo: bool, noise: u8, mode: u8) -> Holds {
    arm_with_options(name, lead, echo, noise, mode, ArmOptions::tagged(mode))
}

fn arm_with_options(name: &str, lead: usize, echo: bool, noise: u8, mode: u8, options: ArmOptions) -> Holds {
    let total = (lead + 16) * RATE;
    let far = coded_render(lead, total);
    let l = room(&left_channel(&far), 60);
    let r = room(&far.iter().map(|p| p[1]).collect::<Vec<_>>(), 75);
    let start = (lead - 1) * RATE;
    let mut said = vec![0.0f32; start];
    said.extend(near_signal(name, total - start));
    let mic: Vec<f32> = (0..total)
        .map(|i| {
            said[i]
                + if echo { l[i] + 0.7 * r[i] } else { 0.0 }
                + if matches!(name, "noise" | "human with noise") {
                    (((i as u32).wrapping_mul(2_654_435_761) as f64 / u32::MAX as f64) - 0.5) as f32 * 0.034641
                } else {
                    0.0
                }
        })
        .collect();
    AEC_WARM_ONLY_TWO.store(options.warm_only_two, Ordering::Relaxed);
    AEC_NATIVE_PROFILE.store(options.native_profile, Ordering::Relaxed);
    AEC_NATIVE_CHANNELS.store(options.native_channels, Ordering::Relaxed);
    AEC_NONLINEAR_AT_TAKEOVER.store(options.nonlinear_restart, Ordering::Relaxed);
    let mut p = if options.warm { warm_session() } else { session(LEARNING_FRAMES) };
    p.configure(options.aec, options.gain);
    let mut model = (noise > 0).then(|| gwar_denoise::Model::embedded().unwrap());
    if let Some(model) = &mut model {
        if let Some(minimum) = options.minimum_snr {
            model.set_measurement_min_snr(minimum);
        }
        if let Some((threshold, post_filter)) = options.stages {
            model.set_measurement_stages(threshold, post_filter);
        }
    }
    let mut automatic = super::super::super::gate::InputGate::default();
    let mut manual = super::super::super::gate::InputGate::default();
    let encoder = super::super::super::voice_encoder().unwrap();
    let mut decoder = Decoder::new(SampleRate::Hz48000, OpusChannels::Mono).unwrap();
    let (mut packet, mut decoded) = (vec![0; 1275], [0.0f32; FRAME]);
    let mut run = [0usize; 4];
    let mut maximum = [0usize; 4];
    let (mut speaking, mut kept) = (0.0f32, 0.0f32);
    let mut sent = vec![0usize; lead + 16];
    let mut linear = Vec::with_capacity(total);
    let mut suppressed = Vec::with_capacity(total);
    let mut before_model = Vec::with_capacity(total);
    let mut after_model = Vec::with_capacity(total);
    let mut powers = [0.0f64; 3];
    let mut counts = [0usize; 3];
    let now = Instant::now();
    for at in (0..total).step_by(FRAME) {
        if options.aec && at == lead * RATE {
            p.caught_up();
            p.pin_stereo();
            p.collected();
            if options.native_at_share
                && let Channels::Learning(stage, _) = &mut p.channels
            {
                stage.native_gain_change();
            }
        }
        let mut frame: [f32; FRAME] = mic[at..at + FRAME].try_into().unwrap();
        let was_stereo = p.is_stereo();
        p.render(&far[at..at + FRAME]);
        p.cancel_echo(&mut frame, 20);
        if !was_stereo
            && p.is_stereo()
            && options.reset_model
            && let Some(model) = &mut model
        {
            model.reset();
        }
        if options.warm {
            p.ready_for_the_next_frame();
        }
        if options.mask_after_stereo
            && let Some(model) = &mut model
        {
            model.set_measurement_stages(if p.is_stereo() { -100.0 } else { 35.0 }, true);
        }
        linear.extend(p.echo.apm.measurement_linear().into_iter().flat_map(|v| [v; 3]));
        suppressed.extend_from_slice(&frame);
        if AEC_TRACE.load(Ordering::Relaxed) && at >= (lead - 1) * RATE && at.is_multiple_of(RATE / 2) {
            eprintln!(
                "aec-state,{name},lead={lead},echo={echo},noise={noise},mode={mode},at={:.2},stereo={},active={:?}",
                at as f32 / RATE as f32 - lead as f32,
                p.is_stereo(),
                p.echo.apm.measurement()
            );
            if let Channels::Learning(stage, _) = &p.channels {
                eprintln!(
                    "aec-candidate,mode={mode},at={:.2},state={:?}",
                    at as f32 / RATE as f32 - lead as f32,
                    stage.apm.measurement()
                );
            }
        }
        let raw = db(&frame);
        p.control_gain(&mut frame);
        before_model.extend_from_slice(&frame);
        let normalization =
            if options.normalize { 10f32.powf((-20.0 - db(&frame)) / 20.0).clamp(0.1, 1000.0) } else { 1.0 };
        if let Some(model) = &mut model {
            frame.iter_mut().for_each(|s| *s *= normalization);
            model.process(&mut frame, gwar_denoise::NoiseSuppression::percent(noise)).unwrap();
            frame.iter_mut().for_each(|s| *s /= normalization);
        }
        after_model.extend_from_slice(&frame);
        if at >= lead * RATE {
            let second = (at - lead * RATE) / RATE;
            let window = if second < 3 {
                0
            } else if second < 8 {
                1
            } else {
                2
            };
            powers[window] += frame.iter().map(|s| f64::from(*s) * f64::from(*s)).sum::<f64>();
            counts[window] += FRAME;
        }
        let time = now + Duration::from_millis((at / FRAME * 10) as u64);
        let send = automatic.allows(time, raw, -50.0, None, false);
        let send_manual = manual.allows(time, db(&frame), -50.0, None, false);
        if send {
            let n = encoder.encode_float(&frame, &mut packet).unwrap();
            assert_eq!(
                decoder
                    .decode_float(
                        Some(Packet::try_from(&packet[..n]).unwrap()),
                        MutSignals::try_from(&mut decoded[..]).unwrap(),
                        false
                    )
                    .unwrap(),
                FRAME
            );
        } else {
            decoded.fill(0.0);
        }
        let audible = send && db(&decoded) > -60.0;
        sent[at / RATE] += usize::from(audible);
        let input = db(&said[at..at + FRAME]);
        if at >= lead * RATE && input > -40.0 {
            speaking += 1.0;
            kept += f32::from(u8::from(db(&decoded) > input - 10.0));
            for (i, held) in [raw < -50.0, !send, !send_manual, !audible].into_iter().enumerate() {
                run[i] = if held { run[i] + 1 } else { 0 };
                maximum[i] = maximum[i].max(run[i]);
            }
        }
    }
    AEC_WARM_ONLY_TWO.store(false, Ordering::Relaxed);
    AEC_NATIVE_PROFILE.store(0, Ordering::Relaxed);
    AEC_NATIVE_CHANNELS.store(false, Ordering::Relaxed);
    AEC_NONLINEAR_AT_TAKEOVER.store(0, Ordering::Relaxed);
    Holds {
        echo_levels: (0..32)
            .map(|n| db(&after_model[lead * RATE + n * RATE / 2..lead * RATE + (n + 1) * RATE / 2]))
            .collect(),
        aec_split: if name.starts_with("human") {
            std::array::from_fn(|stage| {
                std::array::from_fn(|w| {
                    let (a, b) = [(0, 3), (3, 8), (8, 16)][w];
                    projection(
                        if stage == 0 { &linear } else { &suppressed },
                        &said,
                        (lead + a) * RATE,
                        (lead + b) * RATE,
                    )
                })
            })
        } else {
            [[[f32::NAN; 2]; 3]; 2]
        },
        maximum,
        kept: kept / speaking,
        sent,
        levels: std::array::from_fn(|i| (10.0 * (powers[i] / counts[i] as f64 + 1e-20).log10()) as f32),
        projection: if name.starts_with("human") {
            std::array::from_fn(|stage| {
                std::array::from_fn(|w| {
                    let (a, b) = [(0, 3), (3, 8), (8, 16)][w];
                    projection(
                        if stage == 0 { &before_model } else { &after_model },
                        &said,
                        (lead + a) * RATE,
                        (lead + b) * RATE,
                    )
                })
            })
        } else {
            [[[f32::NAN; 2]; 3]; 2]
        },
    }
}

#[test]
#[cfg(feature = "deep-filter")]
#[ignore = "leading signal, Opus, gate and model comparison; run in release alone"]
fn realistic_signals_through_the_warmed_switch() {
    for lead in [6, 10] {
        for echo in [false, true] {
            for name in ["steady tone", "sung vibrato", "instrument", "hum plus talker", "talker", "human speech"] {
                for noise in [0, 50, 100] {
                    for mode in [0, 1, 2] {
                        let h = arm(name, lead, echo, noise, mode);
                        eprintln!(
                            "holds,{name},lead={lead},echo={echo},noise={noise},mode={mode},active-frame maxima [aec,auto,manual,decoded]={:?},kept={:.3},sent={:?}",
                            h.maximum, h.kept, h.sent
                        );
                    }
                }
            }
        }
    }
}

#[test]
#[cfg(feature = "deep-filter")]
#[ignore = "isolated model-context diagnostic, run in release"]
fn model_context_after_the_takeover() {
    for mode in 0..7 {
        let h = arm("human speech", 6, true, 100, mode);
        eprintln!("model-context,mode={mode},maxima={:?},kept={:.3},sent={:?}", h.maximum, h.kept, h.sent);
    }
}

#[test]
#[cfg(feature = "deep-filter")]
#[ignore = "model level diagnostic, run in release"]
fn level_before_the_model() {
    for mode in 7..13 {
        let h = arm("human speech", 6, true, 100, mode);
        eprintln!("model-level,mode={mode},maxima={:?},kept={:.3},sent={:?}", h.maximum, h.kept, h.sent);
    }
}

#[test]
#[cfg(feature = "deep-filter")]
#[ignore = "model stage diagnostic, run in release"]
fn model_stages_after_echo() {
    for mode in 13..16 {
        let h = arm("human speech", 6, true, 100, mode);
        eprintln!("model-stages,mode={mode},maxima={:?},kept={:.3},sent={:?}", h.maximum, h.kept, h.sent);
    }
}

#[test]
#[cfg(feature = "deep-filter")]
#[ignore = "post-AEC gain-mask speech/noise/echo comparison, run in release"]
fn gain_mask_after_stereo() {
    for name in ["human speech", "silence", "noise", "human with noise"] {
        for mode in [7, 17] {
            let h = arm(name, 6, true, 100, mode);
            eprintln!(
                "post-aec-mask,{name},mode={mode},maxima={:?},kept={:.3},levels={:?},projection={:?},sent={:?}",
                h.maximum, h.kept, h.levels, h.projection, h.sent
            );
        }
    }
}

#[test]
#[cfg(feature = "deep-filter")]
#[ignore = "native AEC gain-change held-speech diagnostic, run in release"]
fn native_change_at_first_share() {
    ROUTE_MEASUREMENT.store(4, Ordering::Relaxed);
    for name in ["human speech", "silence"] {
        let h = arm(name, 6, true, 100, 20);
        eprintln!(
            "native-aec,{name},maxima={:?},kept={:.3},levels={:?},projection={:?},sent={:?}",
            h.maximum, h.kept, h.levels, h.projection, h.sent
        );
    }
    ROUTE_MEASUREMENT.store(2, Ordering::Relaxed);
}

#[test]
#[cfg(feature = "deep-filter")]
#[ignore = "strict acoustic acceptance, known to fail; run in release alone"]
fn recorded_speech_is_not_held_after_stereo() {
    let mut failures = Vec::new();
    for lead in [6, 10] {
        for echo in [false, true] {
            for noise in [0, 50, 100] {
                let cold = arm("human speech", lead, echo, noise, 8);
                let warm = arm("human speech", lead, echo, noise, 7);
                eprintln!("speech-gate,lead={lead},echo={echo},noise={noise},cold={cold:?},warm={warm:?}");
                if warm.maximum[3] > cold.maximum[3] || warm.kept < cold.kept {
                    failures.push((lead, echo, noise, cold.maximum[3], warm.maximum[3], cold.kept, warm.kept));
                }
            }
        }
    }
    assert!(failures.is_empty(), "warm speech regresses against cold: {failures:?}");
}

#[test]
#[ignore = "native AEC state and linear/suppressor comparison; run in release alone"]
fn aec_linear_and_suppression_at_takeover() {
    AEC_TRACE.store(true, Ordering::Relaxed);
    for mode in [8, 7, 21, 22, 23, 24, 30, 31, 32, 33] {
        let h = arm("human speech", 6, true, 100, mode);
        eprintln!(
            "aec-split,mode={mode},maxima={:?},kept={},linear-and-suppressed={:?},gain-and-model={:?},levels={:?}",
            h.maximum, h.kept, h.aec_split, h.projection, h.levels
        );
    }
    AEC_TRACE.store(false, Ordering::Relaxed);
}

#[test]
#[ignore = "native reverb/noise/state/config isolation; run in release alone"]
fn native_reverb_and_capture_state() {
    AEC_TRACE.store(true, Ordering::Relaxed);
    for mode in [36, 37, 38, 40, 41, 42, 43, 44, 45, 50, 51, 52, 53, 54, 55] {
        let h = arm("human speech", 6, true, 100, mode);
        eprintln!(
            "aec-native,mode={mode},maxima={:?},kept={},linear-and-suppressed={:?},gain-and-model={:?},levels={:?}",
            h.maximum, h.kept, h.aec_split, h.projection, h.levels
        );
    }
    AEC_TRACE.store(false, Ordering::Relaxed);
}

#[test]
#[ignore = "default speech matrix and 500 ms quiet-echo gate; run in release alone"]
fn native_speech_and_quiet_echo_gate() {
    let modes: Vec<u8> = std::env::var("GWAR_NATIVE_ARMS")
        .unwrap_or_else(|_| "7,40,41,51,54,31".into())
        .split(',')
        .map(|v| v.parse().unwrap())
        .collect();
    let mut speech_pass = vec![true; modes.len()];
    let mut echo_pass = vec![true; modes.len()];
    for lead in [6, 10] {
        for echo in [false, true] {
            for noise in [0, 50, 100] {
                let cold = arm("human speech", lead, echo, noise, 8);
                let no_aec = arm("human speech", lead, echo, noise, 9);
                eprintln!(
                    "speech-matrix-control,lead={lead},echo={echo},noise={noise},cold-hold={},cold-kept={},no-aec-hold={},no-aec-kept={}",
                    cold.maximum[3], cold.kept, no_aec.maximum[3], no_aec.kept
                );
                for (i, mode) in modes.iter().copied().enumerate() {
                    let h = arm("human speech", lead, echo, noise, mode);
                    let pass = h.maximum[3] <= cold.maximum[3] && h.kept >= cold.kept;
                    speech_pass[i] &= pass;
                    eprintln!(
                        "speech-matrix,lead={lead},echo={echo},noise={noise},mode={mode},hold={},kept={},pass={pass},near-gain={:?}",
                        h.maximum[3], h.kept, h.projection[1]
                    );
                }
            }
        }
        let mut current_options = ArmOptions::tagged(7);
        current_options.gain = false;
        let current = arm_with_options("silence", lead, true, 0, 7, current_options);
        eprintln!("echo-matrix-control,lead={lead},levels={:?}", current.echo_levels);
        for (i, mode) in modes.iter().copied().enumerate() {
            let mut options = ArmOptions::tagged(mode);
            options.gain = false;
            let h = arm_with_options("silence", lead, true, 0, mode, options);
            let delta: Vec<f32> = h.echo_levels.iter().zip(&current.echo_levels).map(|(a, b)| a - b).collect();
            let worst = delta.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            let pass = worst <= 1.0;
            echo_pass[i] &= pass;
            eprintln!(
                "echo-matrix,lead={lead},mode={mode},levels={:?},delta={delta:?},worst={worst},pass={pass}",
                h.echo_levels
            );
        }
    }
    for (i, mode) in modes.iter().copied().enumerate() {
        eprintln!(
            "native-gate,mode={mode},speech={},quiet-echo={},accepted={}",
            speech_pass[i],
            echo_pass[i],
            speech_pass[i] && echo_pass[i]
        );
    }
}

#[test]
#[ignore = "cold-path native reverb comparison, recorded speech; run in release alone"]
fn cold_path_native_reverb() {
    for lead in [6, 10] {
        for noise in [0, 50, 100] {
            for mode in [8, 80, 81] {
                let h = arm("human speech", lead, true, noise, mode);
                eprintln!(
                    "cold-native,lead={lead},noise={noise},mode={mode},maxima={:?},kept={},aec={:?},model={:?}",
                    h.maximum, h.kept, h.aec_split, h.projection
                );
            }
        }
    }
}

#[test]
#[ignore = "bounded native nearend/reverb screening; run in release alone"]
fn bounded_native_reverb_screen() {
    for lead in [6, 10] {
        let cold = arm("human speech", lead, true, 100, 8);
        for mode in [34, 35, 86, 87, 88, 89, 90, 91, 92] {
            let h = arm("human speech", lead, true, 100, mode);
            eprintln!(
                "bounded-native,lead={lead},mode={mode},hold={},kept={},cold-hold={},cold-kept={},speech-pass={}",
                h.maximum[3],
                h.kept,
                cold.maximum[3],
                cold.kept,
                h.maximum[3] <= cold.maximum[3] && h.kept >= cold.kept
            );
        }
    }
}

#[test]
#[ignore = "native nearend-confidence budget screening; run in release alone"]
fn nearend_decay_budget_screen() {
    for lead in [6, 10] {
        let cold = arm("human speech", lead, true, 100, 8);
        for mode in [97, 98] {
            let h = arm("human speech", lead, true, 100, mode);
            eprintln!(
                "decay-budget,lead={lead},mode={mode},hold={},kept={},cold-hold={},cold-kept={},pass={},near-gain={:?}",
                h.maximum[3],
                h.kept,
                cold.maximum[3],
                cold.kept,
                h.maximum[3] <= cold.maximum[3] && h.kept >= cold.kept,
                h.projection[1]
            );
        }
    }
}
