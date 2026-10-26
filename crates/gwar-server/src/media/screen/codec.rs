//! Receive capabilities are read from each offered m-line, before SDP negotiation can replace
//! its fmtp with the SFU's receive capabilities. There is no transcoding or profile conversion.
use super::{VideoCodec, sections};
use std::collections::HashMap;
use str0m::media::{Mid, Pt};

/// Profiles accepted by A2. Main (4d001f) and unconstrained Baseline are deliberately excluded.
pub(super) fn profile(id: u32) -> Option<u8> {
    if id > 0xffffff {
        return None;
    }
    let [_, idc, iop, _] = id.to_be_bytes();
    match (idc, iop) {
        (0x42, iop) if iop & 0x4f == 0x40 => Some(0),
        (0x4d, iop) if iop & 0x8f == 0x80 => Some(0),
        (0x58, iop) if iop & 0xcf == 0xc0 => Some(0),
        (0x64, 0x0c) => Some(1),
        (0x64, 0) => Some(2),
        _ => None,
    }
}

#[derive(Clone, Debug)]
pub(super) struct Limits {
    profile: u32,
    receive_level: u32,
    resources: [u32; 4],
}

// H.264 Annex A: MaxMBPS, MaxFS, MaxDpbMbs, MaxBR (kbps, baseline/main units).
fn resources(id: u32) -> Option<[u32; 4]> {
    if id as u8 == 11 && id & 0x1000 != 0 && profile(id) == Some(0) {
        return Some([1485, 99, 396, 128]); // Level 1b
    }
    Some(match id as u8 {
        10 => [1485, 99, 396, 64],
        11 => [3000, 396, 900, 192],
        12 => [6000, 396, 2376, 384],
        13 => [11880, 396, 2376, 768],
        20 => [11880, 396, 2376, 2000],
        21 => [19800, 792, 4752, 4000],
        22 => [20250, 1620, 8100, 4000],
        30 => [40500, 1620, 8100, 10000],
        31 => [108000, 3600, 18000, 14000],
        32 => [216000, 5120, 20480, 20000],
        40 => [245760, 8192, 32768, 20000],
        41 => [245760, 8192, 32768, 50000],
        42 => [522240, 8704, 34816, 50000],
        50 => [589824, 22080, 110400, 135000],
        51 => [983040, 36864, 184320, 240000],
        // Receive-only levels: viewers may decode above the 5.1 publish ceiling.
        52 => [2073600, 36864, 184320, 240000],
        60 => [4177920, 139264, 696320, 240000],
        61 => [8355840, 139264, 696320, 480000],
        62 => [16711680, 139264, 696320, 800000],
        _ => return None,
    })
}

/// Validate an SPS against the H.264 receive profile/level actually answered on ingress.
pub(super) fn stream_fits(declared: u32, actual: u32) -> bool {
    profile(actual).is_some()
        && profile(actual) == profile(declared)
        && resources(actual)
            .zip(resources(declared))
            .is_some_and(|(need, cap)| need.iter().zip(cap).all(|(&need, cap)| need <= cap))
}

impl Limits {
    pub fn accepts(&self, codec: VideoCodec) -> bool {
        let VideoCodec::H264(id) = codec else { return false };
        profile(id) == profile(self.profile)
            && stream_fits(self.receive_level, id)
            && resources(id).is_some_and(|r| r[2] <= self.resources[2] && r[3] <= self.resources[3])
    }

    pub fn accepts_stream(&self, stream: &super::h264_stream::Requirements) -> bool {
        self.accepts(VideoCodec::H264(stream.profile))
            && stream.frame_mbs <= self.resources[1]
            && u64::from(stream.mbps) * 100 <= u64::from(self.resources[0]) * 110
    }

    fn parse(fmtp: &str, verified: &[gwar_proto::H264Capability]) -> Option<Self> {
        let params: HashMap<_, _> = fmtp.split(';').filter_map(|p| p.trim().split_once('=')).collect();
        if params.get("packetization-mode") != Some(&"1") {
            return None;
        }
        let id = u32::from_str_radix(params.get("profile-level-id").copied().unwrap_or("42e01f"), 16).ok()?;
        profile(id)?;
        resources(id)?;
        let mut level = id;
        if let Some(value) = params.get("max-recv-level") {
            level = (id & 0xff0000) | u16::from_str_radix(value, 16).ok()? as u32;
        }
        let cap = verified.iter().find(|c| c.valid() && profile(c.profile_level_id) == profile(id));
        if let Some(cap) = cap
            && stream_fits(cap.profile_level_id, level)
        {
            level = cap.profile_level_id;
        }
        let mut r = resources(level)?;
        for (index, name) in ["max-mbps", "max-fs", "max-dpb", "max-br"].iter().enumerate() {
            if let Some(value) = params.get(name) {
                let value = value.parse::<u32>().ok()?;
                let value = if index == 2 { value.checked_mul(3)? / 8 } else { value };
                r[index] = if cap.is_some() { r[index].min(value) } else { r[index].max(value) };
            }
        }
        if let Some(cap) = cap {
            r[0] = r[0].min(cap.max_mbps);
            r[1] = r[1].min(cap.max_fs);
        }
        Some(Self { profile: id, receive_level: level, resources: r })
    }
}

/// What one viewer's AV1 payload type receives: the `level-idx` and `tier` its offer declared
/// (AV1 RTP §7.2.3; defaults 5 and 0), and the sizes its decode probe verified. A stream must
/// satisfy both. Sizes compare by area and by the longer side, which admits portrait shares of
/// a landscape probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Av1Limits {
    pixels: u64,
    side: u32,
    level: u8,
    tier: u8,
}

/// The `level-idx` and `tier` an offer declares for an AV1 payload type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Av1Declared {
    pub level: u8,
    pub tier: u8,
}

impl Default for Av1Declared {
    fn default() -> Self {
        Self { level: 5, tier: 0 }
    }
}

impl Av1Limits {
    pub fn new(cap: &gwar_proto::Av1Capability, declared: Av1Declared) -> Self {
        Self {
            pixels: u64::from(cap.width) * u64::from(cap.height),
            side: cap.width.max(cap.height),
            level: declared.level,
            tier: declared.tier,
        }
    }

    pub fn accepts(&self, need: &super::av1_stream::Requirements) -> bool {
        need.pixels() <= self.pixels
            && need.width.max(need.height) <= self.side
            && need.level <= self.level
            && need.tier <= self.tier
    }
}

/// Each m-line's AV1 profile 0 payload types and what they declare. Unparsable values drop the
/// payload type: nothing is assumed beyond the defaults of omitted parameters.
pub(super) fn offered_av1(sdp: &str) -> HashMap<Mid, HashMap<Pt, Av1Declared>> {
    sections(sdp)
        .into_iter()
        .filter_map(|lines| {
            let mid = lines.iter().find_map(|l| l.strip_prefix("a=mid:"))?;
            let mut declared = HashMap::new();
            for line in &lines {
                let Some(pt) =
                    line.strip_prefix("a=rtpmap:").and_then(|s| s.split_once(' ')).and_then(|(pt, codec)| {
                        codec.eq_ignore_ascii_case("AV1/90000").then(|| pt.parse::<u8>().ok()).flatten()
                    })
                else {
                    continue;
                };
                let prefix = format!("a=fmtp:{pt} ");
                let fmtp = lines.iter().find_map(|l| l.strip_prefix(prefix.as_str())).unwrap_or("");
                let mut av1 = Some(Av1Declared::default());
                let mut profile = 0;
                for (key, value) in fmtp.split(';').filter_map(|p| p.trim().split_once('=')) {
                    let value = value.trim().parse::<u8>().ok();
                    match key.trim() {
                        "profile" => profile = value.unwrap_or(u8::MAX),
                        "level-idx" => av1 = av1.zip(value).map(|(a, level)| Av1Declared { level, ..a }),
                        "tier" => av1 = av1.zip(value).map(|(a, tier)| Av1Declared { tier, ..a }),
                        _ => {}
                    }
                }
                if let Some(av1) = av1.filter(|_| profile == 0) {
                    declared.insert(Pt::from(pt), av1);
                }
            }
            Some((Mid::from(mid), declared))
        })
        .collect()
}

pub(super) fn level_resources(id: u32) -> Option<(u32, u32)> {
    resources(id).map(|r| (r[1], r[0]))
}

pub(super) fn offered(sdp: &str, verified: &[gwar_proto::H264Capability]) -> HashMap<Mid, HashMap<Pt, Limits>> {
    sections(sdp)
        .into_iter()
        .filter_map(|lines| {
            let mid = lines.iter().find_map(|l| l.strip_prefix("a=mid:"))?;
            let mut limits = HashMap::new();
            for line in &lines {
                let Some((pt, fmtp)) = line.strip_prefix("a=fmtp:").and_then(|s| s.split_once(' ')) else { continue };
                let Ok(pt) = pt.parse::<u8>() else { continue };
                let rtpmap = format!("a=rtpmap:{pt} H264/90000");
                if lines.iter().any(|l| l.eq_ignore_ascii_case(&rtpmap))
                    && let Some(limit) = Limits::parse(fmtp, verified)
                {
                    limits.insert(pt.into(), limit);
                }
            }
            Some((Mid::from(mid), limits))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn av1_limits_follow_the_verified_size_in_either_orientation() {
        use super::super::av1_stream::Requirements;
        let declared = Av1Declared { level: 13, tier: 0 };
        let limits = Av1Limits::new(&gwar_proto::Av1Capability { width: 1920, height: 1080 }, declared);
        for (width, height, fits) in [
            (1920, 1080, true),
            (1080, 1920, true),
            (1280, 720, true),
            (2560, 1440, false),
            (1921, 1080, false),
            (2000, 1000, false),
        ] {
            assert_eq!(limits.accepts(&Requirements { width, height, level: 9, tier: 0 }), fits, "{width}x{height}");
        }
    }

    #[test]
    fn av1_streams_stay_within_the_level_and_tier_each_viewer_declared() {
        use super::super::av1_stream::Requirements;
        let sdp = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 35 36 37 38 39\r\na=mid:0\r\n\
            a=rtpmap:35 AV1/90000\r\na=fmtp:35 level-idx=5;profile=0;tier=0\r\n\
            a=rtpmap:36 AV1/90000\r\na=fmtp:36 profile=0;level-idx=8;tier=1\r\n\
            a=rtpmap:37 av1/90000\r\na=fmtp:37 level-idx=12\r\n\
            a=rtpmap:38 AV1/90000\r\n\
            a=rtpmap:39 AV1/90000\r\na=fmtp:39 level-idx=13;profile=1;tier=0\r\n\
            a=rtpmap:40 AV1/90000\r\na=fmtp:40 level-idx=x\r\n";
        let offered = &offered_av1(sdp)[&Mid::from("0")];
        let at = |level, tier| Av1Declared { level, tier };
        assert_eq!(
            offered,
            &HashMap::from([
                (35.into(), at(5, 0)),
                (36.into(), at(8, 1)),
                (37.into(), at(12, 0)),
                (38.into(), at(5, 0))
            ])
        );
        let cap = gwar_proto::Av1Capability { width: 2560, height: 1440 };
        let stream = |level, tier| Requirements { width: 1920, height: 1080, level, tier };
        // (declared level, declared tier, stream level, stream tier, accepted)
        for (level, tier, stream_level, stream_tier, accepted) in [
            (5, 0, 9, 0, false),
            (8, 0, 9, 0, false),
            (8, 1, 8, 0, true),
            (8, 0, 8, 1, false),
            (12, 0, 9, 0, true),
            (12, 0, 13, 0, false),
            (13, 0, 13, 0, true),
            (13, 1, 13, 1, true),
            (13, 0, 13, 1, false),
        ] {
            let limits = Av1Limits::new(&cap, at(level, tier));
            assert_eq!(
                limits.accepts(&stream(stream_level, stream_tier)),
                accepted,
                "declared {level}/{tier}, stream {stream_level}/{stream_tier}"
            );
        }
    }

    #[test]
    fn verified_1440p60_viewer_limits_admit_the_level_51_publish_stream() {
        let cap = gwar_proto::H264Capability { profile_level_id: 0x42e033, max_fs: 14_400, max_mbps: 864_000 };
        let limits = Limits::parse("packetization-mode=1;profile-level-id=42e01f", &[cap]).unwrap();
        let stream = super::super::h264_stream::Requirements {
            sps_id: 0,
            profile: 0x42e033,
            frame_mbs: 14_400,
            width: 2560,
            height: 1440,
            mbps: 864_000,
        };
        assert!(limits.accepts_stream(&stream));
        assert!(!Limits::parse("packetization-mode=1;profile-level-id=42e032", &[]).unwrap().accepts_stream(&stream));
    }

    #[test]
    fn promotion_requires_matching_verified_capability_even_for_explicit_low_levels() {
        let cap = gwar_proto::H264Capability { profile_level_id: 0x42e02a, max_fs: 8160, max_mbps: 489600 };
        for id in ["42e014", "42e01f"] {
            let fmtp = format!("packetization-mode=1;profile-level-id={id}");
            assert!(!Limits::parse(&fmtp, &[]).unwrap().accepts(VideoCodec::H264(0x42e02a)));
            let limit = Limits::parse(&fmtp, std::slice::from_ref(&cap)).unwrap();
            assert!(limit.accepts(VideoCodec::H264(0x42e02a)));
            assert!(!limit.accepts(VideoCodec::H264(0x42e033)));
            assert!(!limit.accepts(VideoCodec::H264(0x640c2a)));
        }
        let low = Limits::parse("packetization-mode=1;profile-level-id=42e014", &[]).unwrap();
        assert!(!low.accepts(VideoCodec::H264(0x42e01f)));
        let explicit = Limits::parse("packetization-mode=1;profile-level-id=42e01f;max-fs=3600", &[cap]).unwrap();
        assert_eq!(explicit.resources[1], 3600);
    }
    #[test]
    fn receivers_above_the_publish_ceiling_accept_lower_level_streams() {
        for id in ["640034", "640c3c", "42e03e"] {
            let fmtp = format!("packetization-mode=1;profile-level-id={id}");
            let limit = Limits::parse(&fmtp, &[]).unwrap();
            let stream = u32::from_str_radix(id, 16).unwrap() & 0xffff00 | 0x33;
            assert!(limit.accepts(VideoCodec::H264(stream)));
        }
        // A publisher SPS above its answered 5.1 level is still refused.
        assert!(!stream_fits(0x640033, 0x640034));
    }
    #[test]
    fn native_extensions_and_m_lines_remain_independent() {
        let limit = Limits::parse("packetization-mode=1;profile-level-id=640c1f;max-recv-level=0c33;max-fs=40000", &[])
            .unwrap();
        assert!(limit.accepts(VideoCodec::H264(0x640c28)));
        assert!(!limit.accepts(VideoCodec::H264(0x640028)));
        assert_eq!(limit.resources[1], 40000);
        for fmtp in [
            "packetization-mode=0",
            "packetization-mode=1;profile-level-id=4d001f",
            "packetization-mode=1;max-dpb=bogus",
        ] {
            assert!(Limits::parse(fmtp, &[]).is_none());
        }
        let caps = offered(
            "m=video 9 X 100\na=mid:one\na=rtpmap:100 H264/90000\na=fmtp:100 packetization-mode=1;profile-level-id=42e01f\nm=video 9 X 102\na=mid:two\na=rtpmap:102 H264/90000\na=fmtp:102 packetization-mode=1;profile-level-id=640c28\n",
            &[],
        );
        assert!(caps[&Mid::from("one")][&100.into()].accepts(VideoCodec::H264(0x42e01f)));
        assert!(!caps[&Mid::from("two")][&102.into()].accepts(VideoCodec::H264(0x42e01f)));
    }
}
