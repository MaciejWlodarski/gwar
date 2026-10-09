//! Audio device I/O. Everything inside the engine is 48 kHz mono `f32`;
//! device formats are adapted here.

use std::{
    sync::{Arc, mpsc as std_mpsc},
    thread,
};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rtrb::{Consumer, Producer, RingBuffer};

pub const RATE: u32 = 48_000;

/// Ring ends handed to the engine. Dropping `_guard` stops the devices.
pub struct AudioPorts {
    pub mic: Consumer<f32>,
    pub speaker: Producer<f32>,
    pub _guard: Box<dyn Send>,
}

pub trait AudioIo: Send {
    fn open(self: Box<Self>) -> Result<AudioPorts>;
}

/// Lists device names for the settings UI.
pub fn devices() -> (Vec<String>, Vec<String>) {
    let host = cpal::default_host();
    let name = |d: cpal::Device| d.description().ok().map(|d| d.name().to_owned());
    let inputs = host.input_devices().map(|it| it.filter_map(name).collect()).unwrap_or_default();
    let outputs = host.output_devices().map(|it| it.filter_map(name).collect()).unwrap_or_default();
    (inputs, outputs)
}

/// A problem with one of the audio devices, reported while the engine runs.
/// The engine keeps going without the affected side (e.g. you can still listen
/// when there is no microphone).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceIssue {
    /// The microphone could not be opened or stopped working.
    Input(String),
    /// The speakers could not be opened or stopped working.
    Output(String),
}

/// Receives [`DeviceIssue`]s from any thread.
pub type IssueSink = Arc<dyn Fn(DeviceIssue) + Send + Sync>;

/// Real devices via cpal. `None` picks the system default.
///
/// A named device that is gone falls back to the default one. If a side
/// cannot be opened at all, `on_issue` is told and that side stays silent
/// (open fails only when neither works).
#[derive(Default)]
pub struct DeviceIo {
    pub input: Option<String>,
    pub output: Option<String>,
    pub on_issue: Option<IssueSink>,
}

impl DeviceIo {
    pub fn new(input: Option<String>, output: Option<String>) -> Self {
        Self { input, output, on_issue: None }
    }

    pub fn with_issue_sink(mut self, sink: IssueSink) -> Self {
        self.on_issue = Some(sink);
        self
    }
}

struct StopOnDrop(std_mpsc::Sender<()>);
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

/// Converts between a device's native rate/channels and 48 kHz mono using
/// linear interpolation. Adequate for speech; a polyphase resampler can
/// replace it without touching the engine.
struct Resampler {
    step: f64,
    position: f64,
    last: f32,
}

impl Resampler {
    fn new(from: u32, to: u32) -> Self {
        Self { step: f64::from(from) / f64::from(to), position: 0.0, last: 0.0 }
    }

    fn process(&mut self, input: &[f32], mut emit: impl FnMut(f32)) {
        if (self.step - 1.0).abs() < f64::EPSILON {
            input.iter().for_each(|s| emit(*s));
            return;
        }
        for &sample in input {
            while self.position < 1.0 {
                emit(self.last + (sample - self.last) * self.position as f32);
                self.position += self.step;
            }
            self.position -= 1.0;
            self.last = sample;
        }
    }
}

fn find<E>(devices: Result<impl Iterator<Item = cpal::Device>, E>, name: &str) -> Option<cpal::Device> {
    devices.ok()?.find(|d| d.description().is_ok_and(|desc| desc.name() == name))
}

fn open_input(
    host: &cpal::Host,
    name: Option<&str>,
    mut mic_tx: Producer<f32>,
    on_issue: Option<IssueSink>,
) -> Result<cpal::Stream> {
    let device = match name.and_then(|n| find(host.input_devices(), n)) {
        Some(device) => device,
        None => host.default_input_device().context("no microphone found")?,
    };
    let config = device.default_input_config()?;
    let channels = usize::from(config.channels());
    let mut to_engine = Resampler::new(config.sample_rate(), RATE);
    let mut mono = Vec::new();
    let stream = device.build_input_stream(
        config.config(),
        move |data: &[f32], _| {
            mono.clear();
            mono.extend(data.chunks(channels).map(|frame| frame.iter().sum::<f32>() / frame.len() as f32));
            to_engine.process(&mono, |s| {
                let _ = mic_tx.push(s);
            });
        },
        move |e| {
            tracing::warn!("microphone stream error: {e}");
            if let Some(sink) = &on_issue {
                sink(DeviceIssue::Input(e.to_string()));
            }
        },
        None,
    )?;
    stream.play()?;
    Ok(stream)
}

fn open_output(
    host: &cpal::Host,
    name: Option<&str>,
    mut speaker_rx: Consumer<f32>,
    on_issue: Option<IssueSink>,
) -> Result<cpal::Stream> {
    let device = match name.and_then(|n| find(host.output_devices(), n)) {
        Some(device) => device,
        None => host.default_output_device().context("no output device found")?,
    };
    let config = device.default_output_config()?;
    let channels = usize::from(config.channels());
    let mut to_device = Resampler::new(RATE, config.sample_rate());
    let mut pending: Vec<f32> = Vec::new();
    let stream = device.build_output_stream(
        config.config(),
        move |data: &mut [f32], _| {
            let frames = data.len() / channels;
            while pending.len() < frames {
                let Ok(sample) = speaker_rx.pop() else { break };
                to_device.process(&[sample], |s| pending.push(s));
            }
            let take = pending.len().min(frames);
            for (i, frame) in data.chunks_mut(channels).enumerate() {
                frame.fill(if i < take { pending[i] } else { 0.0 });
            }
            pending.drain(..take);
        },
        move |e| {
            tracing::warn!("output stream error: {e}");
            if let Some(sink) = &on_issue {
                sink(DeviceIssue::Output(e.to_string()));
            }
        },
        None,
    )?;
    stream.play()?;
    Ok(stream)
}

impl AudioIo for DeviceIo {
    fn open(self: Box<Self>) -> Result<AudioPorts> {
        let (mic_tx, mic) = RingBuffer::<f32>::new(RATE as usize); // 1 s
        let (speaker, speaker_rx) = RingBuffer::<f32>::new(RATE as usize / 2);
        let (stop_tx, stop_rx) = std_mpsc::channel::<()>();
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<()>>();
        // cpal streams are not `Send` on every platform; they live on this thread.
        thread::Builder::new().name("vc-audio-devices".into()).spawn(move || {
            let host = cpal::default_host();
            let report = |issue: DeviceIssue| {
                if let Some(sink) = &self.on_issue {
                    sink(issue);
                }
            };
            let input = open_input(&host, self.input.as_deref(), mic_tx, self.on_issue.clone());
            let output = open_output(&host, self.output.as_deref(), speaker_rx, self.on_issue.clone());
            if let (Err(i), Err(o)) = (&input, &output) {
                let _ = ready_tx.send(Err(anyhow!("no audio devices: {i:#}; {o:#}")));
                return;
            }
            if let Err(e) = &input {
                tracing::warn!("microphone unavailable: {e:#}");
                report(DeviceIssue::Input(format!("{e:#}")));
            }
            if let Err(e) = &output {
                tracing::warn!("output unavailable: {e:#}");
                report(DeviceIssue::Output(format!("{e:#}")));
            }
            let _ = ready_tx.send(Ok(()));
            let _ = stop_rx.recv();
            drop((input, output));
        })?;
        ready_rx.recv().map_err(|_| anyhow!("audio thread died"))??;
        Ok(AudioPorts { mic, speaker, _guard: Box::new(StopOnDrop(stop_tx)) })
    }
}

#[cfg(test)]
mod tests {
    use super::Resampler;

    #[test]
    fn resampler_keeps_duration() {
        let mut up = Resampler::new(44_100, 48_000);
        let mut out = 0;
        up.process(&vec![0.5; 44_100], |_| out += 1);
        assert!((47_990..=48_010).contains(&out), "{out}");
        let mut down = Resampler::new(48_000, 16_000);
        let mut out = 0;
        down.process(&vec![0.5; 48_000], |_| out += 1);
        assert!((15_990..=16_010).contains(&out), "{out}");
    }
}
