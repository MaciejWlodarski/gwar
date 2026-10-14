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
        #[serde(rename = "mls.history.share")]
        HistoryShare { group: String, recipient: String, chunk: String },
        #[serde(rename = "mls.history.fetch")]
        HistoryFetch { #[serde(default)] group: Option<String>, #[serde(flatten)] page: MlsPage },
        #[serde(rename = "mls.history.wanted")]
        HistoryWanted { #[serde(flatten)] page: MlsPage },
        #[serde(rename = "mls.history.ack")]
        HistoryAck { ids: Vec<String> },
        #[serde(rename = "mls.history.request")]
        HistoryRequest { group: String },
    }

    #[serde(untagged)]
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
    }

    pub struct MlsPackage { pub device_key: String, pub package: String }
    pub struct MlsDevice { pub uid: Uid, pub device_key: String, pub hpke_key: String, #[cfg_attr(feature = "ts", ts(type = "number"))] pub created_at: i64, pub signature: String }
    pub struct MlsGroup { pub conversation: String, pub group: String, #[cfg_attr(feature = "ts", ts(type = "number"))] pub epoch: u64, pub pending: bool }
    pub struct MlsLogEntry { #[cfg_attr(feature = "ts", ts(type = "number"))] pub epoch: u64, pub proposals: Vec<String>, pub commit: String }
    pub struct MlsWelcome { pub id: String, pub group: String, pub welcome: String }
    pub struct MlsHistoryChunk { pub id: String, pub group: String, pub sender_uid: Uid, pub chunk: String }
    pub struct MlsHistoryWanted { pub group: String, pub device: String }

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
        HistoryWanted { group: String, device: String },
    }
}
