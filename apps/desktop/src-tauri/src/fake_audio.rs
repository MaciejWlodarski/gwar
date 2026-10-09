//! Synthetic audio device for automated verification (`VC_FAKE_AUDIO=1`,
//! debug builds only; see [`enabled`]). The "microphone" is silence or a
//! 440 Hz tone and the "speaker" records how much sound it was given.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use rtrb::RingBuffer;
use vc_client::voice::io::{AudioIo, AudioPorts};

/// Fake audio is only honoured in debug builds.
pub fn enabled() -> bool {
    cfg!(debug_assertions) && std::env::var("VC_FAKE_AUDIO").is_ok_and(|v| v != "0" && v != "fail" && !v.is_empty())
}

/// `VC_FAKE_TONE=1` makes the fake microphone emit a tone instead of silence.
pub fn tone() -> bool {
    std::env::var("VC_FAKE_TONE").is_ok_and(|v| v != "0" && !v.is_empty())
}

/// What the fake speaker heard.
#[derive(Default)]
pub struct Heard {
    /// RMS of the most recent 100 ms, as `f32` bits.
    rms: AtomicU32,
    /// Largest 100 ms RMS seen so far, as `f32` bits.
    peak_rms: AtomicU32,
    /// Samples with |x| > 0.01.
    loud_samples: AtomicU64,
}

impl Heard {
    pub fn rms(&self) -> f32 {
        f32::from_bits(self.rms.load(Ordering::Relaxed))
    }
    pub fn peak_rms(&self) -> f32 {
        f32::from_bits(self.peak_rms.load(Ordering::Relaxed))
    }
    pub fn loud_samples(&self) -> u64 {
        self.loud_samples.load(Ordering::Relaxed)
    }
}

pub struct FakeIo {
    pub tone: bool,
    pub heard: Arc<Heard>,
}

struct StopOnDrop(Arc<AtomicBool>);
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

impl AudioIo for FakeIo {
    fn open(self: Box<Self>) -> anyhow::Result<AudioPorts> {
        let (mut mic_tx, mic) = RingBuffer::<f32>::new(48_000);
        let (speaker, mut speaker_rx) = RingBuffer::<f32>::new(24_000);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let FakeIo { tone, heard } = *self;
        // Real-time pacing: 10 ms of samples in, 10 ms out, like a device.
        thread::Builder::new().name("vc-fake-audio".into()).spawn(move || {
            let mut t = 0u64;
            let (mut sum, mut n) = (0f32, 0usize);
            while !stopped.load(Ordering::Relaxed) {
                for _ in 0..480 {
                    let s = if tone { (t as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.3 } else { 0.0 };
                    t += 1;
                    let _ = mic_tx.push(s);
                }
                for _ in 0..480 {
                    let s = speaker_rx.pop().unwrap_or(0.0);
                    if s.abs() > 0.01 {
                        heard.loud_samples.fetch_add(1, Ordering::Relaxed);
                    }
                    sum += s * s;
                    n += 1;
                    if n == 4800 {
                        let rms = (sum / n as f32).sqrt();
                        heard.rms.store(rms.to_bits(), Ordering::Relaxed);
                        if rms > f32::from_bits(heard.peak_rms.load(Ordering::Relaxed)) {
                            heard.peak_rms.store(rms.to_bits(), Ordering::Relaxed);
                        }
                        (sum, n) = (0.0, 0);
                    }
                }
                thread::sleep(Duration::from_millis(10));
            }
        })?;
        Ok(AudioPorts { mic, speaker, _guard: Box::new(StopOnDrop(stop)) })
    }
}

/// No audio at all: a silent microphone and a speaker that goes nowhere.
/// Used when no real device could be opened so that listening/chat still work.
pub struct NullIo;

impl AudioIo for NullIo {
    fn open(self: Box<Self>) -> anyhow::Result<AudioPorts> {
        let (_mic_tx, mic) = RingBuffer::<f32>::new(1);
        let (speaker, _speaker_rx) = RingBuffer::<f32>::new(48_000 / 2);
        // The unused ring ends are dropped; the engine only ever sees empty/full rings.
        Ok(AudioPorts { mic, speaker, _guard: Box::new(()) })
    }
}
