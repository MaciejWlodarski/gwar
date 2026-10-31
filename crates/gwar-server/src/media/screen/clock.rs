//! Preserve the publisher's common audio/video source clock across application queueing.
//! One anchor per publish transport; packet arrival remains the queue deadline's clock.
use std::time::{Duration, Instant, SystemTime};
use str0m::rtp::rtcp::SenderInfo;

#[derive(Default)]
pub(super) struct Clock {
    anchor: Option<(SystemTime, Instant)>,
}

impl Clock {
    pub fn map(&mut self, timestamp: u32, arrival: Instant, report: Option<SenderInfo>, rate: u32) -> Instant {
        let Some(report) = report else { return arrival };
        let ticks = timestamp.wrapping_sub(report.rtp_time.numer() as u32) as i32;
        let delta = Duration::from_secs_f64(f64::from(ticks.unsigned_abs()) / f64::from(rate));
        let source = if ticks >= 0 { report.ntp_time.checked_add(delta) } else { report.ntp_time.checked_sub(delta) };
        let Some(source) = source else { return arrival };
        let (remote, local) = *self.anchor.get_or_insert((source, arrival));
        let mapped = match source.duration_since(remote) {
            Ok(delta) => local.checked_add(delta),
            Err(e) => local.checked_sub(e.duration()),
        };
        // A faster path must move the common mapping back, not leave every subsequent
        // wallclock ahead of receipt (str0m cannot produce a sender report for that).
        // Rebase both clocks together; clamping only this packet would lose A/V sync.
        if mapped.is_none_or(|at| at > arrival || arrival.duration_since(at) >= Duration::from_secs(10)) {
            self.anchor = Some((source, arrival));
            arrival
        } else {
            mapped.unwrap()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use str0m::media::MediaTime;

    #[test]
    fn queued_video_and_immediate_audio_keep_the_same_source_time() {
        let now = Instant::now();
        let ntp = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let report = |rtp, rate| SenderInfo {
            ssrc: 1.into(),
            ntp_time: ntp,
            rtp_time: MediaTime::new(rtp, str0m::media::Frequency::new(rate).unwrap()),
            sender_packet_count: 1,
            sender_octet_count: 100,
        };
        let mut clock = Clock::default();
        let audio = clock.map(48_000, now, Some(report(48_000, 48_000)), 48_000);
        let video = clock.map(90_000, now + Duration::from_millis(160), Some(report(90_000, 90_000)), 90_000);
        assert_eq!(audio, video, "network arrival and release delays do not redefine source time");
        let next_audio = clock.map(48_960, now + Duration::from_millis(25), Some(report(48_000, 48_000)), 48_000);
        let next_video = clock.map(91_800, now + Duration::from_millis(190), Some(report(90_000, 90_000)), 90_000);
        assert_eq!(next_audio, next_video);
        assert_eq!(next_audio.duration_since(audio), Duration::from_millis(20));
    }

    #[test]
    fn a_faster_path_rebases_both_clocks_instead_of_clamping_each_packet() {
        let start = Instant::now();
        let ntp = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let report = |rate| SenderInfo {
            ssrc: 1.into(),
            ntp_time: ntp,
            rtp_time: MediaTime::new(rate as u64, str0m::media::Frequency::new(rate).unwrap()),
            sender_packet_count: 1,
            sender_octet_count: 100,
        };
        let mut clock = Clock::default();
        clock.map(48_000, start + Duration::from_millis(100), Some(report(48_000)), 48_000);
        let audio = clock.map(96_000, start + Duration::from_millis(1020), Some(report(48_000)), 48_000);
        let video = clock.map(180_000, start + Duration::from_millis(1180), Some(report(90_000)), 90_000);
        assert_eq!(audio, start + Duration::from_millis(1020));
        assert_eq!(video, audio, "delayed video keeps the adjusted common mapping");
        let next = clock.map(181_800, start + Duration::from_millis(1200), Some(report(90_000)), 90_000);
        assert_eq!(next - video, Duration::from_millis(20));
    }
}
