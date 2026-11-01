//! Echo cancellation and automatic gain control of the microphone: WebRTC's audio processing
//! (AEC3, AGC2), the one browsers run on `getUserMedia`, through `sonora`, its port to Rust.
//!
//! Two stages, so the voice gate can measure between them: the canceller, then gain control. The
//! canceller gets what the speaker plays as the pipeline queues it, ahead of the device, and finds
//! the echo's delay itself. It adds ~9 ms to the microphone (AEC3 works in 4 ms blocks and overlaps
//! them, after splitting 48 kHz into three bands); gain control adds nothing. A stage turned off
//! passes the microphone untouched.
//!
//! Voice alone uses one render channel, and a voice session that never plays share sound stays
//! there. The first share sound really played starts a stereo canceller beside it, which takes
//! over once it has learned the room and stays until the session ends: [`Processing::pin_stereo`].

use sonora::{
    AudioProcessing, Config, StreamConfig,
    config::{AdaptiveDigital, EchoCanceller, GainController2, MaxProcessingRate, Pipeline, TransparentModeType},
};

use super::FRAME;
use std::{
    sync::mpsc::{Receiver, sync_channel},
    thread,
};

/// Nothing on, at the full 48 kHz: by default the processing runs at 32 kHz, which would take
/// everything above 16 kHz out of the microphone.
fn base() -> Config {
    Config {
        pipeline: Pipeline { maximum_internal_processing_rate: MaxProcessingRate::Rate48kHz, ..Default::default() },
        ..Default::default()
    }
}

/// How long the stereo canceller learns the room beside the mono one before it takes over, in
/// 10 ms frames with something playing. AEC3 holds a voice down hard for its first 2.5 s of sound,
/// so voice stays on the mono canceller for all of that, as `the_switch_against_staying_mono` shows.
const LEARNING_FRAMES: usize = 300;
/// A frame's power that counts as playing, summed over its samples and channels: -50 dBFS, where
/// AEC3 itself starts to count a block as sound.
const PLAYING: f32 = 1e-5 * FRAME as f32;

/// One of WebRTC's processors, set up for a single stage (or none, while it is off).
struct Stage {
    apm: AudioProcessing,
    on: bool,
}

impl Stage {
    fn new(render_channels: u16) -> Self {
        Self {
            apm: AudioProcessing::builder()
                .config(base())
                .capture_config(StreamConfig::new(super::io::RATE, 1))
                .render_config(StreamConfig::new(super::io::RATE, render_channels))
                .build(),
            on: false,
        }
    }

    /// A running stereo canceller, its memory all allocated: this one frame of silence builds what
    /// the first real frame would otherwise build on the thread voice runs on.
    fn stereo() -> Self {
        let mut stage = Self::new(2);
        stage.apm.apply_config(canceller(true));
        stage.on = true;
        let silence = [0f32; FRAME];
        let (mut left, mut right, mut near) = ([0f32; FRAME], [0f32; FRAME], [0f32; FRAME]);
        let _ = stage.apm.process_render_f32(&[&silence, &silence], &mut [&mut left, &mut right]);
        let _ = stage.apm.process_capture_f32(&[&silence], &mut [&mut near]);
        stage
    }

    /// Whether `on` changed; one turned on starts from scratch.
    fn set(&mut self, on: bool, config: impl FnOnce() -> Config) -> bool {
        if on == self.on {
            return false;
        }
        self.on = on;
        self.apm.apply_config(if on { config() } else { base() });
        true
    }
}

/// The canceller's settings. Stereo never watches its content: it stays on both channels whatever
/// they hold, because the detector would wait two seconds for stereo and forget it after five
/// minutes, and short bursts of wide sound would never reach it at all.
fn canceller(stereo: bool) -> Config {
    let canceller = EchoCanceller {
        transparent_mode: TransparentModeType::Hmm,
        detect_stereo_content: !stereo,
        ..Default::default()
    };
    Config { echo_canceller: Some(canceller), ..base() }
}

/// Where the canceller is on its one-way trip from mono to stereo in a voice session.
enum Channels {
    /// Voice alone: one channel, as if share sound did not exist.
    Mono,
    /// Share sound played. The stereo canceller is being built off this thread.
    Building(Receiver<Stage>),
    /// It hears everything the mono one does, and voice still comes from the mono one. The count
    /// is the frames of sound still to hear: over silence there is nothing to learn from.
    Learning(Box<Stage>, usize),
    /// `echo` is the stereo canceller until the session ends. The box holds the old one: freeing
    /// it here is not worth the wait on a thread that voice runs on.
    Stereo(#[allow(dead_code)] Box<Stage>),
    /// No thread could be had: voice goes on as it was, and share sound's sides leak.
    Refused,
}

pub(crate) struct Processing {
    echo: Stage,
    gain: Stage,
    channels: Channels,
    learning_frames: usize,
    /// What each speaker was given since the last whole frame went to the canceller.
    render: [[f32; FRAME]; 2],
    rendered: usize,
    render_scratch: [[f32; FRAME]; 2],
    scratch: [f32; FRAME],
    learned: [f32; FRAME],
}

impl Processing {
    pub(crate) fn new() -> Self {
        Self {
            echo: Stage::new(1),
            gain: Stage::new(1),
            channels: Channels::Mono,
            learning_frames: LEARNING_FRAMES,
            render: [[0.0; FRAME]; 2],
            rendered: 0,
            render_scratch: [[0.0; FRAME]; 2],
            scratch: [0.0; FRAME],
            learned: [0.0; FRAME],
        }
    }

    /// Turns the stages on or off, as the settings choose them. The canceller enforces its
    /// high-pass filter, as in browsers, and finds out when there is no echo to cancel
    /// (headphones) with its model-based detector: the default one never lets go of a steady
    /// voice the speaker's sound overlaps. Gain control is digital only (the device's own input
    /// volume is the user's).
    pub(crate) fn configure(&mut self, echo: bool, gain: bool) {
        if !echo {
            self.take_over(None);
        }
        let stereo = matches!(self.channels, Channels::Stereo(_));
        if self.echo.set(echo, || canceller(stereo)) {
            self.rendered = 0;
        }
        self.gain.set(gain, || Config {
            gain_controller2: Some(GainController2 {
                input_volume_controller: false,
                adaptive_digital: Some(AdaptiveDigital::default()),
                ..Default::default()
            }),
            ..base()
        });
    }

    /// Share sound is playing here: from now until this voice session ends the canceller is
    /// stereo. Costs this thread one thread start, once. Voice alone never gets here.
    pub(crate) fn pin_stereo(&mut self) {
        if !matches!(self.channels, Channels::Mono) {
            return;
        }
        let (built, receiver) = sync_channel(1);
        let build = move || {
            let _ = built.send(Stage::stereo());
        };
        self.channels = match thread::Builder::new().name("gwar-stereo-canceller".into()).spawn(build) {
            Ok(_) => Channels::Building(receiver),
            Err(error) => {
                tracing::warn!("the stereo echo canceller did not start: {error}");
                Channels::Refused
            }
        };
    }

    /// Takes the built canceller if it has arrived. Never waits.
    fn collect_stereo(&mut self) {
        let Channels::Building(receiver) = &self.channels else { return };
        let Ok(stage) = receiver.try_recv() else { return };
        self.channels = Channels::Learning(Box::new(stage), self.learning_frames);
    }

    /// The stereo canceller replaces the mono one, at once if nothing is playing through it.
    /// `crossfade` is the frame it has just processed; `self.learned` holds its output there.
    fn take_over(&mut self, crossfade: Option<&mut [f32; FRAME]>) {
        if !matches!(self.channels, Channels::Learning(..)) {
            return;
        }
        let Channels::Learning(mut stage, _) = std::mem::replace(&mut self.channels, Channels::Refused) else { return };
        if let Some(frame) = crossfade {
            for (index, (sample, learned)) in frame.iter_mut().zip(&self.learned).enumerate() {
                let phase = (index + 1) as f32 / FRAME as f32 * std::f32::consts::FRAC_PI_2;
                *sample = *sample * phase.cos().powi(2) + learned * phase.sin().powi(2);
            }
        }
        std::mem::swap(&mut self.echo, &mut *stage);
        self.channels = Channels::Stereo(stage);
    }

    /// What the speakers will play, as it is queued for them: the echo's reference.
    pub(crate) fn render(&mut self, samples: &[[f32; 2]]) {
        if !self.echo.on {
            return;
        }
        for &[left, right] in samples {
            self.render[0][self.rendered] = left;
            self.render[1][self.rendered] = right;
            self.rendered += 1;
            if self.rendered == FRAME {
                self.rendered = 0;
                self.render_frame();
            }
        }
    }

    /// Hands one whole frame to the canceller. Errors are only for formats other than the ones built above.
    fn render_frame(&mut self) {
        let [left, right] = &self.render;
        let [left_out, right_out] = &mut self.render_scratch;
        let stereo = match &mut self.channels {
            Channels::Stereo(_) => Some(&mut self.echo),
            Channels::Learning(stage, left_to_hear) => {
                let power: f32 = left.iter().chain(right).map(|sample| sample * sample).sum();
                *left_to_hear -= usize::from(power >= PLAYING && *left_to_hear > 0);
                Some(&mut **stage)
            }
            _ => None,
        };
        if let Some(stage) = stereo {
            let _ = stage.apm.process_render_f32(&[left, right], &mut [&mut *left_out, &mut *right_out]);
        }
        if !matches!(self.channels, Channels::Stereo(_)) {
            let mid: [f32; FRAME] = std::array::from_fn(|index| 0.5 * (left[index] + right[index]));
            let _ = self.echo.apm.process_render_f32(&[&mid], &mut [left_out]);
        }
    }

    /// Takes the speaker out of the microphone's whole 10 ms frames, in place. `delay_ms` is how
    /// long the speaker's reference waits before it is played (the canceller finds the rest).
    pub(crate) fn cancel_echo(&mut self, frames: &mut [f32], delay_ms: u32) {
        if !self.echo.on {
            return;
        }
        self.collect_stereo();
        for frame in frames.as_chunks_mut::<FRAME>().0 {
            self.scratch.copy_from_slice(frame);
            let mut learned = false;
            if let Channels::Learning(stage, left) = &mut self.channels {
                let _ = stage.apm.set_stream_delay_ms(delay_ms.min(500) as i32);
                let _ = stage.apm.process_capture_f32(&[&self.scratch], &mut [&mut self.learned]);
                learned = *left == 0;
            }
            let _ = self.echo.apm.set_stream_delay_ms(delay_ms.min(500) as i32);
            let _ = self.echo.apm.process_capture_f32(&[&self.scratch], &mut [frame]);
            if learned {
                self.take_over(Some(frame));
            }
        }
    }

    /// Brings the voice in the microphone's whole 10 ms frames to a steady level, in place.
    pub(crate) fn control_gain(&mut self, frames: &mut [f32]) {
        if !self.gain.on {
            return;
        }
        for frame in frames.as_chunks_mut::<FRAME>().0 {
            self.scratch.copy_from_slice(frame);
            let _ = self.gain.apm.process_capture_f32(&[&self.scratch], &mut [frame]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: usize = super::super::io::RATE as usize;

    /// A voice-like signal: harmonics of a moving pitch, four syllables a second, at `dbfs`.
    fn voice(len: usize, pitch: f32, dbfs: f32) -> Vec<f32> {
        let mut phase = 0f32;
        let mut out: Vec<f32> = (0..len)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                let f0 = pitch * (1.0 + 0.15 * (std::f32::consts::TAU * 2.7 * t).sin());
                phase += std::f32::consts::TAU * f0 / RATE as f32;
                let harmonics: f32 =
                    (1..).map(|h| h as f32).take_while(|h| h * f0 < 5_000.0).map(|h| (h * phase).sin() / h).sum();
                harmonics * (std::f32::consts::PI * ((4.0 * t) % 1.0)).sin().powi(2)
            })
            .collect();
        let gain = 10f32.powf(dbfs / 20.0) / (out.iter().map(|x| x * x).sum::<f32>() / len as f32).sqrt();
        out.iter_mut().for_each(|x| *x *= gain);
        out
    }

    fn db(x: &[f32]) -> f32 {
        10.0 * (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32 + 1e-20).log10()
    }

    /// Runs `render` and `capture` through the stages, render first, a frame at a time, the way the
    /// pipeline does; the processed capture.
    fn run(processing: &mut Processing, render: &[f32], capture: &[f32]) -> Vec<f32> {
        let mut out = capture.to_vec();
        for (r, c) in render.as_chunks::<FRAME>().0.iter().zip(out.as_chunks_mut::<FRAME>().0) {
            processing.render(&r.map(|s| [s; 2]));
            processing.cancel_echo(c, 20);
            processing.control_gain(c);
        }
        out
    }

    #[test]
    fn cancels_the_speaker_in_the_microphone() {
        let len = 10 * RATE;
        let far = voice(len, 120.0, -20.0);
        // The room: 60 ms away, 10 dB down.
        let delay = 60 * RATE / 1000;
        let echo: Vec<f32> = (0..len).map(|i| if i >= delay { far[i - delay] * 0.316 } else { 0.0 }).collect();
        let mut processing = Processing::new();
        processing.configure(true, false);
        let out = run(&mut processing, &far, &echo);
        let tail = len - 4 * RATE..len;
        let reduced = db(&echo[tail.clone()]) - db(&out[tail]);
        assert!(reduced > 25.0, "echo down only {reduced:.1} dB");
    }

    #[test]
    fn leaves_the_voice_alone_with_nothing_played() {
        let len = 6 * RATE;
        let near = voice(len, 200.0, -22.0);
        let mut processing = Processing::new();
        processing.configure(true, false);
        let out = run(&mut processing, &vec![0.0; len], &near);
        let change = db(&out[RATE..]) - db(&near[RATE..]);
        assert!(change.abs() < 1.0, "voice changed by {change:.1} dB");
    }

    #[test]
    fn lets_a_voice_through_once_there_is_no_echo() {
        // Headphones: the speaker's sound never reaches the microphone. A tone is the hardest to
        // tell from an echo; the canceller takes the voice down hard while it starts.
        let len = 6 * RATE;
        let far: Vec<f32> =
            (0..len).map(|i| 0.3 * (std::f32::consts::TAU * 440.0 * i as f32 / RATE as f32).sin()).collect();
        let near = voice(len, 200.0, -14.0);
        let mut processing = Processing::new();
        processing.configure(true, false);
        let out = run(&mut processing, &far, &near);
        let change = db(&out[3 * RATE..]) - db(&near[3 * RATE..]);
        assert!(change.abs() < 1.5, "the voice changed by {change:.1} dB");
    }

    #[test]
    fn keeps_the_whole_band() {
        // 18 kHz: gone if the processing ran at 32 kHz.
        let tone: Vec<f32> =
            (0..2 * RATE).map(|i| 0.1 * (std::f32::consts::TAU * 18_000.0 * i as f32 / RATE as f32).sin()).collect();
        let mut processing = Processing::new();
        processing.configure(true, false);
        let out = run(&mut processing, &vec![0.0; tone.len()], &tone);
        let change = db(&out[RATE..]) - db(&tone[RATE..]);
        assert!(change.abs() < 1.0, "an 18 kHz tone changed by {change:.1} dB");
    }

    #[test]
    fn raises_a_quiet_voice() {
        let len = 10 * RATE;
        let quiet = voice(len, 140.0, -40.0);
        let mut processing = Processing::new();
        processing.configure(false, true);
        let out = run(&mut processing, &vec![0.0; len], &quiet);
        let level = db(&out[len - 4 * RATE..]);
        assert!(level > -30.0, "a -40 dBFS voice came out at {level:.1} dBFS");
    }

    #[test]
    fn passes_the_microphone_untouched_when_off() {
        let near = voice(RATE, 200.0, -22.0);
        let mut processing = Processing::new();
        processing.configure(true, true);
        processing.configure(false, false);
        assert_eq!(run(&mut processing, &vec![0.5; RATE], &near), near);
    }

    /// A small room: a direct echo `delay_ms` away, 10 dB down, and two reflections after it.
    fn room(far: &[f32], delay_ms: usize) -> Vec<f32> {
        let ms = RATE / 1000;
        let at = |i: usize, back: usize| if i >= back { far[i - back] } else { 0.0 };
        (0..far.len())
            .map(|i| {
                0.316 * at(i, delay_ms * ms) + 0.1 * at(i, (delay_ms + 15) * ms) + 0.05 * at(i, (delay_ms + 40) * ms)
            })
            .collect()
    }

    /// How much of `out` is `near`, in dB, at the best lag up to 25 ms, and `near`'s level change.
    fn fidelity(out: &[f32], near: &[f32]) -> (f32, f32) {
        let lag = (0..1200)
            .max_by_key(|lag| {
                let dot: f32 = near[..near.len() - 1200].iter().zip(&out[*lag..]).map(|(n, o)| n * o).sum();
                (dot * 1e3) as i64
            })
            .unwrap();
        let near = &near[..near.len() - 1200];
        let out = &out[lag..lag + near.len()];
        let scale = near.iter().zip(out).map(|(n, o)| n * o).sum::<f32>() / near.iter().map(|n| n * n).sum::<f32>();
        let error: Vec<f32> = near.iter().zip(out).map(|(n, o)| o - scale * n).collect();
        (db(near) + 20.0 * scale.abs().log10() - db(&error), 20.0 * scale.abs().log10())
    }

    impl Processing {
        /// As the pipeline would find it some frames later, for tests that count frames.
        fn built(&mut self) {
            if let Channels::Building(receiver) = &self.channels {
                let stage = receiver.recv().unwrap();
                self.channels = Channels::Learning(Box::new(stage), self.learning_frames);
            }
        }

        fn is_mono(&self) -> bool {
            matches!(self.channels, Channels::Mono)
        }

        fn is_stereo(&self) -> bool {
            matches!(self.channels, Channels::Stereo(_))
        }
    }

    fn pairs(signal: &[f32]) -> Vec<[f32; 2]> {
        signal.iter().map(|s| [*s; 2]).collect()
    }

    /// Far pairs and near samples through `processing`, pinning at sample `pin` if there is one.
    fn through(processing: &mut Processing, far: &[[f32; 2]], near: &[f32], pin: Option<usize>) -> Vec<f32> {
        let mut out = near.to_vec();
        let frames = far.as_chunks::<FRAME>().0.iter().zip(out.as_chunks_mut::<FRAME>().0);
        for (index, (far, frame)) in frames.enumerate() {
            if pin.is_some_and(|at| index * FRAME >= at) {
                processing.pin_stereo();
                processing.built();
            }
            processing.render(far);
            processing.cancel_echo(frame, 20);
        }
        out
    }

    /// The sample at which stereo took over, for `far` with a pin at sample `pin`.
    fn take_over_at(far: &[[f32; 2]], pin: usize, learning_frames: usize) -> usize {
        let mut processing = session(learning_frames);
        for at in (0..far.len()).step_by(FRAME) {
            through(&mut processing, &far[at..at + FRAME], &[0.0; FRAME], (at >= pin).then_some(0));
            if processing.is_stereo() {
                return at + FRAME;
            }
        }
        panic!("stereo never took over");
    }

    fn session(learning_frames: usize) -> Processing {
        let mut processing = Processing::new();
        processing.learning_frames = learning_frames;
        processing.configure(true, false);
        processing
    }

    /// Exactly what `origin/main` builds, from nothing of this file but `base`.
    fn mains_canceller() -> AudioProcessing {
        let stream = StreamConfig::new(super::super::io::RATE, 1);
        let mut apm = AudioProcessing::builder().config(base()).capture_config(stream).render_config(stream).build();
        let canceller = EchoCanceller { transparent_mode: TransparentModeType::Hmm, ..Default::default() };
        apm.apply_config(Config { echo_canceller: Some(canceller), ..base() });
        apm
    }

    #[test]
    fn voice_alone_is_mains_canceller_to_the_bit() {
        let len = 8 * RATE;
        let far = voice(len, 120.0, -20.0);
        let near: Vec<f32> =
            room(&far, 60).iter().zip(voice(len, 200.0, -24.0)).map(|(echo, voice)| echo + voice).collect();
        let mut mine = session(LEARNING_FRAMES);
        let out = through(&mut mine, &pairs(&far), &near, None);
        assert!(mine.is_mono(), "nothing here asked for stereo");
        let mut main = mains_canceller();
        let mut expected = near.clone();
        let mut scratch = [0f32; FRAME];
        for (far, near) in far.as_chunks::<FRAME>().0.iter().zip(expected.as_chunks_mut::<FRAME>().0) {
            let _ = main.process_render_f32(&[far], &mut [&mut scratch]);
            let _ = main.set_stream_delay_ms(20);
            let input = *near;
            let _ = main.process_capture_f32(&[&input], &mut [near]);
        }
        assert!(out.iter().zip(&expected).all(|(a, b)| a.to_bits() == b.to_bits()), "voice alone changed");
    }

    #[test]
    fn silence_teaches_the_stereo_canceller_nothing() {
        let mut processing = session(50);
        let silent = vec![0.0; 5 * RATE];
        through(&mut processing, &pairs(&silent), &silent, Some(0));
        assert!(!processing.is_stereo());
        through(&mut processing, &pairs(&voice(3 * RATE, 120.0, -20.0)), &silent[..3 * RATE], None);
        assert!(processing.is_stereo());
    }

    #[test]
    fn share_sound_pins_stereo_for_the_rest_of_the_session() {
        let len = 3 * RATE;
        let far = pairs(&voice(len, 120.0, -20.0));
        let silent = vec![0.0; len];
        let mut processing = session(50);
        through(&mut processing, &far, &silent, Some(RATE));
        assert!(processing.is_stereo(), "pinned once it has learned");
        // The share has ended: voice alone, for as long as you like, and the pin stays.
        processing.pin_stereo();
        through(&mut processing, &far, &silent, None);
        assert!(processing.is_stereo());
        assert!(session(50).is_mono(), "a new voice session starts on one channel");
    }

    #[test]
    fn voice_stays_on_the_mono_canceller_while_stereo_learns() {
        let len = 4 * RATE;
        let far = pairs(&voice(len, 120.0, -20.0));
        let left: Vec<f32> = far.iter().map(|pair| pair[0]).collect();
        let near = room(&left, 60);
        let alone = through(&mut session(100), &far, &near, None);
        let learning = through(&mut session(100), &far, &near, Some(2 * RATE));
        let take_over = take_over_at(&far, 2 * RATE, 100);
        assert!(take_over > 3 * RATE, "a frame of sound at a time");
        assert_eq!(alone[..take_over - FRAME], learning[..take_over - FRAME]);
    }

    #[test]
    fn the_switch_has_no_click_and_no_gap() {
        let len = 6 * RATE;
        let far = pairs(&voice(len, 120.0, -20.0));
        let left: Vec<f32> = far.iter().map(|pair| pair[0]).collect();
        let near_voice = voice(len, 200.0, -24.0);
        let near: Vec<f32> = room(&left, 60).iter().zip(&near_voice).map(|(echo, voice)| echo + voice).collect();
        let alone = through(&mut session(100), &far, &near, None);
        let switched = through(&mut session(100), &far, &near, Some(2 * RATE));
        let take_over = take_over_at(&far, 2 * RATE, 100);
        let around = take_over - 4 * FRAME..take_over + 4 * FRAME;
        let step = |signal: &[f32]| signal.windows(2).map(|pair| (pair[1] - pair[0]).abs()).fold(0.0, f32::max);
        assert_eq!(switched.len(), alone.len());
        let steady = step(&alone[RATE..take_over]);
        assert!(step(&switched[around.clone()]) <= steady, "a click at the switch");
        let change = db(&switched[around.clone()]) - db(&alone[around]);
        assert!(change.abs() < 3.0, "voice jumped {change:.1} dB at the switch");
    }

    #[test]
    fn the_switch_keeps_the_voice_in_time() {
        // Headphones: sound plays and none of it comes back, so the two cancellers should agree.
        let len = 10 * RATE;
        let far = pairs(&voice(len, 120.0, -20.0));
        let near = voice(len, 200.0, -24.0);
        let alone = through(&mut session(100), &far, &near, None);
        let switched = through(&mut session(100), &far, &near, Some(2 * RATE));
        let take_over = take_over_at(&far, 2 * RATE, 100);
        let after = take_over..take_over + 2 * RATE;
        let lag = |out: &[f32]| {
            let chunk = &near[after.clone()];
            (0..1200usize)
                .max_by_key(|lag| {
                    let dot: f32 =
                        chunk[..chunk.len() - 1200].iter().zip(&out[after.start + lag..]).map(|(n, o)| n * o).sum();
                    (dot * 1e3) as i64
                })
                .unwrap()
        };
        assert_eq!(lag(&alone), lag(&switched), "both cancellers delay the voice alike");
    }

    #[test]
    fn a_canceller_turned_off_while_learning_comes_back_stereo() {
        let len = 6 * RATE;
        let far = pairs(&voice(len, 120.0, -20.0));
        let left: Vec<f32> = far.iter().map(|pair| pair[0]).collect();
        let echo = room(&left, 60);
        let mut processing = session(100);
        through(&mut processing, &far[..RATE], &echo[..RATE], Some(RATE / 2));
        processing.configure(false, false);
        assert!(processing.is_stereo());
        let off = through(&mut processing, &far[..RATE], &echo[..RATE], None);
        assert_eq!(off, echo[..RATE], "off passes the microphone untouched");
        processing.configure(true, false);
        let out = through(&mut processing, &far, &echo, None);
        assert!(db(&out[len - RATE..]) < db(&echo[len - RATE..]) - 20.0);
    }

    fn wide(signal: &[f32]) -> Vec<[f32; 2]> {
        signal.iter().map(|s| [*s, -*s]).collect()
    }

    /// What is left in dBFS when only the left speaker reaches the microphone, in half seconds.
    fn residual(far: &[[f32; 2]], pin: usize, windows: &[(usize, usize)]) -> Vec<f32> {
        let left: Vec<f32> = far.iter().map(|pair| pair[0]).collect();
        let echo = room(&left, 60);
        let out = through(&mut session(LEARNING_FRAMES), far, &echo, Some(pin));
        windows.iter().map(|(from, to)| db(&out[from * RATE / 2..to * RATE / 2])).collect()
    }

    #[test]
    fn stereo_bursts_never_leak_once_pinned() {
        // One second of each never reaches the detector's two seconds, so it would never adapt.
        let mut far = wide(&voice(40 * RATE, 150.0, -18.0));
        for block in far.chunks_mut(RATE).skip(1).step_by(2) {
            block.fill([0.0; 2]);
        }
        let on: Vec<(usize, usize)> = (15..20).map(|s| (4 * s, 4 * s + 2)).collect();
        let bursts = residual(&far, 0, &on);
        assert!(bursts.iter().all(|level| *level < -60.0), "{bursts:?}");
    }

    #[test]
    fn wide_sound_after_voice_is_cancelled_from_when_it_is_learned() {
        let sound = voice(24 * RATE, 150.0, -18.0);
        let mut far = pairs(&sound[..12 * RATE]);
        far.extend(wide(&sound[12 * RATE..]));
        let after = residual(&far, 12 * RATE, &[(36, 40), (40, 48)]);
        assert!(after.iter().all(|level| *level < -45.0), "{after:?}");
    }

    /// The numbers behind [`LEARNING_FRAMES`]: voice through the switch against staying mono with
    /// no share at all. `echo`: what is left of a far voice. `talk`: all of the output while both
    /// talk. `share`: how much of the near voice is left beside wide share sound, from the switch.
    #[test]
    #[ignore = "a comparison, run in release"]
    fn the_switch_against_staying_mono() {
        let second = RATE;
        let pin = 6 * second;
        let total = 30 * second;
        let windows = [(0, 1), (1, 2), (2, 3), (3, 4), (4, 6), (6, 8), (8, 12), (32, 48)];
        let span = |at: (usize, usize)| pin + at.0 * second / 2..pin + at.1 * second / 2;
        for learning in [LEARNING_FRAMES] {
            let mut rows = vec![[(0f32, 0f32, 0f32); 8]; 3];
            let mut count = 0.0;
            for far_pitch in [100.0, 150.0, 230.0] {
                for delay in [20, 60, 150] {
                    for near_pitch in [180.0, 280.0] {
                        count += 1.0;
                        let far = voice(total, far_pitch, -20.0);
                        let echo = room(&far, delay);
                        let near = voice(total, near_pitch, -24.0);
                        let talk: Vec<f32> = echo.iter().zip(&near).map(|(e, n)| e + n).collect();
                        let same = pairs(&far);
                        let hum = voice(total, 170.0, -24.0);
                        let shared: Vec<[f32; 2]> = far
                            .iter()
                            .zip(&hum)
                            .enumerate()
                            .map(|(index, (f, h))| if index < pin { [*f; 2] } else { [f + h, f - h] })
                            .collect();
                        let left: Vec<f32> = shared.iter().map(|pair| pair[0]).collect();
                        let heard: Vec<f32> = room(&left, delay).iter().zip(&near).map(|(e, n)| e + n).collect();
                        let stay = through(&mut session(learning), &same, &echo, None);
                        let moved = through(&mut session(learning), &same, &echo, Some(pin));
                        let stay_talk = through(&mut session(learning), &same, &talk, None);
                        let moved_talk = through(&mut session(learning), &same, &talk, Some(pin));
                        let moved_share = through(&mut session(learning), &shared, &heard, Some(pin));
                        for (index, window) in windows.iter().enumerate() {
                            let r = span(*window);
                            let mut add = |row: usize, a: f32, b: f32| {
                                rows[row][index].0 += a;
                                rows[row][index].1 += b;
                                rows[row][index].2 = rows[row][index].2.min(a - b);
                            };
                            add(0, db(&stay[r.clone()]), db(&moved[r.clone()]));
                            add(1, db(&stay_talk[r.clone()]), db(&moved_talk[r.clone()]));
                            let kept = |out: &[f32]| fidelity(&out[r.clone()], &near[r.clone()]).1;
                            add(2, kept(&stay_talk), kept(&moved_share));
                        }
                    }
                }
            }
            for (row, name) in rows.iter().zip(["echo", "talk", "share"]) {
                let cells: Vec<String> = row
                    .iter()
                    .map(|(stay, moved, worst)| format!("{:.1}/{:.1}({:.1})", stay / count, moved / count, worst))
                    .collect();
                eprintln!("learning {learning} {name}: {}", cells.join(" "));
            }
        }
    }

    /// Headphones: nothing comes back, and voice must not pay for a canceller it does not need.
    #[test]
    #[ignore = "a comparison, run in release"]
    fn the_switch_on_headphones() {
        let second = RATE;
        let pin = 6 * second;
        let total = 24 * second;
        for learning in [300, 400, 600, 800] {
            let mut cells = Vec::new();
            for (far_pitch, near_pitch, far_db) in [(120.0, 200.0, -20.0), (230.0, 150.0, -20.0), (150.0, 280.0, -14.0)]
            {
                let far = pairs(&voice(total, far_pitch, far_db));
                let near = voice(total, near_pitch, -24.0);
                let stay = through(&mut session(learning), &far, &near, None);
                let moved = through(&mut session(learning), &far, &near, Some(pin));
                let row: Vec<String> = [(0, 2), (2, 4), (4, 6), (6, 8), (8, 10), (10, 14), (14, 18)]
                    .iter()
                    .map(|(from, to)| {
                        let r = pin + from * second..pin + to * second;
                        format!("{:.1}/{:.1}", db(&stay[r.clone()]), db(&moved[r]))
                    })
                    .collect();
                cells.push(row.join(" "));
            }
            eprintln!("headphones learning {learning}: {}", cells.join(" | "));
        }
    }

    /// Wide sound from the moment of the pin, against how long the stereo canceller learned.
    #[test]
    #[ignore = "a comparison, run in release"]
    fn wide_sound_through_the_switch() {
        let second = RATE;
        for pitch in [120.0, 190.0] {
            let sound = voice(24 * second, pitch, -18.0);
            let mut far = pairs(&sound[..12 * second]);
            far.extend(wide(&sound[12 * second..]));
            let left: Vec<f32> = far.iter().map(|pair| pair[0]).collect();
            let echo = room(&left, 60);
            for learning in [100, 300] {
                let out = through(&mut session(learning), &far, &echo, Some(12 * second));
                let at =
                    |from: usize, to: usize| db(&out[12 * second + from * second / 2..12 * second + to * second / 2]);
                eprintln!(
                    "wide {pitch} learning {learning}: 0-.5 s {:.1}, .5-1 {:.1}, 1-1.5 {:.1}, 1.5-2 {:.1}, 2-3 {:.1}, 3-4 {:.1}, 8-12 {:.1}",
                    at(0, 1),
                    at(1, 2),
                    at(2, 3),
                    at(3, 4),
                    at(4, 6),
                    at(6, 8),
                    at(16, 24)
                );
            }
        }
    }

    /// The case that kept the owner's caveat: the longest stretch of voice before the first share.
    /// Sound at another pitch still takes the canceller a moment, from the pin or from long before.
    #[test]
    fn wide_sound_after_five_minutes_of_voice() {
        let second = RATE;
        let wide_part = wide(&voice(20 * second, 120.0, -18.0));
        let mut far = pairs(&voice(310 * second, 150.0, -18.0));
        far.extend_from_slice(&wide_part);
        let back = 2 * 310;
        let steps = [
            (back - 10, back),
            (back, back + 1),
            (back + 1, back + 2),
            (back + 2, back + 8),
            (back + 8, back + 16),
            (back + 20, back + 40),
        ];
        let at_first_sound = residual(&far, 310 * second, &steps);
        let from_the_start = residual(&far, 0, &steps);
        eprintln!("pinned at the first share: {at_first_sound:?}; pinned long before: {from_the_start:?}");
        assert!(at_first_sound[5] < -60.0, "it settles once pinned: {at_first_sound:?}");
        assert!(at_first_sound[1] > -40.0, "while the mono canceller still holds: {at_first_sound:?}");
        assert!(from_the_start[1] > -40.0, "the caveat is real either way: {from_the_start:?}");
        assert!(from_the_start[3] < -60.0, "{from_the_start:?}");
    }

    /// One number for all of voice alone, to hold against `origin/main` built without any of this.
    #[test]
    #[ignore = "printed by hand beside the same test on origin/main"]
    fn voice_alone_fingerprint() {
        let mut hash = 0xcbf29ce484222325u64;
        for (far_pitch, delay) in [(100.0, 20), (150.0, 60), (230.0, 150)] {
            let len = 20 * RATE;
            let far = voice(len, far_pitch, -20.0);
            let near: Vec<f32> = room(&far, delay).iter().zip(voice(len, 200.0, -24.0)).map(|(e, n)| e + n).collect();
            let mut processing = Processing::new();
            processing.configure(true, true);
            for sample in run(&mut processing, &far, &near) {
                hash = (hash ^ u64::from(sample.to_bits())).wrapping_mul(0x100000001b3);
            }
        }
        eprintln!("voice alone fingerprint {hash:016x}");
    }

    /// One arm's canceller time per 10 ms frame, render and capture together, past its first 3 s.
    fn frame_cost(mut frame: impl FnMut(usize)) -> (std::time::Duration, std::time::Duration) {
        let mut spent: Vec<_> = (0..3_000)
            .map(|index| {
                let started = std::time::Instant::now();
                frame(index);
                started.elapsed()
            })
            .skip(300)
            .collect();
        spent.sort();
        (spent[spent.len() / 2], spent[spent.len() * 99 / 100])
    }

    /// The price of the stereo reference: `cargo test --release ... stereo_reference_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "a timing measurement, run in release"]
    fn stereo_reference_cost() {
        let len = 30 * RATE;
        let left = voice(len, 120.0, -20.0);
        let right = voice(len, 190.0, -20.0);
        let echo: Vec<f32> = left.iter().map(|sample| sample * 0.3).collect();
        let block = |signal: &[f32], index: usize| -> [f32; FRAME] {
            signal[index * FRAME..(index + 1) * FRAME].try_into().unwrap()
        };
        let mut mono = mains_canceller();
        let (mut far_out, mut near_out) = ([0f32; FRAME], [0f32; FRAME]);
        let one = frame_cost(|index| {
            let far = block(&left, index);
            let near = block(&echo, index);
            let _ = mono.process_render_f32(&[&far], &mut [&mut far_out]);
            let _ = mono.set_stream_delay_ms(20);
            let _ = mono.process_capture_f32(&[&near], &mut [&mut near_out]);
        });
        let arm = |right: &[f32], pin: bool| {
            let mut processing = session(0);
            if pin {
                processing.pin_stereo();
                processing.built();
            }
            frame_cost(|index| {
                let (far_left, far_right) = (block(&left, index), block(right, index));
                let pairs: [[f32; 2]; FRAME] = std::array::from_fn(|i| [far_left[i], far_right[i]]);
                let mut near = block(&echo, index);
                processing.render(&pairs);
                processing.cancel_echo(&mut near, 20);
            })
        };
        let same = arm(&left, false);
        let different = arm(&right, true);
        let started = std::time::Instant::now();
        let (built, receiver) = sync_channel(1);
        thread::spawn(move || built.send(()));
        let spawned = started.elapsed();
        receiver.recv().unwrap();
        let started = std::time::Instant::now();
        drop(Stage::stereo());
        eprintln!("thread start {spawned:?}, canceller built on that thread in {:?}", started.elapsed());
        let mut processing = session(0);
        processing.pin_stereo();
        processing.built();
        let (mut slowest, mut near) = (std::time::Duration::ZERO, block(&echo, 0));
        for _ in 0..3 {
            let started = std::time::Instant::now();
            processing.render(&[[0.1; 2]; FRAME]);
            processing.cancel_echo(&mut near, 20);
            slowest = slowest.max(started.elapsed());
        }
        eprintln!("slowest of the three frames through the switch: {slowest:?}");
        let budget = std::time::Duration::from_millis(10);
        for (name, (median, p99)) in [("main", one), ("voice alone", same), ("pinned, share sound", different)] {
            let share = 100.0 * median.as_secs_f64() / budget.as_secs_f64();
            eprintln!("{name}: median {median:?} ({share:.2}% of a core), p99 {p99:?}");
        }
    }
}
