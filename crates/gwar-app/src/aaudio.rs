//! Android audio for the voice engine, opened through AAudio as a voice call:
//! the microphone with the `VoiceCommunication` preset (the platform's echo
//! cancellation and noise suppression) and the speaker with the
//! `VoiceCommunication` usage (call volume and routing). Both run at the
//! engine's 48 kHz mono `f32`; AAudio converts when the hardware differs.
//!
//! The callbacks only move samples through lock-free rings, as the desktop's
//! device callbacks do (`vc_client::voice::io`).

use std::{
    ffi::c_void,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use anyhow::{Context, Result, bail};
use ndk::audio::{
    AudioCallbackResult, AudioContentType, AudioDirection, AudioFormat, AudioInputPreset, AudioPerformanceMode,
    AudioSharingMode, AudioStream, AudioStreamBuilder, AudioUsage,
};
use rtrb::RingBuffer;
use vc_client::voice::io::{AudioIo, AudioPorts, CaptureSample, OutputTiming, RATE};

pub struct AAudioIo {
    /// Set when a stream fails, e.g. its device (a headset) went away: the run
    /// needs new streams.
    pub lost: Arc<AtomicBool>,
}

/// The open streams; dropping them stops and closes both.
struct Streams {
    input: AudioStream,
    output: AudioStream,
}

// SAFETY: an AAudio stream may be stopped and closed from any thread other
// than its own callbacks. The guard does only that, once, when dropped by the
// engine (never from a callback).
unsafe impl Send for Streams {}

impl Drop for Streams {
    fn drop(&mut self) {
        let _ = self.input.request_stop();
        let _ = self.output.request_stop();
    }
}

fn builder(direction: AudioDirection) -> Result<AudioStreamBuilder> {
    Ok(AudioStreamBuilder::new()?
        .direction(direction)
        .sharing_mode(AudioSharingMode::Shared)
        .performance_mode(AudioPerformanceMode::LowLatency)
        .format(AudioFormat::PCM_Float)
        .channel_count(1)
        .sample_rate(RATE as i32))
}

fn check(stream: &AudioStream, what: &str) -> Result<()> {
    if stream.sample_rate() != RATE as i32 || stream.channel_count() != 1 || stream.format() != AudioFormat::PCM_Float {
        bail!(
            "{what} opened as {} Hz, {} channels, {:?}; the engine needs {RATE} Hz mono float",
            stream.sample_rate(),
            stream.channel_count(),
            stream.format()
        );
    }
    Ok(())
}

impl AudioIo for AAudioIo {
    fn open(self: Box<Self>) -> Result<AudioPorts> {
        let (mut mic_tx, mic) = RingBuffer::<CaptureSample>::new(RATE as usize);
        let (speaker, mut speaker_rx) = RingBuffer::<f32>::new(RATE as usize / 2);
        let output = Arc::new(OutputTiming::default());

        let timing = output.clone();
        let lost = self.lost.clone();
        let output_stream = builder(AudioDirection::Output)?
            .usage(AudioUsage::VoiceCommunication)
            .content_type(AudioContentType::Speech)
            .data_callback(Box::new(move |_, data: *mut c_void, frames| {
                // SAFETY: AAudio hands the callback `frames` writable samples in the opened format (mono f32).
                let out = unsafe { std::slice::from_raw_parts_mut(data.cast::<f32>(), frames.max(0) as usize) };
                let mut missing = 0;
                for sample in out.iter_mut() {
                    *sample = speaker_rx.pop().unwrap_or_else(|_| {
                        missing += 1;
                        0.0
                    });
                }
                timing.report(out.len(), missing);
                AudioCallbackResult::Continue
            }))
            .error_callback(Box::new(move |_, _| lost.store(true, Ordering::Relaxed)))
            .open_stream()
            .context("open the speaker")?;
        check(&output_stream, "the speaker")?;

        let lost = self.lost.clone();
        let mut position = 0u64;
        let input_stream = builder(AudioDirection::Input)?
            .input_preset(AudioInputPreset::VoiceCommunication)
            .data_callback(Box::new(move |_, data: *mut c_void, frames| {
                // SAFETY: AAudio hands the callback `frames` captured samples in the opened format (mono f32).
                let samples = unsafe { std::slice::from_raw_parts(data.cast::<f32>(), frames.max(0) as usize) };
                let captured_at = Instant::now();
                for &value in samples {
                    // A full ring drops the sample; its position still advances, so the engine sees the gap.
                    let _ = mic_tx.push(CaptureSample { position, captured_at, value });
                    position += 1;
                }
                AudioCallbackResult::Continue
            }))
            .error_callback(Box::new(move |_, _| lost.store(true, Ordering::Relaxed)))
            .open_stream()
            .context("open the microphone")?;
        check(&input_stream, "the microphone")?;

        output_stream.request_start().context("start the speaker")?;
        input_stream.request_start().context("start the microphone")?;
        Ok(AudioPorts {
            mic,
            speaker,
            output,
            _guard: Box::new(Streams { input: input_stream, output: output_stream }),
        })
    }
}
