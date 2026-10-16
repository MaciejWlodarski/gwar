//! Shared complete Opus framing validation; no decoding.
pub use vc_proto::opus::packet_samples;

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
