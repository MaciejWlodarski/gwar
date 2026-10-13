//! Optional native playout metadata. Negotiated via SDP; browsers and older
//! peers omit it. Every packet repeats the slot generation so loss of the first
//! packet or ordering relative to `voice.slot` cannot obscure a speaker change.

use std::sync::Arc;
use str0m::rtp::{Extension, ExtensionSerializer, ExtensionValues};

pub const SLOT_GENERATION_ID: u8 = 9;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotGeneration(pub u64);

pub fn slot_generation_extension() -> Extension {
    Extension::UnknownUri("urn:gwar:rtp-hdrext:slot-generation".into(), Arc::new(Serializer))
}

#[derive(Debug)]
struct Serializer;

impl ExtensionSerializer for Serializer {
    fn write_to(&self, buf: &mut [u8], values: &ExtensionValues) -> usize {
        let generation = values.user_values.get::<SlotGeneration>().map_or(0, |g| g.0);
        buf[..8].copy_from_slice(&generation.to_be_bytes());
        8
    }

    fn parse_value(&self, buf: &[u8], values: &mut ExtensionValues) -> bool {
        let Ok(bytes) = buf.try_into() else { return false };
        values.user_values.set(SlotGeneration(u64::from_be_bytes(bytes)));
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
    fn generation_round_trips_and_rejects_truncation() {
        let mut values = ExtensionValues::default();
        values.user_values.set(SlotGeneration(0x1234_5678_9abc_def0));
        let mut bytes = [0; 8];
        assert_eq!(Serializer.write_to(&mut bytes, &values), 8);
        let mut received = ExtensionValues::default();
        assert!(!Serializer.parse_value(&bytes[..7], &mut received));
        assert!(Serializer.parse_value(&bytes, &mut received));
        assert_eq!(received.user_values.get::<SlotGeneration>(), Some(&SlotGeneration(0x1234_5678_9abc_def0)));
    }
}
