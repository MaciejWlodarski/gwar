//! Call-scoped voice bindings. Epochs and runs must survive JSON exactly.
use super::*;

pub(crate) fn safe_u64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let value = u64::deserialize(d)?;
    if value > MAX_SAFE_INTEGER as u64 {
        return Err(serde::de::Error::custom("unsafe integer"));
    }
    Ok(value)
}
pub(crate) fn required_option<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    Option::deserialize(d)
}
pub(crate) fn optional_safe_u64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    let value = Option::<u64>::deserialize(d)?;
    if value.is_some_and(|v| v > MAX_SAFE_INTEGER as u64) {
        return Err(serde::de::Error::custom("unsafe integer"));
    }
    Ok(value)
}
fn run<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let value = safe_u64(d)?;
    if value == 0 {
        return Err(serde::de::Error::custom("run must be positive"));
    }
    Ok(value)
}
fn optional_run<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    let value = Option::<u64>::deserialize(d)?;
    if value.is_some_and(|v| v == 0 || v > MAX_SAFE_INTEGER as u64) {
        return Err(serde::de::Error::custom("invalid run"));
    }
    Ok(value)
}
wire! {
    #[derive(Eq)]
    #[serde(deny_unknown_fields)]
    pub struct VoiceParticipation { pub call: String, pub participation: String }
    #[derive(Eq)]
    pub struct VoiceBinding {
        pub call: String,
        pub participation: String,
        #[serde(deserialize_with = "optional_run")]
        #[cfg_attr(feature = "ts", ts(type = "number | null"))]
        pub run: Option<u64>,
    }
    #[derive(Eq)]
    pub struct VoiceTransportBinding {
        pub call: String,
        pub participation: String,
        #[serde(deserialize_with = "run")]
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        pub run: u64,
    }
    #[serde(deny_unknown_fields)]
    pub struct VoiceState {
        pub channel: ChannelId,
        pub call: String,
        pub participation: String,
        pub encrypted: bool,
        pub channel_group: Option<String>,
        pub group: Option<String>,
        #[serde(deserialize_with = "optional_safe_u64")]
        #[cfg_attr(feature = "ts", ts(type = "number | null"))]
        pub epoch: Option<u64>,
    }
    #[serde(deny_unknown_fields)]
    pub struct VoiceOffer {
        pub channel: ChannelId,
        pub call: String,
        pub participation: String,
        #[serde(deserialize_with = "run")]
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        pub run: u64,
        pub group: Option<String>,
        pub ssrc: u32,
        pub sdp: String,
    }
    #[serde(deny_unknown_fields)]
    pub struct VoiceEpoch {
        pub call: String,
        pub participation: String,
        #[serde(deserialize_with = "run")]
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        pub run: u64,
        pub group: String,
        #[serde(deserialize_with = "safe_u64")]
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        pub epoch: u64,
    }
    #[derive(Copy, Eq)]
    #[serde(rename_all = "snake_case")]
    pub enum VoiceClosedReason {
        TransportFailed, ChannelRestarted, ChannelDeleted, EpochExhausted, StreamLimit, StateLimit,
    }
}
impl VoiceOffer {
    pub fn binding(&self) -> VoiceTransportBinding {
        VoiceTransportBinding { call: self.call.clone(), participation: self.participation.clone(), run: self.run }
    }
}
impl From<&VoiceTransportBinding> for VoiceBinding {
    fn from(b: &VoiceTransportBinding) -> Self {
        Self { call: b.call.clone(), participation: b.participation.clone(), run: Some(b.run) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn rejects_unsafe_fractional_negative_and_forged_ack_fields() {
        let valid = json!({"id":1,"op":"voice.epoch.ack","d":{"call":"AAAAAAAAAAAAAAAAAAAAAA","participation":"AAAAAAAAAAAAAAAAAAAAAA","run":1,"group":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","epoch":0}});
        assert!(serde_json::from_value::<ClientFrame>(valid.clone()).is_ok());
        for field in ["run", "epoch"] {
            for number in [json!(-1), json!(1.5), json!(9007199254740992_u64)] {
                let mut frame = valid.clone();
                frame["d"][field] = number;
                assert!(serde_json::from_value::<ClientFrame>(frame).is_err());
            }
        }
        let mut forged = valid.clone();
        forged["d"]["device"] = json!("chosen device");
        assert!(serde_json::from_value::<ClientFrame>(forged).is_err());
        let mut zero = valid;
        zero["d"]["run"] = json!(0);
        assert!(serde_json::from_value::<ClientFrame>(zero).is_err());
        assert_eq!(serde_json::from_value::<Response>(json!({})).unwrap(), Response::Empty {});
        assert_eq!(
            serde_json::from_value::<Response>(json!({"voice":null})).unwrap(),
            Response::VoiceState { voice: None }
        );
    }
    #[test]
    fn mls_epochs_are_exact_wire_integers() {
        for epoch in [json!(-1), json!(0.5), json!(9007199254740992_u64)] {
            assert!(
                serde_json::from_value::<MlsRequest>(
                    json!({"op":"mls.commit","d":{"group":"g","epoch":epoch,"commit":"c","group_info":"i"}})
                )
                .is_err()
            );
            assert!(serde_json::from_value::<MlsResponse>(json!({"epoch":epoch})).is_err());
        }
    }
}
