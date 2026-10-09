//! Minimal Opus packet inspection (RFC 6716 §3.1); no decoding.

/// Samples per channel at 48 kHz carried by one Opus packet, or `None` if malformed.
pub fn packet_samples(packet: &[u8]) -> Option<u32> {
    let toc = *packet.first()?;
    let config = toc >> 3;
    let frame = match config {
        // SILK-only: 10, 20, 40, 60 ms
        0..=11 => [480, 960, 1920, 2880][(config & 3) as usize],
        // Hybrid: 10, 20 ms
        12..=15 => [480, 960][(config & 1) as usize],
        // CELT-only: 2.5, 5, 10, 20 ms
        _ => [120, 240, 480, 960][(config & 3) as usize],
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => u32::from(*packet.get(1)? & 0x3f),
    };
    let total = frame * frames;
    // RFC 6716: a packet never exceeds 120 ms.
    (frames > 0 && total <= 5760).then_some(total)
}

#[cfg(test)]
mod tests {
    use super::packet_samples;

    #[test]
    fn durations() {
        assert_eq!(packet_samples(&[0x78]), Some(960)); // hybrid FB 20 ms, one frame
        assert_eq!(packet_samples(&[0x08]), Some(960)); // SILK NB 20 ms
        assert_eq!(packet_samples(&[0xf8]), Some(960)); // CELT FB 20 ms
        assert_eq!(packet_samples(&[0x79]), Some(1920)); // two 20 ms frames
        assert_eq!(packet_samples(&[0x7b, 0x03]), Some(2880)); // code 3, three frames
        assert_eq!(packet_samples(&[0x7b, 0x07]), None); // 140 ms > 120 ms
        assert_eq!(packet_samples(&[]), None);
    }
}
