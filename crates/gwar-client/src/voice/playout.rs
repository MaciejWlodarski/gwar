//! Callback-sized mixing with a separate decoded tail for each receive slot.

use std::time::{Duration, Instant};

use anyhow::Result;
use audiopus::{Channels, MutSignals, SampleRate, coder::Decoder, packet::Packet};

use super::{
    jitter::{JitterBuffer, Playout, SHRINK_CORRELATION},
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
    /// Test-only: samples output so far, and every edit of the decoded stream.
    #[cfg(test)]
    played: u64,
    #[cfg(test)]
    edits: Vec<Edit>,
}

/// Test-only: `removed` samples of the decoded stream are missing from the output from sample
/// `at` on, and the `modified` samples there are a crossfade; `correlation` is a period's.
#[cfg(test)]
#[derive(Debug)]
struct Edit {
    at: u64,
    removed: usize,
    modified: usize,
    correlation: Option<f64>,
}

const FADE: usize = 120; // 2.5 ms at a latency correction boundary.
const QUIET_SKIP: usize = 240; // Unmatched cuts only in quiet audio, 5 ms at a time.
const MIN_LAG: usize = 120; // Search 2.5--15 ms pitch periods at 48 kHz.
const MAX_LAG: usize = 720;
const SPLICE_EVERY: usize = 480; // At most one splice per 10 ms, as NetEq's ticks.
const NEAR_BEST: f64 = 0.02;
const SMOOTH: usize = 24; // A moving average with its first null at 2 kHz, NetEq's 4 kHz Nyquist.
const PICK_WINDOW: usize = MAX_LAG; // 15 ms.
const PERIODIC: f64 = 0.5; // NetEq's fast-mode threshold.
// Decode ahead so that a block holds three splice points with a full search after each.
const STRETCH_BLOCK: usize = 2 * MAX_LAG + 3 * SPLICE_EVERY;

/// Remove one period, retaining the phase at both ends of the overlap, and return its length
/// and correlation. A sub-period cut is not a substitute: the period comes from every lag
/// `pcm` holds two windows of, and it stays if it exceeds `budget` (the caller waits for more).
///
/// This is libwebrtc NetEq's accelerate (modules/audio_coding/neteq/accelerate.cc and
/// time_stretch.cc, BSD-licensed; see THIRD_PARTY_NOTICES.md), reimplemented in floating
/// point: the lag at which the audio best repeats, removed by crossfading one period into the
/// next over its whole length, so no splice clicks. Like NetEq it picks the lag below 2 kHz,
/// over a fixed window rather than one period, and judges the splice at full band. NetEq
/// downsamples to 4 kHz and splices 15 ms into a 30 ms buffer; this low-passes and searches
/// every lag at full rate from the start of `pcm`, which follows the audio played before it
/// continuously. Of correlation peaks within `NEAR_BEST` of the best it takes the shortest: a
/// periodic voice's period rather than a multiple. The period goes only if it correlates above
/// `threshold`, and never at `PERIODIC` or less, whatever the caller accepts: audio without a
/// period (noise, unvoiced or whispered speech, most music) is never spliced. Windows whose
/// energies differ by more than 25 % (an onset or a decay) or that hold silence or DC never
/// splice.
///
/// Less than 30 ms cannot hold every lag of 2.5--15 ms and is not searched: over a shorter
/// range a fraction of a longer period can correlate well (review of #201: 120 samples of a
/// 100 Hz voice at 0.90).
fn accelerate(pcm: &mut [f32], budget: usize, threshold: f64) -> (usize, f64) {
    if pcm.len() < 2 * MAX_LAG {
        return (0, 0.0);
    }
    let longest = MAX_LAG;
    let window = PICK_WINDOW.min(pcm.len() - longest - (SMOOTH - 1));
    // Pick the lag below 2 kHz, as NetEq does at 4 kHz, so that broadband noise does not move the
    // peak. Every average is over `SMOOTH` real samples: a filter's startup is no signal.
    let mut smooth = [0.0_f32; MAX_LAG + PICK_WINDOW];
    let mut sum: f32 = pcm[..SMOOTH - 1].iter().sum();
    for i in 0..longest + window {
        sum += pcm[i + SMOOTH - 1];
        smooth[i] = sum / SMOOTH as f32;
        sum -= pcm[i];
    }
    // Each lag correlates one fixed window with the window `lag` later, as NetEq's
    // autocorrelation does over 12.5 ms: one period of a low voice alone is too noisy a guide.
    let mut scores = [f64::NEG_INFINITY; MAX_LAG + 1];
    let mut best = f64::NEG_INFINITY;
    let (a_sums, b) = (sums(&smooth[..window]), &smooth[MIN_LAG..MIN_LAG + window]);
    let (mut b_sum, mut bb) = sums(b);
    for lag in MIN_LAG..=longest {
        if lag > MIN_LAG {
            let (gone, gained) = (f64::from(smooth[lag - 1]), f64::from(smooth[lag + window - 1]));
            (b_sum, bb) = (b_sum - gone + gained, bb - gone * gone + gained * gained);
        }
        let windows = (&smooth[..window], &smooth[lag..lag + window]);
        if let Some(score) = centered_correlation(windows, a_sums, (b_sum, bb)) {
            scores[lag] = score;
            best = best.max(score);
        }
    }
    // The shortest correlation peak near the best: a peak, not the rising edge of one. No lag
    // that correlates at all is no period.
    if !best.is_finite() {
        return (0, 0.0);
    }
    let peak = |lag: usize| {
        let score = scores[lag];
        score >= best - NEAR_BEST && score >= scores[lag - 1] && (lag == longest || score >= scores[lag + 1])
    };
    let Some(period) = (MIN_LAG..=longest).find(|&lag| peak(lag)) else { return (0, 0.0) };
    // NetEq's fast-mode threshold, whatever the caller accepts.
    let threshold = threshold.max(PERIODIC);
    // The splice itself is judged at full band, as NetEq's `best_correlation` is.
    let (first, second) = (&pcm[..period], &pcm[period..2 * period]);
    let Some(correlation) = centered_correlation((first, second), sums(first), sums(second)) else { return (0, 0.0) };
    if correlation <= threshold || period > budget {
        return (0, correlation);
    }
    for i in 0..period {
        let blend = i as f32 / (period - 1) as f32;
        pcm[i] = pcm[i] * (1.0 - blend) + pcm[i + period] * blend;
    }
    pcm.copy_within(2 * period.., period);
    (period, correlation)
}

/// The correlation of two windows of the same length, given their sums and energies, both
/// centered: none if either holds silence or DC, or if their energies differ by more than 25 %.
fn centered_correlation((a, b): (&[f32], &[f32]), (a_sum, aa): (f64, f64), (b_sum, bb): (f64, f64)) -> Option<f64> {
    // Center the windows: DC and silence must not masquerade as periodic speech.
    let count = a.len() as f64;
    let a_energy = aa - a_sum * a_sum / count;
    let b_energy = bb - b_sum * b_sum / count;
    if a_energy < count * 1e-6 || b_energy < count * 1e-6 || !(0.8..=1.25).contains(&(a_energy / b_energy)) {
        return None;
    }
    let ab = dot(a, b) - a_sum * b_sum / count;
    Some(ab / (a_energy * b_energy).sqrt())
}

/// The sum and the energy of `pcm`.
fn sums(pcm: &[f32]) -> (f64, f64) {
    pcm.iter().fold((0.0, 0.0), |(sum, energy), &x| (sum + f64::from(x), energy + f64::from(x) * f64::from(x)))
}

/// Eight independent sums, which the compiler can keep in vector registers.
fn dot(a: &[f32], b: &[f32]) -> f64 {
    let (a_chunks, a_rest) = a.as_chunks::<8>();
    let (b_chunks, b_rest) = b.as_chunks::<8>();
    let mut lanes = [0.0_f64; 8];
    for (x, y) in a_chunks.iter().zip(b_chunks) {
        for lane in 0..8 {
            lanes[lane] += f64::from(x[lane]) * f64::from(y[lane]);
        }
    }
    let rest = a_rest.iter().zip(b_rest).map(|(&x, &y)| f64::from(x) * f64::from(y)).sum::<f64>();
    lanes.iter().sum::<f64>() + rest
}

impl Slot {
    pub fn new() -> Result<Self> {
        Ok(Self {
            buffer: JitterBuffer::new(),
            decoder: Decoder::new(SampleRate::Hz48000, Channels::Mono)?,
            // A frame and the packets decoded after it (decode-ahead, a backlog cut).
            decoded: vec![0.0; 2 * 5760],
            position: 0,
            end: 0,
            generation: None,
            last_sample: 0.0,
            fade_from: 0.0,
            fade_left: 0,
            previous_level: -120.0,
            #[cfg(test)]
            played: 0,
            #[cfg(test)]
            edits: Vec::new(),
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
                    Playout::Packet { data, skip, quiet_only: _, fade, correlation } => {
                        let input = Packet::try_from(data.as_slice()).ok();
                        let output = MutSignals::try_from(&mut self.decoded[..5760])?;
                        let mut n = self.decoder.decode_float(input, output, false).unwrap_or(0);
                        let level = level_db(&self.decoded[..n]);
                        let quiet = n > 0 && level < -42.0;
                        // A due backlog cut also takes the first quiet frame it decodes ahead
                        // (`ahead`), at its pace: a block of speech used to carry it past the cut.
                        let backlog = skip > 0 && correlation < SHRINK_CORRELATION;
                        let mut ahead = None;
                        let mut onset = !quiet && level > self.previous_level + 6.0;
                        self.previous_level = level;
                        if n > 0 && skip > 0 && !quiet && !onset && !fade {
                            // Decode ahead only on proven surplus. This buys a whole
                            // overlap even for 10 ms packets, without startup delay or
                            // replaying history. A gap/recovery stops lookahead unchanged.
                            while n < STRETCH_BLOCK {
                                let Playout::Packet { data, .. } = self.buffer.pop_for(now, 0) else { break };
                                let input = Packet::try_from(data.as_slice()).ok();
                                let output = MutSignals::try_from(&mut self.decoded[n..n + 5760])?;
                                let next = self.decoder.decode_float(input, output, false).unwrap_or(0);
                                let level = level_db(&self.decoded[n..n + next]);
                                onset |= next == 0 || (level >= -42.0 && level > self.previous_level + 6.0);
                                self.previous_level = level;
                                if backlog && !onset && level < -42.0 {
                                    ahead = Some((n, next));
                                }
                                n += next;
                                if onset || ahead.is_some() {
                                    break;
                                }
                            }
                        }
                        // Speech only ever loses whole pitch periods, as the jitter buffer's
                        // `correlation` allows: the gentle shrink's near-identical ones, or a due
                        // backlog cut's (`jitter::BACKLOG_CORRELATION`, then its relaxed rule).
                        // The next splice starts past this one's crossfade; a block that matched
                        // nothing once is not searched again.
                        let (mut removed, mut at) = (0, 0);
                        while !onset && !quiet && ahead.is_none() && !fade && removed < skip {
                            // Splices shorten the block while the next point moves on.
                            let Some(block) = self.decoded.get_mut(at..n) else { break };
                            let (period, _correlation) = accelerate(block, skip - removed, correlation);
                            if period == 0 {
                                break;
                            }
                            #[cfg(test)]
                            self.edits.push(Edit {
                                at: self.played + at as u64,
                                removed: period,
                                modified: period,
                                correlation: Some(_correlation),
                            });
                            n -= period;
                            removed += period;
                            at += SPLICE_EVERY.max(period);
                        }
                        self.buffer.accelerated(now, removed);
                        // A quiet frame decoded ahead goes from its start, up to the cut's credit,
                        // and is paced as periods are; the rest of the frame fades in.
                        if let Some((start, len)) = ahead {
                            let cut = skip.min(len.saturating_sub(FADE));
                            for i in 0..FADE {
                                let blend = i as f32 / FADE as f32;
                                self.decoded[start + i] =
                                    self.decoded[start + i] * (1.0 - blend) + self.decoded[start + cut + i] * blend;
                            }
                            self.decoded.copy_within(start + cut + FADE..n, start + FADE);
                            n -= cut;
                            self.buffer.accelerated(now, cut);
                            #[cfg(test)]
                            if cut > 0 {
                                let at = self.played + start as u64;
                                self.edits.push(Edit { at, removed: cut, modified: FADE, correlation: None });
                            }
                        }
                        let skip = if !onset && quiet { skip.min(QUIET_SKIP).min(n) } else { 0 };
                        self.buffer.shrunk(now, skip);
                        #[cfg(test)]
                        if skip > 0 {
                            self.edits.push(Edit { at: self.played, removed: skip, modified: FADE, correlation: None });
                        }
                        // A due backlog cut drops the quiet packets after a quiet frame, and
                        // the first loud one plays after this frame: speech stays. The decoder
                        // runs through the dropped audio, so the next packet continues its state.
                        if quiet {
                            while self.buffer.backlog_room() {
                                let Playout::Packet { data, .. } = self.buffer.pop_for(now, 0) else { break };
                                let input = Packet::try_from(data.as_slice()).ok();
                                let output = MutSignals::try_from(&mut self.decoded[n..n + 5760])?;
                                let next = self.decoder.decode_float(input, output, false).unwrap_or(0);
                                let level = level_db(&self.decoded[n..n + next]);
                                if next == 0 || level >= -42.0 {
                                    self.previous_level = level;
                                    n += next;
                                    break;
                                }
                                self.buffer.dropped(now, next);
                                #[cfg(test)]
                                self.edits.push(Edit {
                                    at: self.played + (n - skip) as u64,
                                    removed: next,
                                    modified: 0,
                                    correlation: None,
                                });
                            }
                        }
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
            #[cfg(test)]
            {
                self.played += take as u64;
            }
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
            // Less than the jitter buffer's `BACKLOG_TRIM` surplus: a shrink proposal, not a cut.
            for i in 0..5 {
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

    /// 11 s of a `pitch` Hz voice with `noise` peak added (loud noise alone if `pitch` is 0)
    /// that speaks at `gain(i)` in 20 ms frame `i` (0: silence), over a background of `fan`
    /// peak noise throughout, encoded in order, and each frame as a decoder of the whole stream
    /// plays it.
    fn voice(pitch: f32, noise_level: f32, fan: f32, gain: impl Fn(usize) -> f32) -> (Vec<Vec<u8>>, Vec<Vec<f32>>) {
        let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
        let mut reference = Decoder::new(SampleRate::Hz48000, Channels::Mono).unwrap();
        let pcm = if pitch > 0.0 { harmonic(pitch, 0.02, 0.3, 550 * 960) } else { vec![0.0; 550 * 960] };
        let pcm: Vec<f32> = pcm.iter().zip(noise(7, noise_level, pcm.len())).map(|(v, n)| v + n).collect();
        let fan = noise(9, fan, pcm.len());
        let frames: Vec<_> = pcm
            .chunks(960)
            .zip(fan.chunks(960))
            .enumerate()
            .map(|(i, (frame, fan))| {
                let frame: Vec<f32> = frame.iter().zip(fan).map(|(&v, &f)| v * gain(i) + f).collect();
                let mut data = vec![0; 1275];
                let n = encoder.encode_float(&frame, &mut data).unwrap();
                data.truncate(n);
                data
            })
            .collect();
        let decoded = frames.iter().map(|data| decode(&mut reference, Some(data))).collect();
        (frames, decoded)
    }

    /// One 20 ms refill of `samples` after `backlog` of `frames` arrived at once at `start` and
    /// then one per refill. Returns the output, the frames taken from the queue (played or
    /// dropped) and the samples removed.
    fn backlog_refill(
        slot: &mut Slot,
        frames: &[Vec<u8>],
        backlog: u64,
        start: Instant,
        refill: u64,
        samples: usize,
    ) -> (Vec<f32>, std::ops::Range<u64>, u64) {
        use super::super::jitter::Clock;
        let now = start + Duration::from_millis(refill * 20);
        if refill == 0 {
            for i in 0..backlog {
                slot.buffer.push_at(i * 960, 960, frames[i as usize].clone(), start, Clock::Rtp);
            }
        }
        let i = refill + backlog;
        slot.buffer.push_at(i * 960, 960, frames[i as usize].clone(), now, Clock::Rtp);
        let (head, shrunk) = (slot.buffer.head().unwrap() / 960, slot.buffer.shrink_samples);
        let mut out = vec![0.0; samples];
        slot.mix_into(&mut out, 1.0, now).unwrap();
        (out, head..slot.buffer.head().unwrap() / 960, slot.buffer.shrink_samples - shrunk)
    }

    /// What `drain` measured: the first refill that removed audio and the first after it with
    /// at most 100 ms queued (the window proves the surplus at refill 250); speech (packets that
    /// decode at -42 dB or louder) and quiet packets dropped whole, and the rest removed (pitch
    /// periods and 5 ms quiet skips), in samples; the largest sample-to-sample step of the
    /// output against the reference's; the largest share of any second of output that was
    /// removed by less than a packet (how much faster than real time speech played); and every
    /// period removed, with the refill that removed it and its correlation.
    #[derive(Debug)]
    struct Drain {
        first: Option<u64>,
        done: Option<u64>,
        queued: u64,
        speech_dropped: u64,
        quiet_dropped: u64,
        stretched: u64,
        jump: f32,
        reference_jump: f32,
        speedup: f64,
        periods: Vec<(u64, usize, f64)>,
    }

    /// Plays 10 s of `voice(pitch, noise_level, fan, gain)` after a backlog of `backlog`
    /// packets. Every output sample outside a crossfade must be the reference decoder's, shifted
    /// by what the edits before it removed: nothing is lost but what was counted, and the decoder
    /// runs through everything dropped. Nothing but pitch periods goes from a frame the voice
    /// speaks in, however softly.
    fn drain(pitch: f32, noise_level: f32, fan: f32, gain: impl Fn(usize) -> f32, backlog: u64) -> Drain {
        let (frames, decoded) = voice(pitch, noise_level, fan, &gain);
        let content: Vec<f32> = decoded.concat();
        let quiet = |i: u64| level_db(&decoded[i as usize]) < -42.0;
        let mut slot = Slot::new().unwrap();
        let start = Instant::now();
        let (mut first, mut done) = (None, None);
        let mut drain = Drain {
            first: None,
            done: None,
            queued: 0,
            speech_dropped: 0,
            quiet_dropped: 0,
            stretched: 0,
            jump: 0.0,
            reference_jump: decoded.iter().map(|f| maximum_jump(f)).fold(0.0, f32::max),
            speedup: 0.0,
            periods: Vec::new(),
        };
        let mut output = Vec::new();
        let mut removals = Vec::new();
        for refill in 0..500 {
            let edits = slot.edits.len();
            let (out, taken, shrunk) = backlog_refill(&mut slot, &frames, backlog, start, refill, 960);
            drain
                .periods
                .extend(slot.edits[edits..].iter().filter_map(|e| e.correlation.map(|c| (refill, e.removed, c))));
            // Dropped packets follow the frames played; the first loud one after them, if
            // taken, plays next. A period or quiet skip is less than a packet.
            let k = shrunk / 960;
            let last = taken.end - u64::from(!quiet(taken.end - 1));
            for i in last - k..last {
                *(if quiet(i) { &mut drain.quiet_dropped } else { &mut drain.speech_dropped }) += 960;
            }
            drain.stretched += shrunk % 960;
            output.extend(out);
            removals.push(shrunk % 960);
            if shrunk > 0 {
                first.get_or_insert(refill);
            }
            if first.is_some() && done.is_none() && slot.buffer.depth().1 <= 960 * 5 {
                done = Some(refill);
            }
            if refill >= 300 {
                drain.queued = drain.queued.max(slot.buffer.depth().1);
            }
        }
        let (mut shift, mut crossfade, mut edits) = (0, 0..0, slot.edits.iter().peekable());
        for (at, &sample) in output.iter().enumerate() {
            while let Some(edit) = edits.next_if(|e| e.at <= at as u64) {
                shift += edit.removed;
                crossfade = edit.at as usize..edit.at as usize + edit.modified;
            }
            if !crossfade.contains(&at) {
                let error = (sample - content[at + shift]).abs();
                assert!(error < 1e-6, "sample {at}: {error} from the reference after {shift} removed");
            }
        }
        let mut shift = 0;
        for edit in &slot.edits {
            let start = edit.at as usize + shift;
            if edit.correlation.is_none() {
                let mut frames = start / 960..(start + edit.removed).div_ceil(960);
                assert!(frames.all(|i| gain(i) == 0.0), "{edit:?}: {start}.. is in a word");
            }
            shift += edit.removed;
        }
        (drain.first, drain.done) = (first, done);
        let (first, done) = (first.unwrap_or(250) as usize, done.unwrap_or(490) as usize);
        drain.jump = maximum_jump(&output[first * 960..(done + 10) * 960]);
        drain.speedup = removals.windows(50).map(|w| w.iter().sum::<u64>() as f64 / 48_000.0).fold(0.0, f64::max);
        assert_eq!(slot.buffer.concealments, 0);
        drain
    }

    #[test]
    fn a_backlog_cut_drops_quiet_packets_and_accelerates_speech() {
        // The window proves the surplus at 5 s (refill 250). In speech with 120 ms pauses every
        // 320 ms the quiet packets cover most of the cut; with short gaps between words, or none,
        // speech plays up to 1.2 times as fast, a pitch period at a time. Before, a cut that
        // found no quiet packets within 2 s dropped what was left at once: 200--380 ms of speech
        // (or noise) in every case but the pauses (docs/latency-congested-uplink.md). A noisy
        // voice (periods correlating at 0.6--0.85) waits the 2 s, then still loses only whole
        // periods (review of #201).
        let pauses: fn(usize) -> bool = |i| i % 16 < 10;
        let gaps: fn(usize) -> bool = |i| i % 10 >= 2;
        let steady: fn(usize) -> bool = |_| true;
        let cases = [
            ("pauses", 150.0, 0.0, pauses, 15),
            ("word gaps", 150.0, 0.0, gaps, 15),
            ("continuous", 150.0, 0.0, steady, 15),
            ("continuous, 430 ms", 150.0, 0.0, steady, 22),
            ("continuous 100 Hz, 430 ms", 100.0, 0.0, steady, 22),
            ("continuous 250 Hz, 430 ms", 250.0, 0.0, steady, 22),
            ("word gaps, 430 ms", 150.0, 0.0, gaps, 22),
            ("noisy 100 Hz voice, 430 ms", 100.0, 0.16, steady, 22),
            ("noisy 150 Hz voice, 430 ms", 150.0, 0.16, steady, 22),
            ("loud noise, 430 ms", 0.0, 0.3, steady, 22),
            ("noise with pauses, 430 ms", 0.0, 0.3, pauses, 22),
        ];
        for (name, pitch, noise_level, speaks, backlog) in cases {
            let drain = drain(pitch, noise_level, 0.0, |i| f32::from(u8::from(speaks(i))), backlog);
            let periods = drain.periods.len();
            let correlations =
                drain.periods.iter().map(|p| p.2).fold((1.0, -1.0), |(lo, hi), c| (c.min(lo), c.max(hi)));
            let summary = format!(
                "first {:?} done {:?} queued {} speech {} quiet {} stretched {} jump {:.4}/{:.4} speedup {:.3} \
                 periods {periods} correlation {:.2}..{:.2}",
                drain.first,
                drain.done,
                drain.queued,
                drain.speech_dropped,
                drain.quiet_dropped,
                drain.stretched,
                drain.jump,
                drain.reference_jump,
                drain.speedup,
                correlations.0,
                correlations.1
            );
            eprintln!("{name}: {summary}");
            assert_eq!(drain.speech_dropped, 0, "{name}: {summary}");
            assert!(drain.jump <= drain.reference_jump * 1.05, "{name}: {summary}");
            assert!(drain.speedup <= 0.24, "{name}: {summary}");
            // A voice only ever loses whole periods, at NetEq's fast-mode threshold or above.
            for &(refill, removed, correlation) in &drain.periods {
                assert!(
                    whole_periods(removed, pitch) && correlation >= 0.5,
                    "{name}: refill {refill} removed {removed} at {correlation:.3}"
                );
            }
            let (first, done) = (drain.first.unwrap_or(0), drain.done.unwrap_or(u64::MAX));
            if pitch == 0.0 {
                // No period: nothing splices, and only quiet packets and frames go. Without
                // any (40 ms gaps in noise decode at about -40 dB) the backlog stays as it was,
                // bounded, and the cut gives up after `BACKLOG_STUCK`.
                assert!(drain.periods.is_empty(), "{name}: {summary}");
                if name.contains("pauses") {
                    assert!(done <= first + 100, "{name}: {summary}");
                } else {
                    // Continuous noise: no background either.
                    assert_eq!(drain.first, None, "{name}: {summary}");
                    assert!(drain.queued <= (backlog + 1) * 960, "{name}: {summary}");
                }
            } else if noise_level == 0.0 {
                // Within 2 s of the window, when the old cut went ahead at its cap.
                assert!((250..255).contains(&first), "{name}: {summary}");
                assert!(done <= 360, "{name}: {summary}");
            } else {
                // No period above NetEq's threshold: the cut waits `BACKLOG_WAIT`.
                assert!((345..=355).contains(&first), "{name}: {summary}");
                assert!(done <= first + 100, "{name}: {summary}");
            }
            if name == "pauses" {
                assert!(drain.quiet_dropped >= 5 * 960, "{name}: {summary}");
            }
        }
    }

    #[test]
    fn quiet_gaps_decoded_ahead_drain_and_words_stay_whole() {
        // Review of #201: in noise in words with 40 ms gaps the quiet frame of each gap (below
        // -42 dB) sat inside a block decoded ahead with speech, which nothing searched for
        // quiet: its 430 ms backlog stayed for good (main dropped 380 ms of it, words included).
        // It now goes, paced, from frames the fixture marks silent only (`drain`). Over a fan
        // the gaps never decode below -42 dB, and a level alone cannot tell them from soft
        // speech (`soft_words_lose_nothing_but_whole_periods`): without a period, that backlog
        // stays, bounded, as with steady noise. Noise suppression removes the fan.
        let gaps = |i| f32::from(u8::from(i % 10 >= 2));
        for (name, pitch, noise_level, fan, drains) in [
            ("noise in words", 0.0, 0.3, 0.0, true),
            ("voice over a fan", 150.0, 0.0, 0.03, true),
            ("noise in words over a fan", 0.0, 0.3, 0.03, false),
        ] {
            let drain = drain(pitch, noise_level, fan, gaps, 22);
            eprintln!("{name}: {drain:?}");
            assert_eq!(drain.speech_dropped, 0, "{name}");
            assert!(drain.speedup <= 0.24, "{name}: {}", drain.speedup);
            for &(refill, removed, correlation) in &drain.periods {
                assert!(
                    whole_periods(removed, pitch) && correlation >= 0.5,
                    "{name}: {refill} {removed} {correlation}"
                );
            }
            if drains {
                let first = drain.first.expect("nothing removed");
                assert!((250..255).contains(&first), "{name}: {first}");
                assert!(drain.done.is_some_and(|done| done <= first + 200), "{name}: {:?}", drain.done);
            } else {
                assert_eq!(drain.first, None, "{name}");
                assert!(drain.queued <= 23 * 960, "{name}: {}", drain.queued);
            }
        }
    }

    #[test]
    fn soft_words_lose_nothing_but_whole_periods() {
        // Review of #201: a voice 20 dB softer for a second (about -34 dB) set the background
        // floor of a relative level rule itself and lost cuts of 720 and 192 samples of a
        // 320-sample period. Its level proves nothing: only quiet audio (below -42 dB) goes but
        // in periods.
        let drain = drain(150.0, 0.0, 0.0, |i| if i % 150 < 100 { 1.0 } else { 0.1 }, 22);
        assert_eq!(drain.speech_dropped, 0, "{drain:?}");
        assert!(drain.done.is_some(), "{drain:?}");
        for &(refill, removed, correlation) in &drain.periods {
            assert!(whole_periods(removed, 150.0) && correlation >= 0.5, "{refill} {removed} {correlation}");
        }
    }

    #[test]
    fn soft_speech_after_a_long_pause_loses_nothing_but_whole_periods() {
        use super::super::jitter::Clock;
        // Review of #201: loud speech (-4.5 dB), a minute of silence, then speech at -37 dB with
        // a backlog, in both clocks: what the slot had heard a minute ago called the soft speech
        // background and cut 147 ms of it here (Rtp). A fresh slot removed none; nothing is kept
        // across frames to judge levels by now.
        for clock in [Clock::Rtp, Clock::Sequence] {
            let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
            let pcm = harmonic(150.0, 0.02, 0.3, 700 * 960);
            let packet = |i: usize, gain: f32| {
                let frame: Vec<f32> = pcm[i * 960..(i + 1) * 960].iter().map(|v| v * gain).collect();
                let mut data = vec![0; 1275];
                let n = encoder.encode_float(&frame, &mut data).unwrap();
                data.truncate(n);
                data
            };
            let mut slot = Slot::new().unwrap();
            let start = Instant::now();
            let at = |refill: u64| start + Duration::from_millis(refill * 20);
            let rtp = |i: usize, refill: u64| if clock == Clock::Rtp { refill * 960 } else { i as u64 * 960 };
            // Two seconds of loud speech, one packet per refill.
            for i in 0..100 {
                slot.buffer.push_at(rtp(i, i as u64), 960, packet(i, 3.0), at(i as u64), clock);
                slot.mix_into(&mut [0.0; 960], 1.0, at(i as u64)).unwrap();
            }
            // A minute without packets, then 430 ms at once and one per refill for 10 s.
            for refill in 100..3100 {
                slot.mix_into(&mut [0.0; 960], 1.0, at(refill)).unwrap();
            }
            let edits = slot.edits.len();
            for (k, refill) in (3100..3600).enumerate() {
                let arrived = if k == 0 { 0..23 } else { k + 22..k + 23 };
                for j in arrived {
                    let i = 100 + j;
                    slot.buffer.push_at(rtp(i, 3100 + j as u64), 960, packet(i, 0.07), at(refill), clock);
                }
                slot.mix_into(&mut [0.0; 960], 1.0, at(refill)).unwrap();
            }
            for edit in &slot.edits[edits..] {
                let correlation = edit.correlation.unwrap_or(f64::NAN);
                assert!(whole_periods(edit.removed, 150.0) && correlation >= 0.5, "{clock:?}: {edit:?}");
            }
        }
    }

    /// Plays 10 s of `pcm` in `frame`-sample packets, 20 ms of them per refill after `backlog`
    /// refills' worth arrived at once, with the packets `lost(i)` never arriving.
    fn play_backlog(pcm: &[f32], frame: usize, backlog: usize, lost: impl Fn(usize) -> bool) -> Slot {
        use super::super::jitter::Clock;
        let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
        let mut slot = Slot::new().unwrap();
        let start = Instant::now();
        let per_refill = 960 / frame;
        for refill in 0..500 {
            let now = start + Duration::from_millis(refill as u64 * 20);
            let first = if refill == 0 { 0 } else { (backlog + refill) * per_refill };
            for i in (first..(backlog + refill + 1) * per_refill).filter(|&i| !lost(i)) {
                let mut data = vec![0; 1275];
                let n = encoder.encode_float(&pcm[i * frame..(i + 1) * frame], &mut data).unwrap();
                data.truncate(n);
                slot.buffer.push_at((i * frame) as u64, frame as u64, data, now, Clock::Rtp);
            }
            slot.mix_into(&mut [0.0; 960], 1.0, now).unwrap();
        }
        slot
    }

    #[test]
    fn splices_stay_within_a_block_they_shortened() {
        // Review of #201: the next splice point moves on by at least 10 ms while each splice
        // shortens the block, so five 120-sample periods of a 400 Hz voice left a 2880-sample
        // block 2280 long with the next search at 2400.
        let drain = drain(400.0, 0.0, 0.0, |_| 1.0, 20);
        assert_eq!(drain.speech_dropped, 0, "{drain:?}");
        for &(refill, removed, correlation) in &drain.periods {
            assert!(
                whole_periods(removed, 400.0) && correlation >= 0.5,
                "refill {refill}: {removed} at {correlation:.3}"
            );
        }
    }

    #[test]
    fn a_splice_in_a_ten_ms_block_before_a_gap_stays_within_it() {
        // Review of #201: a 10 ms packet before a lost one is a whole block, shorter than the
        // step to the next splice point. Every third packet is lost: no block holds more than
        // two.
        let pcm = harmonic(400.0, 0.02, 0.3, 1100 * 480);
        let slot = play_backlog(&pcm, 480, 22, |i| i % 3 == 2);
        // Less than 30 ms is no longer searched at all (review of #201, below).
        assert!(slot.edits.iter().all(|e| e.correlation.is_none()), "{:?}", slot.edits);
    }

    /// A `pitch` Hz voice with its harmonics up to 4.4 kHz shaped by formants `width` Hz wide at
    /// `centres` (the second half as strong).
    fn formant_voice(pitch: f32, centres: [f32; 2], width: f32, n: usize) -> Vec<f32> {
        let formant = |f: f32, centre: f32| 1.0 / (1.0 + ((f - centre) / width).powi(2));
        (0..n)
            .map(|i| {
                let phase = std::f32::consts::TAU * pitch * i as f32 / 48_000.0;
                (1..=(4400.0 / pitch) as usize)
                    .map(|k| {
                        let f = pitch * k as f32;
                        0.1 * (formant(f, centres[0]) + 0.5 * formant(f, centres[1])) * (k as f32 * phase).sin()
                    })
                    .sum()
            })
            .collect()
    }

    #[test]
    fn a_search_that_finds_no_valid_lag_never_splices() {
        use super::super::jitter::RELAXED_CORRELATION;
        // Review of #201: when no lag passed the energy check, every score stayed -inf, the
        // shortest lag counted as a peak and was removed: half a period of this voice. Its
        // low-passed start held only the filter's startup, which no later window matched.
        // Formants at 2 and 4 kHz, where the lag search's low-pass has its nulls: below 2 kHz
        // it is all but silent.
        let pcm = formant_voice(200.0, [2000.0, 4000.0], 50.0, 550 * 960);
        for start in (0..9600).step_by(97) {
            let mut block = pcm[start..start + 2880].to_vec();
            let (removed, correlation) = accelerate(&mut block, MAX_LAG, RELAXED_CORRELATION);
            assert!(removed == 0 || whole_periods(removed, 200.0), "at {start}: {removed} at {correlation:.3}");
        }
        let slot = play_backlog(&pcm, 960, 22, |_| false);
        for edit in slot.edits.iter().filter(|e| e.correlation.is_some()) {
            assert!(whole_periods(edit.removed, 200.0) && edit.correlation >= Some(0.5), "{edit:?}");
        }
    }

    #[test]
    fn quiet_candidate_is_shrunk_and_boundary_fades_for_120_samples() {
        let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
        let mut slot = Slot::new().unwrap();
        let now = Instant::now();
        // Less than the jitter buffer's `BACKLOG_TRIM` surplus: a shrink proposal, not a cut.
        for i in 0..6 {
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
                    for (excess, signal) in [(0, "reset"), (480, "reset"), (480, "continuous"), (480, "paused")] {
                        // The voices differ from the tone only in what playout may remove: one
                        // clock and 30 s (convergence is due by 20 s) keep the test's time.
                        if signal != "reset" && clock == Clock::Sequence {
                            continue;
                        }
                        let seconds = if signal == "reset" { 60 } else { 30 };
                        let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
                        let mut slot = Slot::new().unwrap();
                        let now = Instant::now();
                        // Cover one refill when a packet arrives just after it.
                        slot.end = callback + excess;
                        let mut packet = 0u64;
                        let mut at_twenty_seconds = 0;
                        for tick in (0..seconds * 48_000u64).step_by(callback) {
                            let at = now + Duration::from_secs_f64(tick as f64 / 48_000.0);
                            loop {
                                // Deterministic +/-3 ms jitter, independent of refills.
                                let jitter = [0, 144, -96, 48, -144, 96, -48][packet as usize % 7];
                                let arrival = (packet as i64 * 960 + phase + jitter).max(0) as u64;
                                if arrival > tick {
                                    break;
                                }
                                // A tone reset every packet, or a phase-continuous voice, with a
                                // 240 ms pause every second or without.
                                let data = if signal == "reset" {
                                    encode(&encoder, true)
                                } else {
                                    let phase = (packet * 960) as f32 * std::f32::consts::TAU * 150.0 / 48_000.0;
                                    let pause = signal == "paused" && packet % 50 >= 38;
                                    let pcm = if pause { vec![0.0; 960] } else { harmonic(150.0, 0.0, phase, 960) };
                                    let mut data = vec![0; 1275];
                                    let n = encoder.encode_float(&pcm, &mut data).unwrap();
                                    data.truncate(n);
                                    data
                                };
                                slot.buffer.push_at(
                                    packet * 960,
                                    960,
                                    data,
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
                        let label = format!("{clock:?}, callback={callback}, phase={phase}, excess={excess}, {signal}");
                        assert_eq!(slot.buffer.concealments, 0, "{label}, shrunk={}", slot.buffer.shrink_samples);
                        assert_eq!(slot.buffer.concealments_after_trim, 0, "{label}");
                        if excess == 0 {
                            assert_eq!(slot.buffer.shrink_samples, 0, "{label}: tail is refill margin");
                        } else {
                            // Less than 30 ms decoded is never searched for a period, and this
                            // queue holds nothing to decode ahead: the surplus goes in pauses.
                            // A tone reset every packet has no period across the reset anyway.
                            if signal == "paused" {
                                assert!(at_twenty_seconds > 0, "{label}: persistent surplus did not converge");
                            }
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

    /// 60 s of ordered delivery without loss; the first packet at or after every `period`
    /// is `delay` late. Playout starts 50 ms behind; refill `k` runs `late(k)` samples after
    /// its callback's start. Returns the concealments and the samples cut.
    fn periodic_delay(
        clock: super::super::jitter::Clock,
        durations: &[u64],
        callbacks: &[usize],
        period: u64,
        delay: u64,
        late: impl Fn(u64) -> u64,
    ) -> (u64, u64) {
        periodic_delay_by(false, clock, durations, callbacks, period, delay, late)
    }

    /// `periodic_delay`, optionally by the reserve rule alone (the rule on `main` before the
    /// arrival rule).
    fn periodic_delay_by(
        reserve_only: bool,
        clock: super::super::jitter::Clock,
        durations: &[u64],
        callbacks: &[usize],
        period: u64,
        delay: u64,
        late: impl Fn(u64) -> u64,
    ) -> (u64, u64) {
        use super::super::jitter::Clock;
        let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
        let mut slot = Slot::new().unwrap();
        slot.buffer.reserve_only = reserve_only;
        let start = Instant::now();
        // Silence: one packet per duration does.
        let mut packets = std::collections::HashMap::new();
        let (mut arrivals, mut media, mut last, mut boundary) = (Vec::new(), 0u64, 0u64, 0u64);
        for i in 0.. {
            let n = durations[i % durations.len()];
            let late = if media >= boundary + period {
                boundary += period;
                delay
            } else {
                0
            };
            let time = (media + late).saturating_sub(2400).max(last);
            last = time;
            let key = if clock == Clock::Sequence { i as u64 * 960 } else { media };
            let data = packets.entry(n).or_insert_with(|| encode_n(&encoder, false, n as usize)).clone();
            arrivals.push((time, key, n, data));
            media += n;
            if media > 61 * 48_000 {
                break;
            }
        }
        let mut arrivals = arrivals.into_iter().peekable();
        let mut tick = 0u64;
        for (k, &callback) in callbacks.iter().cycle().enumerate() {
            if tick >= 60 * 48_000 {
                break;
            }
            let at = tick + late(k as u64);
            let now = start + Duration::from_secs_f64(at as f64 / 48_000.0);
            while let Some((time, key, n, data)) = arrivals.next_if(|a| a.0 <= at) {
                slot.buffer.push_at(key, n, data, start + Duration::from_secs_f64(time as f64 / 48_000.0), clock);
            }
            slot.mix_refill(&mut vec![0.0; callback], 1.0, now, callback).unwrap();
            tick += callback as u64;
        }
        (slot.buffer.concealments, slot.buffer.shrink_samples)
    }

    #[test]
    fn rare_delays_with_varying_callbacks_or_durations_keep_the_jitter_reserve() {
        // Each delay can meet a refill at another point than the window's delays did, so the
        // five-second minimum alone can understate the next one's need. The arrival rule covers
        // it for these refills, and the jitter reserve for mixed packet durations
        // (`JitterBuffer::refresh_surplus`). With neither, the four gave 4, 2, 9 and 2
        // concealments.
        use super::super::jitter::Clock;
        for clock in [Clock::Rtp, Clock::Sequence] {
            // 10 ms packets, callbacks cycling 10/20/5 ms, 35 ms late every 4.8 s.
            let (varying_callbacks, _) = periodic_delay(clock, &[480], &[480, 960, 240], 4800 * 48, 35 * 48, |_| 0);
            assert!(varying_callbacks <= 1, "{clock:?}: {varying_callbacks}");
            // 40/2.5/10/5/40 ms packets, 10 ms callbacks, 5 ms late after every 3 s.
            let (varying_durations, _) =
                periodic_delay(clock, &[1920, 120, 480, 240, 1920], &[480], 3 * 48_000, 5 * 48, |_| 0);
            assert_eq!(varying_durations, 0, "{clock:?}");
            // 10 ms packets, fixed 8 ms callbacks, 35 ms late every 3.3 s: every refill asks for
            // the same amount, but 8 ms refills walk through the 10 ms packets' phases (1
            // concealment with the reserve alone).
            let (walking_phase, _) = periodic_delay(clock, &[480], &[384], 3300 * 48, 35 * 48, |_| 0);
            assert!(walking_phase <= 1, "{clock:?}: {walking_phase}");
            // 10 ms packets and 10 ms refills, 35 ms late every 3 s, but each refill runs at
            // another point of its period: equal whole-packet refills whose times walk through
            // the packets' phases.
            let (walking_times, _) =
                periodic_delay(clock, &[480], &[480], 3 * 48_000, 35 * 48, |k| 479 + (17 * k) % 480);
            assert_eq!(walking_times, 0, "{clock:?}");
        }
    }

    #[test]
    fn a_refill_slightly_early_does_not_miss_a_delayed_burst() {
        // Review round 4: 10 ms packets and refills 5 ms into each callback, 35 ms late every
        // 1.1 s, so the delayed burst lands exactly on a refill; every hundredth refill runs
        // 0.5 ms early and misses it. Capping the reserve at the refills' phase spread (24
        // samples) gave 5 concealments and 2,640 samples of cuts; the reserve rule alone gives
        // none of either. A refill 0.5 ms early, just before the burst, still has 360 samples
        // (7.5 ms) over its demand and the safety margin at the 50 ms start, and that is all the
        // arrival rule may cut.
        use super::super::jitter::Clock;
        for clock in [Clock::Rtp, Clock::Sequence] {
            let late = |k| if k % 100 == 98 { 216 } else { 240 };
            let (concealments, cut) = periodic_delay(clock, &[480], &[480], 1100 * 48, 35 * 48, late);
            assert_eq!(concealments, 0, "{clock:?}");
            assert!(cut <= 360, "{clock:?}: {cut}");
        }
    }

    /// Refills 20 ms late for half of every hundred callbacks, then back on time in 5 ms steps.
    fn late_then_catch_up(k: u64) -> u64 {
        match k % 100 {
            10..=59 => 960,
            60 => 720,
            61 => 480,
            62 => 240,
            _ => 0,
        }
    }

    #[test]
    fn refills_catching_up_meet_the_packets_that_arrived_before_earlier_refills() {
        // Review round 5: 10 ms packets and refills, 35 ms late every 1.1 s, refills running
        // 20 ms late for half of each second and then catching up. Every refill still consumes
        // one packet, so they enter the packets alike, but the on-time refills ran 20 ms earlier
        // than the late ones: further back than the previous observation. Counting only the
        // arrivals since then gave 5 concealments and 2,760 samples of cuts; the reserve rule
        // alone gives 0 and 480.
        use super::super::jitter::Clock;
        for clock in [Clock::Rtp, Clock::Sequence] {
            let (concealments, cut) = periodic_delay(clock, &[480], &[480], 1100 * 48, 35 * 48, late_then_catch_up);
            let (_, reserve_cut) =
                periodic_delay_by(true, clock, &[480], &[480], 1100 * 48, 35 * 48, late_then_catch_up);
            assert_eq!(concealments, 0, "{clock:?}");
            assert!(cut <= reserve_cut, "{clock:?}: {cut} > {reserve_cut}");
        }
    }

    /// The arrival rule against the reserve rule alone (`main` before it) over a grid of
    /// `periodic_delay` schedules: packets of 10, 20, 5 ms and 40/2.5/10/5/40 ms; callbacks of
    /// 10, 8, 10/20/5, 20, 5 ms and 441 samples; 5/20/35 ms delays every 0.5, 1.1, 2, 3, 3.3 and
    /// 4.9 s; refills on time, 0.5 ms early every hundredth, 0-2 ms late at random, or 20 ms
    /// late for half of every hundred and then catching up (`late_then_catch_up`); both
    /// clocks. No schedule may conceal more. About a minute in release:
    /// `cargo test --release -p gwar-client --lib drain_grid -- --ignored --nocapture`.
    #[test]
    fn a_delayed_refill_over_short_packets_is_not_an_overflow() {
        use super::super::jitter::Clock;
        // 64 packets of 2.5 or 5 ms are only 160 or 320 ms of audio, which a refill serviced late
        // with speaker headroom can queue legitimately. A 64-packet cap trimmed that to the
        // target and dropped what the catch-up refill needed: 1,920 and 3,840 samples cut, and
        // 51 and 40 concealments instead of 27 and 20. The cap is 1 s of audio now.
        for clock in [Clock::Rtp, Clock::Sequence] {
            for (packet, callback, late_ms) in [(120, 5760, 60), (240, 11_520, 100)] {
                let late = periodic_delay(clock, &[packet], &[callback], u64::MAX / 2, 0, |k| {
                    if k == 10 { late_ms * 48 } else { 0 }
                });
                let (on_time, _) = periodic_delay(clock, &[packet], &[callback], u64::MAX / 2, 0, |_| 0);
                assert_eq!(late, (on_time, 0), "{clock:?} {packet}");
            }
        }
    }

    #[test]
    #[ignore = "slow; run on demand"]
    fn drain_grid_never_conceals_more_than_the_reserve_rule() {
        use super::super::jitter::Clock;
        let durations: [&[u64]; 4] = [&[480], &[960], &[240], &[1920, 120, 480, 240, 1920]];
        let callbacks: [&[usize]; 6] = [&[480], &[384], &[480, 960, 240], &[960], &[240], &[441]];
        let timings = ["exact", "early", "random", "catch-up"];
        let late = |timing: usize, k: u64| match timing {
            0 => 0,
            1 if k % 100 == 98 => 216,
            1 => 240,
            3 => late_then_catch_up(k),
            _ => {
                // splitmix64
                let mut z = k.wrapping_add(0x9e37_79b9_7f4a_7c15);
                z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                (z ^ (z >> 31)) % 97
            }
        };
        let mut schedules = Vec::new();
        for clock in [Clock::Rtp, Clock::Sequence] {
            for timing in 0..timings.len() {
                for d in durations {
                    for c in callbacks {
                        for delay in [5u64, 20, 35] {
                            for period in [500u64, 1100, 2000, 3000, 3300, 4900] {
                                schedules.push((clock, timing, d, c, delay, period));
                            }
                        }
                    }
                }
            }
        }
        let next = std::sync::atomic::AtomicUsize::new(0);
        let results = std::sync::Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(&(clock, timing, d, c, delay, period)) = schedules.get(i) else { break };
                        let run = |reserve_only| {
                            periodic_delay_by(reserve_only, clock, d, c, period * 48, delay * 48, |k| late(timing, k))
                        };
                        let (main, new) = (run(true), run(false));
                        results.lock().unwrap().push((i, main, new));
                    }
                });
            }
        });
        let mut results = results.into_inner().unwrap();
        results.sort_by_key(|r| r.0);
        let mut worse = Vec::new();
        println!("| Clock | Timing | PLC main | PLC new | Cut main | Cut new | Drains more | Drains less |");
        println!("| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |");
        for clock in [Clock::Rtp, Clock::Sequence] {
            for (timing, name) in timings.iter().enumerate() {
                let (mut plc, mut cut, mut more, mut less) = ((0, 0), (0, 0), 0, 0);
                for &(i, main, new) in &results {
                    let (c, t, d, cb, delay, period) = schedules[i];
                    if c != clock || t != timing {
                        continue;
                    }
                    plc = (plc.0 + main.0, plc.1 + new.0);
                    cut = (cut.0 + main.1, cut.1 + new.1);
                    more += u32::from(new.1 > main.1);
                    less += u32::from(new.1 < main.1);
                    if new.0 > main.0 {
                        worse.push(format!("{c:?} {name} {d:?} {cb:?} {delay} ms/{period} ms: {main:?} -> {new:?}"));
                    }
                }
                println!("| {clock:?} | {name} | {} | {} | {} | {} | {more} | {less} |", plc.0, plc.1, cut.0, cut.1);
            }
        }
        println!("\n| Packets | Callbacks | PLC main | PLC new | Cut main | Cut new |");
        println!("| --- | --- | ---: | ---: | ---: | ---: |");
        for d in durations {
            for cb in callbacks {
                let (mut plc, mut cut) = ((0, 0), (0, 0));
                for &(i, main, new) in &results {
                    if schedules[i].2 == d && schedules[i].3 == cb {
                        plc = (plc.0 + main.0, plc.1 + new.0);
                        cut = (cut.0 + main.1, cut.1 + new.1);
                    }
                }
                println!("| {d:?} | {cb:?} | {} | {} | {} | {} |", plc.0, plc.1, cut.0, cut.1);
            }
        }
        assert!(worse.is_empty(), "more concealments than the reserve rule:\n{}", worse.join("\n"));
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
    fn a_small_residual_drains_through_frequent_loss_of_voice() {
        use super::super::jitter::Clock;
        for clock in [Clock::Rtp, Clock::Sequence] {
            for excess in [480u64, 960] {
                let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
                let mut slot = Slot::new().unwrap();
                let now = Instant::now();
                let mut packet = 0u64;
                let mut losses = 0;
                for tick in (0..20 * 48_000u64).step_by(480) {
                    let at = now + Duration::from_secs_f64(tick as f64 / 48_000.0);
                    loop {
                        let scheduled = (packet * 480).saturating_sub(excess);
                        if scheduled > tick {
                            break;
                        }
                        // Two losses a second, one of them a burst of two.
                        let phase = scheduled % 48_000;
                        let lost = scheduled > 0 && (phase == 0 || phase == 480 || phase == 24_000);
                        losses += u64::from(lost);
                        if !lost {
                            // Phase-continuous voice: the decoder only cuts a correlated period.
                            let pcm = harmonic(
                                150.0,
                                0.0,
                                (packet * 480) as f32 * std::f32::consts::TAU * 150.0 / 48_000.0,
                                480,
                            );
                            let mut data = vec![0; 1275];
                            let n = encoder.encode_float(&pcm, &mut data).unwrap();
                            data.truncate(n);
                            slot.buffer.push_at(
                                packet * if clock == Clock::Sequence { 960 } else { 480 },
                                480,
                                data,
                                now + Duration::from_secs_f64(scheduled as f64 / 48_000.0),
                                clock,
                            );
                        }
                        packet += 1;
                    }
                    slot.mix_refill(&mut [0.0; 480], 1.0, at, 480).unwrap();
                }
                let tail = slot.buffer.depth().1 + slot.staged_samples() as u64;
                let label = format!("{clock:?}, excess={excess}");
                assert!(
                    tail <= slot.buffer.target_samples() + 240,
                    "{label}: tail={tail}, shrunk {}",
                    slot.buffer.shrink_samples
                );
                assert_eq!(slot.buffer.concealments, losses, "{label}: draining must not starve");
            }
        }
    }

    #[test]
    fn mixed_packet_durations_drain_alike_on_both_clocks() {
        // TS packet counters step one stride per packet whatever its duration, so depth must
        // come from the packets' own durations: a sequence stream may not drain more or less
        // than the same stream with RTP timestamps.
        use super::super::jitter::Clock;
        for (pattern, delay) in [(&[1920u64, 120, 480, 240, 1920][..], 960u64), (&[960, 480][..], 0)] {
            let mut results = Vec::new();
            for clock in [Clock::Sequence, Clock::Rtp] {
                let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
                let mut slot = Slot::new().unwrap();
                let start = Instant::now();
                let (mut arrivals, mut media, mut last) = (Vec::new(), 0u64, 0u64);
                for i in 0.. {
                    let n = pattern[i % pattern.len()];
                    // Every three seconds one refill's worth of packets comes late, in order.
                    let late = media > 0 && media % (3 * 48_000) < 960;
                    let time = (media + if late { delay } else { 0 }).max(last);
                    last = time;
                    let key = if clock == Clock::Sequence { i as u64 * 960 } else { media };
                    arrivals.push((time, key, n, encode_n(&encoder, false, n as usize)));
                    media += n;
                    if media > 30 * 48_000 {
                        break;
                    }
                }
                slot.end = 1440;
                let mut arrivals = arrivals.into_iter().peekable();
                for tick in (0..30 * 48_000u64).step_by(960) {
                    let now = start + Duration::from_secs_f64(tick as f64 / 48_000.0);
                    while let Some((time, key, n, data)) = arrivals.next_if(|a| a.0 <= tick) {
                        let at = start + Duration::from_secs_f64(time as f64 / 48_000.0);
                        slot.buffer.push_at(key, n, data, at, clock);
                    }
                    for (offset, take) in [(0, 480), (480, 480)] {
                        slot.mix_refill(&mut [0.0; 480][..take], 1.0, now, 960 - offset).unwrap();
                    }
                }
                let tail = slot.buffer.depth().1 + slot.staged_samples() as u64;
                results.push((slot.buffer.concealments, tail));
            }
            assert_eq!(results[0], results[1], "pattern {pattern:?}: (concealments, tail) sequence vs rtp");
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
                            // Less than 30 ms decoded is never searched for a period, and here
                            // the queue holds nothing to decode ahead: the surplus goes in quiet
                            // frames, a 240 ms pause every second.
                            let pcm = harmonic(240.0, 0.0, tick as f32 * std::f32::consts::TAU / 200.0, 960);
                            let pcm = if tick % 48_000 >= 36_480 { vec![0.0; 960] } else { pcm };
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
                    let removed = accelerate(&mut actual, MAX_LAG, SHRINK_CORRELATION).0;
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
            assert_eq!(accelerate(&mut pcm, MAX_LAG, SHRINK_CORRELATION).0, 0);
            assert_eq!(pcm, original);
        }
        let original = harmonic(100.0, 0.02, 0.5, 2160);
        for (length, budget) in [(2160, 119), (2160, 240), (480, MAX_LAG)] {
            let mut pcm = original[..length].to_vec();
            assert_eq!(accelerate(&mut pcm, budget, SHRINK_CORRELATION).0, 0);
            assert_eq!(pcm, original[..length]);
        }
    }

    #[test]
    fn acceleration_never_exceeds_budget_and_removes_exactly_one_overlap() {
        let original = harmonic(200.0, 0.0, 0.5, 2160);
        for budget in 0..=800 {
            let mut pcm = original.clone();
            let removed = accelerate(&mut pcm, budget, SHRINK_CORRELATION).0;
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

    fn noise(seed: u32, amplitude: f32, n: usize) -> Vec<f32> {
        let mut seed = seed;
        (0..n)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                (seed as f32 / u32::MAX as f32 - 0.5) * 2.0 * amplitude
            })
            .collect()
    }

    /// The normalized correlation of `pcm` with itself `lag` samples later.
    fn periodicity(pcm: &[f32], lag: usize) -> f64 {
        let (a, b) = (&pcm[..pcm.len() - lag], &pcm[lag..]);
        let dot = |x: &[f32], y: &[f32]| x.iter().zip(y).map(|(&x, &y)| f64::from(x) * f64::from(y)).sum::<f64>();
        dot(a, b) / (dot(a, a) * dot(b, b)).sqrt()
    }

    /// 45 ms of a `pitch` Hz voice with 3 % vibrato and noise at `noise_level` peak.
    fn rough_voice(pitch: f32, noise_level: f32, seed: u32) -> Vec<f32> {
        let voice = harmonic(pitch, 0.03, 0.3, 2160);
        voice.iter().zip(noise(seed, noise_level, 2160)).map(|(v, n)| v + n).collect()
    }

    /// Whether `removed` is a whole number of `pitch` Hz periods, within 5 %.
    fn whole_periods(removed: usize, pitch: f32) -> bool {
        let periods = removed as f64 / (48_000.0 / f64::from(pitch));
        periods >= 0.95 && (periods - periods.round()).abs() <= 0.05 * periods.round()
    }

    #[test]
    fn backlog_acceleration_takes_periods_neteq_accepts_without_clicks() {
        use super::super::jitter::BACKLOG_CORRELATION;
        // Noise 12 dB down: adjacent periods correlate between NetEq's threshold and the gentle
        // shrink's.
        for pitch in [100.0, 150.0, 220.0] {
            let input = rough_voice(pitch, 0.07, 3);
            let mut strict = input.clone();
            assert_eq!(accelerate(&mut strict, MAX_LAG, SHRINK_CORRELATION).0, 0, "pitch={pitch}");
            let mut actual = input.clone();
            let (removed, correlation) = accelerate(&mut actual, MAX_LAG, BACKLOG_CORRELATION);
            assert!(whole_periods(removed, pitch), "pitch={pitch}: {removed} at {correlation:.3}");
            actual.truncate(input.len() - removed);
            // The splice is continuous at both ends and keeps the rest unchanged.
            assert_eq!(actual[0], input[0]);
            assert_eq!(&actual[removed..], &input[2 * removed..]);
            assert!(maximum_jump(&actual) <= maximum_jump(&input) * 1.05, "pitch={pitch}");
            // The voice stays as periodic as it was.
            let lag = (48_000.0 / pitch).round() as usize;
            let (before, after) = (periodicity(&input, lag), periodicity(&actual, lag));
            assert!(after >= before - 0.02, "pitch={pitch}: periodicity {before:.3} -> {after:.3}");
        }
    }

    #[test]
    fn the_relaxed_rule_still_removes_only_whole_periods_of_a_voice() {
        use super::super::jitter::{BACKLOG_CORRELATION, RELAXED_CORRELATION};
        // Review of #201: the relaxed rule searched only lags within the budget, so a budget
        // shorter than the period took a sub-period lag (141 samples of a 150 Hz voice at 0.80,
        // periodicity 0.995 -> 0.948). Noisy voices whose periods correlate at 0.6--0.85: below
        // NetEq's threshold, above its fast mode's.
        for pitch in [80.0, 100.0, 150.0, 220.0] {
            for seed in 1..=4 {
                let input = rough_voice(pitch, 0.16, seed);
                let mut pcm = input.clone();
                let (removed, correlation) = accelerate(&mut pcm, MAX_LAG, BACKLOG_CORRELATION);
                assert_eq!(removed, 0, "pitch={pitch} seed={seed}: {correlation:.3}");
                assert!((0.6..0.9).contains(&correlation), "pitch={pitch} seed={seed}: {correlation:.3}");
                for budget in [MIN_LAG, 240, 360, 480, 600, MAX_LAG] {
                    let mut pcm = input.clone();
                    let (removed, correlation) = accelerate(&mut pcm, budget, RELAXED_CORRELATION);
                    let label = format!("pitch={pitch} seed={seed} budget={budget}: {removed} at {correlation:.3}");
                    if removed == 0 {
                        // Only a period beyond the budget waits.
                        assert!(budget < 2 * (48_000.0 / pitch) as usize, "{label}");
                        assert_eq!(pcm, input, "{label}");
                        continue;
                    }
                    assert!(removed <= budget && whole_periods(removed, pitch), "{label}");
                    assert!(correlation >= 0.5, "{label}");
                    pcm.truncate(input.len() - removed);
                    assert!(maximum_jump(&pcm) <= maximum_jump(&input) * 1.05, "{label}");
                }
            }
        }
    }

    #[test]
    fn aperiodic_audio_never_splices() {
        use super::super::jitter::RELAXED_CORRELATION;
        // Review of #201: loud noise (or unvoiced or whispered speech) has no period, and the
        // relaxed rule used to remove its best lag at -0.08--0.25. No caller's threshold goes
        // below NetEq's fast mode's.
        let input = noise(11, 0.3, 2160);
        for threshold in [RELAXED_CORRELATION, -1.0] {
            let mut actual = input.clone();
            let (removed, correlation) = accelerate(&mut actual, MAX_LAG, threshold);
            assert_eq!(removed, 0, "{correlation}");
            assert!(correlation < 0.5, "{correlation}");
            assert_eq!(actual, input);
        }
        // Silence, DC and less than 30 ms never splice.
        let voice = harmonic(150.0, 0.0, 0.3, 2160);
        for original in [vec![0.0; 2160], vec![0.2; 2160], voice[..2 * MAX_LAG - 1].to_vec()] {
            let mut pcm = original.clone();
            assert_eq!(accelerate(&mut pcm, MAX_LAG, RELAXED_CORRELATION).0, 0);
            assert_eq!(pcm, original);
        }
    }

    #[test]
    fn a_block_shorter_than_every_lag_never_splices() {
        // Review of #201: 5 ms packets with every third lost leave blocks of 10 ms at most, and
        // a search over the lags they held took 120 samples of a 100 Hz voice (period 480) at 0.90:
        // its fourth harmonic, near a formant, repeats every 120.
        let pcm = formant_voice(100.0, [400.0, 1200.0], 30.0, 2200 * 240);
        let slot = play_backlog(&pcm, 240, 22, |i| i % 3 == 2);
        for edit in slot.edits.iter().filter(|e| e.correlation.is_some()) {
            assert!(whole_periods(edit.removed, 100.0) && edit.correlation >= Some(0.5), "{edit:?}");
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
