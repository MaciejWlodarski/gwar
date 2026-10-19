//! Native playout metadata, negotiated via SDP; browsers omit it. Every packet
//! repeats the slot generation so loss of the first packet or ordering relative
//! to `voice.slot` cannot obscure a speaker change.

use std::sync::Arc;
use str0m::rtp::{Extension, ExtensionSerializer, ExtensionValues};

pub const SLOT_GENERATION_ID: u8 = 9;

/// Two-byte, network-order slot ownership counter, incremented with wrapping.
/// Serial-number ordering assumes fewer than 32768 ownership changes in flight;
/// equal values and the ambiguous half-range distance are never newer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotGeneration(pub u16);

impl SlotGeneration {
    pub fn is_newer_than(self, old: Self) -> bool {
        (self.0.wrapping_sub(old.0) as i16) > 0
    }
}

pub fn slot_generation_extension() -> Extension {
    Extension::UnknownUri("urn:gwar:rtp-hdrext:slot-generation".into(), Arc::new(Serializer))
}

#[derive(Debug)]
struct Serializer;

impl ExtensionSerializer for Serializer {
    fn write_to(&self, buf: &mut [u8], values: &ExtensionValues) -> usize {
        let Some(generation) = values.user_values.get::<SlotGeneration>() else { return 0 };
        buf[..2].copy_from_slice(&generation.0.to_be_bytes());
        2
    }

    fn parse_value(&self, buf: &[u8], values: &mut ExtensionValues) -> bool {
        let Ok(bytes) = buf.try_into() else { return false };
        values.user_values.set(SlotGeneration(u16::from_be_bytes(bytes)));
        true
    }

    fn is_video(&self) -> bool {
        false
    }
    fn is_audio(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_generation_writes_nothing() {
        let mut bytes = [0xaa; 2];
        assert_eq!(Serializer.write_to(&mut bytes, &ExtensionValues::default()), 0);
        assert_eq!(bytes, [0xaa; 2]);
        assert_eq!(Serializer.write_to(&mut [], &ExtensionValues::default()), 0);
    }

    #[test]
    fn generation_wraps_with_serial_number_ordering() {
        for n in [65535u16, 0, 1] {
            let old = SlotGeneration(n.wrapping_sub(1));
            let new = SlotGeneration(n);
            assert!(new.is_newer_than(old));
            assert!(!old.is_newer_than(new));
            assert!(!new.is_newer_than(new));
            let mut values = ExtensionValues::default();
            values.user_values.set(new);
            let mut bytes = [0; 2];
            assert_eq!(Serializer.write_to(&mut bytes, &values), 2);
            assert_eq!(bytes, n.to_be_bytes());
            let mut received = ExtensionValues::default();
            assert!(Serializer.parse_value(&bytes, &mut received));
            assert_eq!(received.user_values.get::<SlotGeneration>(), Some(&new));
        }
        assert!(!SlotGeneration(32768).is_newer_than(SlotGeneration(0)));
        assert!(!SlotGeneration(0).is_newer_than(SlotGeneration(32768)));
    }

    #[test]
    fn generation_round_trips_and_rejects_truncation() {
        let mut values = ExtensionValues::default();
        values.user_values.set(SlotGeneration(0x1234));
        let mut bytes = [0; 2];
        assert_eq!(Serializer.write_to(&mut bytes, &values), 2);
        let mut received = ExtensionValues::default();
        assert!(!Serializer.parse_value(&bytes[..1], &mut received));
        assert!(!Serializer.parse_value(&[0; 8], &mut received));
        assert!(Serializer.parse_value(&bytes, &mut received));
        assert_eq!(received.user_values.get::<SlotGeneration>(), Some(&SlotGeneration(0x1234)));
    }
}
