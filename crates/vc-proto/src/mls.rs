//! Delivery service requests and notifications. Binary fields are base64url.

use super::*;

wire! {
    #[derive(Default)]
    pub struct MlsPage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub after: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub limit: Option<u32>,
    }

    #[serde(tag = "op", content = "d")]
    pub enum MlsRequest {
        #[serde(rename = "mls.device.publish")]
        DevicePublish { hpke_key: String, #[cfg_attr(feature = "ts", ts(type = "number"))] created_at: i64, signature: String },
        #[serde(rename = "mls.key_packages.publish")]
        KeyPackagesPublish { packages: Vec<String>, #[serde(default)] last_resort: Option<String> },
        #[serde(rename = "mls.key_packages.claim")]
        KeyPackagesClaim { uids: Vec<Uid>, #[serde(flatten)] page: MlsPage },
        #[serde(rename = "mls.devices")]
        Devices { uids: Vec<Uid>, #[serde(flatten)] page: MlsPage },
        #[serde(rename = "mls.group.create")]
        GroupCreate { conversation: String, group_info: String },
        #[serde(rename = "mls.group_info")]
        GroupInfo { group: String },
        #[serde(rename = "mls.commit")]
        Commit { group: String, #[cfg_attr(feature = "ts", ts(type = "number"))] epoch: u64, commit: String, group_info: String, #[serde(default)] welcome: Option<String> },
        #[serde(rename = "mls.log")]
        Log { group: String, #[cfg_attr(feature = "ts", ts(type = "number"))] since: u64, #[serde(default)] limit: Option<u32> },
        #[serde(rename = "mls.groups")]
        Groups { #[serde(flatten)] page: MlsPage },
        #[serde(rename = "mls.welcomes")]
        Welcomes { #[serde(flatten)] page: MlsPage },
        #[serde(rename = "mls.welcomes.ack")]
        WelcomesAck { ids: Vec<String> },
        #[serde(rename = "mls.group_history")]
        GroupHistory { conversation: String, #[serde(flatten)] page: MlsPage },
        #[serde(rename = "mls.history.share")]
        HistoryShare { group: String, recipient: String, chunk: String, #[serde(default)] assignment: Option<String> },
        #[serde(rename = "mls.history.decline")]
        HistoryDecline { group: String, device: String, assignment: String },
        #[serde(rename = "mls.history.fetch")]
        HistoryFetch { #[serde(default)] group: Option<String>, #[serde(flatten)] page: MlsPage },
        #[serde(rename = "mls.history.wanted")]
        HistoryWanted { #[serde(flatten)] page: MlsPage },
        #[serde(rename = "mls.history.ack")]
        HistoryAck { ids: Vec<String> },
        #[serde(rename = "mls.history.request")]
        HistoryRequest { group: String },
    }

    #[serde(untagged, deny_unknown_fields)]
    pub enum MlsResponse {
        Published { count: u32 },
        Created { group: String },
        GroupInfo { group_info: String, #[cfg_attr(feature = "ts", ts(type = "number"))] epoch: u64 },
        Committed { #[cfg_attr(feature = "ts", ts(type = "number"))] epoch: u64 },
        Packages { packages: Vec<MlsPackage>, next: Option<String> },
        Devices { devices: Vec<MlsDevice>, next: Option<String> },
        Log { #[cfg_attr(feature = "ts", ts(type = "number"))] head: u64, #[cfg_attr(feature = "ts", ts(type = "number"))] oldest: u64, entries: Vec<MlsLogEntry>, pending: Vec<String>, #[cfg_attr(feature = "ts", ts(type = "number | null"))] next: Option<u64> },
        Groups { groups: Vec<MlsGroup>, next: Option<String> },
        Welcomes { welcomes: Vec<MlsWelcome>, next: Option<String> },
        Chunks { chunks: Vec<MlsHistoryChunk>, next: Option<String> },
        Wanted { wanted: Vec<MlsHistoryWanted>, next: Option<String> },
        Incarnations { incarnations: Vec<MlsIncarnation>, next: Option<String> },
    }

    pub struct MlsPackage { pub device_key: String, pub package: String }
    pub struct MlsDevice { pub uid: Uid, pub device_key: String, pub hpke_key: String, #[cfg_attr(feature = "ts", ts(type = "number"))] pub created_at: i64, pub signature: String }
    pub struct MlsGroup { pub conversation: String, pub group: String, #[cfg_attr(feature = "ts", ts(type = "number"))] pub epoch: u64, pub pending: bool }
    pub struct MlsLogEntry { #[cfg_attr(feature = "ts", ts(type = "number"))] pub epoch: u64, pub proposals: Vec<String>, pub commit: String }
    pub struct MlsWelcome { pub id: String, pub group: String, pub welcome: String }
    pub struct MlsHistoryChunk { pub id: String, pub group: String, pub sender_uid: Uid, pub chunk: String }
    pub struct MlsHistoryWanted { pub group: String, pub device: String, #[serde(default, skip_serializing_if = "Option::is_none")] pub assignment: Option<String> }
    /// A conversation's group of one incarnation, with its encoded `SignedDescriptor`.
    pub struct MlsIncarnation { pub group: String, pub descriptor: String }

    #[serde(tag = "ev", content = "d")]
    pub enum MlsEvent {
        #[serde(rename = "mls.commit")]
        Commit { group: String, #[cfg_attr(feature = "ts", ts(type = "number"))] epoch: u64, proposals: Vec<String>, commit: String },
        #[serde(rename = "mls.proposal")]
        Proposal { group: String, #[cfg_attr(feature = "ts", ts(type = "number"))] epoch: u64, proposal: String },
        #[serde(rename = "mls.welcome")]
        Welcome { group: String },
        #[serde(rename = "mls.history")]
        History { group: String },
        #[serde(rename = "mls.history.wanted")]
        HistoryWanted { group: String, device: String, #[serde(default, skip_serializing_if = "Option::is_none")] assignment: Option<String> },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_mls_response_round_trips_through_the_server_envelope() {
        let mut replies = vec![
            json!({"count":0}),
            json!({"group":"g"}),
            json!({"group_info":"info","epoch":1}),
            json!({"epoch":1}),
            json!({"packages":[{"device_key":"d","package":"p"}],"next":"cursor"}),
            json!({"devices":[{"uid":"u","device_key":"d","hpke_key":"h","created_at":1,"signature":"s"}],"next":null}),
            json!({"head":1,"oldest":0,"entries":[{"epoch":0,"proposals":["p"],"commit":"c"}],"pending":["p"],"next":1}),
            json!({"groups":[{"conversation":"c","group":"g","epoch":1,"pending":false}],"next":null}),
            json!({"welcomes":[{"id":"i","group":"g","welcome":"w"}],"next":null}),
            json!({"chunks":[{"id":"i","group":"g","sender_uid":"u","chunk":"c"}],"next":null}),
            json!({"wanted":[{"group":"g","device":"d"}],"next":null}),
            json!({"wanted":[{"group":"g","device":"d","assignment":"a"}],"next":null}),
            json!({"incarnations":[{"group":"g","descriptor":"d"}],"next":null}),
            json!({"head":0,"oldest":0,"entries":[],"pending":[],"next":null}),
        ];
        for field in ["packages", "devices", "groups", "welcomes", "chunks", "wanted", "incarnations"] {
            for next in [json!(null), json!("cursor")] {
                let mut reply = json!({"next":next});
                reply[field] = json!([]);
                replies.push(reply);
            }
        }
        for value in replies {
            let mls: MlsResponse = serde_json::from_value(value.clone()).unwrap();
            let frame = ServerFrame::Ok { re: 7, ok: Response::Mls(mls) };
            let wire = serde_json::to_vec(&frame).unwrap();
            let decoded: ServerFrame = serde_json::from_slice(&wire).unwrap();
            assert_eq!(decoded, frame, "{value}");
            assert_eq!(serde_json::to_value(decoded).unwrap(), json!({"re":7,"ok":value}));
        }
        for groups in [vec![], vec![1, 2]] {
            let response = Response::Groups { groups };
            assert_eq!(serde_json::from_value::<Response>(serde_json::to_value(&response).unwrap()).unwrap(), response);
        }
        assert_eq!(serde_json::from_str::<Response>("{}").unwrap(), Response::Empty {});
    }
}
