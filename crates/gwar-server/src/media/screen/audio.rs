//! Opus for plaintext screen shares. Uses the screen task and a bounded, separate ingress
//! bucket. At egress its fixed allowance is subtracted from video's existing 80% allocation:
//! voice's reserve and the voice task are never borrowed for share sound.
use super::{
    DIRECTIONS, Watcher,
    egress::{Egress, ORIGINAL_OVERHEAD, Queued},
    ingress::Bucket,
    sections, wipe,
};
use std::{
    collections::{HashSet, VecDeque},
    time::{Duration, Instant, SystemTime},
};
use str0m::{
    Rtc,
    format::Codec,
    media::{MediaKind, Mid, Pt},
    rtp::{RtpPacket, RtpWrite, Ssrc, rtcp::SenderInfo},
};

pub(super) const WIRE_BPS: u64 = 128_000;
const RATE: u32 = 48_000;
/// A gap in the sound's RTP time this much shorter than the silence it spans was a stop, not
/// loss; shorter differences are the network's.
const STOP_GAP: Duration = Duration::from_millis(250);

pub(super) fn publish_section(lines: &[&str]) -> bool {
    lines[0].starts_with("m=audio ")
        && lines.iter().filter(|l| DIRECTIONS.contains(&l.trim_end())).eq(["a=sendonly"].iter())
        && lines.iter().any(|l| l.to_ascii_lowercase().contains(" opus/48000/2"))
}

fn opus(rtc: &Rtc, mid: Mid) -> Option<Pt> {
    let media = rtc.media(mid)?;
    rtc.codec_config()
        .params()
        .iter()
        .find(|p| p.spec().codec == Codec::Opus && media.remote_pts().contains(&p.pt()))
        .map(|p| p.pt())
}

pub(super) struct Publisher {
    ssrc: Ssrc,
    pt: Pt,
    bytes: Bucket,
    packets: Bucket,
    pub time: SoundTime,
}

/// Keeps a share's sound on its publisher's clock across a stop.
///
/// A browser's sound RTP time counts the samples it sent, so sound stopped and resumed on the
/// same sender comes back a frame after where it left off, however long it was silent, and the
/// publisher's next sender report moves its line by the silence. Forwarded as it is, the line in
/// the reports a viewer gets moves too: its engine fits a clock estimate to reports from both
/// lines and holds the picture by their difference until it gives the old ones up, seconds
/// later. Here the RTP time the viewers get moves on by the silence instead, and until the
/// publisher reports its new line, its old report places the sound with the same correction.
#[derive(Default)]
pub(super) struct SoundTime {
    /// The publisher's RTP time and arrival of the last packet, and the line of the report
    /// that came with it.
    last: Option<(u32, Instant, Option<Line>)>,
    /// Resumes seen, from `ScreenOrigin::audio_resumes`.
    resumes: u64,
    /// Added to every RTP time forwarded: the silences of all stops so far.
    shift: u32,
    /// The last stop's silence while the publisher's reports still have the line from before
    /// it, that line, and when the sound came back.
    stale: Option<(u32, Line, Instant)>,
}

/// A sender report's NTP and RTP times.
type Line = (SystemTime, u32);

/// A publisher reports sound at least this often; a stale line is not kept longer.
const STALE_FOR: Duration = Duration::from_secs(10);

impl SoundTime {
    /// The RTP time to forward, and the one that places this packet on the publisher's clock
    /// with `report`. `resumes` counts the publisher's resumes of stopped sound.
    pub fn place(&mut self, rtp: u32, arrival: Instant, report: Option<&SenderInfo>, resumes: u64) -> (u32, u32) {
        let line = report.map(|r| (r.ntp_time, r.rtp_time.numer() as u32));
        if resumes != self.resumes
            && let Some((last, at, before)) = self.last
        {
            let missing = arrival.saturating_duration_since(at).as_secs_f64() - seconds(rtp.wrapping_sub(last));
            if missing > STOP_GAP.as_secs_f64() {
                let missing = (missing * f64::from(RATE)) as u32;
                self.shift = self.shift.wrapping_add(missing);
                // A report on a line other than the one before the stop already has the new one.
                let old = line.filter(|l| before.is_none_or(|b| !moved(*l, b)));
                self.stale = old.map(|l| (missing, l, arrival));
            }
        }
        self.resumes = resumes;
        self.last = Some((rtp, arrival, line));
        // A report sent during the silence is still on the old line; the first that is not has
        // the new one.
        if let Some((_, old, since)) = self.stale
            && (line.is_some_and(|l| moved(l, old)) || arrival.saturating_duration_since(since) >= STALE_FOR)
        {
            self.stale = None;
        }
        (rtp.wrapping_add(self.shift), rtp.wrapping_add(self.stale.map_or(0, |(missing, ..)| missing)))
    }
}

/// Whether two reports put the same RTP time a stop's worth apart.
fn moved(a: Line, b: Line) -> bool {
    let ntp = match a.0.duration_since(b.0) {
        Ok(d) => d.as_secs_f64(),
        Err(e) => -e.duration().as_secs_f64(),
    };
    (ntp - seconds(a.1.wrapping_sub(b.1))).abs() >= STOP_GAP.as_secs_f64()
}

/// An RTP time difference, either way, in seconds.
fn seconds(ticks: u32) -> f64 {
    f64::from(ticks as i32) / f64::from(RATE)
}

impl Publisher {
    /// The offer's one sound stream, if it has one. Read from the text, so a running transport
    /// can refuse a bad offer before anything about it changes.
    pub fn offered(sdp: &str) -> Result<Option<Ssrc>, &'static str> {
        let sections = sections(sdp);
        let audio: Vec<_> = sections.iter().filter(|s| s[0].starts_with("m=audio ")).collect();
        let Some(lines) = audio.first() else { return Ok(None) };
        if audio.len() != 1 || !publish_section(lines) {
            return Err("invalid screen audio offer");
        }
        let ssrcs: HashSet<u32> =
            lines.iter().filter_map(|l| l.strip_prefix("a=ssrc:")?.split_whitespace().next()?.parse().ok()).collect();
        if ssrcs.len() != 1 || ssrcs.contains(&0) {
            return Err("screen audio must name one SSRC");
        }
        Ok(Some(Ssrc::from(*ssrcs.iter().next().unwrap())))
    }

    pub fn from_sdp(rtc: &mut Rtc, sdp: &str, now: Instant) -> Result<Option<Self>, &'static str> {
        let Some(ssrc) = Self::offered(sdp)? else { return Ok(None) };
        let mid = rtc.direct_api().stream_rx(&ssrc).map(|s| s.mid()).ok_or("audio SSRC not negotiated")?;
        if rtc.media(mid).is_none_or(|m| m.kind() != MediaKind::Audio || !m.direction().is_receiving()) {
            return Err("audio not negotiated");
        }
        let pt = opus(rtc, mid).ok_or("screen audio requires Opus")?;
        Ok(Some(Self {
            ssrc,
            pt,
            bytes: Bucket::new(WIRE_BPS as f64 / 8.0, 2400.0, now),
            packets: Bucket::new(100.0, 4.0, now),
            time: SoundTime::default(),
        }))
    }

    pub fn accepts(&mut self, packet: &RtpPacket, now: Instant) -> bool {
        packet.header.ssrc == self.ssrc
            && packet.header.payload_type == self.pt
            && !packet.payload.is_empty()
            && packet.payload.len() <= 1200
            && self.packets.take(1.0, now)
            && self.bytes.take((packet.payload.len() + 64) as f64, now)
    }
}

pub(super) struct Slot {
    pub(super) mid: Mid,
    pub(super) pt: Option<Pt>,
    pub ssrc: u32,
    source: Option<(Ssrc, u64, u64, u32, u32)>,
    publication: Option<u64>,
    next: u64,
    ticks: u32,
    pending: VecDeque<Queued>,
    /// RTP time and publisher wallclock of the last packet written.
    last_written: Option<(u32, Instant)>,
}

impl Slot {
    pub fn new(rtc: &mut Rtc, mid: Mid) -> Self {
        let pt = opus(rtc, mid);
        let ssrc = rtc.direct_api().stream_tx_by_mid(mid, None).map_or(0, |s| {
            s.set_unpaced(true);
            *s.ssrc()
        });
        Self {
            mid,
            pt,
            ssrc,
            source: None,
            publication: None,
            next: 0,
            ticks: 0,
            pending: VecDeque::new(),
            last_written: None,
        }
    }

    pub fn clear(&mut self, rtc: &mut Rtc) {
        self.source = None;
        self.publication = None;
        self.pending.clear();
        self.last_written = None;
        wipe(rtc, self.mid);
    }

    /// `rtp` is the packet's RTP time on the share's own timeline (`SoundTime::place`).
    pub fn queue(&mut self, packet: &RtpPacket, rtp: u32, publication: u64, wallclock: Instant) {
        let Some(pt) = self.pt else { return };
        let source = *packet.seq_no;
        if self.publication != Some(publication) || self.source.is_none_or(|(ssrc, ..)| ssrc != packet.header.ssrc) {
            self.pending.clear();
            self.publication = Some(publication);
            self.source = Some((packet.header.ssrc, source, self.next, rtp, self.ticks));
        }
        let (_, first, out, clock, start) = self.source.unwrap();
        if source < first {
            return;
        }
        let sequence = out + source - first;
        let ticks = start.wrapping_add(rtp.wrapping_sub(clock));
        if sequence >= self.next {
            self.next = sequence + 1;
            self.ticks = ticks.wrapping_add(960);
        }
        // A short, bounded queue, shared release pacing with video; no stale sound backlog.
        if self.pending.len() == 4 {
            self.pending.pop_front();
        }
        self.pending.push_back(
            Queued::new(sequence, ticks, packet.header.marker, pt, packet.timestamp, packet.payload.clone(), 0, false)
                .with_wallclock(wallclock),
        );
    }

    pub fn wake(&self, egress: &Egress, now: Instant) -> Option<Instant> {
        self.pending.front().map(|p| {
            egress.audio_wake(p.payload.len() + ORIGINAL_OVERHEAD, now).min(p.arrival + Duration::from_millis(100))
        })
    }

    pub fn release(&mut self, rtc: &mut Rtc, egress: &mut Egress, now: Instant, enabled: bool) {
        if !enabled {
            self.pending.clear();
            return;
        }
        while self.pending.front().is_some_and(|p| p.arrival + Duration::from_millis(100) <= now) {
            self.pending.pop_front();
        }
        while let Some(packet) = self.pending.front() {
            if !egress.audio_due(packet.payload.len() + ORIGINAL_OVERHEAD, now) {
                break;
            }
            let packet = self.pending.pop_front().unwrap();
            if let Some(stream) = rtc.direct_api().stream_tx_by_mid(self.mid, None) {
                self.ssrc = *stream.ssrc();
                stream.set_unpaced(true);
                self.last_written = Some((packet.timestamp, packet.wallclock));
                stream.write_rtp(
                    RtpWrite::new(packet.pt, packet.out.into(), packet.timestamp, packet.wallclock, packet.payload)
                        .marker(packet.marker),
                );
            }
        }
    }
}

/// Further apart than this, the last sound and picture are not of the same stretch of a share.
const MAX_ASSOCIATED_GAP: Duration = Duration::from_secs(2);
const VIDEO_RATE: f64 = 90_000.0;

/// Sound and picture RTP times of one instant. Both wallclocks come from the publisher's common
/// clock, so the sender reports' association survives this SFU's queues and each slot's own
/// timestamps. A viewer whose sound does not reach its webview's engine cannot get it elsewhere.
fn associate_pair(video: (u32, Instant), audio: (u32, Instant)) -> Option<(u32, u32)> {
    let (later, earlier, sign) = if audio.1 >= video.1 { (audio.1, video.1, 1.0) } else { (video.1, audio.1, -1.0) };
    let gap = later.duration_since(earlier);
    if gap > MAX_ASSOCIATED_GAP {
        return None;
    }
    let ticks = (sign * gap.as_secs_f64() * VIDEO_RATE).round() as i64;
    Some((audio.0, video.0.wrapping_add(ticks as u32)))
}

/// Publishes each watched slot's association to the signalling side.
pub(super) fn associate(w: &Watcher) {
    for (slot, sound) in w.slots.iter().zip(&w.audio) {
        let (Some(grant), Some(video), Some(audio)) = (&slot.grant, slot.last_written, sound.last_written) else {
            continue;
        };
        grant.set_sound_clock(associate_pair(video, audio));
    }
}

pub(super) fn reserve(w: &mut Watcher, now: Instant) {
    let count = w
        .slots
        .iter()
        .enumerate()
        .filter(|(i, s)| {
            *i < w.audio.len()
                && s.grant
                    .as_ref()
                    .is_some_and(|g| g.alive() && g.origin.has_audio.load(std::sync::atomic::Ordering::Relaxed))
        })
        .count();
    w.reserve_audio(count, now);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::screen::{
        SCREEN_SLOTS,
        egress::{Class, Egress, PLAN},
        publish_ssrcs, watch_offer,
    };
    use std::time::Duration;

    /// The publisher's report: its clock at `at`, and the RTP time it gives that instant.
    fn report(at: Duration, rtp: u32) -> SenderInfo {
        SenderInfo {
            ssrc: 1.into(),
            ntp_time: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000) + at,
            rtp_time: str0m::media::MediaTime::new(u64::from(rtp), str0m::media::Frequency::FORTY_EIGHT_KHZ),
            sender_packet_count: 1,
            sender_octet_count: 1,
        }
    }

    #[test]
    fn sound_resumed_after_a_stop_keeps_one_clock_for_its_viewers() {
        // Sound for 2 s, stopped for 5 s, then back on the same sender with the RTP time a frame
        // on. The publisher reports its old line through the silence and its new one some time
        // after the sound is back, or before the server lets the sound through again.
        for (frames_from, new_line_at) in [(7_000, 8_500), (6_800, 6_900)] {
            let start = Instant::now();
            let path = Duration::from_millis(30);
            let (mut time, mut clock) = (SoundTime::default(), super::super::clock::Clock::default());
            let (mut rtp, mut seen) = (7_000u32, Vec::new());
            // Where the RTP time is when frames start again: 2 s of them were sent.
            let back = 7_000 + 2 * 48_000;
            for ms in (0..12_000u64).step_by(20) {
                if (2_000..frames_from).contains(&ms) {
                    continue;
                }
                let line = if ms >= new_line_at {
                    report(Duration::from_millis(new_line_at), back + ((new_line_at - frames_from) * 48) as u32)
                } else if ms >= 4_000 {
                    // Sent during the silence, on the old line.
                    report(Duration::from_secs(4), 7_000 + 4 * 48_000)
                } else {
                    report(Duration::ZERO, 7_000)
                };
                // The server lets sound through again at 7 s.
                if !(2_000..7_000).contains(&ms) {
                    let arrival = start + Duration::from_millis(ms) + path;
                    let (out, placed) = time.place(rtp, arrival, Some(&line), u64::from(ms >= 7_000));
                    seen.push((ms, out, clock.map(placed, arrival, Some(line), 48_000)));
                }
                rtp = rtp.wrapping_add(960);
            }
            let (first, out, wall) = seen[0];
            for &(ms, o, w) in &seen {
                let sent = Duration::from_millis(ms - first);
                let forwarded = f64::from(o.wrapping_sub(out)) / 48_000.0;
                assert!(
                    (forwarded - sent.as_secs_f64()).abs() < 0.001,
                    "{frames_from}/{new_line_at}: {ms} ms forwarded as {forwarded:.3} s"
                );
                let placed = (w - wall).as_secs_f64() - sent.as_secs_f64();
                assert!(
                    placed.abs() < 0.001,
                    "{frames_from}/{new_line_at}: {ms} ms placed {placed:.3} s off its clock"
                );
            }
        }
    }

    #[test]
    fn sound_that_only_arrives_late_keeps_its_rtp_time() {
        // A stall on the way in, with no resume: the sound was sent all along.
        let start = Instant::now();
        let mut time = SoundTime::default();
        for n in 0..200u32 {
            let late = if (50..100).contains(&n) {
                Duration::from_millis(3_000 - 20 * u64::from(n - 50))
            } else {
                Duration::ZERO
            };
            let rtp = n * 960;
            let arrival = start + Duration::from_millis(20 * u64::from(n)) + late;
            assert_eq!(time.place(rtp, arrival, Some(&report(Duration::ZERO, 0)), 0), (rtp, rtp));
        }
    }

    #[test]
    fn combined_production_audio_and_video_release_keeps_its_allocation() {
        use super::super::{ScreenOrigin, WatchGrant, assign, tests::Pair};
        use std::sync::{Arc, atomic::Ordering};
        for cadence in [5, 10, 20, 30] {
            let mut pair = Pair::with_audio(true);
            let start = pair.now;
            for i in 0..2 {
                let origin = ScreenOrigin::new(100 + i as u32, format!("audio-{i}"), 1, false, false, 2_500_000);
                origin.has_audio.store(true, Ordering::Relaxed);
                assign(&mut pair.w, i, WatchGrant::new(origin), i as u64, start);
            }
            reserve(&mut pair.w, start);
            pair.w.egress.set_estimate(2_500_000, start);
            assert_eq!(pair.w.egress.plan(), 1_744_000.0);
            let video_pt = *pair.w.slots[0].pts.values().next().unwrap();
            let mut credit = 0.0;
            let mut video = 0;
            for ms in 0..10_500 {
                pair.now = start + Duration::from_millis(ms);
                if ms < 10_000 {
                    if ms % 20 == 0 {
                        for slot in &mut pair.w.audio[..2] {
                            slot.pending.push_back(Queued::new(
                                ms / 20,
                                ms as u32 * 48,
                                false,
                                slot.pt.unwrap(),
                                pair.now,
                                Arc::from(vec![0; 160]),
                                0,
                                false,
                            ));
                        }
                    }
                    credit += 1_470_000.0 / 8.0 / 1000.0;
                    while credit >= 1164.0 {
                        credit -= 1164.0;
                        let packet = Queued::new(
                            video,
                            video as u32 * 3000,
                            true,
                            video_pt,
                            pair.now,
                            Arc::from(vec![0x08; 1100]),
                            video as u32,
                            video == 0,
                        );
                        assert!(pair.w.egress.admit(0, packet, pair.now).is_empty());
                        video += 1;
                    }
                }
                if ms % cadence == 0 {
                    pair.drain();
                }
            }
            eprintln!(
                "cadence={cadence} estimate={} plan={} counters={:?} written={}/{}",
                pair.w.egress.estimate(),
                pair.w.egress.plan(),
                pair.w.egress.counters,
                pair.w.slots[0].written,
                video
            );
            assert_eq!(pair.w.egress.counters.evicted, 0, "{cadence} ms must not evict in-budget video");
            assert_eq!(pair.w.egress.counters.originals_dropped, 0, "{cadence} ms socket guard");
            assert_eq!(pair.w.slots[0].written, video, "{cadence} ms usable throughput");
            assert_eq!(
                pair.received.get(&pair.w.slots[0].ssrc.unwrap()).copied(),
                Some(video as usize),
                "{cadence} ms delivered video"
            );
            for slot in &pair.w.audio[..2] {
                assert_eq!(pair.received.get(&slot.ssrc).copied(), Some(500), "{cadence} ms delivered Opus");
            }
            assert!(pair.w.audio[..2].iter().all(|s| s.pending.is_empty()));
        }
    }

    #[test]
    fn legacy_and_audio_watch_offers_coexist_without_sending_media() {
        let video = "m=video 9 UDP/TLS/RTP/SAVPF 98\r\na=recvonly\r\n";
        let audio = "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=recvonly\r\n";
        assert!(watch_offer(&video.repeat(SCREEN_SLOTS), SCREEN_SLOTS));
        assert!(watch_offer(&format!("{video}{audio}").repeat(SCREEN_SLOTS), SCREEN_SLOTS));
        assert!(!watch_offer(
            &format!("{video}{audio}").repeat(SCREEN_SLOTS).replace("a=recvonly", "a=sendrecv"),
            SCREEN_SLOTS
        ));
        assert!(!watch_offer(&format!("{video}{audio}").repeat(SCREEN_SLOTS - 1), SCREEN_SLOTS));
    }

    #[test]
    fn each_negotiated_pair_shares_its_sdp_and_rtcp_sync_identity() {
        use super::super::{Caps, Lease, watcher};
        use str0m::media::Direction;
        let now = Instant::now();
        let mut peer = Rtc::new(now);
        let mut api = peer.sdp_api();
        for _ in 0..SCREEN_SLOTS {
            api.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
            api.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None);
        }
        let (offer, _) = api.apply().unwrap();
        let (w, answer) = watcher(
            1,
            2,
            3,
            Lease::new(),
            SCREEN_SLOTS,
            &offer.to_sdp_string(),
            Caps { browser: false, h264: &[], av1: None },
            "127.0.0.1:6000".parse().unwrap(),
            now,
        )
        .unwrap();
        let sections = sections(&answer);
        let mut groups = HashSet::new();
        for pair in sections.chunks(2) {
            let identities: Vec<_> = pair
                .iter()
                .map(|lines| {
                    let mid = lines.iter().find_map(|l| l.strip_prefix("a=mid:")).unwrap();
                    let media = w.rtc.media(Mid::from(mid)).unwrap();
                    assert!(lines.iter().any(|l| l.contains(&format!("cname:{}", media.cname()))));
                    assert!(lines.iter().any(|l| l.starts_with(&format!("a=msid:{} ", media.stream_id()))));
                    (media.stream_id(), media.cname(), media.track_id())
                })
                .collect();
            assert_eq!(identities[0].0, identities[1].0);
            assert_eq!(identities[0].1, identities[1].1);
            assert_ne!(identities[0].2, identities[1].2);
            assert!(groups.insert(identities[0].0));
        }
    }

    #[test]
    fn audio_publish_keeps_legacy_video_identity_and_refuses_other_audio_directions() {
        let video = "m=video 9 UDP/TLS/RTP/SAVPF 98 99\r\na=sendonly\r\na=ssrc-group:FID 1 2\r\na=ssrc:1 cname:v\r\na=ssrc:2 cname:v\r\n";
        let audio =
            "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=sendonly\r\na=rtpmap:111 opus/48000/2\r\na=ssrc:3 cname:v\r\n";
        assert_eq!(publish_ssrcs(video), Some((1, 2)));
        assert_eq!(publish_ssrcs(&format!("{video}{audio}")), Some((1, 2)));
        assert_eq!(publish_ssrcs(&format!("{video}{audio}{audio}")), None);
        assert_eq!(publish_ssrcs(&format!("{video}{}", audio.replace("sendonly", "sendrecv"))), None);
    }

    #[test]
    fn audio_is_carved_from_screen_allocation_and_cannot_spend_voice_headroom() {
        let now = Instant::now();
        for estimate in [100_000, 500_000, 1_000_000, 10_000_000] {
            let mut egress = Egress::new(now);
            egress.set_estimate(estimate, now);
            let before = egress.plan();
            egress.reserve_audio(4, now);
            assert!(egress.plan() <= before);
            let mut bytes = 0;
            for ms in 0..1000 {
                let at = now + Duration::from_millis(ms);
                while egress.charge(Class::Audio, 200, at) {
                    bytes += 200;
                }
            }
            let ceiling = (PLAN * estimate as f64).min(4.0 * WIRE_BPS as f64);
            assert!(bytes as f64 * 8.0 <= ceiling + 2400.0 * 8.0);
            assert!(egress.plan() + ceiling <= PLAN * estimate as f64);
            egress.reserve_audio(0, now + Duration::from_secs(1));
            assert_eq!(egress.plan(), before);
        }
    }

    #[test]
    fn adversarial_audio_bursts_cannot_spend_a_second_wire_burst() {
        let now = Instant::now();
        let mut e = Egress::new(now);
        e.set_estimate(1_000_000, now);
        e.reserve_audio(1, now);
        assert!(e.charge(Class::Audio, 1100, now));
        assert!(!e.charge(Class::Audio, 1100, now), "the second packet must wait for aggregate pacing");
        assert!(!e.charge(Class::Original, 1100, now), "video cannot spend another screen burst");
        assert!(!e.charge(Class::Extra, 1100, now), "repairs/probes share the final wire guard");
        assert!(e.charge(Class::Control, 80, now));
    }

    #[test]
    fn a_probe_cannot_spend_credit_already_reserved_for_released_media() {
        let now = Instant::now();
        let mut e = Egress::new(now);
        e.set_estimate(1_000_000, now);
        e.reserve_audio(1, now);
        assert!(e.audio_due(1164, now), "application release reserves this packet");
        assert!(!e.charge(Class::Extra, 1100, now), "a probe cannot steal the released packet's wire credit");
        assert!(e.charge(Class::Audio, 1100, now), "released media reaches the socket without a pacing drop");
    }

    #[test]
    fn pre_twcc_probe_plan_changes_cannot_mint_another_wire_burst() {
        let now = Instant::now();
        let mut e = Egress::new(now);
        e.set_estimate(1_000_000, now);
        e.reserve_audio(1, now);
        let mut gate = e.send_gate();
        assert!(gate.poll_budget(now, None).media_bytes >= 1148);
        gate.sent(now, 1100, false);
        for probe in [Some(str0m::bwe::Bitrate::bps(2_000_000)), None] {
            let remaining = gate.poll_budget(now, probe);
            assert!(remaining.media_bytes < 1148, "a probe plan shares the occupied audio/video wire clock");
            assert!(remaining.extra_bytes < 1148, "repairs cannot buy another burst either");
        }
    }

    #[test]
    fn low_bandwidth_bursts_preserve_voice_with_and_without_video() {
        // Production release and socket guards on a 0.5/1/2 Mbps FIFO. The publisher's
        // admissible pair arrives together every 150 ms, deliberately ahead of voice.
        for bandwidth in [500_000, 1_000_000, 2_000_000] {
            for cadence in [1, 5, 10] {
                for video in [false, true] {
                    let start = Instant::now();
                    let mut e = Egress::new(start);
                    e.set_estimate(bandwidth, start);
                    e.reserve_audio(1, start);
                    let mut pending = 0;
                    let mut busy = 0.0_f64;
                    let mut voice = Vec::new();
                    for ms in 0..30_000 {
                        let now = start + Duration::from_millis(ms);
                        let time = ms as f64 / 1000.0;
                        if ms % 150 == 0 {
                            pending += 2;
                        }
                        if ms % cadence == 0 {
                            e.begin_release(now);
                            while pending > 0 && e.audio_due(1164, now) {
                                pending -= 1;
                                if e.charge(Class::Audio, 1100, now) {
                                    busy = busy.max(time) + 1128.0 * 8.0 / bandwidth as f64;
                                }
                            }
                            if video && e.charge(Class::Original, 1100, now) {
                                busy = busy.max(time) + 1128.0 * 8.0 / bandwidth as f64;
                            }
                        }
                        // Match the 1 Mbps review probe's 100 kb/s voice; on the
                        // 0.5 Mbps link use 32 kb/s, inside the same voice reserve.
                        let (interval, bytes) = if bandwidth >= 1_000_000 { (10, 125.0) } else { (20, 80.0) };
                        if ms % interval == 0 {
                            voice.push((busy - time).max(0.0));
                            busy = busy.max(time) + bytes * 8.0 / bandwidth as f64;
                        }
                    }
                    voice.sort_by(f64::total_cmp);
                    let p99 = voice[voice.len() * 99 / 100];
                    let bound = (1128.0 * 8.0 / bandwidth as f64).max(0.010);
                    eprintln!("{bandwidth} bps, {cadence} ms, video={video}: voice p99={p99:.6}s");
                    assert!(
                        p99 <= bound + 0.000001,
                        "{bandwidth} bps, {cadence} ms, video={video}: p99={p99:.4} > {bound:.4}"
                    );
                    assert!(pending < 3, "no unbounded sound backlog");
                }
            }
        }
    }
}

#[cfg(test)]
mod sync_tests {
    use super::*;

    #[test]
    fn the_association_is_one_instant_on_the_pictures_clock() {
        let at = Instant::now();
        let later = at + Duration::from_millis(20);
        // Sound 20 ms after the picture: the picture's time moves on by those 20 ms.
        assert_eq!(associate_pair((1_000, at), (500, later)), Some((500, 2_800)));
        assert_eq!(associate_pair((1_000, later), (500, at)), Some((500, 1_000u32.wrapping_sub(1_800))));
        assert_eq!(associate_pair((u32::MAX, at), (7, later)), Some((7, 1_799)));
    }

    #[test]
    fn sound_and_picture_of_different_stretches_are_not_associated() {
        let at = Instant::now();
        assert_eq!(associate_pair((1, at), (2, at + Duration::from_secs(3))), None);
    }
}
