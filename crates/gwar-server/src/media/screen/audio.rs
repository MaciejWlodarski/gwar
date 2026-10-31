//! Opus for plaintext screen shares. Uses the screen task and a bounded, separate ingress
//! bucket. At egress its fixed allowance is subtracted from video's existing 80% allocation:
//! voice's reserve and the voice task are never borrowed for share sound.
use super::{DIRECTIONS, Watcher, ingress::Bucket, sections, wipe};
use std::{collections::HashSet, time::Instant};
use str0m::{
    Rtc,
    format::Codec,
    media::{MediaKind, Mid, Pt},
    rtp::{RtpPacket, RtpWrite, Ssrc},
};

pub(super) const WIRE_BPS: u64 = 128_000;

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
}

impl Publisher {
    pub fn from_sdp(rtc: &mut Rtc, sdp: &str, now: Instant) -> Result<Option<Self>, &'static str> {
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
        let ssrc = Ssrc::from(*ssrcs.iter().next().unwrap());
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
    mid: Mid,
    pt: Option<Pt>,
    pub ssrc: u32,
    source: Option<(Ssrc, u64, u64, u32, u32)>,
    publication: Option<u64>,
    next: u64,
    ticks: u32,
}

impl Slot {
    pub fn new(rtc: &mut Rtc, mid: Mid) -> Self {
        let pt = opus(rtc, mid);
        let ssrc = rtc.direct_api().stream_tx_by_mid(mid, None).map_or(0, |s| {
            s.set_unpaced(true);
            *s.ssrc()
        });
        Self { mid, pt, ssrc, source: None, publication: None, next: 0, ticks: 0 }
    }

    pub fn clear(&mut self, rtc: &mut Rtc) {
        self.source = None;
        self.publication = None;
        wipe(rtc, self.mid);
    }

    pub fn write(&mut self, rtc: &mut Rtc, packet: &RtpPacket, publication: u64) {
        let Some(pt) = self.pt else { return };
        let source = *packet.seq_no;
        if self.publication != Some(publication) || self.source.is_none_or(|(ssrc, ..)| ssrc != packet.header.ssrc) {
            self.publication = Some(publication);
            self.source = Some((packet.header.ssrc, source, self.next, packet.header.timestamp, self.ticks));
        }
        let (_, first, out, clock, start) = self.source.unwrap();
        if source < first {
            return;
        }
        let sequence = out + source - first;
        let ticks = start.wrapping_add(packet.header.timestamp.wrapping_sub(clock));
        if sequence >= self.next {
            self.next = sequence + 1;
            self.ticks = ticks.wrapping_add(960);
        }
        if let Some(stream) = rtc.direct_api().stream_tx_by_mid(self.mid, None) {
            self.ssrc = *stream.ssrc();
            stream.set_unpaced(true);
            stream.write_rtp(
                RtpWrite::new(pt, sequence.into(), ticks, packet.timestamp, packet.payload.clone())
                    .marker(packet.header.marker),
            );
        }
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
    w.egress.reserve_audio(count, now);
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
}
