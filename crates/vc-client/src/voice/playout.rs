//! Callback-sized mixing with a separate decoded tail for each receive slot.

use std::time::Instant;

use anyhow::Result;
use audiopus::{Channels, MutSignals, SampleRate, coder::Decoder, packet::Packet};

use super::{
    jitter::{JitterBuffer, Playout},
    level_db,
};

/// One callback plus another callback of scheduling headroom (at least 5 ms).
pub(super) fn speaker_target(callback: usize, capacity: usize) -> usize {
    let callback = if callback == 0 { 480 } else { callback };
    callback.saturating_add(callback.max(240)).min(capacity)
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
    fn target_tracks_callback_and_never_overshoots_with_frame_sized_refills() {
        for callback in [120, 480, 512, 960, 2048] {
            let target = speaker_target(callback, 24_000);
            assert!(target >= 2 * callback);
            assert!(target <= callback * 2 + 240);
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
        assert_eq!(speaker_target(48_000, 24_000), 24_000);
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
