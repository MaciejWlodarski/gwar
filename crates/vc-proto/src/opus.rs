//! Complete Opus packet framing validation, without decoding (RFC 6716 §3.2).

/// Samples per channel at 48 kHz, only after validating every frame boundary.
/// Zero-byte frames are legal DTX/loss frames; an empty packet is not.
pub fn packet_samples(packet: &[u8]) -> Option<u32> {
    let (&toc, mut body) = packet.split_first()?;
    let config = toc >> 3;
    let frame = match config {
        0..=11 => [480, 960, 1920, 2880][(config & 3) as usize],
        12..=15 => [480, 960][(config & 1) as usize],
        _ => [120, 240, 480, 960][(config & 3) as usize],
    };
    let (frames, vbr) = match toc & 3 {
        0 => (1, false),
        1 => (2, false),
        2 => (2, true),
        _ => {
            let (&control, rest) = body.split_first()?;
            body = rest;
            if control & 0x40 != 0 {
                let mut padding = 0usize;
                loop {
                    let (&byte, rest) = body.split_first()?;
                    body = rest;
                    // 255 contributes 254 bytes and requires another length byte.
                    padding = padding.checked_add(if byte == 255 { 254 } else { usize::from(byte) })?;
                    if byte != 255 {
                        break;
                    }
                }
                body = body.get(..body.len().checked_sub(padding)?)?;
            }
            (usize::from(control & 0x3f), control & 0x80 != 0)
        }
    };
    let samples = frame * frames as u32;
    if frames == 0 || samples > 5760 {
        return None;
    }
    if vbr {
        let mut used = 0usize;
        for _ in 1..frames {
            used = used.checked_add(frame_length(&mut body)?)?;
        }
        // Length bytes all precede frame data; the last frame is implicit.
        if body.len().checked_sub(used)? > 1275 {
            return None;
        }
    } else if !body.len().is_multiple_of(frames) || body.len() / frames > 1275 {
        return None;
    }
    Some(samples)
}

fn frame_length(body: &mut &[u8]) -> Option<usize> {
    let (&first, rest) = body.split_first()?;
    *body = rest;
    let length = if first < 252 {
        usize::from(first)
    } else {
        let (&second, rest) = body.split_first()?;
        *body = rest;
        usize::from(first) + 4 * usize::from(second)
    };
    (length <= 1275).then_some(length)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_codes_and_length_boundaries() {
        for (packet, samples) in [
            (vec![0xf0], 480),
            (vec![0xf0, 1], 480),
            (vec![0xf1, 1, 2], 960),
            (vec![0xf1], 960),
            (vec![0xf2, 1, 1, 2, 3], 960),
            (vec![0xf2, 0], 960),
            (vec![0xf3, 3, 1, 2, 3], 1440),
            (vec![0xf3, 0x83, 1, 2, 1, 2, 3, 4], 1440),
            (vec![0xf3, 0xc2, 1, 1, 10, 20, 0], 960),
            (vec![0x83, 48], 5760),
        ] {
            assert_eq!(packet_samples(&packet), Some(samples), "{packet:?}");
        }
        let mut long = vec![0xf2, 255, 255]; // first frame = 1275
        long.resize(3 + 1275, 0);
        assert_eq!(packet_samples(&long), Some(960));
        let mut padded = vec![0xf3, 0x41, 255, 1, 10];
        padded.resize(5 + 255, 0);
        assert_eq!(packet_samples(&padded), Some(480));
        let mut maximal = vec![0xf0];
        maximal.resize(1276, 0);
        assert_eq!(packet_samples(&maximal), Some(480));
        maximal.push(0);
        assert_eq!(packet_samples(&maximal), None);
    }

    #[test]
    fn malformed_cbr_vbr_padding_and_duration() {
        for packet in [
            vec![],
            vec![0xf2],
            vec![0xf1, 1],
            vec![0xf2, 2, 1],
            vec![0xf2, 252],
            vec![0xf3],
            vec![0xf3, 0],
            vec![0xf3, 3, 1, 2],
            vec![0xf3, 0x82],
            vec![0xf3, 0x83, 1],
            vec![0xf3, 0x82, 5, 1],
            vec![0xf3, 0x41],
            vec![0xf3, 0x41, 255],
            vec![0xf3, 0x41, 2, 0],
            vec![0xf3, 0xc2, 1, 1, 0],
            vec![0xfb, 7],
            vec![0x83, 49],
        ] {
            assert_eq!(packet_samples(&packet), None, "{packet:?}");
        }
        for mut packet in [vec![0xf1], vec![0xf3, 2], vec![0xf2, 0]] {
            packet.resize(packet.len() + 2552, 0);
            assert_eq!(packet_samples(&packet), None);
        }
    }
}
