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
/// the voice thread: nothing has used its output yet, which has to be in step before the crossfade.
const HANDOVER_FRAMES: usize = 5;
/// How far the warming canceller must take the microphone down, in dB, before it counts as having
/// found an echo. Measured beside a voice that overlaps the far one, the most headphones reach is
/// 9.1 dB; speakers pass 12 within seconds of the far side talking alone, unless they are very
/// quiet. Nothing else tells the two apart, and they want opposite things:
/// [`Processing::warm_stereo`].
const ECHO_FOUND: f64 = 12.0;
/// Frames the warming canceller may fall behind, render and capture counted apart: 640 ms.
const WARMING_RING: usize = 128;
/// Samples over which a replacement joins what its predecessor last put out: 2.5 ms.
const SPLICE: usize = 120;
/// How many times a warmed canceller may arrive behind before it is refused: a quarter of a second.
/// It normally arrives caught up the first time.
const HANDOVER_TRIES: usize = 50;
/// How many frames of speakers wait for the worker, so that it wakes 25 times a second.
const WARMING_BATCH: usize = 4;
/// Frames after the speakers fall silent that the warming canceller still hears: the echo's tail.
/// Past that nothing plays and there is nothing to learn, so it costs nothing either.
const WARMING_TAIL: usize = 50;
/// Frames in one look at whether the echo is still there: 250 ms, longer than most echoes take.
const ECHO_WINDOW: usize = 25;
/// How far under the echo a canceller believes in the microphone must be, in dB, for a look to say
/// the echo is gone. Speakers in a room never put the microphone under it: a near voice only adds.
/// Measured, speakers read 7 dB over it at the least, headphones 27 dB under.
const ECHO_GONE: f32 = 15.0;
/// The least echo return loss, in dB, that is a belief in an echo. A canceller that has found none
/// reads -30, AEC3's start.
const ECHO_BELIEVED: f64 = 0.0;
/// Looks at the speakers playing, of the last 16, that must find the echo gone: half a second.
const ECHO_GONE_LOOKS: u32 = 2;

/// One of WebRTC's processors, set up for a single stage (or none, while it is off).
struct Stage {
    apm: AudioProcessing,
    on: bool,
    #[cfg(test)]
    notified_absence: bool,
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
            #[cfg(test)]
            notified_absence: false,
        }
    }

    /// A running stereo canceller, its memory all allocated: this one frame of silence builds what
    /// the first real frame would otherwise build on the thread voice runs on.
    fn stereo() -> Self {
        Self::stereo_config(canceller(true))
    }

    fn stereo_config(config: Config) -> Self {
        let mut stage = Self::new(2);
        stage.apm.apply_config(config);
        stage.on = true;
        let silence = [0f32; FRAME];
        let (mut left, mut right, mut near) = ([0f32; FRAME], [0f32; FRAME], [0f32; FRAME]);
        let _ = stage.apm.process_render_f32(&[&silence, &silence], &mut [&mut left, &mut right]);
        let _ = stage.apm.process_capture_f32(&[&silence], &mut [&mut near]);
        stage
    }

    #[cfg(test)]
    fn native_gain_change(&mut self) {
        self.apm.notify_echo_gain_change();
        self.notified_absence = true;
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
    #[cfg(test)]
    let canceller = EchoCanceller {
        multichannel_suppressor_like_mono: stereo && MEASURE_MONO_SUPPRESSOR.load(Ordering::Relaxed),
        measurement_two_channel_start: stereo && AEC_NATIVE_CHANNELS.load(Ordering::Relaxed),
        measurement_native_profile: if stereo { AEC_NATIVE_PROFILE.load(Ordering::Relaxed) } else { 0 },
        ..canceller
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

/// Whether the echo a canceller learned is still in the microphone. A room that changes without
/// notice, headphones in a jack the system does not report or a default a sound server moved,
/// leaves the canceller believing in an echo that is no longer there, and that belief holds the
/// near voice down. Over windows of [`ECHO_WINDOW`] frames with the speakers playing, it compares
/// the microphone with the echo the canceller's own echo return loss predicts from the speakers. A
/// near voice or a new room's echo only add to the microphone: only an echo that went away, or
/// speakers turned far down, leaves it [`ECHO_GONE`] dB under. A canceller that believes in no
/// echo is never told this. This observer runs only on the warming worker.
#[derive(Default)]
struct EchoGone {
    /// This window's speaker energy, the louder side, and the window's before it.
    speakers: f32,
    before: f32,
    microphone: f32,
    frames: usize,
    /// One bit for each of the last 16 windows with the speakers playing: whether the echo was gone.
    looks: u16,
}

impl EchoGone {
    /// One frame of the speakers, as the canceller hears them.
    fn speakers(&mut self, [left, right]: &[[f32; FRAME]; 2]) {
        let energy = |side: &[f32; FRAME]| side.iter().map(|sample| sample * sample).sum::<f32>();
        self.speakers += energy(left).max(energy(right));
    }

    /// One frame of the microphone, before the canceller; `return_loss` is the canceller's echo
    /// return loss after it. True when the echo is gone: then it starts over.
    fn microphone(&mut self, frame: &[f32; FRAME], return_loss: impl FnOnce() -> Option<f64>) -> bool {
        self.microphone += frame.iter().map(|sample| sample * sample).sum::<f32>();
        self.frames += 1;
        if self.frames < ECHO_WINDOW {
            return false;
        }
        // An echo up to a window late still comes from speakers that played in both windows.
        let played = self.speakers.min(self.before);
        let microphone = self.microphone;
        (self.before, self.speakers, self.microphone, self.frames) = (self.speakers, 0.0, 0.0, 0);
        if played < (ECHO_WINDOW / 2) as f32 * PLAYING {
            return false;
        }
        let Some(loss) = return_loss().filter(|loss| *loss >= ECHO_BELIEVED) else {
            self.looks = 0;
            return false;
        };
        let believed = played * 10f32.powf(-(loss as f32) / 10.0);
        self.looks = self.looks << 1 | u16::from(microphone < believed * 10f32.powf(-ECHO_GONE / 10.0));
        let needed = ECHO_GONE_LOOKS;
        #[cfg(test)]
        let needed = if ROUTE_MEASUREMENT.load(Ordering::Relaxed) >= 2 { needed } else { 4 };
        let gone = self.looks.count_ones() >= needed;
        #[cfg(test)]
        let gone = gone && !ECHO_GONE_IGNORED.load(Ordering::Relaxed);
        if gone {
            self.looks = 0;
        }
        gone
    }
}

/// Comparisons against `origin/main` turn [`EchoGone`] off with this, everywhere in the process:
/// run them alone.
#[cfg(test)]
static ECHO_GONE_IGNORED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Only the isolated channel comparison changes this; run that test alone.
#[cfg(test)]
static AEC_NATIVE_PROFILE: AtomicU8 = AtomicU8::new(0);
#[cfg(test)]
static AEC_WARM_ONLY_TWO: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[cfg(test)]
static AEC_NATIVE_CHANNELS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[cfg(test)]
static AEC_NONLINEAR_AT_TAKEOVER: AtomicU8 = AtomicU8::new(0);
#[cfg(test)]
static AEC_ECHO_COMPARE_PROFILE: AtomicU8 = AtomicU8::new(0);
#[cfg(test)]
static AEC_ECHO_REGRESSIONS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static AEC_ECHO_WORST_BITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
#[cfg(test)]
static AEC_TRACE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[cfg(test)]
static MEASURE_MONO_SUPPRESSOR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Isolated route comparisons: worker only, worker faster, worker faster and kept eligible.
#[cfg(test)]
static ROUTE_MEASUREMENT: AtomicU8 = AtomicU8::new(2);

/// A stereo canceller trained on everything the voice session hears, with no output anyone uses.
struct Warming {
    stage: Box<Stage>,
    /// Frames of sound it has learned from, for [`Processing::collect_stereo`].
    sound: Arc<AtomicUsize>,
    /// Whether it ever reached [`ECHO_FOUND`].
    found_echo: bool,
    /// The unbroken stretch of the session it has heard: [`Processing::break_history`].
    history: u32,
    /// The room it learned may be gone without a notice: then it starts over, as at a break.
    echo: EchoGone,
    /// Measurement only: retain the old filter, but defer its takeover after echo disappears.
    #[cfg(test)]
    absence_seen: bool,
}

impl Warming {
    fn new(sound: &Arc<AtomicUsize>, history: u32) -> Self {
        sound.store(0, Ordering::Relaxed);
        let config = canceller(true);
        #[cfg(test)]
        let config = {
            let mut config = config;
            if AEC_WARM_ONLY_TWO.load(Ordering::Relaxed) {
                config.echo_canceller.as_mut().unwrap().measurement_two_channel_start = true;
            }
            config
        };
        Self {
            stage: Box::new(Stage::stereo_config(config)),
            sound: sound.clone(),
            found_echo: false,
            history,
            echo: EchoGone::default(),
            #[cfg(test)]
            absence_seen: false,
        }
    }

    /// Nothing learned so far counts.
    fn start_over(&mut self) {
        *self.stage = Stage::stereo();
        self.sound.store(0, Ordering::Relaxed);
        self.found_echo = false;
        self.echo = EchoGone::default();
        #[cfg(test)]
        {
            self.absence_seen = false;
        }
    }

    /// Everything waiting in the ring. A frame from a later stretch starts it all over, here on the
    /// worker: nothing learned before a break says anything about what comes after it.
    fn catch_up(&mut self, ring: &mut Consumer<(u32, Heard)>) {
        let (mut left, mut right, mut near) = ([0f32; FRAME], [0f32; FRAME], [0f32; FRAME]);
        while let Ok((history, heard)) = ring.pop() {
            if history != self.history {
                self.history = history;
                self.start_over();
            }
            match heard {
                Heard::Speakers(far) => {
                    let power: f32 = far.iter().flatten().map(|sample| sample * sample).sum();
                    self.sound.fetch_add(usize::from(power >= PLAYING), Ordering::Relaxed);
                    self.echo.speakers(&far);
                    let [far_left, far_right] = &far;
                    let _ = self.stage.apm.process_render_f32(&[far_left, far_right], &mut [&mut left, &mut right]);
                }
                Heard::Microphone(microphone, delay_ms) => {
                    let _ = self.stage.apm.set_stream_delay_ms(delay_ms);
                    let _ = self.stage.apm.process_capture_f32(&[&microphone], &mut [&mut near]);
                    let statistics = self.stage.apm.statistics();
                    let taken_down = statistics.echo_return_loss_enhancement;
                    self.found_echo |= taken_down.is_some_and(|enhancement| enhancement >= ECHO_FOUND);
                    if self.echo.microphone(&microphone, || statistics.echo_return_loss) {
                        #[cfg(test)]
                        if ROUTE_MEASUREMENT.load(Ordering::Relaxed) == 5 {
                            self.echo = EchoGone::default();
                            self.absence_seen = true;
                            continue;
                        }
                        #[cfg(test)]
                        if ROUTE_MEASUREMENT.load(Ordering::Relaxed) == 4 {
                            self.echo = EchoGone::default();
                            if !self.stage.notified_absence {
                                self.stage.native_gain_change();
                            }
                            continue;
                        }
                        #[cfg(test)]
                        let eligible = self.found_echo;
                        self.start_over();
                        #[cfg(test)]
                        if ROUTE_MEASUREMENT.load(Ordering::Relaxed) == 3 {
                            self.found_echo = eligible;
                        }
                    }
                }
            }
        }
    }
}

/// What the voice thread gives back of a warmed canceller it was offered.
enum Returned {
    /// The ring still holds frames it has not heard: it goes on and is offered again.
    Behind(Warming),
    /// It will not do: a new one comes the way one always did.
    Refused(Box<Stage>),
    /// The one in charge learned another room. A new one is to hear `sound` frames of sound from
    /// stretch `history` on, here, and is offered then.
    Replace { sound: usize, history: u32 },
    /// The one just replaced, to be freed here.
    Retired(#[allow(dead_code)] Box<Stage>),
}

/// Waits through a voice session, in case share sound plays, and provides the stereo canceller
/// then. Told to warm, it trains that canceller meanwhile on what the voice thread leaves in a ring;
/// otherwise it builds one when asked. It starts with the session because nothing about starting a
/// thread is fit for the voice thread; feeding it, asking it and taking what it has touch neither
/// the heap nor a lock. A worker that falls a whole ring behind loses frames: that costs it all
/// it learned, and voice nothing, since nothing waits for it.
struct StereoWorker {
    thread: Thread,
    order: Arc<AtomicU8>,
    built: Receiver<Box<Stage>>,
    /// A canceller on offer, with the ring still on the worker's side.
    warmed: Consumer<Warming>,
    returned: Producer<Returned>,
    feed: Producer<(u32, Heard)>,
    #[cfg(test)]
    sound: Arc<AtomicUsize>,
    #[cfg(test)]
    hold: Arc<AtomicU8>,
}

const WAIT: u8 = 0;
const BUILD: u8 = 1;
const LEAVE: u8 = 2;
const WARM: u8 = 3;

impl StereoWorker {
    fn start() -> Option<Self> {
        let (send, built) = sync_channel(1);
        let (feed, heard) = RingBuffer::new(WARMING_RING);
        let (mut offer, warmed) = RingBuffer::new(1);
        // A canceller retired, and a device right after it.
        let (returned, mut back) = RingBuffer::new(2);
        let order = Arc::new(AtomicU8::new(WAIT));
        let sound = Arc::new(AtomicUsize::new(0));
        let told = order.clone();
        let learned = sound.clone();
        #[cfg(test)]
        let hold = Arc::new(AtomicU8::new(0));
        #[cfg(test)]
        let held = hold.clone();
        let work = move || {
            let mut heard = heard;
            let mut warming: Option<Warming> = None;
            loop {
                #[cfg(test)]
                stall(&held, HOLD_WORK);
                let order = told.load(Ordering::Acquire);
                if let Some(warming) = &mut warming {
                    warming.catch_up(&mut heard);
                }
                match order {
                    WAIT => thread::park(),
                    WARM if warming.is_some() => thread::park(),
                    WARM => warming = Some(Warming::new(&learned, 0)),
                    BUILD => match warming.take() {
                        Some(warmed) if warmed.found_echo => {
                            #[cfg(test)]
                            stall(&held, HOLD_DELIVERY);
                            let _ = offer.push(warmed);
                            match wait_for_return(&mut back, &told) {
                                Some(Returned::Behind(warmed)) => warming = Some(warmed),
                                // It goes after the new one, which arrives when it would have anyway.
                                Some(Returned::Refused(stage)) => {
                                    let _ = send.send(Box::new(Stage::stereo()));
                                    drop(stage);
                                    return;
                                }
                                Some(Returned::Replace { sound, history }) => {
                                    return replace_when_told(
                                        (sound, history),
                                        &mut heard,
                                        &mut offer,
                                        &mut back,
                                        &told,
                                        &learned,
                                        #[cfg(test)]
                                        &held,
                                    );
                                }
                                _ => return,
                            }
                        }
                        warmed => {
                            let _ = send.send(Box::new(Stage::stereo()));
                            drop(warmed);
                            return;
                        }
                    },
                    _ => return,
                }
            }
        };
        match thread::Builder::new().name("gwar-stereo-canceller".into()).spawn(work) {
            Ok(worker) => Some(Self {
                thread: worker.thread().clone(),
                order,
                built,
                warmed,
                returned,
                feed,
                #[cfg(test)]
                sound,
                #[cfg(test)]
                hold,
            }),
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

/// What is left of the worker once a warmed canceller is in charge: each time the voice thread says
/// that canceller's room is gone, one new canceller heard from there on, offered when it has heard
/// as much as one that learned beside the mono canceller.
fn replace_when_told(
    (mut needed, history): (usize, u32),
    ring: &mut Consumer<(u32, Heard)>,
    offer: &mut Producer<Warming>,
    back: &mut Consumer<Returned>,
    told: &AtomicU8,
    learned: &Arc<AtomicUsize>,
    #[cfg(test)] held: &AtomicU8,
) {
    let mut learning = Some(Warming::new(learned, history));
    loop {
        if let Some(mut new) = learning.take() {
            new.catch_up(ring);
            if learned.load(Ordering::Relaxed) < needed {
                learning = Some(new);
            } else {
                #[cfg(test)]
                stall(held, HOLD_DELIVERY);
                let _ = offer.push(new);
            }
        }
        match back.pop() {
            Ok(Returned::Behind(new)) => learning = Some(new),
            Ok(Returned::Replace { sound, history }) => {
                needed = sound;
                learning = Some(Warming::new(learned, history));
            }
            // What is retired ends here.
            Ok(_) => {}
            Err(_) if told.load(Ordering::Acquire) == LEAVE => return,
            Err(_) => thread::park(),
        }
    }
}

/// Asleep until the warmed canceller it offered is returned; `None` once the session is over.
fn wait_for_return(back: &mut Consumer<Returned>, told: &AtomicU8) -> Option<Returned> {
    loop {
        if let Ok(returned) = back.pop() {
            return Some(returned);
        }
        if told.load(Ordering::Acquire) == LEAVE {
            return None;
        }
        thread::park();
    }
}

/// Where a test keeps the worker from getting on: as a busy machine would.
#[cfg(test)]
const HOLD_WORK: u8 = 1;
#[cfg(test)]
const HOLD_DELIVERY: u8 = 2;

/// Set by the worker for as long as it is held.
#[cfg(test)]
const HELD: u8 = 4;

#[cfg(test)]
fn stall(hold: &AtomicU8, place: u8) {
    while hold.load(Ordering::Acquire) & !HELD == place {
        hold.fetch_or(HELD, Ordering::AcqRel);
        thread::yield_now();
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
    /// The worker is hearing what the mono canceller does.
    warming: bool,
    /// The unbroken stretch of the session being fed: [`Processing::break_history`].
    history: u32,
    /// Times a warmed canceller was offered behind.
    handover_tries: usize,
    /// The stereo canceller, learning or in charge, comes from the worker: it knows one room.
    warmed: bool,
    /// That room is gone, and the worker is training what replaces the one in charge.
    replacing: bool,
    /// The replacement came in since the last frame out, which ended on `last_sample`.
    spliced: bool,
    last_sample: f32,
    /// Frames since the speakers last played anything, up to [`WARMING_TAIL`].
    silent_frames: usize,
    /// Reproduces the inherited voice-thread detector only in isolated comparisons.
    #[cfg(test)]
    echo_gone: EchoGone,
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
            history: 0,
            handover_tries: 0,
            warmed: false,
            replacing: false,
            spliced: false,
            last_sample: 0.0,
            silent_frames: WARMING_TAIL,
            #[cfg(test)]
            echo_gone: EchoGone::default(),
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
            // It starts from scratch when it is turned on again, so there is no room to outlive.
            self.warmed = false;
            self.warming &= !self.replacing;
            self.replacing = false;
        }
        let stereo = matches!(self.channels, Channels::Stereo(_));
        if self.echo.set(echo, || canceller(stereo)) {
            self.rendered = 0;
            if !echo {
                self.break_history();
            }
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

    /// Nothing heard so far counts: the frames from here on start a new stretch, and the worker
    /// starts over when it reaches them. A canceller offered from an older stretch is refused, so
    /// what takes over has heard an unbroken history, and nothing of another room.
    fn break_history(&mut self) {
        self.history = self.history.wrapping_add(1);
    }

    #[cfg(test)]
    pub(crate) fn history(&self) -> u32 {
        self.history
    }

    /// The microphone or the speakers are other devices now, so the room is another one: speakers
    /// that had an echo may be headphones that have none, which a canceller trained on the echo
    /// would take the voice down for. One rule, for as long as the session lasts, however late
    /// the news comes: a stereo canceller that learned the old room on the worker goes. Beside the
    /// mono one, where voice is not on it, it goes at once and the old path follows. In charge, it
    /// stays until the worker has trained another on the new room, off this thread, for as long as
    /// one learns beside the mono one. Nothing runs here for that but copying the frames: voice
    /// stays stereo until the swap, a frame like any other. One that did not warm stays either
    /// way: it learned here as it always did, and goes on to learn the new room.
    pub(crate) fn route_changed(&mut self) {
        self.break_history();
        #[cfg(test)]
        {
            self.echo_gone = EchoGone::default();
        }
        if !self.warmed {
            return;
        }
        let Some(worker) = &mut self.stereo else { return };
        match self.channels {
            Channels::Learning(..) => {
                let Channels::Learning(stage, _) = std::mem::replace(&mut self.channels, Channels::Building) else {
                    return;
                };
                self.warmed = false;
                let _ = worker.returned.push(Returned::Refused(stage));
            }
            Channels::Stereo(_) if !self.replacing => {
                self.replacing = true;
                self.warming = true;
                let (sound, history) = (self.learning_frames, self.history);
                let _ = worker.returned.push(Returned::Replace { sound, history });
            }
            _ => return,
        }
        worker.thread.unpark();
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
        self.collect_warmed();
        if !matches!(self.channels, Channels::Building) {
            return;
        }
        let Some(worker) = &self.stereo else { return };
        if let Ok(stage) = worker.built.try_recv() {
            self.warming = false;
            self.channels = Channels::Learning(stage, self.learning_frames);
        }
    }

    /// Takes the canceller the worker offers if it has heard every frame so far, before the next
    /// one is given to anybody: then it goes on from the frame after with nothing missing, whatever
    /// kept it, and however far behind that frame the echo comes back. Feeding only stops here for
    /// that reason. It goes back if the ring holds more. A warmed canceller is refused if it never
    /// catches up or its history was broken: voice stays on the mono canceller, which it never left,
    /// while a new stereo one comes and learns, the way a session that never warmed goes. A
    /// replacement goes back as well, since the worker starts it over at a break by itself.
    fn collect_warmed(&mut self) {
        if !self.warming || !(self.replacing || matches!(self.channels, Channels::Building)) {
            return;
        }
        let Some(worker) = &mut self.stereo else { return };
        let Ok(warmed) = worker.warmed.pop() else { return };
        let unbroken = warmed.history == self.history;
        let returned = if unbroken && worker.feed.slots() == WARMING_RING {
            self.warming = false;
            if !self.replacing {
                let left_to_hear = self.learning_frames.saturating_sub(warmed.sound.load(Ordering::Relaxed));
                let left_to_hear = left_to_hear.max(HANDOVER_FRAMES.min(self.learning_frames));
                #[cfg(test)]
                let left_to_hear = if warmed.absence_seen { self.learning_frames } else { left_to_hear };
                self.channels = Channels::Learning(warmed.stage, left_to_hear);
                self.warmed = true;
                return;
            }
            self.replacing = false;
            self.spliced = true;
            #[cfg(test)]
            {
                self.echo_gone = EchoGone::default();
            }
            let mut retired = warmed.stage;
            std::mem::swap(&mut self.echo, &mut *retired);
            Returned::Retired(retired)
        } else if self.replacing || (unbroken && self.handover_tries < HANDOVER_TRIES) {
            self.handover_tries += 1;
            Returned::Behind(warmed)
        } else {
            self.warming = false;
            Returned::Refused(warmed.stage)
        };
        let _ = worker.returned.push(returned);
        worker.thread.unpark();
    }

    /// Whether the warming canceller hears this frame. Called once for each frame of speakers. It
    /// needs nothing to even out when a rest cuts more of one side than the other: what comes after
    /// goes on in the order it happened, and the echo's tail has run out in what it heard last.
    fn warming_hears_speakers(&mut self) -> bool {
        if !self.warming {
            return false;
        }
        let [left, right] = &self.render;
        let power: f32 = left.iter().chain(right).map(|sample| sample * sample).sum();
        self.silent_frames = if power >= PLAYING { 0 } else { (self.silent_frames + 1).min(WARMING_TAIL) };
        self.silent_frames < WARMING_TAIL
    }

    /// Leaves a frame for the warming canceller. Never waits: a frame without room breaks the
    /// history, since one side lost it and the other goes on.
    fn overhear(&mut self, heard: Heard) {
        if let Some(worker) = &mut self.stereo
            && worker.feed.push((self.history, heard)).is_err()
        {
            self.break_history();
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
        #[cfg(test)]
        {
            let mode = AEC_NONLINEAR_AT_TAKEOVER.load(Ordering::Relaxed);
            if mode != 0 && (self.warmed || !AEC_WARM_ONLY_TWO.load(Ordering::Relaxed)) {
                stage.apm.measurement_restart_nonlinear(mode);
            }
        }
        std::mem::swap(&mut self.echo, &mut *stage);
        self.channels = Channels::Stereo(stage);
        #[cfg(test)]
        {
            self.echo_gone = EchoGone::default();
        }
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
        self.collect_warmed();
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
        #[cfg(test)]
        if self.watching_the_echo() {
            self.echo_gone.speakers(&self.render);
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

    /// Reproduces the inherited voice-thread replacement detector for measurements.
    #[cfg(test)]
    fn watching_the_echo(&self) -> bool {
        if !matches!(ROUTE_MEASUREMENT.load(Ordering::Relaxed), 0 | 4) {
            return false;
        }
        self.warmed && !self.replacing && matches!(self.channels, Channels::Stereo(_))
    }

    /// Takes the speaker out of the microphone's whole 10 ms frames, in place. `delay_ms` is how
    /// long the speaker's reference waits before it is played (the canceller finds the rest).
    pub(crate) fn cancel_echo(&mut self, frames: &mut [f32], delay_ms: u32) {
        if !self.echo.on {
            return;
        }
        self.collect_stereo();
        #[cfg(test)]
        let mut echo_gone = false;
        for frame in frames.as_chunks_mut::<FRAME>().0 {
            self.scratch.copy_from_slice(frame);
            if self.warming && self.silent_frames < WARMING_TAIL {
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
            if std::mem::take(&mut self.spliced) {
                // Both cancellers pass a voice alike and neither has the other's output, so what
                // is left to join is one step. It is taken out over the next few milliseconds.
                let step = self.last_sample - frame[0];
                for (index, sample) in frame[..SPLICE].iter_mut().enumerate() {
                    let phase = index as f32 / SPLICE as f32 * std::f32::consts::FRAC_PI_2;
                    *sample += step * phase.cos().powi(2);
                }
            }
            if self.replacing {
                self.last_sample = frame[FRAME - 1];
            }
            #[cfg(test)]
            if self.watching_the_echo() {
                let echo = &self.echo;
                echo_gone |= self.echo_gone.microphone(&self.scratch, || echo.apm.statistics().echo_return_loss);
            }
        }
        // As the notice of a new device would be, between frames.
        #[cfg(test)]
        if echo_gone {
            if ROUTE_MEASUREMENT.load(Ordering::Relaxed) == 4 {
                if !self.echo.notified_absence {
                    self.echo.native_gain_change();
                }
            } else {
                self.route_changed();
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
            self.collected();
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
        /// Waits until the worker has taken everything out of the ring, or has offered a canceller
        /// and is itself waiting for the voice thread, as it would for the next frame.
        fn caught_up(&self) {
            let worker = self.stereo.as_ref().unwrap();
            while worker.feed.slots() < WARMING_RING && worker.warmed.is_empty() {
                worker.thread.unpark();
                thread::yield_now();
            }
        }

        /// Waits as a voice thread does for its next frame, a long time for the worker: until the
        /// ring is empty and, if a replacement has heard enough, it is on offer. These tests run
        /// frames as fast as they can, so an offer can stand while the ring holds more, and go back
        /// and come again for as long as a loaded machine keeps the worker slower than they are.
        fn ready_for_the_next_frame(&mut self) {
            loop {
                self.caught_up();
                let worker = self.stereo.as_ref().unwrap();
                let (empty, offered) = (worker.feed.slots() == WARMING_RING, !worker.warmed.is_empty());
                let due = self.replacing && worker.sound.load(Ordering::Relaxed) >= self.learning_frames;
                if empty && (offered || !due) {
                    return;
                }
                if !empty {
                    self.collect_stereo();
                }
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

        /// Keeps the worker from getting past `place`, or lets it go on with 0. At
        /// [`HOLD_WORK`] it has stopped when this returns; [`HOLD_DELIVERY`] it reaches by itself.
        fn hold_worker(&self, place: u8) {
            let worker = self.stereo.as_ref().unwrap();
            worker.hold.store(place, Ordering::Release);
            while place == HOLD_WORK && worker.hold.load(Ordering::Acquire) & HELD == 0 {
                worker.thread.unpark();
                thread::yield_now();
            }
        }

        /// Waits until the worker has reached where it is held.
        fn worker_held(&self) {
            let worker = self.stereo.as_ref().unwrap();
            while worker.hold.load(Ordering::Acquire) & HELD == 0 {
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

    /// One frame of everything, the way the pipeline gives it, with the worker left to itself.
    fn tick(processing: &mut Processing, far: &[[f32; 2]], near: &[f32]) -> Vec<f32> {
        let mut out = near.to_vec();
        processing.render(far);
        processing.cancel_echo(&mut out, 20);
        out
    }

    /// A session on speakers that has found the room: `seconds` of the far side talking alone.
    fn warmed_on_speakers(far: &[[f32; 2]], echo: &[f32], seconds: usize) -> Processing {
        let mut processing = warm_session();
        warm_through(&mut processing, &far[..seconds * RATE], &echo[..seconds * RATE], None);
        processing.caught_up();
        processing
    }

    fn left_channel(far: &[[f32; 2]]) -> Vec<f32> {
        far.iter().map(|pair| pair[0]).collect()
    }

    #[test]
    fn a_new_device_forgets_the_room() {
        // Speakers for six seconds, headphones for six, and only then share sound.
        let far = pairs(&voice(13 * RATE, 120.0, -20.0));
        let echo = room(&left_channel(&far), 60);
        let near = voice(13 * RATE, 200.0, -24.0);
        let mut processing = warmed_on_speakers(&far, &echo, 6);
        processing.route_changed();
        warm_through(&mut processing, &far[6 * RATE..12 * RATE], &near[6 * RATE..12 * RATE], None);
        processing.caught_up();
        assert!(
            processing.sound_heard_warming() < 6 * 100 + 10,
            "the sound of the old room is gone: {}",
            processing.sound_heard_warming()
        );
        processing.pin_stereo();
        processing.collected();
        assert_eq!(processing.left_to_hear(), Some(LEARNING_FRAMES), "and so is what it qualified on");
    }

    #[test]
    fn a_new_device_can_qualify_all_over_again() {
        let far = pairs(&voice(13 * RATE, 120.0, -20.0));
        let echo = room(&left_channel(&far), 60);
        let mut processing = warmed_on_speakers(&far, &echo, 6);
        processing.route_changed();
        warm_through(&mut processing, &far[6 * RATE..13 * RATE], &echo[6 * RATE..13 * RATE], None);
        processing.caught_up();
        processing.pin_stereo();
        processing.collected();
        assert_eq!(processing.left_to_hear(), Some(HANDOVER_FRAMES));
    }

    #[test]
    fn a_new_device_while_the_candidate_is_on_its_way_sends_it_back() {
        let far = pairs(&voice(7 * RATE, 120.0, -20.0));
        let echo = room(&left_channel(&far), 60);
        let mut processing = warmed_on_speakers(&far, &echo, 6);
        processing.hold_worker(HOLD_DELIVERY);
        processing.pin_stereo();
        processing.route_changed();
        processing.hold_worker(0);
        processing.collected();
        assert_eq!(processing.left_to_hear(), Some(LEARNING_FRAMES));
    }

    /// The candidate had taken its place beside the mono canceller when the device changed.
    #[test]
    fn a_new_device_during_the_handover_goes_the_old_way_without_a_click() {
        let len = 12 * RATE;
        let far = pairs(&voice(len, 120.0, -20.0));
        let left = left_channel(&far);
        let pin = 6 * RATE;
        let near: Vec<f32> = room(&left, 60)
            .iter()
            .zip(voice(len, 200.0, -24.0))
            .enumerate()
            .map(|(index, (echo, voice))| echo + if index < 5 * RATE { 0.0 } else { voice })
            .collect();
        let alone = through(&mut session(LEARNING_FRAMES), &far, &near, None);

        let mut processing = warmed_on_speakers(&far, &near, 6);
        let mut out = alone[..pin].to_vec();
        processing.pin_stereo();
        processing.collected();
        assert_eq!(processing.left_to_hear(), Some(HANDOVER_FRAMES));
        processing.route_changed();
        processing.collected();
        assert_eq!(processing.left_to_hear(), Some(LEARNING_FRAMES), "a new one learns beside the mono one");
        let (mut at, mut take_over) = (pin, None);
        while at + FRAME <= len {
            out.extend(tick(&mut processing, &far[at..at + FRAME], &near[at..at + FRAME]));
            at += FRAME;
            if take_over.is_none() && processing.is_stereo() {
                take_over = Some(at);
            }
        }
        let take_over = take_over.expect("the old way gets there");
        assert!(take_over >= pin + LEARNING_FRAMES * FRAME, "after all of it: {take_over}");
        assert_eq!(alone[pin..take_over - FRAME], out[pin..take_over - FRAME], "voice stayed on the mono one");
        let around = take_over - 4 * FRAME..take_over + 4 * FRAME;
        let step = |signal: &[f32]| signal.windows(2).map(|pair| (pair[1] - pair[0]).abs()).fold(0.0, f32::max);
        assert!(step(&out[around.clone()]) <= step(&alone[RATE..take_over]), "a click at the switch");
        let change = db(&out[around.clone()]) - db(&alone[around]);
        assert!(change.abs() < 3.0, "voice jumped {change:.1} dB at the switch");
    }

    /// A session past the take-over on speakers, with the sample it got to.
    fn past_the_take_over(far: &[[f32; 2]], echo: &[f32]) -> (Processing, usize) {
        let mut processing = warmed_on_speakers(far, echo, 6);
        processing.pin_stereo();
        processing.collected();
        let mut at = 6 * RATE;
        while !processing.is_stereo() {
            tick(&mut processing, &far[at..at + FRAME], &echo[at..at + FRAME]);
            at += FRAME;
        }
        assert!(processing.warmed, "it took over warmed");
        (processing, at)
    }

    /// A warmed session on speakers until sample `moved`, then on headphones. The notice of the
    /// new device comes at sample `told`, if at all, and however long after `moved` that takes.
    fn moved_to_headphones(moved: usize, told: Option<usize>, again: Option<usize>, len: usize) -> Vec<f32> {
        let far = pairs(&voice(len, 120.0, -20.0));
        let echo = room(&left_channel(&far), 60);
        let said = voice(len, 200.0, -24.0);
        let (mut processing, mut at) = past_the_take_over(&far, &echo);
        let mut out = vec![0.0; at];
        while at + FRAME <= len {
            if told == Some(at) || again == Some(at) {
                processing.route_changed();
            }
            let near = if at < moved { &echo } else { &said };
            out.extend(tick(&mut processing, &far[at..at + FRAME], &near[at..at + FRAME]));
            processing.ready_for_the_next_frame();
            at += FRAME;
        }
        out
    }

    /// The first sample that came out differently because of the notice.
    fn replaced_at(told: &[f32], untold: &[f32]) -> usize {
        let frames = told.as_chunks::<FRAME>().0.iter().zip(untold.as_chunks::<FRAME>().0);
        let differs = |(index, (told, untold)): (usize, (&[f32; FRAME], &[f32; FRAME]))| {
            told.iter().zip(untold).any(|(a, b)| a.to_bits() != b.to_bits()).then_some(index * FRAME)
        };
        frames.enumerate().find_map(differs).expect("the warmed canceller stayed in charge")
    }

    /// However long after the take-over the notice comes, the canceller from the old room goes.
    #[test]
    fn a_new_device_any_time_after_the_take_over_replaces_the_warmed_canceller() {
        let (len, moved) = (16 * RATE, 7 * RATE);
        let untold = moved_to_headphones(moved, None, None, len);
        for notice in [moved, moved + 100 * FRAME] {
            let told = moved_to_headphones(moved, Some(notice), None, len);
            let at = replaced_at(&told, &untold);
            assert!(at >= notice + LEARNING_FRAMES * FRAME, "it learned the new room first: {at} after {notice}");
            assert!(at <= notice + 2 * LEARNING_FRAMES * FRAME, "and took no longer: {at} after {notice}");
        }
    }

    #[test]
    fn the_replacement_comes_in_without_a_click() {
        let (len, moved) = (14 * RATE, 7 * RATE);
        let untold = moved_to_headphones(moved, None, None, len);
        let told = moved_to_headphones(moved, Some(moved), None, len);
        let at = replaced_at(&told, &untold);
        let step = |signal: &[f32]| signal.windows(2).map(|pair| (pair[1] - pair[0]).abs()).fold(0.0, f32::max);
        let around = at - 4 * FRAME..at + 4 * FRAME;
        assert!(step(&told[around]) <= step(&told[moved..at - 4 * FRAME]), "a click at the swap");
    }

    #[test]
    fn another_new_device_while_one_is_learning_starts_it_over() {
        let (len, moved) = (16 * RATE, 7 * RATE);
        let again = moved + 100 * FRAME;
        let untold = moved_to_headphones(moved, None, None, len);
        let told = moved_to_headphones(moved, Some(moved), Some(again), len);
        let at = replaced_at(&told, &untold);
        assert!(at >= again + LEARNING_FRAMES * FRAME, "it heard the first of them only: {at}");
    }

    #[test]
    fn a_stereo_canceller_that_never_warmed_stays_on_a_new_device() {
        let far = pairs(&voice(6 * RATE, 120.0, -20.0));
        let silent = vec![0.0; 6 * RATE];
        let mut told = session(50);
        let mut untold = session(50);
        let one = through(&mut told, &far[..3 * RATE], &silent[..3 * RATE], Some(RATE));
        assert_eq!(one, through(&mut untold, &far[..3 * RATE], &silent[..3 * RATE], Some(RATE)));
        assert!(told.is_stereo());
        told.route_changed();
        let near = voice(3 * RATE, 200.0, -24.0);
        let told = through(&mut told, &far[3 * RATE..], &near, None);
        let untold = through(&mut untold, &far[3 * RATE..], &near, None);
        assert!(told.iter().zip(&untold).all(|(a, b)| a.to_bits() == b.to_bits()), "as on main");
    }

    #[test]
    fn a_replacement_on_offer_when_the_next_device_comes_starts_over() {
        let len = 20 * RATE;
        let far = pairs(&voice(len, 120.0, -20.0));
        let echo = room(&left_channel(&far), 60);
        let (mut processing, mut at) = past_the_take_over(&far, &echo);
        processing.hold_worker(HOLD_DELIVERY);
        processing.route_changed();
        let held =
            |processing: &Processing| processing.stereo.as_ref().unwrap().hold.load(Ordering::Acquire) & HELD != 0;
        while !held(&processing) {
            tick(&mut processing, &far[at..at + FRAME], &echo[at..at + FRAME]);
            at += FRAME;
            while !held(&processing) && processing.stereo.as_ref().unwrap().feed.slots() < WARMING_RING {
                processing.stereo.as_ref().unwrap().thread.unpark();
                thread::yield_now();
            }
        }
        processing.route_changed();
        processing.hold_worker(0);
        while processing.stereo.as_ref().unwrap().warmed.is_empty() {
            thread::yield_now();
        }
        processing.collect_stereo();
        assert!(processing.replacing, "it learned the wrong room too");
        let frames = 50;
        for _ in 0..frames {
            tick(&mut processing, &far[at..at + FRAME], &echo[at..at + FRAME]);
            processing.caught_up();
            at += FRAME;
        }
        assert!(processing.replacing && processing.sound_heard_warming() <= frames, "it starts over");
    }

    #[test]
    fn turning_the_canceller_off_ends_a_replacement() {
        let far = pairs(&voice(9 * RATE, 120.0, -20.0));
        let echo = room(&left_channel(&far), 60);
        let (mut processing, at) = past_the_take_over(&far, &echo);
        processing.route_changed();
        tick(&mut processing, &far[at..at + FRAME], &echo[at..at + FRAME]);
        processing.configure(false, false);
        let fed = processing.fed_frames;
        assert_eq!(warm_through(&mut processing, &far[at..], &echo[at..], None), echo[at..], "off is off");
        processing.configure(true, false);
        warm_through(&mut processing, &far[at..], &echo[at..], None);
        assert_eq!(processing.fed_frames, fed, "nothing is learning for it now");
        assert!(processing.is_stereo());
    }

    #[test]
    fn a_new_device_leaves_a_cold_canceller_learning() {
        let far = pairs(&voice(3 * RATE, 120.0, -20.0));
        let silent = vec![0.0; 3 * RATE];
        let mut processing = session(LEARNING_FRAMES);
        through(&mut processing, &far, &silent, Some(RATE));
        let left = processing.left_to_hear();
        assert!(left.is_some_and(|left| left < LEARNING_FRAMES));
        processing.route_changed();
        assert_eq!(processing.left_to_hear(), left, "as on main: it is on its way to learning this room");
    }

    /// The worker comes back between render and capture, so that one speaker frame and no
    /// microphone frame was lost, and everything it knows about the delay is a frame out.
    #[test]
    fn a_frame_lost_to_a_full_ring_costs_the_candidate_its_training() {
        let far = pairs(&voice(11 * RATE, 120.0, -20.0));
        let echo = room(&left_channel(&far), 60);
        let mut processing = warmed_on_speakers(&far, &echo, 6);
        processing.hold_worker(HOLD_WORK);
        let mut at = 6 * RATE;
        for _ in 0..WARMING_RING / 2 {
            tick(&mut processing, &far[at..at + FRAME], &echo[at..at + FRAME]);
            at += FRAME;
        }
        assert_eq!(processing.stereo.as_ref().unwrap().feed.slots(), 0, "the ring is exactly full");
        // This frame of speakers finds no room, the microphone's that follows it does.
        processing.render(&far[at..at + FRAME]);
        processing.hold_worker(0);
        processing.caught_up();
        let mut near = [0f32; FRAME];
        near.copy_from_slice(&echo[at..at + FRAME]);
        processing.cancel_echo(&mut near, 20);
        at += FRAME;
        for chunk in 0..50 {
            let from = at + chunk * FRAME;
            tick(&mut processing, &far[from..from + FRAME], &echo[from..from + FRAME]);
            processing.caught_up();
        }
        processing.pin_stereo();
        processing.collected();
        assert_eq!(processing.left_to_hear(), Some(LEARNING_FRAMES), "not continuous, so not handed over");
    }

    #[test]
    fn a_broken_history_can_be_learned_again() {
        let far = pairs(&voice(15 * RATE, 120.0, -20.0));
        let echo = room(&left_channel(&far), 60);
        let mut processing = warmed_on_speakers(&far, &echo, 6);
        processing.hold_worker(HOLD_WORK);
        let frames = WARMING_RING / 2 + 5;
        for index in 0..frames {
            let at = 6 * RATE + index * FRAME;
            tick(&mut processing, &far[at..at + FRAME], &echo[at..at + FRAME]);
        }
        processing.hold_worker(0);
        let from = 6 * RATE + frames * FRAME;
        warm_through(&mut processing, &far[from..from + 8 * RATE], &echo[from..from + 8 * RATE], None);
        processing.caught_up();
        processing.pin_stereo();
        processing.collected();
        assert_eq!(processing.left_to_hear(), Some(HANDOVER_FRAMES));
    }

    /// What is left in the quarter second after a pin right out of a rest, on speakers, in a session
    /// whose rests each took `extra` more frames from the speakers than from the microphone.
    fn leak_after_uneven_rests(extra: usize) -> f32 {
        let sound = voice(20 * RATE, 120.0, -20.0);
        let far = pairs(&sound);
        let echo = room(&sound, 60);
        let (silence, nothing) = (vec![[0f32; 2]; FRAME], [0f32; FRAME]);
        let mut processing = warmed_on_speakers(&far, &echo, 6);
        let mut at = 6 * RATE;
        for _ in 0..3 {
            for _ in 0..50 {
                tick(&mut processing, &far[at..at + FRAME], &echo[at..at + FRAME]);
                processing.caught_up();
                at += FRAME;
            }
            for _ in 0..200 {
                tick(&mut processing, &silence, &nothing);
            }
            for _ in 0..extra {
                processing.render(&silence);
            }
            processing.caught_up();
        }
        processing.pin_stereo();
        processing.collected();
        assert_eq!(processing.left_to_hear(), Some(HANDOVER_FRAMES));
        let mut out = Vec::new();
        for _ in 0..25 {
            out.extend(tick(&mut processing, &far[at..at + FRAME], &echo[at..at + FRAME]));
            at += FRAME;
        }
        db(&out)
    }

    #[test]
    fn rests_need_not_cut_speakers_and_microphone_evenly() {
        let even = leak_after_uneven_rests(0);
        let uneven = leak_after_uneven_rests(8);
        assert!((even - uneven).abs() < 1.0, "{uneven:.1} dBFS uneven against {even:.1} even");
    }

    /// The worker sleeps through the pin and through 100 ms of sound, with the echo 150 ms away.
    /// Five frames beside the mono canceller do not cover what it would then have missed.
    fn echo_after_a_late_candidate(stall_frames: usize) -> f32 {
        let far = pairs(&voice(10 * RATE, 120.0, -20.0));
        let left = left_channel(&far);
        let echo = room(&left, 150);
        let mut processing = warmed_on_speakers(&far, &echo, 8);
        processing.hold_worker(HOLD_DELIVERY);
        processing.pin_stereo();
        processing.worker_held();
        let mut at = 8 * RATE;
        for _ in 0..stall_frames {
            tick(&mut processing, &far[at..at + FRAME], &echo[at..at + FRAME]);
            at += FRAME;
        }
        processing.hold_worker(0);
        processing.collected();
        assert_eq!(processing.left_to_hear(), Some(HANDOVER_FRAMES), "it was not stale");
        let mut out = Vec::new();
        for _ in 0..25 {
            out.extend(tick(&mut processing, &far[at..at + FRAME], &echo[at..at + FRAME]));
            at += FRAME;
        }
        assert!(processing.is_stereo());
        db(&out[10 * FRAME..])
    }

    #[test]
    fn a_late_candidate_hears_all_it_missed_before_it_takes_over() {
        let on_time = echo_after_a_late_candidate(0);
        let late = echo_after_a_late_candidate(10);
        assert!(late < on_time + 6.0, "{late:.1} dBFS late against {on_time:.1} on time");
    }

    #[test]
    fn a_session_that_heard_nothing_goes_exactly_the_way_of_one_that_never_warmed() {
        let far = pairs(&voice(6 * RATE, 120.0, -20.0));
        let near = voice(6 * RATE, 200.0, -24.0);
        let plain = through(&mut session(50), &far, &near, Some(0));
        let mut warm = session(50);
        warm.warm_stereo();
        let warmed = through(&mut warm, &far, &near, Some(0));
        assert!(warm.is_stereo());
        assert!(plain.iter().zip(&warmed).all(|(a, b)| a.to_bits() == b.to_bits()), "a canceller that heard nothing");
    }

    #[test]
    fn a_candidate_offered_behind_goes_back_and_comes_again() {
        let far = pairs(&voice(7 * RATE, 120.0, -20.0));
        let echo = room(&left_channel(&far), 60);
        let mut processing = warmed_on_speakers(&far, &echo, 6);
        processing.hold_worker(HOLD_DELIVERY);
        processing.pin_stereo();
        processing.worker_held();
        // Caught up, and on its way, when a frame of each side goes into the ring behind it.
        let at = 6 * RATE;
        tick(&mut processing, &far[at..at + FRAME], &echo[at..at + FRAME]);
        processing.hold_worker(0);
        while processing.stereo.as_ref().unwrap().warmed.is_empty() {
            thread::yield_now();
        }
        processing.collect_stereo();
        assert!(matches!(processing.channels, Channels::Building), "not with a frame unheard");
        assert_eq!(processing.handover_tries, 1);
        processing.collected();
        assert_eq!(processing.left_to_hear(), Some(HANDOVER_FRAMES));
    }

    #[test]
    fn a_candidate_that_keeps_arriving_behind_is_refused() {
        let far = pairs(&voice(7 * RATE, 120.0, -20.0));
        let echo = room(&left_channel(&far), 60);
        let mut processing = warmed_on_speakers(&far, &echo, 6);
        processing.handover_tries = HANDOVER_TRIES;
        processing.pin_stereo();
        let worker = processing.stereo.as_mut().unwrap();
        while worker.warmed.is_empty() {
            thread::yield_now();
        }
        let _ = worker.feed.push((processing.history, Heard::Speakers([[0.0; FRAME]; 2])));
        processing.collected();
        assert_eq!(processing.left_to_hear(), Some(LEARNING_FRAMES));
    }

    #[test]
    fn a_candidate_that_never_catches_up_is_refused() {
        let far = pairs(&voice(9 * RATE, 120.0, -20.0));
        let echo = room(&left_channel(&far), 60);
        let mut processing = warmed_on_speakers(&far, &echo, 6);
        processing.hold_worker(HOLD_DELIVERY);
        processing.pin_stereo();
        processing.worker_held();
        // Longer than the ring holds.
        for index in 0..WARMING_RING {
            let at = 6 * RATE + index * FRAME;
            tick(&mut processing, &far[at..at + FRAME], &echo[at..at + FRAME]);
        }
        processing.hold_worker(0);
        processing.collected();
        assert_eq!(processing.left_to_hear(), Some(LEARNING_FRAMES));
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

    /// Heap trips of frames through a stereo canceller in charge, without a notice; and from a
    /// notice through the swap, which is one pass like the frames around it.
    #[test]
    fn replacing_a_warmed_canceller_adds_nothing_to_the_heap_on_the_voice_thread() {
        let far = pairs(&voice(11 * RATE, 120.0, -20.0));
        let echo = room(&left_channel(&far), 60);
        let frame = |processing: &mut Processing, at: usize| {
            let (speakers, mut near) = (&far[at..at + FRAME], [0f32; FRAME]);
            near.copy_from_slice(&echo[at..at + FRAME]);
            heap::trips(|| {
                processing.render(speakers);
                processing.cancel_echo(&mut near, 20);
            })
        };
        let (mut plain, start) = past_the_take_over(&far, &echo);
        let mut at = start;
        let mut worst = 0;
        for _ in 0..400 {
            worst = worst.max(frame(&mut plain, at));
            at += FRAME;
        }

        let (mut told, start) = past_the_take_over(&far, &echo);
        assert_eq!(heap::trips(|| told.route_changed()), 0, "the notice");
        let mut at = start;
        let mut replacing = 0;
        while told.replacing {
            replacing = replacing.max(frame(&mut told, at));
            told.ready_for_the_next_frame();
            at += FRAME;
        }
        // The frame after the swap too, which the new canceller is in charge of from the start.
        replacing = replacing.max(frame(&mut told, at));
        assert!(replacing <= worst, "the worst frame costs {replacing} trips against {worst} without a notice");
    }

    #[test]
    fn a_refused_candidate_never_touches_the_heap_on_the_voice_thread() {
        let far = pairs(&voice(7 * RATE, 120.0, -20.0));
        let echo = room(&left_channel(&far), 60);
        let mut stale = warmed_on_speakers(&far, &echo, 6);
        assert_eq!(heap::trips(|| stale.route_changed()), 0, "the new device");
        assert_eq!(heap::trips(|| stale.pin_stereo()), 0, "the pin");
        while stale.warming {
            assert_eq!(heap::trips(|| stale.collect_stereo()), 0, "turning it down");
            thread::yield_now();
        }
        while matches!(stale.channels, Channels::Building) {
            assert_eq!(heap::trips(|| stale.collect_stereo()), 0, "taking the new one");
            thread::yield_now();
        }
        assert_eq!(stale.left_to_hear(), Some(LEARNING_FRAMES));

        let mut learning = warmed_on_speakers(&far, &echo, 6);
        learning.pin_stereo();
        learning.collected();
        assert_eq!(learning.left_to_hear(), Some(HANDOVER_FRAMES));
        assert_eq!(heap::trips(|| learning.route_changed()), 0, "sending it back");
        while matches!(learning.channels, Channels::Building) {
            assert_eq!(heap::trips(|| learning.collect_stereo()), 0, "taking the new one");
            thread::yield_now();
        }
        assert_eq!(learning.left_to_hear(), Some(LEARNING_FRAMES));

        let mut behind = warmed_on_speakers(&far, &echo, 6);
        behind.pin_stereo();
        let worker = behind.stereo.as_mut().unwrap();
        while worker.warmed.is_empty() {
            thread::yield_now();
        }
        let _ = worker.feed.push((behind.history, Heard::Speakers([[0.0; FRAME]; 2])));
        assert_eq!(heap::trips(|| behind.collect_stereo()), 0, "sending it on");
        behind.collected();
        assert_eq!(behind.left_to_hear(), Some(HANDOVER_FRAMES));
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
        ECHO_GONE_IGNORED.store(true, Ordering::Relaxed);
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
                        let near: Vec<f32> = talker(total, near_pitch, 3.1, 3.3, -24.0)
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
                        let profile = AEC_ECHO_COMPARE_PROFILE.load(Ordering::Relaxed);
                        let extra = if profile > 0 {
                            let old = AEC_NATIVE_PROFILE.swap(profile, Ordering::Relaxed);
                            let extra = warm(&same, &echo);
                            AEC_NATIVE_PROFILE.store(old, Ordering::Relaxed);
                            Some(extra)
                        } else {
                            None
                        };
                        for (index, window) in windows.iter().enumerate() {
                            let r = span(*window);
                            if let Some(extra) = &extra {
                                record_native_echo_pair(
                                    &format!(
                                        "mono,pin={before},far={far_pitch},delay={delay},near={near_pitch},s={}-{}",
                                        window.0, window.1
                                    ),
                                    db(&arms[0][1][r.clone()]),
                                    db(&extra[r.clone()]),
                                    db(&arms[0][0][r.clone()]),
                                );
                            }
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
        ECHO_GONE_IGNORED.store(false, Ordering::Relaxed);
    }

    #[test]
    #[ignore = "native-profile comparison on the complete #220 echo tables; run in release alone"]
    fn native_profile_echo_tables() {
        let profile: u8 = std::env::var("GWAR_NATIVE_AEC_PROFILE").unwrap_or_else(|_| "0".into()).parse().unwrap();
        assert!(profile <= 2);
        AEC_NATIVE_PROFILE.store(profile, Ordering::Relaxed);
        eprintln!("native echo tables, profile={profile}");
        the_warmed_switch_against_cold_and_staying_mono();
        ECHO_GONE_IGNORED.store(true, Ordering::Relaxed);
        the_wide_leak_cold_and_warm();
        ECHO_GONE_IGNORED.store(false, Ordering::Relaxed);
        AEC_NATIVE_PROFILE.store(0, Ordering::Relaxed);
    }

    fn record_native_echo_pair(label: &str, current: f32, candidate: f32, cold: f32) {
        let delta = candidate - current;
        if delta > 1.0 {
            AEC_ECHO_REGRESSIONS.fetch_add(1, Ordering::Relaxed);
        }
        AEC_ECHO_WORST_BITS.fetch_max(delta.max(0.0).to_bits(), Ordering::Relaxed);
        eprintln!(
            "native-echo-pair,{label},current={current:.9},candidate={candidate:.9},delta={delta:.9},cold={cold:.9}"
        );
    }

    #[test]
    #[ignore = "strict paired per-room +1 dB gate on full #220 echo tables; run in release alone"]
    fn native_paired_echo_gate() {
        let profile: u8 = std::env::var("GWAR_NATIVE_AEC_PROFILE").unwrap_or_else(|_| "1".into()).parse().unwrap();
        assert!([1, 2, 7, 8, 9, 10, 11].contains(&profile));
        AEC_NATIVE_PROFILE.store(0, Ordering::Relaxed);
        AEC_ECHO_COMPARE_PROFILE.store(profile, Ordering::Relaxed);
        AEC_ECHO_REGRESSIONS.store(0, Ordering::Relaxed);
        AEC_ECHO_WORST_BITS.store(0, Ordering::Relaxed);
        the_warmed_switch_against_cold_and_staying_mono();
        ECHO_GONE_IGNORED.store(true, Ordering::Relaxed);
        the_wide_leak_cold_and_warm();
        ECHO_GONE_IGNORED.store(false, Ordering::Relaxed);
        AEC_ECHO_COMPARE_PROFILE.store(0, Ordering::Relaxed);
        let failures = AEC_ECHO_REGRESSIONS.load(Ordering::Relaxed);
        let worst = f32::from_bits(AEC_ECHO_WORST_BITS.load(Ordering::Relaxed));
        eprintln!("native paired echo gate profile={profile}: failures={failures},worst={worst:.9} dB");
        assert_eq!(failures, 0, "native configuration makes a quiet-echo room/window >1 dB louder");
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
        let show = |name: String, pin: usize, cold: &[f32], warm: &[f32], far: &[[f32; 2]], echo: &[f32]| {
            for (arm, out) in [("cold", cold), ("warm", warm)] {
                let levels: Vec<String> = windows
                    .iter()
                    .map(|(from, to)| format!("{:.1}", db(&out[pin + from * second / 4..pin + to * second / 4])))
                    .collect();
                eprintln!("wide {name} {arm}: {}", levels.join(" "));
            }
            let profile = AEC_ECHO_COMPARE_PROFILE.load(Ordering::Relaxed);
            if profile > 0 {
                let old = AEC_NATIVE_PROFILE.swap(profile, Ordering::Relaxed);
                let extra = warm_through(&mut warm_session(), far, echo, Some(pin));
                AEC_NATIVE_PROFILE.store(old, Ordering::Relaxed);
                for (a, b) in windows {
                    let r = pin + a * second / 4..pin + b * second / 4;
                    record_native_echo_pair(
                        &format!("wide,{name},q={a}-{b}"),
                        db(&warm[r.clone()]),
                        db(&extra[r.clone()]),
                        db(&cold[r]),
                    );
                }
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
                show(format!("{name} {before}->{after} pin {pin_at}"), pin, &cold, &warm, &far, &echo);
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
            show(format!("speakers pin {pin_at}"), pin, &cold, &warm, &far, &echo);
        }
        let pin = 310 * second;
        let mut far = pairs(&voice(pin, 150.0, -18.0));
        far.extend(wide(&long_voice(12 * second, 120.0, -18.0)));
        let left: Vec<f32> = far.iter().map(|pair| pair[0]).collect();
        let echo = room(&left, 60);
        let cold = through(&mut session(LEARNING_FRAMES), &far, &echo, Some(pin));
        let warm = warm_through(&mut warm_session(), &far, &echo, Some(pin));
        show("steady pin 310".into(), pin, &cold, &warm, &far, &echo);
    }

    /// How much of a near voice speaking in turns gets through, from the first share sound: the
    /// share of the frames it speaks in that come out within 10 dB of it. On speakers, frames
    /// that only leak also count as through, so `cold`'s first 3 s read better than they sound.
    #[test]
    #[ignore = "a comparison, run in release"]
    fn the_near_voice_cold_and_warm() {
        ECHO_GONE_IGNORED.store(true, Ordering::Relaxed);
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
                    let said: Vec<f32> = talker(total, 213.0, 3.1, 3.3, -24.0)
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
        ECHO_GONE_IGNORED.store(false, Ordering::Relaxed);
    }

    /// What a history that broke costs, run in release. The share of the frames a voice speaks in
    /// that come out within 10 dB of it, 3–8 s after share sound 6 s after speakers became
    /// headphones; the echo in the quarter second after a pin that follows a frame lost to a full
    /// ring; and the echo 100–250 ms after a canceller that arrived 100 ms late takes over.
    #[test]
    #[ignore = "a comparison, run in release"]
    fn what_a_broken_history_costs() {
        let total = 20 * RATE;
        let pin = 12 * RATE;
        let talk = long_voice(total, 150.0, -20.0);
        let (left, right) = (long_voice(total, 120.0, -14.0), long_voice(total, 190.0, -14.0));
        let far: Vec<[f32; 2]> = (0..total).map(|i| if i < pin { [talk[i]; 2] } else { [left[i], right[i]] }).collect();
        let said: Vec<f32> = long_voice(total, 200.0, -24.0)
            .iter()
            .enumerate()
            .map(|(index, sample)| if (index / (2 * RATE)).is_multiple_of(2) { 0.0 } else { *sample })
            .collect();
        let moved = 6 * RATE;
        let echo = room(&left_channel(&far), 60);
        let near: Vec<f32> = (0..total).map(|i| said[i] + if i < moved { echo[i] } else { 0.0 }).collect();
        let kept = |out: &[f32]| {
            let range = pin + 3 * RATE..pin + 8 * RATE;
            let frames = out[range.clone()].as_chunks::<FRAME>().0.iter().zip(said[range].as_chunks::<FRAME>().0);
            let (mut speaking, mut kept) = (0.0f32, 0.0f32);
            for (out, said) in frames.filter(|(_, said)| db(&said[..]) > -40.0) {
                speaking += 1.0;
                kept += f32::from(u8::from(db(&out[..]) > db(&said[..]) - 10.0));
            }
            kept / speaking
        };
        eprintln!("cold: {:.2}", kept(&through(&mut session(LEARNING_FRAMES), &far, &near, Some(pin))));
        for told in [false, true] {
            let mut processing = warm_session();
            let mut out = warm_through(&mut processing, &far[..moved], &near[..moved], None);
            if told {
                processing.route_changed();
            }
            out.extend(warm_through(&mut processing, &far[moved..], &near[moved..], Some(pin - moved)));
            eprintln!("warm, new device told {told}: {:.2}", kept(&out));
        }

        for broken in [false, true] {
            let far = pairs(&voice(11 * RATE, 120.0, -20.0));
            let echo = room(&left_channel(&far), 60);
            let mut processing = warmed_on_speakers(&far, &echo, 6);
            let mut at = 6 * RATE;
            if broken {
                processing.hold_worker(HOLD_WORK);
                for _ in 0..WARMING_RING / 2 {
                    tick(&mut processing, &far[at..at + FRAME], &echo[at..at + FRAME]);
                    at += FRAME;
                }
                processing.render(&far[at..at + FRAME]);
                processing.hold_worker(0);
                processing.caught_up();
                let mut near = [0f32; FRAME];
                near.copy_from_slice(&echo[at..at + FRAME]);
                processing.cancel_echo(&mut near, 20);
                at += FRAME;
            }
            for _ in 0..50 {
                tick(&mut processing, &far[at..at + FRAME], &echo[at..at + FRAME]);
                processing.caught_up();
                at += FRAME;
            }
            processing.pin_stereo();
            processing.collected();
            let mut out = Vec::new();
            for _ in 0..25 {
                out.extend(tick(&mut processing, &far[at..at + FRAME], &echo[at..at + FRAME]));
                at += FRAME;
            }
            eprintln!("frame lost {broken}: {:.1} dBFS, {:?} left", db(&out), processing.left_to_hear());
        }

        eprintln!("on time: {:.1} dBFS", echo_after_a_late_candidate(0));
        eprintln!("100 ms late: {:.1} dBFS", echo_after_a_late_candidate(10));
    }

    /// A late new device, run in release. The share of a voice's frames kept on headphones, by
    /// seconds after the notice; the echo in a new room; and what the frames on the voice thread
    /// cost: Sonora's heap visits, and the time, beside the frames `origin/main` runs too.
    #[test]
    #[ignore = "a comparison, run in release"]
    fn what_a_late_new_device_costs() {
        let notice = 7 * RATE;
        let total = 24 * RATE;
        let windows = [(0usize, 3usize), (3, 5), (5, 8), (8, 16)];
        let far = pairs(&long_voice(total, 150.0, -20.0));
        let left = left_channel(&far);
        let said: Vec<f32> = long_voice(total, 200.0, -24.0)
            .iter()
            .enumerate()
            .map(|(index, sample)| if (index / (2 * RATE)).is_multiple_of(2) { 0.0 } else { *sample })
            .collect();
        let speakers = room(&left, 60);
        let moved = room(&left, 120);
        let moved: Vec<f32> = moved.iter().map(|echo| 0.7 * echo).collect();
        let arm = |told: bool, near: &[f32]| {
            let (mut processing, mut at) = past_the_take_over(&far, &speakers);
            let mut out = vec![0.0; at];
            while at + FRAME <= total {
                if told && at / FRAME == notice / FRAME {
                    processing.route_changed();
                }
                let near = if at < notice { &speakers } else { near };
                out.extend(tick(&mut processing, &far[at..at + FRAME], &near[at..at + FRAME]));
                processing.ready_for_the_next_frame();
                at += FRAME;
            }
            out
        };
        let cold = |near: &[f32]| {
            let pinned: Vec<f32> = (0..total).map(|i| if i < notice { speakers[i] } else { near[i] }).collect();
            through(&mut session(LEARNING_FRAMES), &far, &pinned, Some(6 * RATE))
        };
        let kept = |out: &[f32], from: usize, to: usize| {
            let range = notice + from * RATE..notice + to * RATE;
            let frames = out[range.clone()].as_chunks::<FRAME>().0.iter().zip(said[range].as_chunks::<FRAME>().0);
            let (mut speaking, mut kept) = (0.0f32, 0.0f32);
            for (out, said) in frames.filter(|(_, said)| db(&said[..]) > -40.0) {
                speaking += 1.0;
                kept += f32::from(u8::from(db(&out[..]) > db(&said[..]) - 10.0));
            }
            kept / speaking
        };
        for (name, out) in [("told", arm(true, &said)), ("untold", arm(false, &said)), ("cold", cold(&said))] {
            let cells: Vec<String> = windows.iter().map(|(a, b)| format!("{:.2}", kept(&out, *a, *b))).collect();
            eprintln!("headphones {name}: kept {}", cells.join(" "));
        }
        for (name, out) in [("told", arm(true, &moved)), ("untold", arm(false, &moved)), ("cold", cold(&moved))] {
            let by_second: Vec<String> = (0..9)
                .map(|second| format!("{:.1}", db(&out[notice + second * RATE..notice + (second + 1) * RATE])))
                .collect();
            eprintln!("new room {name}: echo {}", by_second.join(" "));
        }

        let timed = |processing: &mut Processing, at: usize| {
            let (speakers_now, mut near) = (&far[at..at + FRAME], [0f32; FRAME]);
            near.copy_from_slice(&speakers[at..at + FRAME]);
            let start = std::time::Instant::now();
            let trips = heap::trips(|| {
                processing.render(speakers_now);
                processing.cancel_echo(&mut near, 20);
            });
            (start.elapsed(), trips)
        };
        let report = |name: &str, frames: &mut Vec<(std::time::Duration, usize)>| {
            frames.sort();
            let (median, worst) = (frames[frames.len() / 2].0, frames[frames.len() - 1].0);
            let trips = frames.iter().map(|frame| frame.1).max().unwrap();
            eprintln!(
                "{name}: {} frames, median {median:?}, worst {worst:?}, heap visits at most {trips}",
                frames.len()
            );
        };
        let (mut mono, mut learning, mut stereo, mut feeding, mut swaps, mut after) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for _ in 0..12 {
            let mut plain = session(LEARNING_FRAMES);
            for at in (0..6 * RATE).step_by(FRAME) {
                mono.push(timed(&mut plain, at));
            }
            plain.pin_stereo();
            plain.collected();
            let mut at = 6 * RATE;
            while !plain.is_stereo() {
                learning.push(timed(&mut plain, at));
                at += FRAME;
            }
            let (mut told, mut at) = past_the_take_over(&far, &speakers);
            for _ in 0..100 {
                stereo.push(timed(&mut told, at));
                at += FRAME;
            }
            told.route_changed();
            told.caught_up();
            while told.replacing {
                let frame = timed(&mut told, at);
                told.ready_for_the_next_frame();
                at += FRAME;
                if told.replacing {
                    feeding.push(frame);
                } else {
                    swaps.push(frame);
                }
            }
            for _ in 0..100 {
                after.push(timed(&mut told, at));
                at += FRAME;
            }
        }
        report("mono, as on main", &mut mono);
        report("learning beside the mono one, as on main", &mut learning);
        report("stereo in charge", &mut stereo);
        report("stereo while the replacement learns", &mut feeding);
        report("the swap", &mut swaps);
        report("the replacement in charge", &mut after);
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

    #[test]
    fn acoustic_absence_needs_two_complete_playing_windows() {
        let mut observer = EchoGone::default();
        for frame in 0..3 * ECHO_WINDOW {
            observer.speakers(&[[0.1; FRAME]; 2]);
            let gone = observer.microphone(&[0.0001; FRAME], || Some(10.0));
            assert_eq!(gone, frame + 1 == 3 * ECHO_WINDOW);
        }
        let mut room = EchoGone::default();
        for _ in 0..20 * ECHO_WINDOW {
            room.speakers(&[[0.1; FRAME]; 2]);
            assert!(!room.microphone(&[0.03; FRAME], || Some(10.0)));
        }
    }

    #[test]
    fn unreported_headphones_three_seconds_before_pin_refuse_the_old_room() {
        let total = 20 * RATE;
        let far = pairs(&long_voice(total, 150.0, -20.0));
        let echo = room(&left_channel(&far), 60);
        let near: Vec<f32> = talker(total, 213.0, 3.1, 3.3, -24.0)
            .into_iter()
            .enumerate()
            .map(|(i, s)| 0.0001 + if (i / RATE).is_multiple_of(2) { 0.0 } else { s })
            .collect();
        let mut p = warmed_on_speakers(&far, &echo, 13);
        warm_through(&mut p, &far[13 * RATE..16 * RATE], &near[13 * RATE..16 * RATE], None);
        p.caught_up();
        p.pin_stereo();
        p.collected();
        assert!(!p.warmed, "the old room must not qualify the candidate");
        assert_eq!(p.left_to_hear(), Some(LEARNING_FRAMES));
    }

    #[test]
    fn unreported_headphones_do_not_replace_the_canceller_in_charge() {
        let far = pairs(&long_voice(20 * RATE, 150.0, -20.0));
        let echo = room(&left_channel(&far), 60);
        let mut p = warmed_on_speakers(&far, &echo, 8);
        warm_through(&mut p, &far[8 * RATE..12 * RATE], &echo[8 * RATE..12 * RATE], Some(0));
        assert!(p.is_stereo());
        let history = p.history;
        warm_through(&mut p, &far[12 * RATE..], &vec![0.0001; 8 * RATE], None);
        assert_eq!(p.history, history, "an acoustic absence must not enqueue a replacement");
        assert!(!p.replacing);
    }

    /// A device change nobody reports, run in release: headphones in a jack the system does not
    /// show, or a default PulseAudio or PipeWire moves (both look alike to the canceller). Voice
    /// kept on headphones, and the echo in another room, against `origin/main` before the warm
    /// canceller ("before"), the same change reported at once (the reset the code does when told),
    /// and one canceller on one channel that nothing resets, as a browser or Discord runs it.
    /// The near voice is [`talker`]: one that shares nothing with the far voice. 27 rooms.
    fn measure_unreported_device_costs(only_native: bool, accepted_arm: usize) -> bool {
        let mut accepted = true;
        let total = 36 * RATE;
        let windows = [(0usize, 3usize), (3, 5), (5, 8), (8, 16)];
        /// 0: one channel, nothing pinned; 1: before; 2: warm.
        fn arm(
            mode: u8,
            (pin, moved, told): (usize, usize, Option<usize>),
            far: &[[f32; 2]],
            near: [&[f32]; 2],
        ) -> Vec<f32> {
            let mut p = if mode == 2 { warm_session() } else { session(LEARNING_FRAMES) };
            let mut out = Vec::with_capacity(far.len());
            let mut at = 0;
            while at + FRAME <= far.len() {
                if mode == 2 && (at / FRAME).is_multiple_of(WARMING_RING / 4) {
                    p.caught_up();
                }
                if mode > 0 && at == pin {
                    p.caught_up();
                    p.pin_stereo();
                    p.collected();
                }
                if told == Some(at) {
                    p.route_changed();
                }
                let near = near[usize::from(at >= moved)];
                out.extend(tick(&mut p, &far[at..at + FRAME], &near[at..at + FRAME]));
                if mode == 2 {
                    p.ready_for_the_next_frame();
                }
                at += FRAME;
            }
            out
        }
        let arms = [
            "one channel, never reset",
            "before",
            "main, not told",
            "main, told",
            "here, not told",
            "worker only",
            "worker faster",
            "worker faster, keep eligibility",
            "native gain change",
            "retained candidate, deferred takeover",
        ];
        let scenarios = [
            ("in charge, then headphones", 8, 16, false),
            ("warming, headphones 8 s before the share", 16, 8, false),
            ("warming, headphones 3 s before the share", 16, 13, false),
            ("in charge, then another room", 8, 16, true),
        ];
        for (scenario, pin, moved, echo) in scenarios {
            let (pin, moved) = (pin * RATE, moved * RATE);
            let from = pin.max(moved);
            let mut sums = [[0f32; 9]; 10];
            let mut worst = [f32::MIN; 10];
            let mut count = 0.0;
            for far_pitch in [100.0, 150.0, 230.0] {
                for delay in [20, 60, 150] {
                    for (pitch, vibrato, syllables) in [(213.0, 3.1, 3.3), (176.0, 2.3, 4.1), (251.0, 3.7, 2.9)] {
                        count += 1.0;
                        let far = pairs(&long_voice(total, far_pitch, -20.0));
                        let left = left_channel(&far);
                        let hiss =
                            |i: usize| ((i as u32).wrapping_mul(2_654_435_761) as f32 / u32::MAX as f32 - 0.5) * 0.002;
                        // Speaks every other second, so the far side is heard alone in between.
                        let said: Vec<f32> = talker(total, pitch, vibrato, syllables, -24.0)
                            .iter()
                            .enumerate()
                            .map(|(i, sample)| hiss(i) + if (i / RATE).is_multiple_of(2) { 0.0 } else { *sample })
                            .collect();
                        let speakers: Vec<f32> = room(&left, delay).iter().zip(&said).map(|(e, s)| e + s).collect();
                        let after: Vec<f32> = if echo {
                            room(&left, delay + 60).iter().zip(&said).map(|(e, s)| 0.7 * e + s).collect()
                        } else {
                            said.clone()
                        };
                        let mut cells = [[0f32; 9]; 10];
                        let runs = [
                            (0u8, None, true),
                            (1, None, true),
                            (2, None, true),
                            (2, Some(moved), true),
                            (2, None, false),
                            (2, None, false),
                            (2, None, false),
                            (2, None, false),
                            (2, None, false),
                            (2, None, false),
                        ];
                        for (index, (mode, told, blind)) in runs.into_iter().enumerate() {
                            if only_native && !matches!(index, 0 | 1 | 2 | 6 | 8 | 9) {
                                continue;
                            }
                            ECHO_GONE_IGNORED.store(blind, Ordering::Relaxed);
                            ROUTE_MEASUREMENT.store(index.saturating_sub(4) as u8, Ordering::Relaxed);
                            let out = arm(mode, (pin, moved, told), &far, [&speakers, &after]);
                            ECHO_GONE_IGNORED.store(false, Ordering::Relaxed);
                            if !echo && far_pitch == 150.0 && delay == 60 && pitch == 213.0 {
                                let by_second: Vec<f32> = (0..16)
                                    .map(|second| {
                                        let range = from + second * RATE..from + (second + 1) * RATE;
                                        let (mut speaking, mut kept) = (0.0f32, 0.0f32);
                                        for (o, s) in out[range.clone()]
                                            .as_chunks::<FRAME>()
                                            .0
                                            .iter()
                                            .zip(said[range].as_chunks::<FRAME>().0)
                                        {
                                            if db(&s[..]) > -40.0 {
                                                speaking += 1.0;
                                                kept += f32::from(u8::from(db(&o[..]) > db(&s[..]) - 10.0));
                                            }
                                        }
                                        kept / speaking
                                    })
                                    .collect();
                                eprintln!("route-seconds,{scenario},{index},{by_second:?}");
                            }
                            if echo {
                                for (second, cell) in cells[index].iter_mut().enumerate() {
                                    let quiet: Vec<f32> = (from + second * RATE..from + (second + 1) * RATE)
                                        .filter(|i| (i / RATE).is_multiple_of(2))
                                        .map(|i| out[i])
                                        .collect();
                                    *cell = if quiet.is_empty() { f32::NAN } else { db(&quiet) };
                                }
                            } else {
                                for (w, (a, b)) in windows.iter().enumerate() {
                                    let r = from + a * RATE..from + b * RATE;
                                    let frames = out[r.clone()]
                                        .as_chunks::<FRAME>()
                                        .0
                                        .iter()
                                        .zip(said[r].as_chunks::<FRAME>().0);
                                    let (mut speaking, mut kept) = (0.0f32, 0.0f32);
                                    for (o, s) in frames.filter(|(_, s)| db(&s[..]) > -40.0) {
                                        speaking += 1.0;
                                        kept += f32::from(u8::from(db(&o[..]) > db(&s[..]) - 10.0));
                                    }
                                    cells[index][w] = kept / speaking;
                                }
                            }
                        }
                        for index in 0..10 {
                            if only_native && !matches!(index, 0 | 1 | 2 | 6 | 8 | 9) {
                                continue;
                            }
                            eprintln!("route-case,{scenario},{far_pitch},{delay},{pitch},{index},{:?}", cells[index]);
                            for (sum, cell) in sums[index].iter_mut().zip(cells[index]) {
                                *sum += if cell.is_nan() { 0.0 } else { cell };
                            }
                            // How much worse than before this room gets: its loudest second of echo
                            // above before's, or the voice it keeps less over the 16 s.
                            let worse = if echo {
                                (0..9)
                                    .step_by(2)
                                    .map(|second| cells[index][second] - cells[1][second])
                                    .fold(f32::MIN, f32::max)
                            } else {
                                windows
                                    .iter()
                                    .enumerate()
                                    .map(|(w, (a, b))| (b - a) as f32 * (cells[1][w] - cells[index][w]))
                                    .sum::<f32>()
                                    / 16.0
                            };
                            worst[index] = worst[index].max(worse);
                        }
                    }
                }
            }
            eprintln!(
                "{scenario}, {count} rooms, {}",
                if echo { "echo dBFS by second, near silent" } else { "voice kept by 0-3, 3-5, 5-8, 8-16 s" }
            );
            for (window, value) in sums[accepted_arm].iter().enumerate().take(if echo { 9 } else { 4 }) {
                if echo && window % 2 == 1 {
                    continue;
                }
                let got = value / count;
                let before = sums[1][window] / count;
                let main = sums[2][window] / count;
                let passes = if echo { got <= before.min(main) } else { got >= before.max(main) };
                if !passes {
                    eprintln!(
                        "route-acceptance MISS,{scenario},window={window},before={before:.9},main={main:.9},here={got:.9}"
                    );
                }
                accepted &= passes;
            }
            for (index, name) in arms.iter().enumerate() {
                if only_native && !matches!(index, 0 | 1 | 2 | 6 | 8 | 9) {
                    continue;
                }
                let cells: Vec<String> = sums[index]
                    .iter()
                    .take(if echo { 9 } else { 4 })
                    .step_by(if echo { 2 } else { 1 })
                    .map(|v| format!("{:.2}", v / count))
                    .collect();
                eprintln!("  {name}: {}; worst room against before {:+.3}", cells.join(" "), worst[index]);
            }
        }
        ROUTE_MEASUREMENT.store(2, Ordering::Relaxed);
        accepted
    }

    #[test]
    #[ignore = "route comparison, run in release alone"]
    fn what_an_unreported_device_costs() {
        measure_unreported_device_costs(false, 6);
    }

    #[test]
    #[ignore = "native AEC path-change comparison, run in release alone"]
    fn native_gain_change_routes() {
        measure_unreported_device_costs(true, 6);
    }

    #[test]
    #[ignore = "strict acoustic acceptance, known to fail; run in release alone"]
    fn route_gaps_reach_both_baselines() {
        assert!(measure_unreported_device_costs(true, 6), "route gaps do not meet both baseline floors");
    }

    #[test]
    #[ignore = "deferred retained-candidate comparison, run in release alone"]
    fn deferred_candidate_routes() {
        let accepted = measure_unreported_device_costs(true, 9);
        eprintln!("deferred route acceptance: {accepted}");
    }

    /// Measurement-only observer. Four render frames are classified on a separate
    /// thread. Equal/panned channels remain mono; negative correlation is wide.
    struct ContentChoice {
        input: Option<std::sync::mpsc::SyncSender<[[f32; FRAME]; 2]>>,
        output: std::sync::mpsc::Receiver<bool>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl ContentChoice {
        fn new() -> Self {
            let (input, frames) = std::sync::mpsc::sync_channel::<[[f32; FRAME]; 2]>(1);
            let (answer, output) = std::sync::mpsc::sync_channel(1);
            let worker = thread::spawn(move || {
                let (mut left, mut right, mut dot, mut frames_heard) = (0.0f64, 0.0f64, 0.0f64, 0usize);
                let mut wide = false;
                while let Ok(frame) = frames.recv() {
                    for (&l, &r) in frame[0].iter().zip(&frame[1]) {
                        let (l, r) = (f64::from(l), f64::from(r));
                        left += l * l;
                        right += r * r;
                        dot += l * r;
                    }
                    frames_heard += 1;
                    if frames_heard == 4 {
                        wide |= left.min(right) > f64::from(PLAYING) && dot < 0.95 * (left * right).sqrt();
                        (left, right, dot, frames_heard) = (0.0, 0.0, 0.0, 0);
                    }
                    if answer.send(wide).is_err() {
                        return;
                    }
                }
            });
            Self { input: Some(input), output, worker: Some(worker) }
        }
        fn wide(&self, frame: &[[f32; 2]]) -> bool {
            self.input
                .as_ref()
                .unwrap()
                .send(std::array::from_fn(|side| std::array::from_fn(|i| frame[i][side])))
                .unwrap();
            self.output.recv().unwrap()
        }
    }
    impl Drop for ContentChoice {
        fn drop(&mut self) {
            drop(self.input.take());
            let _ = self.worker.take().unwrap().join();
        }
    }

    /// Compares the production Processing path, including warming qualification,
    /// the cold fallback, handover and capture delay. Mono never pins. The near
    /// voice is independent, and echo is measured 250 ms into its silent turns.
    /// The new route detector is disabled to isolate #220's channel/tuning choice.
    /// No gain, denoiser, gate, device or real-time scheduler is involved.
    #[test]
    #[ignore = "a channel/tuning comparison, run in release and alone"]
    fn independent_talker_channel_tradeoffs() {
        ECHO_GONE_IGNORED.store(true, Ordering::Relaxed);
        let pin = 6 * RATE;
        let total = pin + 32 * RATE;
        let windows = [(0, 3), (3, 8), (8, 16), (16, 32)];
        let lead = long_voice(total, 150.0, -20.0);
        let a = long_voice(total, 120.0, -14.0);
        let b = talker(total, 187.0, 2.17, 4.31, -14.0);
        let names = ["stereo", "mono downmix", "stereo mono-like suppressor", "content correlation"];
        for shape in ["mono content", "panned", "wide independent", "wide antiphase"] {
            let far: Vec<[f32; 2]> = (0..total)
                .map(|i| {
                    if i < pin {
                        return [lead[i]; 2];
                    }
                    match shape {
                        "mono content" => [a[i]; 2],
                        "panned" => [a[i], 0.0],
                        "wide independent" => [a[i], b[i]],
                        _ => [a[i], -a[i]],
                    }
                })
                .collect();
            let left = left_channel(&far);
            let right: Vec<f32> = far.iter().map(|pair| pair[1]).collect();
            for acoustic in ["headphones", "left speaker", "both speakers"] {
                let mut kept = [[0.0f32; 4]; 4];
                let mut residual = [[0.0f32; 4]; 4];
                let mut count = 0.0;
                for delay in [20, 60, 150] {
                    let l = room(&left, delay);
                    let r = room(&right, delay + 15);
                    let echo: Vec<f32> = (0..total)
                        .map(|i| match acoustic {
                            "headphones" => 0.0,
                            "left speaker" => l[i],
                            _ => l[i] + 0.7 * r[i],
                        })
                        .collect();
                    for (pitch, vibrato, syllables) in [(213.0, 3.1, 3.3), (176.0, 2.3, 4.1), (251.0, 3.7, 2.9)] {
                        count += 1.0;
                        let said: Vec<f32> = talker(total, pitch, vibrato, syllables, -24.0)
                            .into_iter()
                            .enumerate()
                            .map(|(i, s)| if (i / RATE).is_multiple_of(2) { 0.0 } else { s })
                            .collect();
                        let mic: Vec<f32> = echo.iter().zip(&said).map(|(e, s)| e + s).collect();
                        for mode in 0..4 {
                            MEASURE_MONO_SUPPRESSOR.store(mode == 2, Ordering::Relaxed);
                            let mut p = if mode == 1 { session(LEARNING_FRAMES) } else { warm_session() };
                            let mut out = Vec::with_capacity(total);
                            let content = (mode == 3).then(ContentChoice::new);
                            for at in (0..total).step_by(FRAME) {
                                let pin_now = if let Some(content) = &content {
                                    at >= pin && content.wide(&far[at..at + FRAME])
                                } else {
                                    mode != 1 && at == pin
                                };
                                if pin_now {
                                    p.caught_up();
                                    p.pin_stereo();
                                    p.collected();
                                }
                                out.extend(tick(&mut p, &far[at..at + FRAME], &mic[at..at + FRAME]));
                                if mode != 1 {
                                    p.ready_for_the_next_frame();
                                }
                            }
                            let mut case_kept = [0.0f32; 4];
                            let mut case_echo = [0.0f32; 4];
                            for (w, (from, to)) in windows.iter().enumerate() {
                                let mut speaking = 0;
                                let mut through = 0;
                                let mut quiet = Vec::new();
                                for at in (pin + from * RATE..pin + to * RATE).step_by(FRAME) {
                                    if db(&said[at..at + FRAME]) > -40.0 {
                                        speaking += 1;
                                        through +=
                                            usize::from(db(&out[at..at + FRAME]) > db(&said[at..at + FRAME]) - 10.0);
                                    } else if (at / RATE).is_multiple_of(2) && at % RATE >= RATE / 4 {
                                        quiet.extend_from_slice(&out[at..at + FRAME]);
                                    }
                                }
                                case_kept[w] = through as f32 / speaking as f32;
                                case_echo[w] = db(&quiet);
                                kept[mode][w] += case_kept[w];
                                residual[mode][w] += case_echo[w];
                            }
                            eprintln!(
                                "channel-case,{shape},{acoustic},{delay},{pitch},{mode},{case_kept:?},{case_echo:?}"
                            );
                        }
                    }
                }
                for mode in 0..4 {
                    eprintln!(
                        "channels {shape} / {acoustic} / {} / {count} cases: kept {:?}; quiet dBFS {:?}",
                        names[mode],
                        kept[mode].map(|v| (1000.0 * v / count).round() / 1000.0),
                        residual[mode].map(|v| (10.0 * v / count).round() / 10.0)
                    );
                }
            }
        }
        MEASURE_MONO_SUPPRESSOR.store(false, Ordering::Relaxed);
        ECHO_GONE_IGNORED.store(false, Ordering::Relaxed);
    }

    #[cfg(feature = "deep-filter")]
    mod tones;

    /// A talker that shares nothing with [`long_voice`]: its own vibrato and syllable rates and its
    /// own harmonic phases, so no part of it is a delayed copy of the far voice that a canceller
    /// could learn as echo, as one [`voice`] is of another, locked to the same rates.
    fn talker(len: usize, pitch: f64, vibrato: f64, syllables: f64, dbfs: f32) -> Vec<f32> {
        let mut phase = 0f64;
        let mut out: Vec<f32> = (0..len)
            .map(|i| {
                let t = i as f64 / RATE as f64;
                let f0 = pitch * (1.0 + 0.12 * (std::f64::consts::TAU * vibrato * t + 1.0).sin());
                phase = (phase + std::f64::consts::TAU * f0 / RATE as f64) % std::f64::consts::TAU;
                let harmonics: f64 =
                    (1..).map(f64::from).take_while(|h| h * f0 < 5_000.0).map(|h| (h * phase + h * h).sin() / h).sum();
                (harmonics * (std::f64::consts::PI * ((syllables * t + 0.37) % 1.0)).sin().powi(2)) as f32
            })
            .collect();
        let gain = 10f32.powf(dbfs / 20.0) / (out.iter().map(|x| x * x).sum::<f32>() / len as f32).sqrt();
        out.iter_mut().for_each(|x| *x *= gain);
        out
    }
}
