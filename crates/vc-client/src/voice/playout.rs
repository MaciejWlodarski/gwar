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
    generation: Option<super::Generation>,
    last_sample: f32,
    fade_from: f32,
    fade_left: usize,
    previous_level: f32,
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
            previous_level: -120.0,
        })
    }

    pub fn receive(&mut self, item: super::Incoming) -> Result<()> {
        if let (Some(old), Some(new)) = (self.generation, item.generation)
            && new != old
            && !new.is_newer_than(old)
        {
            return Ok(());
        }
        if item.generation.is_some() && self.generation != item.generation {
            self.buffer.reset();
            self.decoder = Decoder::new(SampleRate::Hz48000, Channels::Mono)?;
            self.position = 0;
            self.end = 0;
            self.last_sample = 0.0;
            self.fade_from = 0.0;
            self.fade_left = 0;
            self.previous_level = -120.0;
            self.generation = item.generation;
        }
        let samples = super::opus_samples(&item.opus).unwrap_or(super::FRAME as u32);
        self.buffer.push_at(item.rtp_time, u64::from(samples), item.opus, item.arrived, item.clock);
        Ok(())
    }

    pub fn generation(&self) -> u64 {
        match self.generation {
            Some(super::Generation::Rtp(g)) => u64::from(g.0),
            Some(super::Generation::Sequence(g)) => g,
            None => 0,
        }
    }

    pub fn staged_samples(&self) -> usize {
        self.end - self.position
    }

    pub fn playing(&self) -> bool {
        self.staged_samples() > 0 || self.buffer.playing()
    }

    #[cfg(test)]
    fn mix_into(&mut self, mix: &mut [f32], gain: f32, now: Instant) -> Result<()> {
        self.mix_refill(mix, gain, now, mix.len())
    }

    pub fn mix_refill(&mut self, mix: &mut [f32], gain: f32, now: Instant, demand: usize) -> Result<()> {
        self.buffer.observe_depth(now, self.staged_samples(), demand);
        let mut written = 0;
        while written < mix.len() {
            if self.position == self.end {
                let (n, skip, fade) = match self.buffer.pop_for(now, demand - written) {
                    Playout::Packet { data, skip, quiet_only, fade } => {
                        let input = Packet::try_from(data.as_slice()).ok();
                        let output = MutSignals::try_from(&mut self.decoded[..])?;
                        let n = self.decoder.decode_float(input, output, false).unwrap_or(0);
                        // Inspect this candidate, never the preceding packet or PLC.
                        let level = level_db(&self.decoded[..n]);
                        let quiet = n > 0 && level < -42.0;
                        // Even forced corrections preserve a rising speech edge,
                        // including the first voiced packet after PLC.
                        let onset = !quiet && level > self.previous_level + 6.0;
                        self.previous_level = level;
                        let skip = if !onset && (!quiet_only || quiet) { skip.min(n) } else { 0 };
                        self.buffer.shrunk(now, skip);
                        (n, skip, fade)
                    }
                    Playout::Conceal(samples) => {
                        self.previous_level = -120.0;
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
        for (plc, delay) in [(false, 1200), (true, 1200), (false, 2500), (true, 2500)] {
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
            for ms in (40..delay).step_by(20) {
                slot.buffer.observe_depth(now + Duration::from_millis(ms), 0, 960);
            }
            let expected = decode(&mut reference, Some(&onset));
            assert!(level_db(&expected) > -42.0);
            let mut actual = [0.0; 960];
            slot.mix_into(&mut actual, 1.0, now + Duration::from_millis(delay)).unwrap();
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
        for ms in (20..=1000).step_by(20) {
            slot.buffer.observe_depth(now + Duration::from_millis(ms), 0, 960);
        }
        slot.mix_into(&mut [0.0; 1], 1.0, now + Duration::from_millis(1000)).unwrap();
        assert_eq!(slot.buffer.shrink_samples, 240);
        assert_eq!(slot.fade_left, 119);
        slot.mix_into(&mut [0.0; 119], 1.0, now + Duration::from_millis(1000)).unwrap();
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
                generation: Some(match clock {
                    Clock::Rtp => super::super::Generation::Rtp(vc_proto::rtp::SlotGeneration(generation as u16)),
                    Clock::Sequence => super::super::Generation::Sequence(generation),
                }),
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
    fn small_arrival_jitter_never_trims_refill_margin_into_starvation() {
        use super::super::jitter::Clock;
        for clock in [Clock::Rtp, Clock::Sequence] {
            for callback in [240, 480, 960] {
                for phase in [-96, 0, 96] {
                    for excess in [0, 480] {
                        let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
                        let mut slot = Slot::new().unwrap();
                        let now = Instant::now();
                        // Cover one refill when a packet arrives just after it.
                        slot.end = callback + excess;
                        let mut packet = 0u64;
                        let mut at_five_seconds = 0;
                        for tick in (0..60 * 48_000u64).step_by(callback) {
                            let at = now + Duration::from_secs_f64(tick as f64 / 48_000.0);
                            loop {
                                // Deterministic +/-3 ms jitter, independent of refills.
                                let jitter = [0, 144, -96, 48, -144, 96, -48][packet as usize % 7];
                                let arrival = (packet as i64 * 960 + phase + jitter).max(0) as u64;
                                if arrival > tick {
                                    break;
                                }
                                slot.buffer.push_at(
                                    packet * 960,
                                    960,
                                    encode(&encoder, true),
                                    now + Duration::from_secs_f64(arrival as f64 / 48_000.0),
                                    clock,
                                );
                                packet += 1;
                            }
                            slot.mix_into(&mut vec![0.0; callback], 1.0, at).unwrap();
                            if tick == 5 * 48_000 {
                                at_five_seconds = slot.buffer.shrink_samples;
                            }
                        }
                        let label = format!("{clock:?}, callback={callback}, phase={phase}, excess={excess}");
                        assert_eq!(slot.buffer.concealments, 0, "{label}, shrunk={}", slot.buffer.shrink_samples);
                        assert_eq!(slot.buffer.concealments_after_trim, 0, "{label}");
                        if excess == 0 {
                            assert_eq!(slot.buffer.shrink_samples, 0, "{label}: tail is refill margin");
                        } else {
                            assert!(at_five_seconds >= 240, "{label}: persistent surplus did not converge");
                            assert!(slot.buffer.shrink_samples <= excess as u64, "{label}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn fractional_staged_surplus_converges_with_bounded_forced_corrections() {
        use super::super::jitter::Clock;
        for clock in [Clock::Rtp, Clock::Sequence] {
            for excess in [240, 480, 720, 912] {
                for callback in [240, 480, 960] {
                    let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
                    let mut slot = Slot::new().unwrap();
                    let now = Instant::now();
                    // An already decoded fractional tail left by startup PLC.
                    slot.end = excess;
                    let mut last_correction = None;
                    let mut corrected = 0;
                    let mut converged = None;
                    for tick in (0..480_000).step_by(callback) {
                        let at = now + Duration::from_secs_f64(tick as f64 / 48_000.0);
                        if tick % 960 == 0 {
                            // Nonquiet carrier: convergence must use the forced deadline.
                            slot.buffer.push_at(tick, 960, encode(&encoder, true), at, clock);
                        }
                        slot.mix_into(&mut vec![0.0; callback], 1.0, at).unwrap();
                        let total = slot.buffer.shrink_samples;
                        if total > corrected {
                            assert!(total - corrected <= 240);
                            assert!(total <= excess as u64);
                            assert!(tick >= 96_000, "forced correction before two seconds");
                            if let Some(previous) = last_correction {
                                assert!(tick - previous >= 48_000, "corrections less than one second apart");
                            }
                            last_correction = Some(tick);
                            corrected = total;
                        }
                        if excess as u64 - total < 240 && converged.is_none() {
                            converged = Some(tick);
                        }
                    }
                    assert!(
                        converged.is_some_and(|tick| tick <= 5 * 48_000),
                        "{clock:?}, excess={excess}, callback={callback}, corrected={corrected}"
                    );
                    // Preserve 2.5 ms of safety; residual corrections below
                    // 2.5 ms are deliberately ignored.
                    assert!((120..240).contains(&(excess as u64 - corrected)));
                    assert_eq!(slot.buffer.concealments, 0);
                }
            }
        }
    }

    #[test]
    fn native_generation_wrap_rejects_stale_packets_and_missing_metadata_preserves_state() {
        use super::super::{Generation, Incoming, jitter::Clock};
        use vc_proto::rtp::SlotGeneration;
        let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
        let mut slot = Slot::new().unwrap();
        let now = Instant::now();
        let opus = encode(&encoder, true);
        let incoming = |generation: Option<u16>, rtp_time| Incoming {
            slot: 0,
            rtp_time,
            arrived: now,
            clock: Clock::Rtp,
            opus: opus.clone(),
            generation: generation.map(|g| Generation::Rtp(SlotGeneration(g))),
        };
        slot.receive(incoming(Some(65535), 0)).unwrap();
        slot.mix_into(&mut [0.0; 480], 1.0, now).unwrap();
        slot.receive(incoming(None, 960)).unwrap();
        assert_eq!(slot.generation(), 65535);
        assert_eq!(slot.staged_samples(), 480);
        slot.receive(incoming(Some(0), 0)).unwrap();
        assert_eq!(slot.generation(), 0);
        assert_eq!(slot.staged_samples(), 0);
        assert_eq!(slot.buffer.depth().0, 1);
        slot.receive(incoming(Some(65535), 1920)).unwrap();
        assert_eq!(slot.buffer.depth().0, 1);
        slot.receive(incoming(None, 960)).unwrap();
        assert_eq!(slot.buffer.depth().0, 2);
        assert_eq!(slot.generation(), 0);
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
