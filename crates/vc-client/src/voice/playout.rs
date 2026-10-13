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
    generation: Option<u64>,
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
            generation: None,
            last_sample: 0.0,
            fade_from: 0.0,
            fade_left: 0,
        })
    }

    pub fn receive(&mut self, item: super::Incoming) -> Result<()> {
        if self.generation.is_some_and(|generation| item.generation < generation) {
            return Ok(());
        }
        if self.generation != Some(item.generation) {
            self.buffer.reset();
            self.decoder = Decoder::new(SampleRate::Hz48000, Channels::Mono)?;
            self.position = 0;
            self.end = 0;
            self.last_sample = 0.0;
            self.fade_from = 0.0;
            self.fade_left = 0;
            self.generation = Some(item.generation);
        }
        let samples = super::opus_samples(&item.opus).unwrap_or(super::FRAME as u32);
        self.buffer.push_at(item.rtp_time, u64::from(samples), item.opus, item.arrived, item.clock);
        Ok(())
    }

    pub fn generation(&self) -> u64 {
        self.generation.unwrap_or(0)
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
                let (n, skip, fade) = match self.buffer.pop_at(now) {
                    Playout::Packet { data, skip, quiet_only, fade } => {
                        let input = Packet::try_from(data.as_slice()).ok();
                        let output = MutSignals::try_from(&mut self.decoded[..])?;
                        let n = self.decoder.decode_float(input, output, false).unwrap_or(0);
                        // Inspect this candidate, never the preceding packet or PLC.
                        let quiet = n > 0 && level_db(&self.decoded[..n]) < -42.0;
                        let skip = if !quiet_only || quiet { skip.min(n) } else { 0 };
                        self.buffer.shrunk(now, skip);
                        (n, skip, fade)
                    }
                    Playout::Conceal(samples) => {
                        let output = MutSignals::try_from(&mut self.decoded[..samples])?;
                        (self.decoder.decode_float(None, output, false).unwrap_or(0), 0, false)
                    }
                    Playout::Idle => break,
                };
                self.position = skip.min(n);
                self.end = n;
                if skip > 0 || fade {
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

    fn encode(encoder: &Encoder, speech: bool) -> Vec<u8> {
        let pcm = (0..960).map(|i| if speech { (i as f32 * 0.06).sin() * 0.3 } else { 0.0 }).collect::<Vec<_>>();
        let mut data = vec![0; 1275];
        let n = encoder.encode_float(&pcm, &mut data).unwrap();
        data.truncate(n);
        data
    }

    fn decode(decoder: &mut Decoder, data: Option<&[u8]>) -> Vec<f32> {
        let mut pcm = vec![0.0; 960];
        let n = decoder
            .decode_float(
                data.map(|p| Packet::try_from(p).unwrap()),
                MutSignals::try_from(&mut pcm[..]).unwrap(),
                false,
            )
            .unwrap();
        pcm.truncate(n);
        pcm
    }

    #[test]
    fn quiet_shrink_preserves_speech_onset_after_silence_or_plc() {
        for plc in [false, true] {
            let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
            let mut reference = Decoder::new(SampleRate::Hz48000, Channels::Mono).unwrap();
            let mut slot = Slot::new().unwrap();
            let now = Instant::now();
            let first = encode(&encoder, plc);
            slot.buffer.push(0, 960, first.clone());
            let onset = encode(&encoder, true);
            let onset_ts = if plc { 1920 } else { 960 };
            for i in 0..8 {
                slot.buffer.push(onset_ts + i * 960, 960, onset.clone());
            }
            let mut initial = [0.0; 960];
            slot.mix_into(&mut initial, 1.0, now).unwrap();
            assert_eq!(initial.as_slice(), decode(&mut reference, Some(&first)));
            if plc {
                let mut concealed = [0.0; 960];
                slot.mix_into(&mut concealed, 1.0, now + Duration::from_millis(20)).unwrap();
                assert_eq!(concealed.as_slice(), decode(&mut reference, None));
            }
            let expected = decode(&mut reference, Some(&onset));
            assert!(level_db(&expected) > -42.0);
            let mut actual = [0.0; 960];
            slot.mix_into(&mut actual, 1.0, now + Duration::from_millis(600)).unwrap();
            assert_eq!(actual.as_slice(), expected);
            assert_eq!(slot.buffer.shrink_samples, 0);
        }
    }

    #[test]
    fn quiet_candidate_is_shrunk_and_boundary_fades_for_120_samples() {
        let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
        let mut slot = Slot::new().unwrap();
        let now = Instant::now();
        for i in 0..8 {
            slot.buffer.push(i * 960, 960, encode(&encoder, false));
        }
        slot.mix_into(&mut [0.0; 960], 1.0, now).unwrap();
        slot.mix_into(&mut [0.0; 1], 1.0, now + Duration::from_millis(600)).unwrap();
        assert_eq!(slot.buffer.shrink_samples, 960);
        assert_eq!(slot.fade_left, 119);
        slot.mix_into(&mut [0.0; 119], 1.0, now + Duration::from_millis(600)).unwrap();
        assert_eq!(slot.fade_left, 0);
    }

    #[test]
    fn new_slot_generation_discards_old_queue_pcm_and_decoder_in_both_transports() {
        use super::super::{Incoming, jitter::Clock};
        for clock in [Clock::Rtp, Clock::Sequence] {
            let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
            let mut slot = Slot::new().unwrap();
            let now = Instant::now();
            let old = encode(&encoder, true);
            let new = encode(&encoder, false);
            let incoming = |generation, rtp_time, opus: Vec<u8>| Incoming {
                slot: 0,
                generation,
                rtp_time,
                arrived: now,
                clock,
                opus,
            };
            slot.receive(incoming(1, (1 << 32) + 9600, old.clone())).unwrap();
            slot.receive(incoming(1, (1 << 32) + 10560, old.clone())).unwrap();
            slot.mix_into(&mut [0.0; 480], 1.0, now).unwrap();
            assert_eq!(slot.staged_samples(), 480);
            assert_eq!(slot.buffer.depth().0, 1);
            slot.receive(incoming(2, 1 << 32, new.clone())).unwrap();
            assert_eq!(slot.staged_samples(), 0);
            assert_eq!(slot.buffer.depth().0, 1);
            // An old transport message cannot roll ownership back.
            slot.receive(incoming(1, (1 << 32) + 11520, old)).unwrap();
            assert_eq!(slot.buffer.depth().0, 1);
            let mut reference = Decoder::new(SampleRate::Hz48000, Channels::Mono).unwrap();
            let expected = decode(&mut reference, Some(&new));
            let mut actual = [0.0; 960];
            slot.mix_into(&mut actual, 1.0, now).unwrap();
            assert_eq!(actual.as_slice(), expected);
            assert_eq!(slot.buffer.late_drops, 0);
            assert_eq!(slot.buffer.concealments, 0);
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
