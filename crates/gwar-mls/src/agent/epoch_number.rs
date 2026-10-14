//! Decimal strings preserve all u64 epoch values at the JavaScript boundary.
use serde::{Deserialize, Deserializer, Serializer, de::Error};

pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&value.to_string())
}
pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let text = String::deserialize(deserializer)?;
    let value: u64 = text.parse().map_err(D::Error::custom)?;
    if value.to_string() != text {
        return Err(D::Error::custom("noncanonical epoch"));
    }
    Ok(value)
}
