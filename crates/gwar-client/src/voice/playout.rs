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
// Leaky event budget: +5 ms per underrun, -5 ms per release interval, so rare
// misses cannot pin 40 ms. A release followed by an underrun within
// RELEASE_PROBE was needed margin: the interval doubles (up to 80 s), so a
// periodic stall costs ever rarer glitches; a clean release resets it to 5 s.
const HEADROOM_RELEASE: Duration = Duration::from_secs(5);
const MAX_RELEASE: Duration = Duration::from_secs(80);
const RELEASE_PROBE: Duration = Duration::from_secs(5);

/// Pipeline-owned policy; the realtime callback only publishes atomic measurements.
pub(super) struct SpeakerBuffer {
    pub callback: usize,
    pub headroom: usize,
    callback_changed: Instant,
    headroom_changed: Instant,
    underruns: u64,
    release_every: Duration,
    /// When the last release happened, until it is proven needed or not.
    released: Option<Instant>,
}

impl SpeakerBuffer {
    pub fn new(now: Instant) -> Self {
        Self {
            callback: 480,
            headroom: 480,
            callback_changed: now,
            headroom_changed: now,
            underruns: 0,
            release_every: HEADROOM_RELEASE,
            released: None,
        }
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
        let missed = underruns > self.underruns;
        let induced = self.released.is_some_and(|at| now.saturating_duration_since(at) <= RELEASE_PROBE);
        let every = self.release_every;
        if missed && induced {
            self.release_every = (self.release_every * 2).min(MAX_RELEASE);
            self.released = None;
        } else if missed || self.released.is_some_and(|at| now.saturating_duration_since(at) > RELEASE_PROBE) {
            // A clean release, or a miss no release caused (headroom did not
            // prevent it): back to the base interval, so it cannot pin headroom.
            self.release_every = HEADROOM_RELEASE;
            self.released = None;
        }
        if self.release_every != every {
            // A new interval starts now: never release retroactively under it.
            self.headroom_changed = now;
        }
        let elapsed = now.saturating_duration_since(self.headroom_changed);
        let steps = elapsed.as_nanos() / self.release_every.as_nanos();
        if steps > 0 {
            let decay = steps.min(usize::MAX as u128) as usize;
            let before = self.headroom;
            self.headroom = self.headroom.saturating_sub(decay.saturating_mul(STEP)).max(base);
            if self.headroom < before {
                self.released = Some(now);
            }
            // Preserve the fractional interval; underruns never restart this clock.
            self.headroom_changed =
                now - Duration::from_nanos((elapsed.as_nanos() % self.release_every.as_nanos()) as u64);
        }
        if missed {
            let events = (underruns - self.underruns).min((MAX_HEADROOM / STEP) as u64) as usize;
            self.headroom = self.headroom.saturating_add(events * STEP).min(MAX_HEADROOM).max(self.headroom);
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
const QUIET_SKIP: usize = 240; // Unmatched cuts only in quiet audio, 5 ms at a time.
const MIN_LAG: usize = 120; // Search 2.5--15 ms pitch periods at 48 kHz.
const MAX_LAG: usize = 720;

/// Remove one period, retaining the phase at both ends of the overlap. Search
/// only the budget we can actually spend; a sub-period cut is not a substitute.
fn accelerate(pcm: &mut [f32], budget: usize) -> usize {
    let mut best = 0;
    let mut correlation = 0.98_f64;
    for lag in MIN_LAG..=MAX_LAG.min(budget).min(pcm.len() / 2) {
        let (mut aa, mut bb, mut ab, mut a_sum, mut b_sum) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for i in 0..lag {
            let a = f64::from(pcm[i]);
            let b = f64::from(pcm[i + lag]);
            aa += a * a;
            bb += b * b;
            ab += a * b;
            a_sum += a;
            b_sum += b;
        }
        // Center the windows: DC and silence must not masquerade as periodic speech.
        let count = lag as f64;
        aa -= a_sum * a_sum / count;
        bb -= b_sum * b_sum / count;
        ab -= a_sum * b_sum / count;
        if aa < count * 1e-6 || bb < count * 1e-6 || !(0.8..=1.25).contains(&(aa / bb)) {
            continue;
        }
        let score = ab / (aa * bb).sqrt();
        if score > correlation {
            correlation = score;
            best = lag;
        }
    }
    if best > 0 {
        for i in 0..best {
            let blend = i as f32 / (best - 1) as f32;
            pcm[i] = pcm[i] * (1.0 - blend) + pcm[i + best] * blend;
        }
        pcm.copy_within(2 * best.., best);
    }
    best
}

impl Slot {
    pub fn new() -> Result<Self> {
        Ok(Self {
            buffer: JitterBuffer::new(),
            decoder: Decoder::new(SampleRate::Hz48000, Channels::Mono)?,
            decoded: vec![0.0; 5760 + 2 * MAX_LAG],
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
        let Some(samples) = super::opus_samples(&item.opus) else { return Ok(()) };
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
                    Playout::Packet { data, skip, quiet_only: _, fade } => {
                        let input = Packet::try_from(data.as_slice()).ok();
                        let output = MutSignals::try_from(&mut self.decoded[..5760])?;
                        let mut n = self.decoder.decode_float(input, output, false).unwrap_or(0);
                        let level = level_db(&self.decoded[..n]);
                        let quiet = n > 0 && level < -42.0;
                        let mut onset = !quiet && level > self.previous_level + 6.0;
                        self.previous_level = level;
                        if n > 0 && skip > 0 && !quiet && !onset && !fade {
                            // Decode ahead only on proven surplus. This buys a whole
                            // overlap even for 10 ms packets, without startup delay or
                            // replaying history. A gap/recovery stops lookahead unchanged.
                            while n < 2 * skip.min(MAX_LAG) {
                                let Playout::Packet { data, .. } = self.buffer.pop_for(now, 0) else { break };
                                let input = Packet::try_from(data.as_slice()).ok();
                                let output = MutSignals::try_from(&mut self.decoded[n..n + 5760])?;
                                let next = self.decoder.decode_float(input, output, false).unwrap_or(0);
                                let level = level_db(&self.decoded[n..n + next]);
                                onset |= next == 0 || (level >= -42.0 && level > self.previous_level + 6.0);
                                self.previous_level = level;
                                n += next;
                                if onset {
                                    break;
                                }
                            }
                        }
                        // The old nonquiet deadline cannot authorize an uncorrelated
                        // cut. Periodic speech can accelerate as soon as surplus is safe.
                        let removed =
                            if !onset && !quiet && !fade { accelerate(&mut self.decoded[..n], skip) } else { 0 };
                        n -= removed;
                        let skip = if !onset && quiet { skip.min(QUIET_SKIP).min(n) } else { 0 };
                        self.buffer.shrunk(now, removed + skip);
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
    fn a_periodic_stall_backs_off_releases_instead_of_glitching_each_time() {
        // Each second a 31 ms scheduling stall right after a refill needs 20 ms of
        // headroom (10 ms callbacks); less underruns. Underruns follow occupancy.
        let start = Instant::now();
        let mut buffer = SpeakerBuffer::new(start);
        let mut underruns = 0;
        for second in 1..=300 {
            if buffer.headroom < 960 {
                underruns += 1;
            }
            buffer.update(start + Duration::from_secs(second), 480, 480, underruns);
        }
        // A fixed 5 s release would glitch after every release below 20 ms (about 60 times).
        assert!(underruns <= 10, "{underruns} underruns in five minutes");
        assert_eq!(buffer.release_every, MAX_RELEASE);
    }

    #[test]
    fn misses_that_headroom_cannot_prevent_reset_the_backoff() {
        // Start fully backed off at 40 ms; stalls every 40 s happen whatever the headroom.
        let start = Instant::now();
        let mut buffer = SpeakerBuffer::new(start);
        buffer.headroom = MAX_HEADROOM;
        buffer.release_every = MAX_RELEASE;
        let mut underruns = 0;
        for second in 1..=600 {
            if second % 40 == 1 {
                // one second after each 80 s release
                underruns += 1;
            }
            buffer.update(start + Duration::from_secs(second), 480, 480, underruns);
        }
        assert!(buffer.headroom <= 480 + 2 * STEP, "{}", buffer.headroom);
    }

    #[test]
    fn a_backoff_reset_does_not_release_retroactively() {
        let start = Instant::now();
        let mut buffer = SpeakerBuffer::new(start);
        buffer.headroom = MAX_HEADROOM;
        buffer.release_every = MAX_RELEASE;
        // An unrelated miss 60 s after the last release resets the interval.
        buffer.update(start + Duration::from_secs(60), 480, 480, 1);
        assert_eq!(buffer.release_every, HEADROOM_RELEASE);
        assert_eq!(buffer.headroom, MAX_HEADROOM, "the shorter interval starts now");
        buffer.update(start + Duration::from_secs(65), 480, 480, 1);
        assert_eq!(buffer.headroom, MAX_HEADROOM - STEP);
    }

    #[test]
    fn occasional_underruns_release_headroom_but_frequent_ones_grow_it() {
        let now = Instant::now();
        let mut buffer = SpeakerBuffer::new(now);
        buffer.update(now, 480, 480, 1);
        assert_eq!(buffer.headroom, 720);
        buffer.update(now, 480, 480, 3);
        assert_eq!(buffer.headroom, 1200); // Count every event between updates.
        buffer.update(now, 480, 480, 100);
        assert_eq!(buffer.headroom, MAX_HEADROOM);
        for second in 1..=120 {
            buffer.update(now + Duration::from_secs(second), 480, 480, 100 + second / 30);
        }
        assert!(buffer.headroom <= 480 + STEP, "{}", buffer.headroom); // A miss every 30 s cannot pin 40 ms.
        for tick in 1..=20 {
            buffer.update(now + Duration::from_millis(120_000 + tick * 100), 480, 480, 104 + tick);
        }
        assert_eq!(buffer.headroom, MAX_HEADROOM); // 10 misses/s still attacks fast.
        for second in 123..=150 {
            buffer.update(now + Duration::from_secs(second), 480, 480, 124 + (second - 120) / 3);
        }
        // A miss every 3 s outpaces the 5 s release: glitches that headroom
        // can prevent keep it instead of settling at an audible rate.
        assert_eq!(buffer.headroom, MAX_HEADROOM);
        let mut buffer = SpeakerBuffer::new(now);
        buffer.update(now, 2400, 2400, 0);
        assert_eq!(buffer.headroom, 2400); // Never less than one whole callback.
        buffer.update(now + Duration::from_secs(5), 480, 480, 1);
        // Release runs even with an underrun; the decaying callback maximum
        // remains the floor, including when it exceeds MAX_HEADROOM.
        assert_eq!(buffer.headroom, 2160);
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
        encode_n(encoder, speech, 960)
    }

    fn encode_n(encoder: &Encoder, speech: bool, samples: usize) -> Vec<u8> {
        let pcm = (0..samples).map(|i| if speech { (i as f32 * 0.06).sin() * 0.3 } else { 0.0 }).collect::<Vec<_>>();
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
        for (plc, delay) in [(false, 1200), (true, 1200), (false, 5200), (true, 5200)] {
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
        for ms in (20..=5000).step_by(20) {
            slot.buffer.observe_depth(now + Duration::from_millis(ms), 0, 960);
        }
        slot.mix_into(&mut [0.0; 1], 1.0, now + Duration::from_millis(5000)).unwrap();
        assert_eq!(slot.buffer.shrink_samples, 240);
        assert_eq!(slot.fade_left, 119);
        slot.mix_into(&mut [0.0; 119], 1.0, now + Duration::from_millis(5000)).unwrap();
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
                    Clock::Rtp => super::super::Generation::Rtp(gwar_proto::rtp::SlotGeneration(generation as u16)),
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
                        let mut at_twenty_seconds = 0;
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
                            if tick == 20 * 48_000 {
                                at_twenty_seconds = slot.buffer.shrink_samples;
                            }
                        }
                        let label = format!("{clock:?}, callback={callback}, phase={phase}, excess={excess}");
                        assert_eq!(slot.buffer.concealments, 0, "{label}, shrunk={}", slot.buffer.shrink_samples);
                        assert_eq!(slot.buffer.concealments_after_trim, 0, "{label}");
                        if excess == 0 {
                            assert_eq!(slot.buffer.shrink_samples, 0, "{label}: tail is refill margin");
                        } else {
                            assert!(at_twenty_seconds > 0, "{label}: persistent surplus did not converge");
                            assert!(slot.buffer.shrink_samples <= excess as u64, "{label}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn sparse_mild_jitter_keeps_refill_margin_in_both_clocks() {
        use super::super::{Incoming, jitter::Clock};
        // Regression from target/review-r3-final/slot_probe.rs: a one-second
        // window produced 38 trims and 10 concealments in each clock.
        for frame in [480, 960] {
            for clock in [Clock::Rtp, Clock::Sequence] {
                let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
                let mut slot = Slot::new().unwrap();
                let now = Instant::now();
                slot.end = 480; // One 10 ms refill of staged PCM, not removable surplus.
                let mut packet = 0u64;
                for tick in (0..60 * 48_000u64).step_by(480) {
                    let at = now + Duration::from_secs_f64(tick as f64 / 48_000.0);
                    loop {
                        let scheduled = packet * frame;
                        let late = scheduled > 0 && scheduled.is_multiple_of(3 * 48_000);
                        let arrival = scheduled + if late { 5 * 48 } else { 0 };
                        if arrival > tick {
                            break;
                        }
                        slot.receive(Incoming {
                            slot: 0,
                            generation: None,
                            rtp_time: if clock == Clock::Sequence { packet * 960 } else { scheduled },
                            arrived: now + Duration::from_secs_f64(arrival as f64 / 48_000.0),
                            clock,
                            opus: encode_n(&encoder, true, frame as usize),
                        })
                        .unwrap();
                        packet += 1;
                    }
                    slot.mix_refill(&mut [0.0; 480], 1.0, at, 480).unwrap();
                }
                assert_eq!(slot.buffer.trim_events, 0, "{clock:?}");
                assert_eq!(slot.buffer.shrink_samples, 0, "{clock:?}");
                assert_eq!(slot.buffer.concealments, 0, "{clock:?}");
                assert_eq!(slot.buffer.concealments_after_trim, 0, "{clock:?}");
            }
        }
    }

    #[test]
    fn recurring_loss_rtp_drains_surplus_and_retains_jitter() {
        recurring_loss_and_jitter(super::super::jitter::Clock::Rtp);
    }

    #[test]
    fn recurring_loss_sequence_drains_surplus_and_retains_jitter() {
        recurring_loss_and_jitter(super::super::jitter::Clock::Sequence);
    }

    fn recurring_loss_and_jitter(clock: super::super::jitter::Clock) {
        use super::super::jitter::Clock;
        for frame in [480u64, 960] {
            // Exercise genuine sparse jitter first, then the same clock and
            // packet duration with a 100 ms burst backlog and one loss/s.
            for excess in [0u64, 4800] {
                let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
                let mut slot = Slot::new().unwrap();
                let now = Instant::now();
                if excess == 0 {
                    slot.end = 480; // Needed for a 5 ms delay every three seconds.
                }
                let mut packet = 0;
                let mut lost = 0;
                for tick in (0..20 * 48_000u64).step_by(480) {
                    let at = now + Duration::from_secs_f64(tick as f64 / 48_000.0);
                    loop {
                        let scheduled = (packet * frame).saturating_sub(excess);
                        let jitter = excess == 0 && scheduled > 0 && scheduled.is_multiple_of(3 * 48_000);
                        let arrival = scheduled + if jitter { 240 } else { 0 };
                        if arrival > tick {
                            break;
                        }
                        let loss = excess > 0 && scheduled > 0 && scheduled.is_multiple_of(48_000);
                        if loss {
                            lost += 1;
                        } else {
                            slot.buffer.push_at(
                                packet * if clock == Clock::Sequence { 960 } else { frame },
                                frame,
                                encode_n(&encoder, false, frame as usize),
                                now + Duration::from_secs_f64(arrival as f64 / 48_000.0),
                                clock,
                            );
                        }
                        packet += 1;
                    }
                    slot.mix_refill(&mut [0.0; 480], 1.0, at, 480).unwrap();
                }
                let label = format!("{clock:?}, frame={frame}, excess={excess}");
                if excess == 0 {
                    assert_eq!(slot.buffer.shrink_samples, 0, "{label}");
                    assert_eq!(slot.buffer.concealments, 0, "{label}");
                } else {
                    let tail = slot.buffer.depth().1 + slot.staged_samples() as u64;
                    // Safety plus the minimum 120-sample correction quantum.
                    assert!(tail <= slot.buffer.target_samples() + 240, "{label}: tail={tail}");
                    assert!(slot.buffer.shrink_samples >= 4800 - frame - 240, "{label}");
                    assert_eq!(lost, 19);
                    assert_eq!(slot.buffer.concealments, lost, "{label}: shrink must not introduce loss");
                }
            }
        }
    }

    #[test]
    fn fractional_staged_surplus_converges_with_bounded_periodic_corrections() {
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
                    for tick in (0..30 * 48_000).step_by(callback) {
                        let at = now + Duration::from_secs_f64(tick as f64 / 48_000.0);
                        if tick % 960 == 0 {
                            // Keep phase continuous across packets: resetting a tone
                            // every 20 ms creates a discontinuity that TSM should reject.
                            let pcm = harmonic(240.0, 0.0, tick as f32 * std::f32::consts::TAU / 200.0, 960);
                            let mut data = vec![0; 1275];
                            let n = encoder.encode_float(&pcm, &mut data).unwrap();
                            data.truncate(n);
                            slot.buffer.push_at(tick, 960, data, at, clock);
                        }
                        slot.mix_into(&mut vec![0.0; callback], 1.0, at).unwrap();
                        let total = slot.buffer.shrink_samples;
                        if total > corrected {
                            assert!(total - corrected <= MAX_LAG as u64);
                            assert!(total <= excess as u64);
                            assert!(tick >= 5 * 48_000, "correction before the full observation window");
                            if let Some(previous) = last_correction {
                                // Historical margins are debited after a cut; another full
                                // five-second window is no longer required.
                                assert!(tick - previous >= 24_000, "corrections less than 500 ms apart");
                            }
                            last_correction = Some(tick);
                            corrected = total;
                        }
                        if excess as u64 - total < 330 && converged.is_none() {
                            converged = Some(tick);
                        }
                    }
                    assert!(
                        converged.is_some_and(|tick| tick <= 25 * 48_000),
                        "{clock:?}, excess={excess}, callback={callback}, corrected={corrected}"
                    );
                    // Preserve safety plus up to one removable period (at most 209
                    // samples here, allowing codec rounding around the 200-sample pitch).
                    assert!((120..330).contains(&(excess as u64 - corrected)));
                    assert_eq!(slot.buffer.concealments, 0);
                }
            }
        }
    }

    #[test]
    fn native_generation_wrap_rejects_stale_packets_and_missing_metadata_preserves_state() {
        use super::super::{Generation, Incoming, jitter::Clock};
        use gwar_proto::rtp::SlotGeneration;
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
    fn generation_switches_reset_duration_decoder_and_concealment_quantum() {
        use super::super::{Generation, Incoming, jitter::Clock};
        use gwar_proto::rtp::SlotGeneration;
        let mut slot = Slot::new().unwrap();
        let now = Instant::now();
        for (generation, samples) in [960, 480, 960, 480].into_iter().enumerate() {
            let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
            let opus = encode_n(&encoder, true, samples);
            let mut reference = Decoder::new(SampleRate::Hz48000, Channels::Mono).unwrap();
            let expected = decode(&mut reference, Some(&opus));
            slot.receive(Incoming {
                slot: 0,
                rtp_time: 0,
                generation: Some(Generation::Rtp(SlotGeneration(generation as u16))),
                arrived: now,
                clock: Clock::Rtp,
                opus,
            })
            .unwrap();
            assert_eq!(slot.staged_samples(), 0);
            assert_eq!(slot.buffer.target_samples(), samples as u64);
            let mut actual = vec![0.0; samples];
            slot.mix_into(&mut actual, 1.0, now).unwrap();
            assert_eq!(actual, expected);
            // One sample of PLC leaves exactly the remainder of this frame staged.
            slot.mix_into(&mut [0.0], 1.0, now).unwrap();
            assert_eq!(slot.staged_samples(), samples - 1);
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
    fn harmonic(pitch: f32, vibrato: f32, phase: f32, n: usize) -> Vec<f32> {
        let mut phase = phase;
        (0..n)
            .map(|i| {
                let t = i as f32 / 48_000.0;
                phase += std::f32::consts::TAU * pitch * (1.0 + vibrato * (std::f32::consts::TAU * 5.0 * t).sin())
                    / 48_000.0;
                0.25 * (phase.sin() + 0.35 * (2.0 * phase).sin() + 0.15 * (3.0 * phase).sin())
            })
            .collect()
    }

    fn maximum_jump(pcm: &[f32]) -> f32 {
        pcm.windows(2).map(|pair| (pair[1] - pair[0]).abs()).fold(0.0, f32::max)
    }

    // Magnitude-spectrum error on the splice, including its entry and exit.
    // Compare to the unmodified signal at the same output duration.
    fn spectral_error(pcm: &[f32], reference: &[f32]) -> f64 {
        let mut error = 0.0;
        let mut power = 0.0;
        for bin in 0..=pcm.len() / 2 {
            let magnitude = |samples: &[f32]| {
                let (mut real, mut imaginary) = (0.0, 0.0);
                for (i, &sample) in samples.iter().enumerate() {
                    let phase = std::f64::consts::TAU * bin as f64 * i as f64 / samples.len() as f64;
                    real += f64::from(sample) * phase.cos();
                    imaginary += f64::from(sample) * phase.sin();
                }
                real.hypot(imaginary)
            };
            let expected = magnitude(reference);
            error += (magnitude(pcm) - expected).powi(2);
            power += expected * expected;
        }
        (error / power).sqrt()
    }

    #[test]
    fn harmonic_acceleration_preserves_pitch_and_splice_better_than_skip_fade() {
        for pitch in [100.0, 125.0, 150.0, 200.0, 250.0, 300.0] {
            for vibrato in [0.0, 0.02] {
                for phase in [0.3, 1.5, 3.0] {
                    let input = harmonic(pitch, vibrato, phase, 2161);
                    let original = &input[1..];
                    let mut actual = original.to_vec();
                    let removed = accelerate(&mut actual, MAX_LAG);
                    assert!((MIN_LAG..=MAX_LAG).contains(&removed), "pitch={pitch}");
                    actual.truncate(original.len() - removed);
                    assert_eq!(actual.len() + removed, original.len());
                    assert_eq!(&actual[removed..], &original[2 * removed..]);
                    let mut baseline = original[removed..].to_vec();
                    for (i, sample) in baseline[..FADE].iter_mut().enumerate() {
                        let blend = i as f32 / FADE as f32;
                        *sample = input[0] * (1.0 - blend) + *sample * blend;
                    }
                    actual.insert(0, input[0]);
                    baseline.insert(0, input[0]);
                    let reference = &input[..actual.len()];
                    let jump = maximum_jump(&actual);
                    let baseline_jump = maximum_jump(&baseline);
                    let error = spectral_error(&actual, reference);
                    let baseline_error = spectral_error(&baseline, reference);
                    eprintln!(
                        "pitch={pitch} vibrato={vibrato} phase={phase} removed={removed} jump={jump:.6}/{baseline_jump:.6} spectral={error:.6}/{baseline_error:.6}"
                    );
                    assert!(jump <= baseline_jump * 1.01, "pitch={pitch}: {jump}/{baseline_jump}");
                    assert!(jump <= maximum_jump(reference) * 1.03);
                    assert!(error < baseline_error, "pitch={pitch}: {error}/{baseline_error}");
                }
            }
        }
    }

    #[test]
    fn acceleration_rejects_noise_silence_dc_and_insufficient_budget_without_mutation() {
        let mut seed = 1_u32;
        let noise: Vec<f32> = (0..2160)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                (seed as f32 / u32::MAX as f32 - 0.5) * 0.5
            })
            .collect();
        for original in [noise, vec![0.0; 2160], vec![0.2; 2160]] {
            let mut pcm = original.clone();
            assert_eq!(accelerate(&mut pcm, MAX_LAG), 0);
            assert_eq!(pcm, original);
        }
        let original = harmonic(100.0, 0.02, 0.5, 2160);
        for (length, budget) in [(2160, 119), (2160, 240), (480, MAX_LAG)] {
            let mut pcm = original[..length].to_vec();
            assert_eq!(accelerate(&mut pcm, budget), 0);
            assert_eq!(pcm, original[..length]);
        }
    }

    #[test]
    fn acceleration_never_exceeds_budget_and_removes_exactly_one_overlap() {
        let original = harmonic(200.0, 0.0, 0.5, 2160);
        for budget in 0..=800 {
            let mut pcm = original.clone();
            let removed = accelerate(&mut pcm, budget);
            assert!(removed <= budget.min(MAX_LAG));
            if removed == 0 {
                assert_eq!(pcm, original);
            } else {
                assert_eq!(pcm[0], original[0]);
                assert_eq!(pcm[removed - 1], original[2 * removed - 1]);
                assert_eq!(&pcm[removed..original.len() - removed], &original[2 * removed..]);
            }
        }
    }

    #[test]
    fn queued_native_packets_accelerate_without_fade_and_commit_actual_samples() {
        let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
        let now = Instant::now();
        let mut slot = Slot::new().unwrap();
        let signal = harmonic(100.0, 0.02, 0.3, 480 * 12);
        for (i, pcm) in signal.as_chunks::<480>().0.iter().enumerate() {
            let mut data = vec![0; 1275];
            let n = encoder.encode_float(pcm, &mut data).unwrap();
            data.truncate(n);
            slot.buffer.push(i as u64 * 480, 480, data);
        }
        slot.mix_into(&mut [0.0; 960], 1.0, now).unwrap();
        for ms in (10..=5000).step_by(10) {
            slot.buffer.observe_depth(now + Duration::from_millis(ms), 0, 480);
        }
        let before = slot.buffer.depth().1;
        slot.mix_into(&mut [0.0; 1], 1.0, now + Duration::from_secs(5)).unwrap();
        let removed = slot.buffer.shrink_samples;
        assert!((450..=510).contains(&removed), "removed={removed}");
        assert_eq!(before - slot.buffer.depth().1 - slot.staged_samples() as u64 - 1, removed);
        assert_eq!(slot.fade_left, 0);
        assert_eq!(slot.buffer.trim_events, 1);
        assert_eq!(slot.buffer.concealments, 0);
    }

    #[test]
    fn lookahead_onset_and_loud_noise_do_not_use_the_forced_skip() {
        for noise in [false, true] {
            let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
            let mut reference = Decoder::new(SampleRate::Hz48000, Channels::Mono).unwrap();
            let now = Instant::now();
            let mut slot = Slot::new().unwrap();
            let mut signal = harmonic(100.0, 0.0, 0.3, 480 * 12);
            let mut seed = 5_u32;
            for (i, sample) in signal.iter_mut().enumerate() {
                if noise {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    *sample = (seed as f32 / u32::MAX as f32 - 0.5) * 0.5;
                } else if i < 3 * 480 {
                    *sample *= 0.15; // The onset is in the second lookahead packet.
                }
            }
            let mut expected = Vec::new();
            for (i, pcm) in signal.as_chunks::<480>().0.iter().enumerate() {
                let mut data = vec![0; 1275];
                let n = encoder.encode_float(pcm, &mut data).unwrap();
                data.truncate(n);
                expected.extend(decode(&mut reference, Some(&data)));
                slot.buffer.push(i as u64 * 480, 480, data);
            }
            slot.mix_into(&mut [0.0; 960], 1.0, now).unwrap();
            for ms in (10..=5000).step_by(10) {
                slot.buffer.observe_depth(now + Duration::from_millis(ms), 0, 960);
            }
            let mut actual = [0.0; 960];
            slot.mix_into(&mut actual, 1.0, now + Duration::from_secs(5)).unwrap();
            assert_eq!(slot.buffer.shrink_samples, 0, "noise={noise}");
            assert_eq!(actual.as_slice(), &expected[960..1920], "noise={noise}");
            assert_eq!(slot.buffer.concealments, 0);
        }
    }
}
