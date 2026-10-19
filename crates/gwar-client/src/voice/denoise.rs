//! Background-noise suppression on the capture path (docs/latency.md, "Noise suppression"): every
//! 10 ms frame goes through it before the encoder; the level meter and the voice-activity gate
//! still measure the microphone as captured. It runs on the device, before encryption.
//!
//! Both modes work on 48 kHz frames of exactly our 480 samples and add one frame (10 ms) of
//! delay, their overlap-add; neither looks ahead. Measured on speech with noise at 5 dB SNR
//! (docs/latency.md, "Noise suppression"):
//!
//! - `Strong`: DeepFilterNet3's low-latency model (no lookahead), run with tract. Much cleaner:
//!   +13 dB SI-SDR in white noise, +10 dB for a fan, +12 dB for typing; ~0.5 ms of CPU per frame.
//!   Its model takes ~0.3 s to load, on a thread of its own; until then `Strong` runs as `Light`.
//! - `Light`: RNNoise (`nnnoiseless`, pure Rust): +9, +6 and +5 dB; 0.04 ms per frame.

use nnnoiseless::DenoiseState;

use super::FRAME;

/// How much background noise to take out. Off unless the app chooses (the desktop app's default
/// is `Strong`): tests and tools that send tones through the pipeline get them through untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NoiseSuppression {
    #[default]
    Off,
    /// RNNoise.
    Light,
    /// DeepFilterNet3 (low latency); RNNoise where it is not built in or not loaded yet.
    Strong,
}

impl NoiseSuppression {
    pub(crate) fn to_u8(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Light => 1,
            Self::Strong => 2,
        }
    }

    pub(crate) fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Light,
            2 => Self::Strong,
            _ => Self::Off,
        }
    }
}

/// DeepFilterNet3's low-latency model on a worker thread of the pipeline's own: tract's model is
/// tied to the thread that built it, and building it takes ~0.3 s, which the pipeline must not
/// wait for. The pipeline hands it one frame at a time and waits for that frame (~0.5 ms).
#[cfg(feature = "deep-filter")]
mod deep {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel},
        },
        time::Duration,
    };

    use df::tract::{DfParams, DfTract, RuntimeParams};

    use super::FRAME;

    /// Longer than any frame takes (a few ms at worst): past it the worker counts as gone.
    const PATIENCE: Duration = Duration::from_millis(40);

    struct Job {
        frame: Box<[f32; FRAME]>,
        /// Start over from the untouched model first (a gap in the capture).
        reset: bool,
    }

    pub(super) struct Worker {
        jobs: SyncSender<Job>,
        done: Receiver<Box<[f32; FRAME]>>,
        ready: Arc<AtomicBool>,
        /// A frame buffer that travels to the worker and back, so nothing is allocated per frame.
        spare: Option<Box<[f32; FRAME]>>,
        /// The next frame starts from the untouched model.
        reset: bool,
        broken: bool,
    }

    impl Worker {
        pub(super) fn start() -> Option<Self> {
            let (jobs, job_rx) = sync_channel::<Job>(1);
            let (done_tx, done) = sync_channel(1);
            let ready = Arc::new(AtomicBool::new(false));
            let ready_flag = ready.clone();
            std::thread::Builder::new()
                .name("gwar-denoise".into())
                .spawn(move || {
                    let base = match DfTract::new(DfParams::default(), &RuntimeParams::default_with_ch(1)) {
                        Ok(model) if model.hop_size == FRAME => model,
                        Ok(model) => {
                            return tracing::warn!(
                                hop = model.hop_size,
                                "DeepFilterNet hop is not 10 ms; using RNNoise"
                            );
                        }
                        Err(e) => return tracing::warn!("DeepFilterNet unavailable, using RNNoise: {e:#}"),
                    };
                    // Frames it holds hold up the pipeline too.
                    let _priority = super::super::priority::Priority::promote();
                    let mut live = base.clone();
                    let mut noisy = ndarray::Array2::<f32>::zeros((1, FRAME));
                    let mut clean = ndarray::Array2::<f32>::zeros((1, FRAME));
                    ready_flag.store(true, Ordering::Release);
                    while let Ok(Job { mut frame, reset }) = job_rx.recv() {
                        if reset {
                            live = base.clone();
                        }
                        noisy.row_mut(0).iter_mut().zip(frame.iter()).for_each(|(n, s)| *n = *s);
                        if let Err(e) = live.process(noisy.view(), clean.view_mut()) {
                            tracing::warn!("DeepFilterNet failed, using RNNoise: {e:#}");
                            return;
                        }
                        frame.iter_mut().zip(clean.row(0).iter()).for_each(|(s, c)| *s = c.clamp(-1.0, 1.0));
                        if done_tx.send(frame).is_err() {
                            return;
                        }
                    }
                })
                .ok()?;
            Some(Self { jobs, done, ready, spare: Some(Box::new([0.0; FRAME])), reset: false, broken: false })
        }

        /// A stand-in worker: hands the first `answers` frames back unchanged, then takes frames
        /// and never answers (stuck).
        #[cfg(test)]
        pub(super) fn fake(answers: usize) -> Self {
            let (jobs, job_rx) = sync_channel::<Job>(1);
            let (done_tx, done) = sync_channel(1);
            std::thread::spawn(move || {
                let mut n = 0;
                while let Ok(Job { frame, .. }) = job_rx.recv() {
                    n += 1;
                    if n <= answers && done_tx.send(frame).is_err() {
                        return;
                    }
                }
            });
            Self {
                jobs,
                done,
                ready: Arc::new(AtomicBool::new(true)),
                spare: Some(Box::new([0.0; FRAME])),
                reset: false,
                broken: false,
            }
        }

        pub(super) fn ready(&self) -> bool {
            !self.broken && self.ready.load(Ordering::Acquire)
        }

        pub(super) fn reset(&mut self) {
            self.reset = true;
        }

        /// Cleans `chunk` in place; false (and `chunk` untouched) once the worker is gone. Waits
        /// [`PATIENCE`] at most: the hand-over never blocks (the worker has taken every earlier
        /// frame before answering it, so its queue is empty unless it is gone or stuck).
        pub(super) fn process(&mut self, chunk: &mut [f32; FRAME]) -> bool {
            let Some(mut frame) = self.spare.take() else {
                self.broken = true;
                return false;
            };
            frame.copy_from_slice(chunk);
            match self.jobs.try_send(Job { frame, reset: std::mem::take(&mut self.reset) }) {
                Ok(()) => {}
                Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                    self.broken = true;
                    return false;
                }
            }
            match self.done.recv_timeout(PATIENCE) {
                Ok(frame) => {
                    chunk.copy_from_slice(&frame[..]);
                    self.spare = Some(frame);
                    true
                }
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                    // A late answer would belong to this frame and arrive for the next: never trust it.
                    tracing::warn!("DeepFilterNet did not answer in time; using RNNoise");
                    self.broken = true;
                    false
                }
            }
        }
    }
}

/// RNNoise takes 16-bit sample values as floats.
const PCM16: f32 = 32768.0;

/// The suppressor of one capture pipeline: its state follows the audio, so it starts over after a
/// gap in the capture and when the mode changes (the old state would smear one into the other).
pub(crate) struct Denoiser {
    /// The engine the state below belongs to.
    running: NoiseSuppression,
    rnnoise: Box<DenoiseState<'static>>,
    input: [f32; FRAME],
    output: [f32; FRAME],
    /// Started the first time `Strong` is asked for; `None` after that if it could not start or failed.
    #[cfg(feature = "deep-filter")]
    deep: Option<deep::Worker>,
    #[cfg(feature = "deep-filter")]
    deep_started: bool,
}

impl Denoiser {
    pub(crate) fn new() -> Self {
        // Allocated here, on the pipeline's start; `process` allocates only when starting over.
        Self {
            running: NoiseSuppression::Off,
            rnnoise: DenoiseState::new(),
            input: [0.0; FRAME],
            output: [0.0; FRAME],
            #[cfg(feature = "deep-filter")]
            deep: None,
            #[cfg(feature = "deep-filter")]
            deep_started: false,
        }
    }

    /// The engine `mode` runs on now: `Strong` is `Light` until DeepFilterNet is there.
    fn engine(&mut self, mode: NoiseSuppression) -> NoiseSuppression {
        #[cfg(feature = "deep-filter")]
        if mode == NoiseSuppression::Strong {
            if !self.deep_started {
                // Loads in the background; RNNoise stands in for the ~0.3 s it takes.
                self.deep_started = true;
                self.deep = deep::Worker::start();
            }
            if self.deep.as_ref().is_some_and(deep::Worker::ready) {
                return NoiseSuppression::Strong;
            }
        }
        if mode == NoiseSuppression::Off { NoiseSuppression::Off } else { NoiseSuppression::Light }
    }

    /// Cleans `frame` (any whole number of 10 ms frames) in place. `continues` is false when the
    /// capture skipped samples since the previous call.
    pub(crate) fn process(&mut self, frame: &mut [f32], mode: NoiseSuppression, continues: bool) {
        let engine = self.engine(mode);
        if engine != self.running || !continues {
            // A fresh state: the old one would smear the earlier audio into this.
            match engine {
                NoiseSuppression::Off => {}
                NoiseSuppression::Light => self.rnnoise = DenoiseState::new(),
                NoiseSuppression::Strong =>
                {
                    #[cfg(feature = "deep-filter")]
                    if let Some(d) = self.deep.as_mut() {
                        d.reset();
                    }
                }
            }
            self.running = engine;
        }
        match engine {
            NoiseSuppression::Off => {}
            NoiseSuppression::Light => {
                for chunk in frame.as_chunks_mut::<FRAME>().0 {
                    for (i, s) in chunk.iter().enumerate() {
                        self.input[i] = s * PCM16;
                    }
                    self.rnnoise.process_frame(&mut self.output, &self.input);
                    for (s, o) in chunk.iter_mut().zip(self.output.iter()) {
                        *s = (o / PCM16).clamp(-1.0, 1.0);
                    }
                }
            }
            NoiseSuppression::Strong => {
                #[cfg(feature = "deep-filter")]
                if let Some(d) = self.deep.as_mut() {
                    let chunks = frame.as_chunks_mut::<FRAME>().0;
                    for i in 0..chunks.len() {
                        if !d.process(&mut chunks[i]) {
                            // Not again for this pipeline: RNNoise from here on, from a fresh state,
                            // for this chunk (left as captured) and the rest; earlier ones are done.
                            self.deep = None;
                            self.running = NoiseSuppression::Off;
                            return self.process(chunks[i..].as_flattened_mut(), NoiseSuppression::Light, false);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic white noise, about -30 dBFS.
    fn noise(seed: &mut u32) -> f32 {
        *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((*seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.1
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    /// A denoiser whose `Strong` engine is DeepFilterNet (when built in), not the RNNoise stand-in.
    fn loaded() -> Denoiser {
        #[allow(unused_mut)]
        let mut d = Denoiser::new();
        #[cfg(feature = "deep-filter")]
        {
            d.process(&mut [0.0; FRAME], NoiseSuppression::Strong, true);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            while !d.deep.as_ref().is_some_and(deep::Worker::ready) {
                assert!(std::time::Instant::now() < deadline, "DeepFilterNet did not load");
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        d
    }

    /// The level steady white noise comes out at, relative to what went in (dB).
    fn noise_left(mode: NoiseSuppression) -> f32 {
        let mut seed = 7;
        let mut d = loaded();
        let mut last = [0f32; FRAME];
        for _ in 0..200 {
            last = std::array::from_fn(|_| noise(&mut seed));
            d.process(&mut last, mode, true);
        }
        let before = rms(&std::array::from_fn::<f32, FRAME, _>(|_| noise(&mut seed)));
        20.0 * (rms(&last).max(1e-9) / before).log10()
    }

    #[test]
    fn steady_noise_is_taken_down_and_off_leaves_the_audio_alone() {
        // RNNoise (2017 weights) takes steady white noise down by about 10 dB.
        assert!(noise_left(NoiseSuppression::Light) < -6.0);
        let mut seed = 7;
        let mut d = Denoiser::new();
        let mut frame: [f32; FRAME] = std::array::from_fn(|_| noise(&mut seed));
        let original = frame;
        d.process(&mut frame, NoiseSuppression::Off, true);
        assert_eq!(frame, original);
    }

    #[cfg(feature = "deep-filter")]
    #[test]
    fn strong_takes_out_far_more_than_light() {
        let (light, strong) = (noise_left(NoiseSuppression::Light), noise_left(NoiseSuppression::Strong));
        assert!(strong < -20.0 && strong < light - 10.0, "strong {strong} dB, light {light} dB");
    }

    /// A vowel-like sound from `start` on: a glottal pulse train with vibrato through two formant
    /// resonators, in syllables (a steady tone is what a neural suppressor rightly takes for noise).
    fn vowels(len: usize, start: usize) -> Vec<f32> {
        use std::f32::consts::PI;
        let (mut phase, mut out) = (0f32, vec![0f32; len]);
        let mut formants = [(700.0f32, [0f32; 2]), (1_200.0, [0f32; 2])];
        for (n, o) in out.iter_mut().enumerate().skip(start) {
            let t = (n - start) as f32 / 48_000.0;
            let f0 = 120.0 + 8.0 * (2.0 * PI * 5.0 * t).sin();
            phase += f0 / 48_000.0;
            let pulse = if phase >= 1.0 {
                phase -= 1.0;
                1.0
            } else {
                0.0
            };
            let mut y = 0.0;
            for (f, state) in &mut formants {
                // Two-pole resonator, ~80 Hz bandwidth.
                let r = (-PI * 80.0 / 48_000.0).exp();
                let v = pulse + 2.0 * r * (2.0 * PI * *f / 48_000.0).cos() * state[0] - r * r * state[1];
                state[1] = state[0];
                state[0] = v;
                y += v;
            }
            let syllables = 0.5 - 0.5 * (2.0 * PI * 4.0 * t).cos();
            *o = y * syllables * 0.02;
        }
        out
    }

    /// Silence, then vowels: how late they come out.
    fn onset_delay(mode: NoiseSuppression) -> i64 {
        let start = 100 * FRAME + 123;
        let input = vowels(200 * FRAME, start);
        let mut d = loaded();
        let mut out = Vec::new();
        for f in 0..200 {
            let mut frame: [f32; FRAME] = std::array::from_fn(|i| input[f * FRAME + i]);
            d.process(&mut frame, mode, true);
            out.extend_from_slice(&frame);
        }
        let kept = rms(&out[start..]) / rms(&input[start..]);
        assert!(kept > 0.5, "{mode:?} took the vowels down to {kept}");
        // The lag at which the output best matches the input.
        let window = start + 20 * FRAME..out.len() - 4 * FRAME;
        let score = |lag: usize| -> f32 { window.clone().map(|i| out[i] * input[i - lag]).sum() };
        (0..4 * FRAME).max_by(|a, b| score(*a).total_cmp(&score(*b))).unwrap() as i64
    }

    #[cfg(feature = "deep-filter")]
    #[test]
    fn a_stuck_worker_costs_one_wait_and_then_rnnoise_takes_over() {
        let mut d = Denoiser::new();
        d.deep_started = true;
        d.deep = Some(deep::Worker::fake(1));
        let mut seed = 3;
        let original: Vec<f32> = (0..2 * FRAME).map(|_| noise(&mut seed)).collect();
        // A TeamSpeak-sized frame: the worker answers its first half, then stops answering.
        let mut frame = original.clone();
        let started = std::time::Instant::now();
        d.process(&mut frame, NoiseSuppression::Strong, true);
        assert!(started.elapsed() < std::time::Duration::from_millis(200), "waited {:?}", started.elapsed());
        assert_eq!(frame[..FRAME], original[..FRAME], "the answered half is what the worker gave");
        // The other half went through a fresh RNNoise, from the audio as captured.
        let mut fresh = Denoiser::new();
        let mut expected = original[FRAME..].to_vec();
        fresh.process(&mut expected, NoiseSuppression::Light, true);
        assert_eq!(frame[FRAME..], expected[..]);
        assert!(d.deep.is_none());
        // From then on, no more waiting.
        let started = std::time::Instant::now();
        d.process(&mut [0.0; FRAME], NoiseSuppression::Strong, true);
        assert!(started.elapsed() < std::time::Duration::from_millis(20));
    }

    #[test]
    fn sound_comes_through_one_frame_late() {
        // One frame of overlap-add; nothing looks ahead.
        for mode in [NoiseSuppression::Light, NoiseSuppression::Strong] {
            assert_eq!(onset_delay(mode), FRAME as i64, "{mode:?}");
        }
    }
}
