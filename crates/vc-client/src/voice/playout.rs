//! Callback-sized mixing with a separate decoded tail for each receive slot.

use std::time::{Duration, Instant};

use anyhow::Result;
use audiopus::{Channels, MutSignals, SampleRate, coder::Decoder, packet::Packet};

use super::{
    jitter::{JitterBuffer, Playout},
    level_db,
};

const STEP: usize = 240; // 5 ms at 48 kHz.
const MAX_HEADROOM: usize = 1920; // 40 ms, unless one device callback itself needs more.
const CALLBACK_HOLD: Duration = Duration::from_secs(5);
const HEADROOM_QUIET: Duration = Duration::from_secs(30);

/// Pipeline-owned policy; the realtime callback only publishes atomic measurements.
pub(super) struct SpeakerBuffer {
    pub callback: usize,
    pub headroom: usize,
    callback_changed: Instant,
    headroom_changed: Instant,
    underruns: u64,
}

impl SpeakerBuffer {
    pub fn new(now: Instant) -> Self {
        Self { callback: 480, headroom: 480, callback_changed: now, headroom_changed: now, underruns: 0 }
    }

    /// `peak` is the largest callback since the previous update, never just the last one.
    pub fn update(&mut self, now: Instant, current: usize, peak: usize, underruns: u64) {
        let current = if current == 0 { 480 } else { current };
        let peak = peak.max(current);
        if peak >= self.callback {
            self.callback = peak;
            self.callback_changed = now;
        } else if now.duration_since(self.callback_changed) >= CALLBACK_HOLD {
            // Route/buffer changes settle down gradually: at most 5 ms every five seconds.
            self.callback = self.callback.saturating_sub(STEP).max(peak);
            self.callback_changed = now;
        }
        let base = self.callback.max(STEP);
        self.headroom = self.headroom.max(base);
        if underruns > self.underruns {
            let events = (underruns - self.underruns).min((MAX_HEADROOM / STEP) as u64) as usize;
            self.headroom = self.headroom.saturating_add(events * STEP).min(MAX_HEADROOM).max(self.headroom);
            self.headroom_changed = now;
        } else if now.duration_since(self.headroom_changed) >= HEADROOM_QUIET {
            self.headroom = self.headroom.saturating_sub(STEP).max(base);
            self.headroom_changed = now;
        }
        self.underruns = underruns;
    }

    pub fn target(&self, capacity: usize) -> usize {
        self.callback.saturating_add(self.headroom).min(capacity)
    }
}

pub(super) struct Slot {
    pub buffer: JitterBuffer,
    decoder: Decoder,
    decoded: Vec<f32>,
    position: usize,
    end: usize,
    quiet: bool,
    last_sample: f32,
    fade_from: f32,
    fade_left: usize,
}

const FADE: usize = 120; // 2.5 ms at a latency correction boundary.

impl Slot {
    pub fn new() -> Result<Self> {
        Ok(Self {
            buffer: JitterBuffer::new(),
            decoder: Decoder::new(SampleRate::Hz48000, Channels::Mono)?,
            decoded: vec![0.0; 5760],
            position: 0,
            end: 0,
            quiet: true,
            last_sample: 0.0,
            fade_from: 0.0,
            fade_left: 0,
        })
    }

    pub fn staged_samples(&self) -> usize {
        self.end - self.position
    }

    pub fn playing(&self) -> bool {
        self.staged_samples() > 0 || self.buffer.playing()
    }

    pub fn mix_into(&mut self, mix: &mut [f32], gain: f32, now: Instant) -> Result<()> {
        let mut written = 0;
        while written < mix.len() {
            if self.position == self.end {
                let (n, skip) = match self.buffer.pop_at(now, self.quiet) {
                    Playout::Packet { data, skip } => {
                        let input = Packet::try_from(data.as_slice()).ok();
                        let output = MutSignals::try_from(&mut self.decoded[..])?;
                        let n = self.decoder.decode_float(input, output, false).unwrap_or(0);
                        self.quiet = n == 0 || level_db(&self.decoded[..n]) < -42.0;
                        (n, skip)
                    }
                    Playout::Conceal(samples) => {
                        let output = MutSignals::try_from(&mut self.decoded[..samples])?;
                        self.quiet = true;
                        (self.decoder.decode_float(None, output, false).unwrap_or(0), 0)
                    }
                    Playout::Idle => break,
                };
                self.position = skip.min(n);
                self.end = n;
                if skip > 0 {
                    self.fade_from = self.last_sample;
                    self.fade_left = FADE;
                }
                if n == 0 {
                    break;
                }
            }
            let take = (self.end - self.position).min(mix.len() - written);
            for (out, &sample) in
                mix[written..written + take].iter_mut().zip(&self.decoded[self.position..self.position + take])
            {
                let sample = if self.fade_left > 0 {
                    let blend = 1.0 - self.fade_left as f32 / FADE as f32;
                    self.fade_left -= 1;
                    self.fade_from * (1.0 - blend) + sample * blend
                } else {
                    sample
                };
                self.last_sample = sample;
                *out += sample * gain;
            }
            self.position += take;
            written += take;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audiopus::{Application, coder::Encoder};

    #[test]
    fn callback_max_holds_varying_sizes_then_decays() {
        let now = Instant::now();
        let mut buffer = SpeakerBuffer::new(now);
        for (i, size) in [480, 960, 240, 480, 481, 480].into_iter().enumerate() {
            buffer.update(now + Duration::from_millis(i as u64 * 10), size, size, 0);
            assert_eq!(buffer.callback, if i == 0 { 480 } else { 960 });
        }
        buffer.update(now + Duration::from_secs(5), 240, 240, 0);
        assert_eq!(buffer.callback, 960); // five seconds since the peak, not startup
        for (seconds, expected) in [(6, 720), (11, 480), (16, 240)] {
            buffer.update(now + Duration::from_secs(seconds), 240, 240, 0);
            assert_eq!(buffer.callback, expected);
        }
        // A peak between pipeline wakeups must also grow the target immediately.
        buffer.update(now + Duration::from_secs(17), 240, 2048, 0);
        assert_eq!(buffer.target(24_000), 4096);
        assert_eq!(buffer.target(1000), 1000);
    }

    #[test]
    fn underruns_grow_headroom_and_quiet_time_decays_without_sleeping() {
        let now = Instant::now();
        let mut buffer = SpeakerBuffer::new(now);
        buffer.update(now, 480, 480, 1);
        assert_eq!(buffer.headroom, 720);
        buffer.update(now, 480, 480, 3); // retain multiple events between wakeups
        assert_eq!(buffer.headroom, 1200);
        buffer.update(now, 480, 480, 100);
        assert_eq!(buffer.headroom, MAX_HEADROOM);
        buffer.update(now + Duration::from_secs(29), 480, 480, 101);
        buffer.update(now + Duration::from_secs(58), 480, 480, 101);
        assert_eq!(buffer.headroom, MAX_HEADROOM);
        for step in 1..=8 {
            buffer.update(now + Duration::from_secs(29 + step * 30), 480, 480, 101);
            assert_eq!(buffer.headroom, MAX_HEADROOM.saturating_sub(step as usize * STEP).max(480));
        }
        buffer.update(now + Duration::from_secs(300), 2400, 2400, 102);
        assert_eq!(buffer.headroom, 2400); // never less than a whole callback
        buffer.update(now + Duration::from_secs(305), 480, 480, 103);
        assert_eq!(buffer.headroom, 2400); // an underrun must not shrink a formerly large callback's headroom
    }

    #[test]
    fn target_never_overshoots_with_frame_sized_refills() {
        let now = Instant::now();
        for callback in [120, 480, 512, 960, 2048] {
            let mut buffer = SpeakerBuffer::new(now);
            buffer.update(now, callback, callback, 0);
            let target = buffer.target(24_000);
            assert!(target >= 2 * callback);
            let mut level = target;
            for _ in 0..100 {
                level -= callback;
                let mut needed = target - level;
                while needed > 0 {
                    let take = needed.min(960);
                    level += take;
                    needed -= take;
                }
                assert_eq!(level, target);
            }
        }
    }

    #[test]
    fn mixed_packet_durations_are_played_in_full_without_padding() {
        let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
        let mut reference = Decoder::new(SampleRate::Hz48000, Channels::Mono).unwrap();
        let mut slot = Slot::new().unwrap();
        let mut expected = Vec::new();
        let mut ts = 0;
        for n in [480, 1920, 960, 2880, 480] {
            let pcm = (0..n).map(|i| ((ts + i) as f32 * 0.06).sin() * 0.1).collect::<Vec<_>>();
            let mut data = vec![0; 1275];
            let encoded = encoder.encode_float(&pcm, &mut data[..]).unwrap();
            data.truncate(encoded);
            let mut decoded = vec![0.0; 5760];
            let len = reference
                .decode_float(
                    Some(Packet::try_from(data.as_slice()).unwrap()),
                    MutSignals::try_from(&mut decoded[..]).unwrap(),
                    false,
                )
                .unwrap();
            expected.extend_from_slice(&decoded[..len]);
            slot.buffer.push(ts as u64, n as u64, data);
            ts += n;
        }
        let mut actual = vec![0.0; expected.len()];
        let now = Instant::now();
        for part in actual.chunks_mut(480) {
            slot.mix_into(part, 1.0, now).unwrap();
        }
        assert_eq!(actual, expected);
        assert_eq!(slot.staged_samples(), 0);
        assert_eq!(slot.buffer.concealments, 0);
    }
}
