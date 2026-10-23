//! Corroborated recovery, retained across publisher replacement and bounded in time.
use gwar_proto::{ScreenRecovery, ScreenRecoveryCodec, SessionId};
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

pub(super) const HOLD_DOWN: Duration = Duration::from_secs(10);
const RESTORE_AFTER: Duration = Duration::from_secs(120);
const MAX_BACKOFF: u8 = 4; // 2, 4, 8, 16, then 32 minutes until the share ends.

#[derive(Debug)]
pub(super) enum Failure {
    Plan(ScreenRecovery),
    Deferred,
    Incompatible,
}

#[derive(Debug, Default)]
pub(super) struct Recovery {
    /// Last fallback rendition; retained during an original-H.264 trial for rollback.
    step: u8,
    last: Option<Instant>,
    target: Option<(u32, u32, u32)>,
    original: Option<(u32, u32)>,
    reporters: HashSet<SessionId>,
    trial: bool,
    backoff: u8,
    /// Old-publisher media is fenced until the replacement offer is accepted.
    pub pending: bool,
}
impl Recovery {
    pub fn fail(
        &mut self,
        share: &str,
        size: (u32, u32),
        vp9: bool,
        reporters: HashSet<SessionId>,
        now: Instant,
    ) -> Failure {
        if self.pending {
            return Failure::Deferred;
        }
        if self.trial {
            // A failed trial must immediately return to the last working rendition,
            // even during hold-down. Keep its dimensions, codec and every watch.
            self.trial = false;
            self.backoff = self.backoff.saturating_add(1).min(MAX_BACKOFF);
        } else {
            if self.last.is_some_and(|at| now.saturating_duration_since(at) < HOLD_DOWN) {
                return Failure::Deferred;
            }
            if self.step >= 2 || (self.step == 1 && !vp9) {
                return Failure::Incompatible;
            }
            let (width, height) = size;
            self.target.get_or_insert(if width > 1920 || height > 1080 {
                (1920, 1080, 60)
            } else if width > 1280 || height > 720 {
                (1280, 720, 30)
            } else {
                (640, 360, 30)
            });
            self.original.get_or_insert(size);
            self.step += 1;
        }
        let (width, height, fps) = self.target.expect("fallback rendition");
        let codec = if self.step == 1 { ScreenRecoveryCodec::H264 } else { ScreenRecoveryCodec::Vp9 };
        self.last = Some(now);
        self.reporters.extend(reporters);
        self.pending = true;
        Failure::Plan(ScreenRecovery { share: share.into(), codec, width, height, fps })
    }

    pub fn restore(&mut self, share: &str, active: &HashSet<SessionId>, now: Instant) -> Option<ScreenRecovery> {
        let elapsed = now.saturating_duration_since(self.last?);
        let delay = RESTORE_AFTER * (1 << self.backoff);
        // Reporter departure can accelerate the first trial, but never defeat backoff
        // after a failed trial. A successful trial stays at the original rendition.
        if self.step == 0
            || self.pending
            || self.trial
            || elapsed < HOLD_DOWN
            || (elapsed < delay && (self.backoff != 0 || !self.reporters.is_disjoint(active)))
        {
            return None;
        }
        let (width, height) = self.original?;
        self.trial = true;
        self.last = Some(now);
        self.pending = true;
        Some(ScreenRecovery { share: share.into(), codec: ScreenRecoveryCodec::H264, width, height, fps: 60 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(failure: Failure) -> ScreenRecovery {
        let Failure::Plan(plan) = failure else { panic!("expected plan, got {failure:?}") };
        plan
    }

    #[test]
    fn failed_restore_rolls_back_immediately_and_trials_back_off_until_share_ends() {
        for vp9 in [false, true] {
            let mut now = Instant::now();
            let mut r = Recovery::default();
            let reporters = HashSet::from([7]);
            let mut working = plan(r.fail("s", (1920, 1080), vp9, reporters.clone(), now));
            assert_eq!(
                (working.codec, working.width, working.height, working.fps),
                (ScreenRecoveryCodec::H264, 1280, 720, 30)
            );
            assert!(matches!(r.fail("s", (1920, 1080), vp9, reporters.clone(), now + HOLD_DOWN), Failure::Deferred));
            r.pending = false;
            if vp9 {
                now += HOLD_DOWN;
                working = plan(r.fail("s", (1280, 720), vp9, reporters.clone(), now));
                assert_eq!(working.codec, ScreenRecoveryCodec::Vp9);
                r.pending = false;
            }
            for minutes in [2, 4, 8, 16, 32, 32] {
                let trial = now + Duration::from_secs(minutes * 60);
                assert!(r.restore("s", &reporters, trial - Duration::from_secs(1)).is_none());
                let restored = r.restore("s", &reporters, trial).unwrap();
                assert_eq!((restored.codec, restored.width, restored.height), (ScreenRecoveryCodec::H264, 1920, 1080));
                r.pending = false;
                // A healthy trial never emits another restore, however long it runs.
                assert!(r.restore("s", &reporters, trial + Duration::from_secs(3600)).is_none());
                now = trial + Duration::from_secs(4);
                assert_eq!(plan(r.fail("s", (1920, 1080), vp9, reporters.clone(), now)), working);
                r.pending = false;
                assert!(
                    r.restore("s", &HashSet::new(), now + HOLD_DOWN).is_none(),
                    "departure does not bypass backoff"
                );
            }
            let mut next_share = Recovery::default();
            plan(next_share.fail("new", (1920, 1080), vp9, reporters.clone(), now));
            next_share.pending = false;
            assert!(next_share.restore("new", &reporters, now + RESTORE_AFTER).is_some());
        }
    }

    #[test]
    fn departing_reporter_restores_after_hold_and_no_vp9_is_never_requested() {
        let now = Instant::now();
        let mut r = Recovery::default();
        plan(r.fail("s", (2560, 1440), false, HashSet::from([7]), now));
        r.pending = false;
        assert!(r.restore("s", &HashSet::new(), now + Duration::from_secs(9)).is_none());
        assert!(matches!(r.fail("s", (1920, 1080), false, HashSet::from([7]), now + HOLD_DOWN), Failure::Incompatible));
        assert_eq!(r.restore("s", &HashSet::from([8]), now + HOLD_DOWN).unwrap().width, 2560);
    }

    #[test]
    fn serialized_restore_plans_match_the_web_parser_vectors() {
        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../../../tests/screen-recovery-plans.json")).unwrap();
        for vector in vectors.as_array().unwrap() {
            let frame = vector.clone();
            let original: ScreenRecovery = serde_json::from_value(frame["d"].clone()).unwrap();
            let now = Instant::now();
            let mut r = Recovery::default();
            plan(r.fail(&original.share, (original.width, original.height), true, HashSet::from([7]), now));
            r.pending = false;
            let restored = r.restore(&original.share, &HashSet::from([7]), now + RESTORE_AFTER).unwrap();
            let event = gwar_proto::Event::ScreenRecover(restored);
            assert_eq!(serde_json::to_value(event).unwrap(), frame);
        }
    }
}
