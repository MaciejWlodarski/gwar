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
        // Until A3 supplies actual size/fps/reference requirements, a stream must fit a
        // whole advertised level. Extensions are retained, but cannot justify a higher SPS
        // level on their own (other Annex A constraints could still exceed the decoder).
        let within_level = resources(id)
            .zip(resources(self.receive_level))
            .is_some_and(|(need, cap)| need.iter().zip(cap).all(|(&need, cap)| need <= cap));
        within_level
            && profile(id).is_some()
            && profile(id) == profile(self.profile)
            && resources(id).is_some_and(|r| r.iter().zip(self.resources).all(|(&need, cap)| need <= cap))
    }

    fn parse(fmtp: &str) -> Option<Self> {
        let params: HashMap<_, _> = fmtp.split(';').filter_map(|p| p.trim().split_once('=')).collect();
        if params.get("packetization-mode") != Some(&"1") {
            return None;
        }
        let id = u32::from_str_radix(params.get("profile-level-id").copied().unwrap_or("42e01f"), 16).ok()?;
        profile(id)?;
        let mut level = id;
        if let Some(value) = params.get("max-recv-level") {
            level = (id & 0xff0000) | u16::from_str_radix(value, 16).ok()? as u32;
        }
        let mut r = resources(level)?;
        for (index, name) in ["max-mbps", "max-fs", "max-dpb", "max-br"].iter().enumerate() {
            if let Some(value) = params.get(name) {
                // max-dpb uses units of 8/3 macroblocks (RFC 6184 section 8.1).
                let value = value.parse::<u32>().ok()?;
                let value = if index == 2 { value.checked_mul(3)? / 8 } else { value };
                r[index] = r[index].max(value);
            }
        }
        Some(Self { profile: id, receive_level: level, resources: r })
    }
}

pub(super) fn offered(sdp: &str) -> HashMap<Mid, HashMap<Pt, Limits>> {
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
                    && let Some(limit) = Limits::parse(fmtp)
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
    fn profiles_levels_and_explicit_extensions_are_viewer_specific() {
        let base = Limits::parse("packetization-mode=1;profile-level-id=42e01f").unwrap();
        assert!(base.accepts(VideoCodec::H264(0x42c01f)));
        assert!(!base.accepts(VideoCodec::H264(0x42e028)));
        assert!(!base.accepts(VideoCodec::H264(0x640c1f)));
        let high = Limits::parse("packetization-mode=1;profile-level-id=640c1f;max-recv-level=0c33").unwrap();
        assert!(high.accepts(VideoCodec::H264(0x640c28)));
        assert!(!high.accepts(VideoCodec::H264(0x640028)));
        let extended = Limits::parse(
            "packetization-mode=1;profile-level-id=42e01f;max-fs=8192;max-mbps=245760;max-dpb=87382;max-br=20000",
        )
        .unwrap();
        assert!(
            !extended.accepts(VideoCodec::H264(0x42e028)),
            "extensions do not establish every constraint of a higher SPS level"
        );
        assert_eq!(extended.resources[2], 32768);
        for fmtp in [
            "packetization-mode=0;profile-level-id=42e01f",
            "packetization-mode=1;profile-level-id=4d001f",
            "packetization-mode=1;profile-level-id=640c1f;max-dpb=bogus",
        ] {
            assert!(Limits::parse(fmtp).is_none());
        }
    }
    #[test]
    fn raw_offers_keep_limits_and_pts_separate_for_each_m_line() {
        let sdp = "m=video 9 X 100\na=mid:one\na=rtpmap:100 H264/90000\na=fmtp:100 packetization-mode=1;profile-level-id=42e01f\nm=video 9 X 102\na=mid:two\na=rtpmap:102 H264/90000\na=fmtp:102 packetization-mode=1;profile-level-id=640c28\n";
        let caps = offered(sdp);
        assert!(caps[&Mid::from("one")][&100.into()].accepts(VideoCodec::H264(0x42e01f)));
        assert!(!caps[&Mid::from("one")][&100.into()].accepts(VideoCodec::H264(0x640c28)));
        assert!(caps[&Mid::from("two")][&102.into()].accepts(VideoCodec::H264(0x640c28)));
    }
}
