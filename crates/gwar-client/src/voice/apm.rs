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
//! there. The first share sound really played brings in a stereo canceller, which stays until the
//! session ends: [`Processing::pin_stereo`]. In a voice session that canceller has been listening
//! from the start, off the voice thread, so on speakers it arrives already knowing the room:
//! [`Processing::warm_stereo`].

use sonora::{
    AudioProcessing, Config, StreamConfig,
    config::{AdaptiveDigital, EchoCanceller, GainController2, MaxProcessingRate, Pipeline, TransparentModeType},
};

use super::FRAME;
use rtrb::{Consumer, Producer, RingBuffer};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicUsize, Ordering},
        mpsc::{Receiver, sync_channel},
    },
    thread::{self, Thread},
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
/// Frames of sound a stereo canceller that has already learned still runs beside the mono one on
/// the voice thread: it missed the frames of its journey there, and its output has to be in step
/// before the crossfade.
const HANDOVER_FRAMES: usize = 5;
/// How far the warming canceller must take the microphone down, in dB, before it counts as having
/// found an echo. Measured beside a voice that overlaps the far one, the most headphones reach is
/// 9.1 dB; speakers pass 12 within seconds of the far side talking alone, unless they are very
/// quiet. Nothing else tells the two apart, and they want opposite things:
/// [`Processing::warm_stereo`].
const ECHO_FOUND: f64 = 12.0;
/// Frames the warming canceller may fall behind, render and capture counted apart: 640 ms.
const WARMING_RING: usize = 128;
/// How many frames of speakers wait for the worker, so that it wakes 25 times a second.
const WARMING_BATCH: usize = 4;
/// Frames after the speakers fall silent that the warming canceller still hears: the echo's tail.
/// Past that nothing plays and there is nothing to learn, so it costs nothing either.
const WARMING_TAIL: usize = 50;

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
    Building,
    /// It hears everything the mono one does, and voice still comes from the mono one. The count
    /// is the frames of sound still to hear: over silence there is nothing to learn from.
    Learning(Box<Stage>, usize),
    /// `echo` is the stereo canceller until the session ends. The box holds the old one: freeing
    /// it here is not worth the wait on a thread that voice runs on.
    Stereo(#[allow(dead_code)] Box<Stage>),
    /// No thread could be had: voice goes on as it was, and share sound's sides leak.
    Refused,
}

/// One frame the warming canceller is to hear, in the order the mono one heard it. The ring holds
/// these by value because it is the only memory they get: boxing them would mean the heap.
#[allow(clippy::large_enum_variant)]
enum Heard {
    Speakers([[f32; FRAME]; 2]),
    Microphone([f32; FRAME], i32),
}

/// A stereo canceller trained on everything the voice session hears, with no output anyone uses.
struct Warming {
    stage: Box<Stage>,
    heard: Consumer<Heard>,
    /// Frames of sound it has learned from, for [`Processing::collect_stereo`].
    sound: Arc<AtomicUsize>,
    /// Whether it ever reached [`ECHO_FOUND`].
    found_echo: bool,
}

impl Warming {
    /// Everything waiting in the ring.
    fn catch_up(&mut self) {
        let (mut left, mut right, mut near) = ([0f32; FRAME], [0f32; FRAME], [0f32; FRAME]);
        while let Ok(heard) = self.heard.pop() {
            match heard {
                Heard::Speakers([far_left, far_right]) => {
                    let power: f32 = far_left.iter().chain(&far_right).map(|sample| sample * sample).sum();
                    self.sound.fetch_add(usize::from(power >= PLAYING), Ordering::Relaxed);
                    let far: [&[f32]; 2] = [&far_left, &far_right];
                    let _ = self.stage.apm.process_render_f32(&far, &mut [&mut left, &mut right]);
                }
                Heard::Microphone(microphone, delay_ms) => {
                    let _ = self.stage.apm.set_stream_delay_ms(delay_ms);
                    let _ = self.stage.apm.process_capture_f32(&[&microphone], &mut [&mut near]);
                    let taken_down = self.stage.apm.statistics().echo_return_loss_enhancement;
                    self.found_echo |= taken_down.is_some_and(|enhancement| enhancement >= ECHO_FOUND);
                }
            }
        }
    }
}

/// Waits through a voice session, in case share sound plays, and provides the stereo canceller
/// then. Told to warm, it trains that canceller meanwhile on what the voice thread leaves in a ring;
/// otherwise it builds one when asked. It starts with the session because nothing about starting a
/// thread is fit for the voice thread; feeding it, asking it and taking what it has touch neither
/// the heap nor a lock. A worker that falls a whole ring behind loses frames: that costs it some
/// of what it learned, and voice nothing, since nothing waits for it.
struct StereoWorker {
    thread: Thread,
    order: Arc<AtomicU8>,
    built: Receiver<Box<Stage>>,
    feed: Producer<Heard>,
    sound: Arc<AtomicUsize>,
}

const WAIT: u8 = 0;
const BUILD: u8 = 1;
const LEAVE: u8 = 2;
const WARM: u8 = 3;

impl StereoWorker {
    fn start() -> Option<Self> {
        let (send, built) = sync_channel(1);
        let (feed, heard) = RingBuffer::new(WARMING_RING);
        let order = Arc::new(AtomicU8::new(WAIT));
        let sound = Arc::new(AtomicUsize::new(0));
        let told = order.clone();
        let learned = sound.clone();
        let work = move || {
            let mut heard = Some(heard);
            let mut warming: Option<Warming> = None;
            loop {
                // Read first: the voice thread stops feeding before it orders BUILD, so what is
                // caught up on after this read is everything.
                let order = told.load(Ordering::Acquire);
                if let Some(warming) = &mut warming {
                    warming.catch_up();
                }
                match order {
                    WAIT => thread::park(),
                    WARM => match heard.take() {
                        Some(heard) => {
                            let stage = Box::new(Stage::stereo());
                            warming = Some(Warming { stage, heard, sound: learned.clone(), found_echo: false });
                        }
                        None => thread::park(),
                    },
                    BUILD => {
                        match warming.take() {
                            Some(warmed) if warmed.found_echo => {
                                let _ = send.send(warmed.stage);
                            }
                            // It goes after the new one, which arrives when it would have anyway.
                            warmed => {
                                learned.store(0, Ordering::Relaxed);
                                let _ = send.send(Box::new(Stage::stereo()));
                                drop(warmed);
                            }
                        }
                        return;
                    }
                    _ => return,
                }
            }
        };
        match thread::Builder::new().name("gwar-stereo-canceller".into()).spawn(work) {
            Ok(worker) => Some(Self { thread: worker.thread().clone(), order, built, feed, sound }),
            Err(error) => {
                tracing::warn!("the stereo echo canceller will not be available: {error}");
                None
            }
        }
    }

    fn tell(&self, order: u8) {
        self.order.store(order, Ordering::Release);
        self.thread.unpark();
    }
}

impl Drop for StereoWorker {
    fn drop(&mut self) {
        self.tell(LEAVE);
    }
}

pub(crate) struct Processing {
    echo: Stage,
    gain: Stage,
    channels: Channels,
    stereo: Option<StereoWorker>,
    learning_frames: usize,
    warming: bool,
    /// Frames since the speakers last played anything, up to [`WARMING_TAIL`].
    silent_frames: usize,
    fed_frames: usize,
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
            stereo: StereoWorker::start(),
            learning_frames: LEARNING_FRAMES,
            warming: false,
            silent_frames: WARMING_TAIL,
            fed_frames: 0,
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

    /// A voice session may play share sound later: from now on the worker trains the stereo
    /// canceller on the same microphone and speakers as the mono one. Voice comes out of the mono
    /// one to the bit as if this were never called.
    ///
    /// On speakers that canceller takes over within frames of the first share sound, so wide sound
    /// does not leak for seconds while one learns. Without an echo there is no leak to stop, and a
    /// canceller trained this long takes the near voice down for the first seconds of new sound
    /// more than a new one does. So one that found no echo is left behind: headphones, and anyone
    /// the far side never talked alone to, go the way of [`Processing::pin_stereo`] without this.
    pub(crate) fn warm_stereo(&mut self) {
        if let Some(worker) = &self.stereo {
            worker.tell(WARM);
            self.warming = true;
        }
    }

    /// Share sound is playing here: from now until this voice session ends the canceller is
    /// stereo. Only wakes the worker, once. Voice alone never gets here.
    pub(crate) fn pin_stereo(&mut self) {
        if !matches!(self.channels, Channels::Mono) {
            return;
        }
        self.channels = match &self.stereo {
            Some(worker) => {
                worker.tell(BUILD);
                Channels::Building
            }
            None => Channels::Refused,
        };
    }

    /// Takes the stereo canceller if it has arrived. Never waits.
    fn collect_stereo(&mut self) {
        if !matches!(self.channels, Channels::Building) {
            return;
        }
        let Some(worker) = &self.stereo else { return };
        if let Ok(stage) = worker.built.try_recv() {
            let sound = worker.sound.load(Ordering::Relaxed);
            let left_to_hear = self.learning_frames.saturating_sub(sound);
            self.channels = Channels::Learning(stage, left_to_hear.max(HANDOVER_FRAMES.min(self.learning_frames)));
        }
    }

    /// Whether the warming canceller hears this frame. Called once for each frame of speakers.
    fn warming_hears_speakers(&mut self) -> bool {
        if !self.warming || !matches!(self.channels, Channels::Mono) {
            return false;
        }
        let [left, right] = &self.render;
        let power: f32 = left.iter().chain(right).map(|sample| sample * sample).sum();
        self.silent_frames = if power >= PLAYING { 0 } else { (self.silent_frames + 1).min(WARMING_TAIL) };
        self.silent_frames < WARMING_TAIL
    }

    /// Leaves a frame for the warming canceller, if it has room. Never waits.
    fn overhear(&mut self, heard: Heard) {
        if let Some(worker) = &mut self.stereo {
            let _ = worker.feed.push(heard);
        }
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
        if self.warming_hears_speakers() {
            self.overhear(Heard::Speakers(self.render));
            self.fed_frames += 1;
            if self.fed_frames.is_multiple_of(WARMING_BATCH)
                && let Some(worker) = &self.stereo
            {
                worker.thread.unpark();
            }
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
            if self.warming && self.silent_frames < WARMING_TAIL && matches!(self.channels, Channels::Mono) {
                self.overhear(Heard::Microphone(self.scratch, delay_ms.min(500) as i32));
            }
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
            if matches!(self.channels, Channels::Building) {
                let stage = self.stereo.as_ref().unwrap().built.recv().unwrap();
                self.channels = Channels::Learning(stage, self.learning_frames);
            }
        }

        fn is_mono(&self) -> bool {
            matches!(self.channels, Channels::Mono)
        }

        fn is_stereo(&self) -> bool {
            matches!(self.channels, Channels::Stereo(_))
        }
    }

    /// Counts this thread's trips to the allocator while it is switched on: the voice thread must
    /// make none on the way to stereo.
    mod heap {
        use std::{
            alloc::{GlobalAlloc, Layout, System},
            cell::Cell,
        };

        thread_local! {
            static COUNTING: Cell<bool> = const { Cell::new(false) };
            static TRIPS: Cell<usize> = const { Cell::new(0) };
            static BYTES: Cell<isize> = const { Cell::new(0) };
        }

        fn trip(bytes: isize) {
            if COUNTING.try_with(Cell::get).unwrap_or(false) {
                let _ = TRIPS.try_with(|trips| trips.set(trips.get() + 1));
                let _ = BYTES.try_with(|total| total.set(total.get() + bytes));
            }
        }

        struct Counting;

        // SAFETY: every call goes straight on to the system allocator; `trip` allocates nothing.
        unsafe impl GlobalAlloc for Counting {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                trip(layout.size() as isize);
                unsafe { System.alloc(layout) }
            }
            unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
                trip(-(layout.size() as isize));
                unsafe { System.dealloc(pointer, layout) }
            }
            unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
                trip(size as isize - layout.size() as isize);
                unsafe { System.realloc(pointer, layout, size) }
            }
        }

        #[global_allocator]
        static COUNTING_ALLOCATOR: Counting = Counting;

        /// How many times `work` went to the allocator on this thread.
        pub fn trips(work: impl FnOnce()) -> usize {
            TRIPS.set(0);
            COUNTING.set(true);
            work();
            COUNTING.set(false);
            TRIPS.get()
        }

        /// How many bytes `work` left allocated on this thread, and what it made.
        pub fn bytes<T>(work: impl FnOnce() -> T) -> (usize, T) {
            BYTES.set(0);
            COUNTING.set(true);
            let made = work();
            COUNTING.set(false);
            (BYTES.get().max(0) as usize, made)
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

    /// Sonora itself goes to the heap for every frame, in `origin/main` too. So what is held to
    /// zero is everything this file adds on the way to stereo, and a frame may cost no more than
    /// the cancellers running in it cost anyway.
    #[test]
    fn the_way_to_stereo_never_touches_the_heap_on_the_voice_thread() {
        let sound = wide(&voice(4 * RATE, 150.0, -18.0));
        let mut frames = sound.as_chunks::<FRAME>().0.iter();
        let mut processing = session(LEARNING_FRAMES);
        let mut step = |processing: &mut Processing| {
            let far = frames.next().expect("enough sound");
            let mut near = [0f32; FRAME];
            heap::trips(|| {
                processing.render(far);
                processing.cancel_echo(&mut near, 20);
            })
        };
        // The first frames set things up: what counts is an ordinary frame.
        for _ in 0..100 {
            step(&mut processing);
        }
        let mono = (0..20).map(|_| step(&mut processing)).max().unwrap();

        assert_eq!(heap::trips(|| processing.pin_stereo()), 0, "the pin");
        while matches!(processing.channels, Channels::Building) {
            assert_eq!(heap::trips(|| processing.collect_stereo()), 0, "taking what was built");
            thread::yield_now();
        }
        assert!(matches!(processing.channels, Channels::Learning(..)));

        let _ = step(&mut processing);
        let learning = (0..20).map(|_| step(&mut processing)).max().unwrap();
        let mut frame = [0f32; FRAME];
        assert_eq!(heap::trips(|| processing.take_over(Some(&mut frame))), 0, "the take-over");
        assert!(processing.is_stereo());
        let _ = step(&mut processing);
        let stereo = (0..20).map(|_| step(&mut processing)).max().unwrap();
        eprintln!("heap trips a frame: mono {mono}, both while learning {learning}, stereo {stereo}");
        assert!(learning <= mono + stereo, "learning costs {learning}, mono {mono}, stereo {stereo}");
    }

    #[test]
    fn a_session_without_share_sound_ends_its_sleeping_worker() {
        let processing = Processing::new();
        let order = processing.stereo.as_ref().unwrap().order.clone();
        drop(processing);
        let started = std::time::Instant::now();
        while Arc::strong_count(&order) > 1 {
            assert!(started.elapsed().as_secs() < 5, "the worker is still there");
            thread::yield_now();
        }
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

    impl Processing {
        /// Waits until the worker has taken everything out of the ring.
        fn caught_up(&self) {
            let worker = self.stereo.as_ref().unwrap();
            while worker.feed.slots() < WARMING_RING {
                worker.thread.unpark();
                thread::yield_now();
            }
        }

        /// As the pipeline would find it some frames later, by its own path.
        fn collected(&mut self) {
            while matches!(self.channels, Channels::Building) {
                self.collect_stereo();
                thread::yield_now();
            }
        }

        fn sound_heard_warming(&self) -> usize {
            self.stereo.as_ref().unwrap().sound.load(Ordering::Relaxed)
        }

        fn left_to_hear(&self) -> Option<usize> {
            match &self.channels {
                Channels::Learning(_, left) => Some(*left),
                _ => None,
            }
        }
    }

    fn warm_session() -> Processing {
        let mut processing = session(LEARNING_FRAMES);
        processing.warm_stereo();
        processing
    }

    /// [`through`] with a session that warms: the worker never loses a frame here, so that what
    /// comes out does not depend on how the threads happened to run.
    fn warm_through(processing: &mut Processing, far: &[[f32; 2]], near: &[f32], pin: Option<usize>) -> Vec<f32> {
        let mut out = near.to_vec();
        let frames = far.as_chunks::<FRAME>().0.iter().zip(out.as_chunks_mut::<FRAME>().0);
        for (index, (far, frame)) in frames.enumerate() {
            if index.is_multiple_of(WARMING_RING / 4) {
                processing.caught_up();
            }
            if pin.is_some_and(|at| index * FRAME >= at) {
                processing.caught_up();
                processing.pin_stereo();
                processing.collected();
            }
            processing.render(far);
            processing.cancel_echo(frame, 20);
        }
        out
    }

    /// Far voice, and a near voice over its echo.
    fn talking_room(len: usize) -> (Vec<f32>, Vec<f32>) {
        let far = voice(len, 120.0, -20.0);
        let near = room(&far, 60).iter().zip(voice(len, 200.0, -24.0)).map(|(echo, voice)| echo + voice).collect();
        (far, near)
    }

    #[test]
    fn warming_leaves_voice_alone_to_the_bit() {
        let len = 8 * RATE;
        let (far, near) = talking_room(len);
        let plain = through(&mut session(LEARNING_FRAMES), &pairs(&far), &near, None);
        let mut warm = warm_session();
        let warmed = warm_through(&mut warm, &pairs(&far), &near, None);
        assert!(warm.is_mono());
        warm.caught_up();
        assert!(warm.sound_heard_warming() > 300, "it really heard: {}", warm.sound_heard_warming());
        assert!(plain.iter().zip(&warmed).all(|(a, b)| a.to_bits() == b.to_bits()), "voice alone changed");
    }

    #[test]
    fn a_full_ring_costs_the_warming_canceller_not_the_voice() {
        let len = 8 * RATE;
        let (far, near) = talking_room(len);
        let far = pairs(&far);
        let plain = through(&mut session(LEARNING_FRAMES), &far, &near, None);
        // Far faster than the worker, which has nothing to say about it.
        let mut warm = warm_session();
        let warmed = through(&mut warm, &far, &near, None);
        assert!(plain.iter().zip(&warmed).all(|(a, b)| a.to_bits() == b.to_bits()), "voice alone changed");
        let quiet = vec![0.0; RATE];
        through(&mut warm, &far[..RATE], &near[..RATE], Some(0));
        warm.collected();
        through(&mut warm, &pairs(&voice(4 * RATE, 120.0, -20.0)), &quiet[..RATE], None);
        through(&mut warm, &pairs(&voice(4 * RATE, 120.0, -20.0)), &vec![0.0; 4 * RATE], None);
        assert!(warm.is_stereo(), "it still gets there");
    }

    #[test]
    fn a_warmed_canceller_takes_over_within_frames_of_the_pin() {
        let len = 7 * RATE;
        let far = pairs(&voice(len, 120.0, -20.0));
        let left: Vec<f32> = far.iter().map(|pair| pair[0]).collect();
        let echo = room(&left, 60);
        let mut processing = warm_session();
        warm_through(&mut processing, &far[..6 * RATE], &echo[..6 * RATE], None);
        processing.caught_up();
        processing.pin_stereo();
        processing.collected();
        assert_eq!(processing.left_to_hear(), Some(HANDOVER_FRAMES));
        let after = 6 * RATE + 12 * FRAME;
        warm_through(&mut processing, &far[6 * RATE..after], &echo[6 * RATE..after], None);
        assert!(processing.is_stereo());
    }

    #[test]
    fn a_canceller_warmed_too_little_still_learns_the_rest_on_the_side() {
        // Long enough a wait that the room is found well before it is over.
        let far = pairs(&voice(10 * RATE, 120.0, -20.0));
        let left: Vec<f32> = far.iter().map(|pair| pair[0]).collect();
        let echo = room(&left, 60);
        let wait = 3 * LEARNING_FRAMES;
        let found = (1..=20).find_map(|step| {
            let upto = step * RATE / 2;
            let mut processing = session(wait);
            processing.warm_stereo();
            warm_through(&mut processing, &far[..upto], &echo[..upto], None);
            processing.caught_up();
            let sound = processing.sound_heard_warming();
            processing.pin_stereo();
            processing.collected();
            processing.left_to_hear().filter(|left| *left < wait).map(|left| (sound, left))
        });
        let (sound, left_to_hear) = found.expect("the room was found");
        assert!((1..wait).contains(&sound), "{sound}");
        assert_eq!(left_to_hear, wait - sound);
    }

    #[test]
    fn headphones_go_the_way_of_a_session_that_never_warmed() {
        // A voice that overlaps the far one, with no echo at all.
        let len = 12 * RATE;
        let far = pairs(&voice(len, 100.0, -20.0));
        let near = voice(len, 200.0, -24.0);
        let mut warm = warm_session();
        warm_through(&mut warm, &far, &near, None);
        warm.caught_up();
        assert!(warm.sound_heard_warming() > LEARNING_FRAMES, "it heard plenty");
        warm.pin_stereo();
        warm.collected();
        assert_eq!(warm.left_to_hear(), Some(LEARNING_FRAMES), "and starts again, as if it had not");
    }

    #[test]
    fn a_silent_session_gives_the_warming_canceller_nothing_to_do() {
        let mut processing = warm_session();
        let silent = vec![0.0; 3 * RATE];
        through(&mut processing, &pairs(&silent), &voice(3 * RATE, 200.0, -24.0), None);
        // Nobody played anything: the microphone alone is not worth a frame of the worker.
        assert_eq!(processing.stereo.as_ref().unwrap().feed.slots(), WARMING_RING);
        assert_eq!(processing.fed_frames, 0);
        processing.pin_stereo();
        processing.collected();
        assert_eq!(processing.left_to_hear(), Some(LEARNING_FRAMES), "and it learned nothing");
    }

    #[test]
    fn warming_rests_when_the_speakers_do_and_keeps_the_echo_tail() {
        let mut processing = warm_session();
        let sound = vec![[0.1; 2]; RATE];
        let silent = vec![0.0; RATE];
        warm_through(&mut processing, &sound, &silent, None);
        let fed = processing.fed_frames;
        warm_through(&mut processing, &pairs(&vec![0.0; 2 * RATE]), &vec![0.0; 2 * RATE], None);
        assert_eq!(processing.fed_frames - fed, WARMING_TAIL - 1, "the tail, then nothing");
        warm_through(&mut processing, &sound, &silent, None);
        assert_eq!(processing.fed_frames - fed - (WARMING_TAIL - 1), 100, "and back with the sound");
    }

    #[test]
    fn warming_pauses_with_the_canceller() {
        let mut processing = warm_session();
        let far = pairs(&voice(2 * RATE, 120.0, -20.0));
        let left: Vec<f32> = far.iter().map(|pair| pair[0]).collect();
        let echo = room(&left, 60);
        warm_through(&mut processing, &far, &echo, None);
        let fed = processing.fed_frames;
        processing.configure(false, false);
        assert_eq!(warm_through(&mut processing, &far, &echo, None), echo, "off passes the microphone untouched");
        assert_eq!(processing.fed_frames, fed);
        processing.configure(true, false);
        warm_through(&mut processing, &far, &echo, None);
        assert!(processing.fed_frames > fed, "and it goes on with the canceller");
        assert!(processing.is_mono());
    }

    #[test]
    fn a_warmed_session_ends_its_worker() {
        let mut processing = warm_session();
        let far = pairs(&voice(RATE, 120.0, -20.0));
        through(&mut processing, &far, &vec![0.0; RATE], None);
        let order = processing.stereo.as_ref().unwrap().order.clone();
        drop(processing);
        let started = std::time::Instant::now();
        while Arc::strong_count(&order) > 1 {
            assert!(started.elapsed().as_secs() < 5, "the worker is still there");
            thread::yield_now();
        }
    }

    #[test]
    fn the_warmed_switch_has_no_click_and_no_gap() {
        let len = 12 * RATE;
        let far = pairs(&voice(len, 120.0, -20.0));
        let left: Vec<f32> = far.iter().map(|pair| pair[0]).collect();
        let pin = 8 * RATE;
        // The far side alone first, so that the room is found; then both.
        let talking = |index: usize, voice: f32| if index < 5 * RATE { 0.0 } else { voice };
        let near: Vec<f32> = room(&left, 60)
            .iter()
            .zip(voice(len, 200.0, -24.0))
            .enumerate()
            .map(|(index, (echo, voice))| echo + talking(index, voice))
            .collect();
        let alone = through(&mut session(LEARNING_FRAMES), &far, &near, None);
        let switched = warm_through(&mut warm_session(), &far, &near, Some(pin));
        assert_eq!(alone[..pin], switched[..pin], "nothing moved before the pin");
        let mut warmed = warm_session();
        warm_through(&mut warmed, &far[..pin], &near[..pin], None);
        warmed.caught_up();
        warmed.pin_stereo();
        warmed.collected();
        assert_eq!(warmed.left_to_hear(), Some(HANDOVER_FRAMES), "this was the warmed way");
        let around = pin - 2 * FRAME..pin + 16 * FRAME;
        let step = |signal: &[f32]| signal.windows(2).map(|pair| (pair[1] - pair[0]).abs()).fold(0.0, f32::max);
        let steady = step(&alone[RATE..pin]);
        assert!(step(&switched[around.clone()]) <= steady, "a click at the switch");
        let change = db(&switched[around.clone()]) - db(&alone[around]);
        assert!(change.abs() < 3.0, "voice jumped {change:.1} dB at the switch");
    }

    #[test]
    fn wide_sound_from_the_first_moment_after_voice_does_not_leak() {
        let sound = voice(12 * RATE, 150.0, -18.0);
        let mut far = pairs(&sound[..8 * RATE]);
        far.extend(wide(&sound[8 * RATE..]));
        let left: Vec<f32> = far.iter().map(|pair| pair[0]).collect();
        let echo = room(&left, 60);
        let out = warm_through(&mut warm_session(), &far, &echo, Some(8 * RATE));
        let leak = db(&out[8 * RATE..8 * RATE + RATE / 4]);
        assert!(leak < -55.0, "{leak:.1} dBFS in the first quarter second");
        let settled = db(&out[10 * RATE..]);
        assert!(settled < -60.0, "{settled:.1} dBFS");
    }

    #[test]
    fn the_warmed_way_to_stereo_never_touches_the_heap_on_the_voice_thread() {
        let sound = voice(6 * RATE, 150.0, -18.0);
        let echo = room(&sound, 60);
        let far = pairs(&sound);
        let step = |processing: &mut Processing, at: &mut usize| {
            let (far, mut near) = (&far[*at..*at + FRAME], [0f32; FRAME]);
            near.copy_from_slice(&echo[*at..*at + FRAME]);
            *at += FRAME;
            heap::trips(|| {
                processing.render(far);
                processing.cancel_echo(&mut near, 20);
            })
        };
        let mut at = 0;
        let mut plain = session(LEARNING_FRAMES);
        let mut warm = warm_session();
        for _ in 0..400 {
            let mut plain_at = at;
            let _ = step(&mut plain, &mut plain_at);
            step(&mut warm, &mut at);
            warm.caught_up();
        }
        let mut plain_at = at;
        let alone = (0..20).map(|_| step(&mut plain, &mut plain_at)).max().unwrap();
        let warming = (0..20).map(|_| step(&mut warm, &mut at)).max().unwrap();
        assert!(warming <= alone, "feeding the worker adds nothing to a frame: {warming} against {alone}");

        warm.caught_up();
        assert_eq!(heap::trips(|| warm.pin_stereo()), 0, "the pin");
        while matches!(warm.channels, Channels::Building) {
            assert_eq!(heap::trips(|| warm.collect_stereo()), 0, "taking what was warmed");
            thread::yield_now();
        }
        assert_eq!(warm.left_to_hear(), Some(HANDOVER_FRAMES), "it found the room");
        let mut frame = [0f32; FRAME];
        assert_eq!(heap::trips(|| warm.take_over(Some(&mut frame))), 0, "the take-over");
        assert!(warm.is_stereo());
    }

    #[test]
    #[ignore = "printed by hand beside voice_alone_fingerprint"]
    fn warmed_voice_alone_fingerprint() {
        let mut hash = 0xcbf29ce484222325u64;
        for (far_pitch, delay) in [(100.0, 20), (150.0, 60), (230.0, 150)] {
            let len = 20 * RATE;
            let far = voice(len, far_pitch, -20.0);
            let near: Vec<f32> = room(&far, delay).iter().zip(voice(len, 200.0, -24.0)).map(|(e, n)| e + n).collect();
            let mut processing = Processing::new();
            processing.configure(true, true);
            processing.warm_stereo();
            for sample in run(&mut processing, &far, &near) {
                hash = (hash ^ u64::from(sample.to_bits())).wrapping_mul(0x100000001b3);
            }
        }
        eprintln!("warmed voice alone fingerprint {hash:016x}");
    }

    /// The numbers behind [`Processing::warm_stereo`]: each window after the share's first sound,
    /// in dBFS, as the mean over the rooms of [`the_switch_against_staying_mono`] with `stay` the
    /// session that never pinned, `cold` the canceller that starts learning at the pin and `warm`
    /// the one that has been listening. `worst` is the room furthest above `stay`, in dB.
    #[test]
    #[ignore = "a comparison, run in release"]
    fn the_warmed_switch_against_cold_and_staying_mono() {
        let second = RATE;
        let windows: Vec<(usize, usize)> = (0..8).map(|at| (at, at + 1)).chain([(8, 12), (12, 16), (16, 24)]).collect();
        for before in [6, 20] {
            let pin = before * second;
            let total = pin + 24 * second;
            let span = |at: (usize, usize)| pin + at.0 * second..pin + at.1 * second;
            let mut sum = vec![[[0f32; 3]; 3]; windows.len()];
            let mut worst = vec![[[f32::MIN; 2]; 3]; windows.len()];
            let mut loudest = vec![[[f32::MIN; 2]; 3]; windows.len()];
            let mut count = 0.0;
            for far_pitch in [100.0, 150.0, 230.0] {
                for delay in [20, 60, 150] {
                    for near_pitch in [180.0, 280.0] {
                        count += 1.0;
                        let far = voice(total, far_pitch, -20.0);
                        let echo = room(&far, delay);
                        // The near voice joins 4 s in: far alone first, or nothing finds the room.
                        let near: Vec<f32> = voice(total, near_pitch, -24.0)
                            .iter()
                            .enumerate()
                            .map(|(index, sample)| if index < 4 * second { 0.0 } else { *sample })
                            .collect();
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
                        let cold =
                            |far: &[[f32; 2]], mic: &[f32]| through(&mut session(LEARNING_FRAMES), far, mic, Some(pin));
                        let warm =
                            |far: &[[f32; 2]], mic: &[f32]| warm_through(&mut warm_session(), far, mic, Some(pin));
                        let stay_echo = through(&mut session(LEARNING_FRAMES), &same, &echo, None);
                        let stay_talk = through(&mut session(LEARNING_FRAMES), &same, &talk, None);
                        let arms = [
                            [cold(&same, &echo), warm(&same, &echo)],
                            [cold(&same, &talk), warm(&same, &talk)],
                            [cold(&shared, &heard), warm(&shared, &heard)],
                        ];
                        for (index, window) in windows.iter().enumerate() {
                            let r = span(*window);
                            let kept = |out: &[f32]| fidelity(&out[r.clone()], &near[r.clone()]).1;
                            let rows: [(f32, [f32; 2]); 3] = [
                                (db(&stay_echo[r.clone()]), [db(&arms[0][0][r.clone()]), db(&arms[0][1][r.clone()])]),
                                (db(&stay_talk[r.clone()]), [db(&arms[1][0][r.clone()]), db(&arms[1][1][r.clone()])]),
                                (kept(&stay_talk), [kept(&arms[2][0]), kept(&arms[2][1])]),
                            ];
                            for (row, (stay, moved)) in rows.iter().enumerate() {
                                sum[index][row][0] += stay;
                                for arm in 0..2 {
                                    sum[index][row][arm + 1] += moved[arm];
                                    let off = [moved[arm] - stay, (moved[arm] - stay).abs(), stay - moved[arm]][row];
                                    worst[index][row][arm] = worst[index][row][arm].max(off);
                                    loudest[index][row][arm] = loudest[index][row][arm].max(moved[arm]);
                                }
                            }
                        }
                    }
                }
            }
            for (row, name) in ["echo", "level", "kept beside wide share"].iter().enumerate() {
                for (index, window) in windows.iter().enumerate() {
                    let [stay, cold, warm] = sum[index][row].map(|total| total / count);
                    eprintln!(
                        "pin {before} {name} {}-{} s: stay {stay:.1} cold {cold:.1} warm {warm:.1}, worst room {:.1}/{:.1} dB, loudest {:.1}/{:.1}",
                        window.0,
                        window.1,
                        worst[index][row][0],
                        worst[index][row][1],
                        loudest[index][row][0],
                        loudest[index][row][1],
                    );
                }
            }
        }
    }

    /// Wide sound from the first share sound, in dBFS, in `WIDE_WINDOWS` after it. `same` is the
    /// voice's own pitch, `new` a pitch the canceller never heard. `steady` trains on [`voice`] past
    /// the 140 s where it stops moving, so on one held note: the one case found that still leaks.
    /// `speakers` is mid and side music-like sound where both speakers reach the microphone.
    #[test]
    #[ignore = "a comparison, run in release"]
    fn the_wide_leak_cold_and_warm() {
        let second = RATE;
        let windows = [(0, 1), (1, 2), (2, 4), (4, 6), (6, 8), (8, 12), (12, 16), (16, 24), (32, 48)];
        let show = |name: String, pin: usize, cold: &[f32], warm: &[f32]| {
            for (arm, out) in [("cold", cold), ("warm", warm)] {
                let levels: Vec<String> = windows
                    .iter()
                    .map(|(from, to)| format!("{:.1}", db(&out[pin + from * second / 4..pin + to * second / 4])))
                    .collect();
                eprintln!("wide {name} {arm}: {}", levels.join(" "));
            }
        };
        for pin_at in [6, 20, 60, 310] {
            let pin = pin_at * second;
            for (name, before, after) in [("same", 120.0, 120.0), ("same", 190.0, 190.0), ("new", 150.0, 120.0)] {
                let mut far = pairs(&long_voice(pin, before, -18.0));
                far.extend(wide(&long_voice(12 * second, after, -18.0)));
                let left: Vec<f32> = far.iter().map(|pair| pair[0]).collect();
                let echo = room(&left, 60);
                let cold = through(&mut session(LEARNING_FRAMES), &far, &echo, Some(pin));
                let warm = warm_through(&mut warm_session(), &far, &echo, Some(pin));
                show(format!("{name} {before}->{after} pin {pin_at}"), pin, &cold, &warm);
            }
            let talk = long_voice(pin + 12 * second, 150.0, -18.0);
            let mid = long_voice(pin + 12 * second, 120.0, -20.0);
            let side = long_voice(pin + 12 * second, 170.0, -28.0);
            let far: Vec<[f32; 2]> = (0..talk.len())
                .map(|at| if at < pin { [talk[at]; 2] } else { [mid[at] + side[at], mid[at] - side[at]] })
                .collect();
            let left: Vec<f32> = far.iter().map(|pair| pair[0]).collect();
            let right: Vec<f32> = far.iter().map(|pair| pair[1]).collect();
            let echo: Vec<f32> = room(&left, 60).iter().zip(room(&right, 75)).map(|(l, r)| l + 0.7 * r).collect();
            let cold = through(&mut session(LEARNING_FRAMES), &far, &echo, Some(pin));
            let warm = warm_through(&mut warm_session(), &far, &echo, Some(pin));
            show(format!("speakers pin {pin_at}"), pin, &cold, &warm);
        }
        let pin = 310 * second;
        let mut far = pairs(&voice(pin, 150.0, -18.0));
        far.extend(wide(&long_voice(12 * second, 120.0, -18.0)));
        let left: Vec<f32> = far.iter().map(|pair| pair[0]).collect();
        let echo = room(&left, 60);
        let cold = through(&mut session(LEARNING_FRAMES), &far, &echo, Some(pin));
        let warm = warm_through(&mut warm_session(), &far, &echo, Some(pin));
        show("steady pin 310".into(), pin, &cold, &warm);
    }

    /// How much of a near voice speaking in turns gets through, from the first share sound: the
    /// share of the frames it speaks in that come out within 10 dB of it. On speakers, frames
    /// that only leak also count as through, so `cold`'s first 3 s read better than they sound.
    #[test]
    #[ignore = "a comparison, run in release"]
    fn the_near_voice_cold_and_warm() {
        let windows = [(0usize, 3usize), (3, 8), (8, 16), (16, 32)];
        for lead_at in [6usize, 30] {
            for (name, speakers) in [("headphones", false), ("speakers", true)] {
                let mut sum = [[0f32; 4]; 3];
                let mut count = 0.0;
                for offset in 0..8 {
                    let pin = lead_at * RATE + offset * 130;
                    let total = pin + 32 * RATE;
                    let talk = long_voice(total, 150.0, -20.0);
                    let (left, right) = (long_voice(total, 120.0, -14.0), long_voice(total, 190.0, -14.0));
                    let far: Vec<[f32; 2]> =
                        (0..total).map(|i| if i < pin { [talk[i]; 2] } else { [left[i], right[i]] }).collect();
                    let said: Vec<f32> = long_voice(total, 200.0, -24.0)
                        .iter()
                        .enumerate()
                        .map(|(index, sample)| if (index / (2 * RATE)).is_multiple_of(2) { 0.0 } else { *sample })
                        .collect();
                    let near: Vec<f32> = if speakers {
                        let channel = |side: usize| far.iter().map(|pair| pair[side]).collect::<Vec<f32>>();
                        let (l, r) = (room(&channel(0), 60), room(&channel(1), 75));
                        (0..total).map(|i| l[i] + 0.7 * r[i] + said[i]).collect()
                    } else {
                        said.clone()
                    };
                    let stay = through(&mut session(LEARNING_FRAMES), &far, &near, None);
                    let cold = through(&mut session(LEARNING_FRAMES), &far, &near, Some(pin));
                    let warm = warm_through(&mut warm_session(), &far, &near, Some(pin));
                    count += 1.0;
                    for (arm, out) in [&stay, &cold, &warm].iter().enumerate() {
                        for (index, (from, to)) in windows.iter().enumerate() {
                            let range = pin + from * RATE..pin + to * RATE;
                            let frames = out[range.clone()]
                                .as_chunks::<FRAME>()
                                .0
                                .iter()
                                .zip(said[range].as_chunks::<FRAME>().0);
                            let (mut speaking, mut kept) = (0.0f32, 0.0f32);
                            for (out, said) in frames.filter(|(_, said)| db(&said[..]) > -40.0) {
                                speaking += 1.0;
                                kept += f32::from(u8::from(db(&out[..]) > db(&said[..]) - 10.0));
                            }
                            sum[arm][index] += kept / speaking;
                        }
                    }
                }
                for (arm, row) in ["stay", "cold", "warm"].iter().zip(&sum) {
                    let cells: Vec<String> = row.iter().map(|total| format!("{:.2}", total / count)).collect();
                    eprintln!("near {name} after {lead_at} s {arm}: {}", cells.join(" "));
                }
            }
        }
    }

    /// [`voice`] for long sessions: `f32` phase stops following the pitch after about 140 s.
    fn long_voice(len: usize, pitch: f32, dbfs: f32) -> Vec<f32> {
        let pitch = f64::from(pitch);
        let mut phase = 0f64;
        let mut out: Vec<f32> = (0..len)
            .map(|i| {
                let t = i as f64 / RATE as f64;
                let f0 = pitch * (1.0 + 0.15 * (std::f64::consts::TAU * 2.7 * t).sin());
                phase = (phase + std::f64::consts::TAU * f0 / RATE as f64) % std::f64::consts::TAU;
                let harmonics: f64 =
                    (1..).map(f64::from).take_while(|h| h * f0 < 5_000.0).map(|h| (h * phase).sin() / h).sum();
                (harmonics * (std::f64::consts::PI * ((4.0 * t) % 1.0)).sin().powi(2)) as f32
            })
            .collect();
        let gain = 10f32.powf(dbfs / 20.0) / (out.iter().map(|x| x * x).sum::<f32>() / len as f32).sqrt();
        out.iter_mut().for_each(|x| *x *= gain);
        out
    }

    /// What warming costs: the worker's canceller, on whatever core runs this, the voice thread's
    /// share of feeding it, and the memory. `taskpolicy -b` puts a whole run on the slow cores.
    #[test]
    #[ignore = "a timing measurement, run in release"]
    fn warming_cost() {
        let len = 30 * RATE;
        let left = voice(len, 120.0, -20.0);
        let echo: Vec<f32> = room(&left, 60);
        let block = |signal: &[f32], index: usize| -> [f32; FRAME] {
            signal[index * FRAME..(index + 1) * FRAME].try_into().unwrap()
        };
        let mono = {
            let mut processing = session(LEARNING_FRAMES);
            frame_cost(|index| {
                let (far, mut near) = (block(&left, index), block(&echo, index));
                processing.render(&far.map(|sample| [sample; 2]));
                processing.cancel_echo(&mut near, 20);
            })
        };
        // The same frames, feeding the worker, which is kept up with outside the timed part.
        let fed = {
            let mut processing = warm_session();
            let mut spent = Vec::new();
            for index in 0..3_000 {
                let (far, mut near) = (block(&left, index), block(&echo, index));
                let far = far.map(|sample| [sample; 2]);
                let started = std::time::Instant::now();
                processing.render(&far);
                processing.cancel_echo(&mut near, 20);
                spent.push(started.elapsed());
                if index.is_multiple_of(16) {
                    processing.caught_up();
                }
            }
            spent.drain(..300);
            spent.sort();
            (spent[spent.len() / 2], spent[spent.len() * 99 / 100])
        };
        let mut stage = Stage::stereo();
        let (mut left_out, mut right_out, mut near_out) = ([0f32; FRAME], [0f32; FRAME], [0f32; FRAME]);
        let shadow = frame_cost(|index| {
            let (far, near) = (block(&left, index), block(&echo, index));
            let _ = stage.apm.process_render_f32(&[&far, &far], &mut [&mut left_out, &mut right_out]);
            let _ = stage.apm.set_stream_delay_ms(20);
            let _ = stage.apm.process_capture_f32(&[&near], &mut [&mut near_out]);
        });
        let budget = std::time::Duration::from_millis(10);
        for (name, (median, p99)) in [("mono on the voice thread", mono), ("and feeding", fed), ("worker", shadow)] {
            let share = 100.0 * median.as_secs_f64() / budget.as_secs_f64();
            eprintln!("{name}: median {median:?} ({share:.2}% of a core), p99 {p99:?}");
        }
        let started = std::time::Instant::now();
        let (bytes, built) = heap::bytes(Stage::stereo);
        eprintln!("a stereo canceller: {} kB, built in {:?}", bytes / 1024, started.elapsed());
        drop(built);
        eprintln!("the ring: {} kB", WARMING_RING * std::mem::size_of::<Heard>() / 1024);
    }
}
