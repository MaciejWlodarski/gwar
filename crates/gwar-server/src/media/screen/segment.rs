//! Stable sequence mapping for one viewer slot (docs/screen-share.md, "Forwarding: stable
//! sequence mapping"). Within a segment a source packet always lands on the same output number,
//! so gaps stay gaps, a repaired or reordered packet arrives where it belongs, and the viewer's
//! own NACKs keep working. A segment starts at a keyframe; anything older than it drops.
use std::time::Instant;

/// The gap left between segments, so the viewer treats the hole as an old loss.
const SEGMENT_GAP: u64 = 64;
const VIDEO_CLOCK: u64 = 90_000;

#[derive(Debug, Default)]
pub struct Segment {
    /// Waiting for a keyframe's first packet before anything is forwarded.
    waiting: bool,
    base_source: u64,
    base_out: u64,
    /// Source packets below this are from before the segment and drop.
    resume: u64,
    timestamp_offset: u32,
    highest_out: Option<u64>,
    /// The last timestamp written and when, so a new segment continues the slot's timeline.
    last: Option<(u32, Instant)>,
}

impl Segment {
    pub fn new() -> Self {
        Self { waiting: true, ..Default::default() }
    }

    /// Stops forwarding until the next keyframe starts a new segment: a new share in the slot,
    /// a drop, a pause.
    pub fn wait(&mut self) {
        self.waiting = true;
    }

    pub fn waiting(&self) -> bool {
        self.waiting
    }

    /// The output sequence number and timestamp for a source packet, or `None` to drop it.
    pub fn map(&mut self, source: u64, timestamp: u32, keyframe: bool, now: Instant) -> Option<(u64, u32)> {
        if self.waiting {
            if !keyframe {
                return None;
            }
            self.waiting = false;
            self.base_source = source;
            self.resume = source;
            self.base_out = self.highest_out.map_or(0, |highest| highest + 1 + SEGMENT_GAP);
            // The new timeline continues the slot's: one wall-clock tick after its last write.
            let continued = self.last.map_or(timestamp, |(last, at)| {
                let elapsed = now.saturating_duration_since(at).as_micros() as u64 * VIDEO_CLOCK / 1_000_000;
                last.wrapping_add(elapsed.max(1) as u32)
            });
            self.timestamp_offset = continued.wrapping_sub(timestamp);
        }
        if source < self.resume {
            return None;
        }
        let out = self.base_out + (source - self.base_source);
        let stamp = timestamp.wrapping_add(self.timestamp_offset);
        if self.highest_out.is_none_or(|highest| out > highest) {
            self.highest_out = Some(out);
            self.last = Some((stamp, now));
        }
        Some((out, stamp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn gaps_and_late_repairs_keep_their_place() {
        let now = Instant::now();
        let mut segment = Segment::new();
        assert_eq!(segment.map(100, 5000, false, now), None, "waits for a keyframe");
        assert_eq!(segment.map(101, 9000, true, now), Some((0, 9000)));
        // 102 is lost on the way in; 103 arrives first, then the repaired 102.
        assert_eq!(segment.map(103, 9000, false, now), Some((2, 9000)));
        assert_eq!(segment.map(102, 9000, false, now), Some((1, 9000)));
        // A retransmission maps consistently.
        assert_eq!(segment.map(102, 9000, false, now), Some((1, 9000)));
        // Anything from before the segment drops, even a late repair.
        assert_eq!(segment.map(100, 5000, false, now), None);
    }

    #[test]
    fn a_new_segment_jumps_ahead_and_continues_the_timeline() {
        let now = Instant::now();
        let mut segment = Segment::new();
        segment.map(10, 1000, true, now);
        segment.map(11, 4000, false, now);
        // A drop: the slot waits; deltas drop until the next keyframe.
        segment.wait();
        let later = now + Duration::from_millis(100);
        assert_eq!(segment.map(12, 7000, false, later), None);
        // Another share's keyframe (unrelated numbers and clock) in the same slot.
        let (out, stamp) = segment.map(5000, 123_456, true, later).unwrap();
        assert_eq!(out, 1 + 1 + SEGMENT_GAP);
        assert_eq!(stamp, 4000 + 9000, "100 ms after the last write at 90 kHz");
        assert_eq!(segment.map(5001, 126_456, false, later), Some((out + 1, stamp + 3000)));
        // Its stale packets from before the new segment drop.
        assert_eq!(segment.map(4999, 120_456, false, later), None);
    }

    #[test]
    fn timestamps_wrap_without_a_jump() {
        let now = Instant::now();
        let mut segment = Segment::new();
        assert_eq!(segment.map(0, u32::MAX - 999, true, now), Some((0, u32::MAX - 999)));
        assert_eq!(segment.map(1, 2000, false, now), Some((1, 2000)));
    }
}
