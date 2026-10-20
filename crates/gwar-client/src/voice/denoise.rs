//! Background-noise suppression on the capture path (docs/latency.md, "Noise suppression"): every
//! 10 ms frame goes through it before the encoder; the level meter and the voice-activity gate
//! still measure the microphone as captured. It runs on the device, before encryption.
//!
//! One model for every kind of noise: DeepFilterNet3's low-latency model (no lookahead), run with
//! tract on 48 kHz frames of exactly our 480 samples. It adds one frame (10 ms) of delay, its
//! overlap-add, and takes ~1.3 ms of CPU per frame. How hard it works is one setting, a strength:
//! the cleaned audio is mixed with what was captured so that no sound goes down by more than a set
//! number of dB (DeepFilterNet's attenuation limit).

#[cfg(any(feature = "deep-filter", test))]
use super::FRAME;

/// How much background noise to take out, in percent: 0 is off, 100 all the model can. In
/// between, no sound is taken down by more than half a dB per percent (50 % caps it at 25 dB). Off
/// unless the app chooses (the desktop app's default is [`NoiseSuppression::FULL`]): tests and
/// tools that send tones through the pipeline get them through untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NoiseSuppression(u8);

impl NoiseSuppression {
    pub const OFF: Self = Self(0);
    pub const FULL: Self = Self(100);

    /// Above 100 counts as 100.
    pub const fn percent(percent: u8) -> Self {
        Self(if percent > 100 { 100 } else { percent })
    }

    pub const fn as_percent(self) -> u8 {
        self.0
    }

    fn is_off(self) -> bool {
        self.0 == 0
    }

    /// DeepFilterNet's attenuation limit; 100 dB or more is none.
    #[cfg_attr(not(feature = "deep-filter"), allow(dead_code))]
    fn limit_db(self) -> f32 {
        if self.0 >= 100 { 100.0 } else { f32::from(self.0) * 0.5 }
    }
}

/// DeepFilterNet3's low-latency model on a worker thread of the pipeline's own: tract's model is
/// tied to the thread that built it, and building it takes ~0.3 s, which the pipeline must not
/// wait for. The pipeline hands it one frame at a time and waits for that frame.
#[cfg(feature = "deep-filter")]
mod deep {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicU8, Ordering},
            mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel},
        },
        time::{Duration, Instant},
    };

    use df::tract::{DfParams, DfTract, RuntimeParams};

    use super::FRAME;

    /// libDF's thresholds on its estimate of the local SNR (dB), as its own `deep-filter` command
    /// sets them: below `MIN_DB` a frame is taken for noise alone and silenced, above `MAX_DB` for
    /// clean speech and left alone. The library's defaults (-10, and 30 and 20 for its two stages)
    /// leave more frames alone: with a quiet background, half the gain (docs/latency.md).
    const MIN_DB: f32 = -15.0;
    const MAX_DB: f32 = 35.0;
    /// DeepFilterNet's post-filter (the command's `--pf`) at its default strength: takes noise
    /// between words further down.
    const POST_FILTER_BETA: f32 = 0.02;
    /// libDF: an attenuation limit of 100 dB or more is none.
    const NO_LIMIT: f32 = 100.0;

    /// A frame that takes longer than this holds up the pipeline: it is passed on as captured.
    const PATIENCE: Duration = Duration::from_millis(20);
    /// The model cannot keep up once the pipeline waits for it this long on average (half of
    /// each 10 ms frame; ~1.3 ms on Apple Silicon), or once this many frames in a row (100 ms)
    /// pass uncleaned. A one-off stall of the worker costs a few frames, not the suppression.
    pub(super) const BUDGET: Duration = Duration::from_millis(5);
    const MISSED: u32 = 10;

    const LOADING: u8 = 0;
    const READY: u8 = 1;
    const FAILED: u8 = 2;

    struct Job {
        seq: u64,
        frame: Box<[f32; FRAME]>,
        /// Start over from the untouched model first (a gap in the capture).
        reset: bool,
        limit_db: f32,
    }

    struct Done {
        seq: u64,
        frame: Box<[f32; FRAME]>,
    }

    pub(super) enum Outcome {
        Cleaned,
        /// Left as captured: the model was late with it.
        Skipped,
        /// The model cannot keep up; this frame is left as captured or cleaned, as it came.
        Behind,
        /// The worker has ended (the model failed); this frame is left as captured.
        Failed,
    }

    /// A worker's thread, after the worker is dropped: a stuck one may never end.
    pub(super) struct Thread(Arc<AtomicU8>);

    impl Thread {
        pub(super) fn running(&self) -> bool {
            // The thread holds the other reference until it returns.
            Arc::strong_count(&self.0) > 1
        }
    }

    /// What the stand-in worker of the tests does with a frame.
    #[cfg(test)]
    pub(super) enum Step {
        Now,
        Sleep(Duration),
        /// Until the test sends (or drops the sender).
        Hold(std::sync::mpsc::Receiver<()>),
        /// Ends the worker without an answer, as a model error does.
        Fail,
    }

    pub(super) struct Worker {
        jobs: SyncSender<Job>,
        done: Receiver<Done>,
        state: Arc<AtomicU8>,
        /// A frame buffer that travels to the worker and back, so nothing is allocated per frame.
        spare: Option<Box<[f32; FRAME]>>,
        seq: u64,
        /// The next frame starts from the untouched model.
        reset: bool,
        patience: Duration,
        budget: Duration,
        /// The pipeline's wait per frame, averaged over ~16 frames (seconds).
        load: f32,
        /// Frames in a row passed on as captured.
        missed: u32,
    }

    impl Worker {
        pub(super) fn start() -> Option<Self> {
            let (jobs, job_rx) = sync_channel::<Job>(1);
            let (done_tx, done) = sync_channel(1);
            let state = Arc::new(AtomicU8::new(LOADING));
            let flag = state.clone();
            std::thread::Builder::new()
                .name("gwar-denoise".into())
                .spawn(move || {
                    let params = RuntimeParams::default_with_ch(1)
                        .with_thresholds(MIN_DB, MAX_DB, MAX_DB)
                        .with_post_filter(POST_FILTER_BETA);
                    let base = match DfTract::new(DfParams::default(), &params) {
                        Ok(model) if model.hop_size == FRAME => model,
                        Ok(model) => {
                            flag.store(FAILED, Ordering::Release);
                            return tracing::warn!(
                                hop = model.hop_size,
                                "DeepFilterNet hop is not 10 ms; no noise suppression"
                            );
                        }
                        Err(e) => {
                            flag.store(FAILED, Ordering::Release);
                            return tracing::warn!("DeepFilterNet unavailable, no noise suppression: {e:#}");
                        }
                    };
                    // Frames it holds hold up the pipeline too.
                    let _priority = super::super::priority::Priority::promote();
                    let mut live = base.clone();
                    let mut limit = NO_LIMIT;
                    let mut noisy = ndarray::Array2::<f32>::zeros((1, FRAME));
                    let mut clean = ndarray::Array2::<f32>::zeros((1, FRAME));
                    flag.store(READY, Ordering::Release);
                    while let Ok(Job { seq, mut frame, reset, limit_db }) = job_rx.recv() {
                        if reset {
                            live = base.clone();
                            limit = NO_LIMIT;
                        }
                        if limit_db != limit {
                            live.set_atten_lim(limit_db);
                            limit = limit_db;
                        }
                        noisy.row_mut(0).iter_mut().zip(frame.iter()).for_each(|(n, s)| *n = *s);
                        if let Err(e) = live.process(noisy.view(), clean.view_mut()) {
                            flag.store(FAILED, Ordering::Release);
                            return tracing::warn!("DeepFilterNet failed: {e:#}");
                        }
                        frame.iter_mut().zip(clean.row(0).iter()).for_each(|(s, c)| *s = c.clamp(-1.0, 1.0));
                        if done_tx.send(Done { seq, frame }).is_err() {
                            return;
                        }
                    }
                })
                .ok()?;
            Some(Self::new(jobs, done, state))
        }

        fn new(jobs: SyncSender<Job>, done: Receiver<Done>, state: Arc<AtomicU8>) -> Self {
            Self {
                jobs,
                done,
                state,
                spare: Some(Box::new([0.0; FRAME])),
                seq: 0,
                reset: false,
                patience: PATIENCE,
                budget: BUDGET,
                load: 0.0,
                missed: 0,
            }
        }

        /// A stand-in worker: does `steps[i]` with the i-th frame ([`Step::Now`] past the list),
        /// then scales it by 0.5, or by 0.25 when it starts the model over.
        #[cfg(test)]
        pub(super) fn fake(steps: Vec<Step>) -> Self {
            let (jobs, job_rx) = sync_channel::<Job>(1);
            let (done_tx, done) = sync_channel(1);
            let state = Arc::new(AtomicU8::new(READY));
            let thread = state.clone();
            std::thread::spawn(move || {
                let mut steps = steps.into_iter();
                while let Ok(Job { seq, mut frame, reset, .. }) = job_rx.recv() {
                    match steps.next() {
                        Some(Step::Sleep(delay)) => std::thread::sleep(delay),
                        Some(Step::Hold(release)) => drop(release.recv()),
                        Some(Step::Fail) => return thread.store(FAILED, Ordering::Release),
                        Some(Step::Now) | None => {}
                    }
                    let scale = if reset { 0.25 } else { 0.5 };
                    frame.iter_mut().for_each(|s| *s *= scale);
                    if done_tx.send(Done { seq, frame }).is_err() {
                        return;
                    }
                }
            });
            Self::new(jobs, done, state)
        }

        /// Waits up to `patience` for a frame, and up to `budget` per frame on average.
        #[cfg(test)]
        pub(super) fn with_limits(mut self, patience: Duration, budget: Duration) -> Self {
            self.patience = patience;
            self.budget = budget;
            self
        }

        pub(super) fn thread(&self) -> Thread {
            Thread(self.state.clone())
        }

        /// `Some(true)` once the model is there, `Some(false)` while it loads, `None` if it could not.
        pub(super) fn ready(&self) -> Option<bool> {
            match self.state.load(Ordering::Acquire) {
                LOADING => Some(false),
                READY => Some(true),
                _ => None,
            }
        }

        pub(super) fn reset(&mut self) {
            self.reset = true;
        }

        /// Cleans `chunk` in place, waiting [`PATIENCE`] at most; past that it is left as captured.
        /// The hand-over never blocks: the worker has taken every earlier frame unless one is late.
        pub(super) fn process(&mut self, chunk: &mut [f32; FRAME], limit_db: f32) -> Outcome {
            let started = Instant::now();
            // Only after a late frame is the buffer still away.
            let mut frame = self.spare.take().unwrap_or_else(|| Box::new([0.0; FRAME]));
            frame.copy_from_slice(chunk);
            self.seq += 1;
            let job = Job { seq: self.seq, frame, reset: std::mem::take(&mut self.reset), limit_db };
            let cleaned = match self.jobs.try_send(job) {
                Ok(()) => self.answer(chunk, started + self.patience),
                // Still on the frame before the last: this one never reaches the model, whose
                // state would bridge the gap.
                Err(TrySendError::Full(job)) => {
                    self.spare = Some(job.frame);
                    self.reset = true;
                    Some(false)
                }
                Err(TrySendError::Disconnected(_)) => None,
            };
            // The thread ends only when the model fails (or panics).
            let Some(cleaned) = cleaned else { return Outcome::Failed };
            self.load += (started.elapsed().as_secs_f32() - self.load) / 16.0;
            self.missed = if cleaned { 0 } else { self.missed + 1 };
            if self.load > self.budget.as_secs_f32() || self.missed >= MISSED {
                tracing::warn!(
                    load_ms = self.load * 1e3,
                    missed = self.missed,
                    "DeepFilterNet cannot keep up; noise suppression paused"
                );
                Outcome::Behind
            } else if cleaned {
                Outcome::Cleaned
            } else {
                Outcome::Skipped
            }
        }

        /// Waits for the answer to the frame just handed over: `Some(true)` with `chunk` cleaned,
        /// `Some(false)` past `deadline`, `None` if the worker is gone.
        fn answer(&mut self, chunk: &mut [f32; FRAME], deadline: Instant) -> Option<bool> {
            loop {
                match self.done.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(Done { seq, frame }) if seq == self.seq => {
                        chunk.copy_from_slice(&frame[..]);
                        self.spare = Some(frame);
                        return Some(true);
                    }
                    // A late answer, for a frame already passed on as captured.
                    Ok(Done { frame, .. }) => {
                        self.spare.get_or_insert(frame);
                    }
                    Err(RecvTimeoutError::Timeout) => return Some(false),
                    Err(RecvTimeoutError::Disconnected) => return None,
                }
            }
        }
    }
}

/// After the model stops keeping up, the wait before it is started again; doubles each time.
#[cfg(feature = "deep-filter")]
const RETRY: std::time::Duration = std::time::Duration::from_secs(10);
#[cfg(feature = "deep-filter")]
const MAX_RETRY: std::time::Duration = std::time::Duration::from_secs(300);

#[cfg(feature = "deep-filter")]
enum Model {
    /// Started the first time suppression is on.
    Unstarted,
    Running(deep::Worker),
    /// Could not keep up: started again (from scratch) at `retry`, once the old worker's thread
    /// has ended (a stuck one is never joined by another).
    Paused {
        retry: std::time::Instant,
        previous: Option<deep::Thread>,
    },
    /// Could not load, or failed on a frame.
    Unavailable,
}

/// The suppressor of one capture pipeline: its state follows the audio, so it starts over after a
/// gap in the capture and when it is turned on (the old state would smear one into the other).
/// Audio passes as captured while the model loads (~0.3 s, so joining a channel never waits) and
/// while it is paused.
pub(crate) struct Denoiser {
    on: bool,
    #[cfg(feature = "deep-filter")]
    model: Model,
    #[cfg(feature = "deep-filter")]
    retry_wait: std::time::Duration,
}

impl Denoiser {
    pub(crate) fn new() -> Self {
        Self {
            on: false,
            #[cfg(feature = "deep-filter")]
            model: Model::Unstarted,
            #[cfg(feature = "deep-filter")]
            retry_wait: RETRY,
        }
    }

    /// Cleans `frame` (any whole number of 10 ms frames) in place. `continues` is false when the
    /// capture skipped samples since the previous call.
    pub(crate) fn process(&mut self, frame: &mut [f32], mode: NoiseSuppression, continues: bool) {
        let fresh = !self.on || !continues;
        self.on = !mode.is_off();
        if !self.on {
            return;
        }
        #[cfg(feature = "deep-filter")]
        self.suppress(frame, mode, fresh);
        #[cfg(not(feature = "deep-filter"))]
        let _ = (frame, fresh);
    }

    #[cfg(feature = "deep-filter")]
    fn suppress(&mut self, frame: &mut [f32], mode: NoiseSuppression, fresh: bool) {
        use std::time::Instant;

        let now = Instant::now();
        let start = match &mut self.model {
            Model::Unstarted => true,
            Model::Paused { retry, previous } if now >= *retry => {
                let stuck = previous.as_ref().is_some_and(deep::Thread::running);
                if stuck {
                    *retry = now + self.retry_wait;
                }
                !stuck
            }
            Model::Running(_) | Model::Paused { .. } | Model::Unavailable => false,
        };
        if start {
            // A new worker starts from the untouched model.
            self.model = match deep::Worker::start() {
                Some(worker) => Model::Running(worker),
                None => self.pause(),
            };
        }
        let Model::Running(worker) = &mut self.model else { return };
        match worker.ready() {
            Some(true) => {}
            Some(false) => return,
            None => {
                self.model = Model::Unavailable;
                return;
            }
        }
        if fresh {
            worker.reset();
        }
        let limit = mode.limit_db();
        let end = frame.as_chunks_mut::<FRAME>().0.iter_mut().find_map(|chunk| match worker.process(chunk, limit) {
            deep::Outcome::Cleaned | deep::Outcome::Skipped => None,
            end => Some(end),
        });
        // The rest of `frame`, and the audio from then on, passes as captured.
        match end {
            None => {}
            Some(deep::Outcome::Failed) => self.model = Model::Unavailable,
            Some(_) => self.model = self.pause(),
        }
    }

    /// Paused from now on; drops a running worker.
    #[cfg(feature = "deep-filter")]
    fn pause(&mut self) -> Model {
        let previous = match &self.model {
            Model::Running(worker) => Some(worker.thread()),
            _ => None,
        };
        let retry = std::time::Instant::now() + self.retry_wait;
        self.retry_wait = (self.retry_wait * 2).min(MAX_RETRY);
        Model::Paused { retry, previous }
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

    #[test]
    fn off_leaves_the_audio_alone() {
        let mut seed = 7;
        let mut d = Denoiser::new();
        let mut frame: [f32; FRAME] = std::array::from_fn(|_| noise(&mut seed));
        let original = frame;
        d.process(&mut frame, NoiseSuppression::OFF, true);
        assert_eq!(frame, original);
    }

    #[test]
    fn strength_is_a_percentage() {
        assert_eq!(NoiseSuppression::percent(250), NoiseSuppression::FULL);
        assert_eq!(NoiseSuppression::percent(50).limit_db(), 25.0);
        assert_eq!(NoiseSuppression::FULL.limit_db(), 100.0);
    }

    #[cfg(feature = "deep-filter")]
    mod model {
        use std::time::{Duration, Instant};

        use super::*;

        fn rms(x: &[f32]) -> f32 {
            (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
        }

        /// A denoiser whose model has loaded, with no time limits: however slow the machine, every
        /// frame is the model's.
        fn loaded() -> Denoiser {
            let mut d = Denoiser::new();
            d.process(&mut [0.0; FRAME], NoiseSuppression::FULL, true);
            let deadline = Instant::now() + Duration::from_secs(60);
            while !matches!(&d.model, Model::Running(w) if w.ready() == Some(true)) {
                assert!(Instant::now() < deadline, "DeepFilterNet did not load");
                std::thread::sleep(Duration::from_millis(20));
            }
            let Model::Running(worker) = std::mem::replace(&mut d.model, Model::Unavailable) else { unreachable!() };
            let unlimited = Duration::from_secs(3600);
            d.model = Model::Running(worker.with_limits(unlimited, unlimited));
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
        fn full_strength_takes_steady_noise_far_down_and_less_caps_the_cut() {
            let full = noise_left(NoiseSuppression::FULL);
            assert!(full < -40.0, "full: {full} dB");
            let half = noise_left(NoiseSuppression::percent(50));
            assert!((-26.0..-24.0).contains(&half), "half: {half} dB");
        }

        /// A vowel-like sound from `start` on: a glottal pulse train with vibrato through two
        /// formant resonators, in syllables (a steady tone is what a neural suppressor rightly
        /// takes for noise).
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

        #[test]
        fn sound_comes_through_one_frame_late() {
            // One frame of overlap-add; nothing looks ahead.
            let start = 100 * FRAME + 123;
            let input = vowels(200 * FRAME, start);
            let mut d = loaded();
            let mut out = Vec::new();
            for f in 0..200 {
                let mut frame: [f32; FRAME] = std::array::from_fn(|i| input[f * FRAME + i]);
                d.process(&mut frame, NoiseSuppression::FULL, true);
                out.extend_from_slice(&frame);
            }
            let kept = rms(&out[start..]) / rms(&input[start..]);
            assert!(kept > 0.5, "the vowels went down to {kept}");
            // The lag at which the output best matches the input.
            let window = start + 20 * FRAME..out.len() - 4 * FRAME;
            let score = |lag: usize| -> f32 { window.clone().map(|i| out[i] * input[i - lag]).sum() };
            assert_eq!((0..4 * FRAME).max_by(|a, b| score(*a).total_cmp(&score(*b))), Some(FRAME));
        }

        use deep::Step;

        /// A denoiser on a stand-in worker (see [`deep::Worker::fake`]).
        fn on(worker: deep::Worker) -> Denoiser {
            let mut d = Denoiser::new();
            d.model = Model::Running(worker);
            d
        }

        /// What a frame of 0.2s comes out as: 0.1 cleaned, 0.05 cleaned by a model started over,
        /// 0.2 passed as captured.
        fn frame(d: &mut Denoiser) -> f32 {
            let mut frame = [0.2; FRAME];
            d.process(&mut frame, NoiseSuppression::FULL, true);
            assert!(frame.iter().all(|s| *s == frame[0]));
            frame[0]
        }

        /// Room for a slow machine: only the frame held on purpose is late.
        fn patient(steps: Vec<Step>) -> deep::Worker {
            deep::Worker::fake(steps).with_limits(Duration::from_millis(200), Duration::from_secs(3600))
        }

        #[test]
        fn a_late_frame_passes_as_captured_and_the_model_goes_on() {
            let (release, held) = std::sync::mpsc::channel();
            let mut d = on(patient(vec![Step::Now, Step::Hold(held)]));
            // The first frame starts the model.
            assert_eq!(frame(&mut d), 0.05);
            assert_eq!(frame(&mut d), 0.2);
            release.send(()).unwrap();
            // The model had that frame: it goes on from it.
            let out: Vec<f32> = (0..10).map(|_| frame(&mut d)).collect();
            assert!(out.iter().all(|s| *s == 0.1), "{out:?}");
            assert!(matches!(d.model, Model::Running(_)));
        }

        #[test]
        fn a_frame_that_never_reached_the_model_starts_it_over() {
            let (release, held) = std::sync::mpsc::channel();
            let mut d = on(patient(vec![Step::Now, Step::Hold(held)]));
            assert_eq!(frame(&mut d), 0.05);
            // Held; the next waits in the queue; the one after cannot be handed over.
            assert_eq!([frame(&mut d), frame(&mut d), frame(&mut d)], [0.2; 3]);
            release.send(()).unwrap();
            // A frame every 10 ms, as captured: one that cannot be handed over does not wait.
            let out: Vec<f32> = (0..8)
                .map(|_| {
                    std::thread::sleep(Duration::from_millis(10));
                    frame(&mut d)
                })
                .collect();
            let first = out.iter().position(|s| *s != 0.2).expect("cleaned again");
            assert_eq!(out[first..first + 2], [0.05, 0.1], "{out:?}");
        }

        #[test]
        fn a_stuck_model_is_paused_and_no_second_one_starts_beside_it() {
            let (release, held) = std::sync::mpsc::channel();
            let mut d = on(deep::Worker::fake(vec![Step::Now, Step::Hold(held)]));
            let started = Instant::now();
            let out: Vec<f32> = (0..15).map(|_| frame(&mut d)).collect();
            // Two waits for answers; the rest could not be handed over.
            assert!(started.elapsed() < Duration::from_secs(1), "waited {:?}", started.elapsed());
            assert_eq!(out[0], 0.05);
            assert!(out[1..].iter().all(|s| *s == 0.2), "{out:?}");
            assert!(matches!(d.model, Model::Paused { .. }));
            assert_eq!(d.retry_wait, RETRY * 2);
            // Paused: no waiting.
            let started = Instant::now();
            assert_eq!(frame(&mut d), 0.2);
            assert!(started.elapsed() < Duration::from_millis(50));
            let retry_now = |d: &mut Denoiser| {
                let Model::Paused { retry, .. } = &mut d.model else { panic!("not paused") };
                *retry = Instant::now();
            };
            // Time to try again, but the old thread is still stuck.
            retry_now(&mut d);
            assert_eq!(frame(&mut d), 0.2);
            assert!(matches!(&d.model, Model::Paused { retry, .. } if *retry > Instant::now()));
            // Once it ends, a new model starts.
            release.send(()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while matches!(&d.model, Model::Paused { previous: Some(t), .. } if t.running()) {
                assert!(Instant::now() < deadline, "the stuck thread did not end");
                std::thread::sleep(Duration::from_millis(5));
            }
            retry_now(&mut d);
            frame(&mut d);
            assert!(matches!(d.model, Model::Running(_)));
        }

        #[test]
        fn a_model_too_slow_for_the_machine_is_paused() {
            // 8 ms a frame: real time, but it leaves the pipeline too little of each 10 ms.
            let worker = deep::Worker::fake((0..40).map(|_| Step::Sleep(Duration::from_millis(8))).collect())
                .with_limits(Duration::from_secs(3600), deep::BUDGET);
            let mut d = on(worker);
            let out: Vec<f32> = (0..40).map(|_| frame(&mut d)).collect();
            let cleaned = out.iter().take_while(|s| **s < 0.2).count();
            // The average wait passes 5 ms within 16 frames.
            assert!((1..=16).contains(&cleaned), "{out:?}");
            assert!(out[cleaned..].iter().all(|s| *s == 0.2));
            assert!(matches!(d.model, Model::Paused { .. }));
        }

        #[test]
        fn a_failed_model_is_not_started_again() {
            let mut d = on(deep::Worker::fake(vec![Step::Now, Step::Fail]));
            assert_eq!(frame(&mut d), 0.05);
            assert_eq!(frame(&mut d), 0.2);
            assert!(matches!(d.model, Model::Unavailable));
        }
    }
}
