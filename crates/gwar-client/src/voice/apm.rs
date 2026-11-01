//! Echo cancellation and automatic gain control of the microphone: WebRTC's audio processing
//! (AEC3, AGC2), the one browsers run on `getUserMedia`, through `sonora`, its port to Rust.
//!
//! Two stages, so the voice gate can measure between them: the canceller, then gain control. The
//! canceller gets what the speaker plays as the pipeline queues it, ahead of the device, and finds
//! the echo's delay itself. It adds ~9 ms to the microphone (AEC3 works in 4 ms blocks and overlaps
//! them, after splitting 48 kHz into three bands); gain control adds nothing. A stage turned off
//! passes the microphone untouched.

use sonora::{
    AudioProcessing, Config, StreamConfig,
    config::{AdaptiveDigital, EchoCanceller, GainController2, MaxProcessingRate, Pipeline, TransparentModeType},
};

use super::FRAME;

/// Nothing on, at the full 48 kHz: by default the processing runs at 32 kHz, which would take
/// everything above 16 kHz out of the microphone.
fn base() -> Config {
    Config {
        pipeline: Pipeline { maximum_internal_processing_rate: MaxProcessingRate::Rate48kHz, ..Default::default() },
        ..Default::default()
    }
}

/// One of WebRTC's processors, set up for a single stage (or none, while it is off).
struct Stage {
    apm: AudioProcessing,
    on: bool,
}

impl Stage {
    fn new() -> Self {
        // Two render channels cost little while they match: the canceller's own detector keeps
        // it on one channel until the speakers really differ, which only share sound makes them.
        Self {
            apm: AudioProcessing::builder()
                .config(base())
                .capture_config(StreamConfig::new(super::io::RATE, 1))
                .render_config(StreamConfig::new(super::io::RATE, 2))
                .build(),
            on: false,
        }
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

pub(crate) struct Processing {
    echo: Stage,
    gain: Stage,
    /// What each speaker was given since the last whole frame went to the canceller.
    render: [[f32; FRAME]; 2],
    rendered: usize,
    render_scratch: [[f32; FRAME]; 2],
    scratch: [f32; FRAME],
}

impl Processing {
    pub(crate) fn new() -> Self {
        Self {
            echo: Stage::new(),
            gain: Stage::new(),
            render: [[0.0; FRAME]; 2],
            rendered: 0,
            render_scratch: [[0.0; FRAME]; 2],
            scratch: [0.0; FRAME],
        }
    }

    /// Turns the stages on or off, as the settings choose them. The canceller enforces its
    /// high-pass filter, as in browsers, and finds out when there is no echo to cancel
    /// (headphones) with its model-based detector: the default one never lets go of a steady
    /// voice the speaker's sound overlaps. Gain control is digital only (the device's own input
    /// volume is the user's).
    pub(crate) fn configure(&mut self, echo: bool, gain: bool) {
        let canceller = EchoCanceller { transparent_mode: TransparentModeType::Hmm, ..Default::default() };
        if self.echo.set(echo, || Config { echo_canceller: Some(canceller), ..base() }) {
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
                let [left, right] = &self.render;
                let [left_out, right_out] = &mut self.render_scratch;
                // Errors are only for formats other than the one built above.
                let _ = self.echo.apm.process_render_f32(&[left, right], &mut [left_out, right_out]);
            }
        }
    }

    /// Takes the speaker out of the microphone's whole 10 ms frames, in place. `delay_ms` is how
    /// long the speaker's reference waits before it is played (the canceller finds the rest).
    pub(crate) fn cancel_echo(&mut self, frames: &mut [f32], delay_ms: u32) {
        if !self.echo.on {
            return;
        }
        for frame in frames.as_chunks_mut::<FRAME>().0 {
            let _ = self.echo.apm.set_stream_delay_ms(delay_ms.min(500) as i32);
            self.scratch.copy_from_slice(frame);
            let _ = self.echo.apm.process_capture_f32(&[&self.scratch], &mut [frame]);
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
        // What voice alone costs before this change: one channel, all the way through.
        let stream = StreamConfig::new(super::super::io::RATE, 1);
        let mut mono = AudioProcessing::builder().config(base()).capture_config(stream).render_config(stream).build();
        let canceller = EchoCanceller { transparent_mode: TransparentModeType::Hmm, ..Default::default() };
        mono.apply_config(Config { echo_canceller: Some(canceller), ..base() });
        let (mut far_out, mut near_out) = ([0f32; FRAME], [0f32; FRAME]);
        let one = frame_cost(|index| {
            let far = block(&left, index);
            let near = block(&echo, index);
            let _ = mono.process_render_f32(&[&far], &mut [&mut far_out]);
            let _ = mono.set_stream_delay_ms(20);
            let _ = mono.process_capture_f32(&[&near], &mut [&mut near_out]);
        });
        let arm = |right: &[f32]| {
            let mut processing = Processing::new();
            processing.configure(true, false);
            frame_cost(|index| {
                let (far_left, far_right) = (block(&left, index), block(right, index));
                let pairs: [[f32; 2]; FRAME] = std::array::from_fn(|i| [far_left[i], far_right[i]]);
                let mut near = block(&echo, index);
                processing.render(&pairs);
                processing.cancel_echo(&mut near, 20);
            })
        };
        let same = arm(&left);
        let different = arm(&right);
        let budget = std::time::Duration::from_millis(10);
        for (name, (median, p99)) in [("one channel", one), ("voice alone", same), ("share sound", different)] {
            let share = 100.0 * median.as_secs_f64() / budget.as_secs_f64();
            eprintln!("{name}: median {median:?} ({share:.2}% of a core), p99 {p99:?}");
        }
    }
}
