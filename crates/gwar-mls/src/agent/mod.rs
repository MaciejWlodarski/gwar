//! One device's MLS state. Persist `take_changes()` atomically after every
//! operation, before sending its output. On any error, discard and reload.

pub mod epoch_number;
mod storage;
mod voice;
pub use storage::{Change, Changes, Store, Stores};
pub use voice::{VoiceKey, VoiceKeys, voice_kid};

use crate::crypto::Provider;
use crate::{
    Credential, DESCRIPTOR_EXTENSION, Revocations, SUITE, SignedDescriptor,
    ds::{self, CommitSender},
    encoding::{b64, fixed},
    history::{self, Chunk, DeviceHpkeKey, Header},
    rules,
    signer::{Ed25519Signer, leaf_credential, server_credential},
};
use ed25519_dalek::SigningKey;
use openmls::prelude::{
    tls_codec::{Deserialize as _, Serialize as _},
    *,
};
use openmls_traits::random::OpenMlsRand;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AgentError {
    #[error("invalid: {0}")]
    Invalid(&'static str),
    #[error("agent must be reloaded")]
    Reload,
    #[error("persist changes before the next operation")]
    Unpersisted,
    #[error("group cannot send")]
    Blocked,
    #[error("device must rejoin")]
    Rejoin,
    #[error(transparent)]
    Rule(#[from] ds::DsError),
}
use AgentError::Invalid;
type Result<T> = std::result::Result<T, AgentError>;
fn err<E>(reason: &'static str) -> impl FnOnce(E) -> AgentError {
    move |_| Invalid(reason)
}
const MAX_HANDSHAKE_BYTES: usize = 128 * 1024;
fn handshake_size(bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_HANDSHAKE_BYTES {
        return Err(Invalid("handshake size"));
    }
    Ok(())
}
fn wire(message: impl Into<MlsMessageOut>) -> Result<Vec<u8>> {
    let bytes = message.into().tls_serialize_detached().map_err(err("MLS encoding"))?;
    handshake_size(&bytes)?;
    Ok(bytes)
}
fn hash(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
fn current_time(now: i64) -> Result<()> {
    if !(0..=vc_proto::MAX_SAFE_INTEGER).contains(&now) {
        return Err(Invalid("time"));
    }
    Ok(())
}
fn client_context(context: &GroupContext, server: &str) -> Result<SignedDescriptor> {
    Ok(rules::context(context, server)?)
}
fn checkpoint(context: &GroupContext) -> Result<Checkpoint> {
    Ok(Checkpoint {
        epoch: context.epoch().as_u64(),
        hash: hash(&context.tls_serialize_detached().map_err(err("context encoding"))?),
    })
}
/// `next` restarts `previous`: the next incarnation, naming it, with the same
/// server, conversation, kind, `private`, `history` and owner.
fn successor(previous: &SignedDescriptor, next: &SignedDescriptor) -> bool {
    let (p, n) = (&previous.descriptor, &next.descriptor);
    p.incarnation.checked_add(1) == Some(n.incarnation)
        && n.previous_group_id.as_deref() == Some(b64(&previous.group_id()).as_str())
        && (&p.server_key, &p.conversation, p.kind, p.private, p.history, &p.founding_owner, &p.channel_group_id)
            == (&n.server_key, &n.conversation, n.kind, n.private, n.history, &n.founding_owner, &n.channel_group_id)
}
fn json<T: Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).expect("record serialization")
}
fn parse<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(err("stored record"))
}
fn group_key(server: &str, id: &[u8; 32]) -> String {
    format!("{server}/{}", b64(id))
}
fn epoch_key(group: &str, epoch: u64) -> String {
    format!("{group}/{epoch:020}")
}
fn capabilities() -> Capabilities {
    Capabilities::new(
        None,
        Some(&[SUITE]),
        Some(&[ExtensionType::Unknown(DESCRIPTOR_EXTENSION), ExtensionType::LastResort]),
        None,
        Some(&[CredentialType::Basic]),
    )
}
fn join_config(kind: crate::Kind) -> MlsGroupJoinConfig {
    MlsGroupJoinConfig::builder()
        .use_ratchet_tree_extension(!kind.separate_tree())
        .wire_format_policy(PURE_PLAINTEXT_WIRE_FORMAT_POLICY)
        .build()
}
fn info(bytes: &[u8]) -> Result<openmls::messages::group_info::VerifiableGroupInfo> {
    handshake_size(bytes)?;
    match MlsMessageIn::tls_deserialize_exact(bytes).map_err(err("group info encoding"))?.extract() {
        MlsMessageBodyIn::GroupInfo(info) => Ok(info),
        _ => Err(Invalid("not a group info")),
    }
}
// An embedded tree wins; separate trees are ignored for DMs and creation.
fn info_tree(
    info: &openmls::messages::group_info::VerifiableGroupInfo,
    kind: crate::Kind,
    tree: Option<&[u8]>,
) -> Result<RatchetTreeIn> {
    if let Some(extension) = info.extensions().ratchet_tree() {
        return Ok(extension.ratchet_tree().clone());
    }
    if !kind.separate_tree() {
        return Err(Invalid("no ratchet tree"));
    }
    RatchetTreeIn::tls_deserialize_exact(tree.ok_or(Invalid("no ratchet tree"))?).map_err(err("ratchet tree encoding"))
}
fn protocol(bytes: &[u8]) -> Result<PublicMessageIn> {
    handshake_size(bytes)?;
    let message = MlsMessageIn::tls_deserialize_exact(bytes)
        .map_err(err("handshake encoding"))?
        .try_into_protocol_message()
        .map_err(err("not a handshake"))?;
    match message {
        ProtocolMessage::PublicMessage(message) => Ok(*message),
        _ => Err(Invalid("not a public message")),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    #[serde(with = "epoch_number")]
    pub epoch: u64,
    pub hash: [u8; 32],
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Creating,
    Active,
    Joining,
    Removed,
    Rejoin,
    /// An earlier incarnation of a conversation that was restarted: no MLS
    /// state, never live again; its epoch keys stay for reading.
    Superseded,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupRecord {
    pub descriptor: Vec<u8>,
    pub status: Status,
    pub checkpoint: Option<Checkpoint>,
    pub welcome_ids: BTreeSet<String>,
    pub server_pending: bool,
    /// Voice-only holds survive proposal-queue clearing on competing commits.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    voice_removals: BTreeSet<u32>,
    /// A joined voice snapshot needs the current epoch's complete pending set.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    voice_needs_sync: bool,
    /// For every uid, the epochs in which it had a leaf, as this device saw
    /// them: sorted, disjoint, non-adjacent inclusive ranges.
    pub evidence: BTreeMap<String, Vec<Interval>>,
    /// When this device's own last commit in the group was accepted; `None`
    /// after a Welcome, whose leaf came from a key package.
    pub own_commit_at: Option<i64>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Interval {
    #[serde(with = "epoch_number")]
    pub first: u64,
    #[serde(with = "epoch_number")]
    pub last: u64,
}
fn witness(intervals: &mut Vec<Interval>, epoch: u64) {
    match intervals.last_mut() {
        Some(i) if i.last >= epoch => {}
        Some(i) if i.last + 1 == epoch => i.last = epoch,
        _ => intervals.push(Interval { first: epoch, last: epoch }),
    }
}
fn witnessed(intervals: &[Interval], epoch: u64) -> bool {
    intervals.iter().any(|i| (i.first..=i.last).contains(&epoch))
}
fn valid_evidence(evidence: &BTreeMap<String, Vec<Interval>>) -> bool {
    evidence.iter().all(|(uid, intervals)| {
        crate::encoding::fixed::<20>(uid).is_some()
            && !intervals.is_empty()
            && intervals.iter().all(|i| i.first <= i.last)
            && intervals.windows(2).all(|w| w[0].last.checked_add(1).is_some_and(|end| end < w[1].first))
    })
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum EpochRecord {
    Derived { key: [u8; 32] },
    Imported { key: [u8; 32], sender_uids: BTreeSet<String> },
    Conflicted,
}
impl EpochRecord {
    pub fn key(&self) -> Option<&[u8; 32]> {
        match self {
            Self::Derived { key, .. } | Self::Imported { key, .. } => Some(key),
            Self::Conflicted => None,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Outbox {
    pub group: [u8; 32],
    #[serde(with = "epoch_number")]
    pub epoch: u64,
    pub hash: [u8; 32],
    pub commit: Vec<u8>,
    pub welcome: Option<Vec<u8>>,
    pub group_info: Vec<u8>,
    pub committed_at: i64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gate {
    pub pending_commit: bool,
    pub blocked: Vec<u32>,
    pub server_pending: bool,
    pub rejoin: bool,
    pub status: Status,
}
impl Gate {
    pub fn can_send(&self) -> bool {
        self.status == Status::Active && self.blocked.is_empty() && !self.server_pending && !self.rejoin
    }
}

/// The authoritative log result for the outbox's source epoch.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "case", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reconcile {
    Entry {
        #[serde(with = "epoch_number")]
        epoch: u64,
        commit: Vec<u8>,
        proposals: Vec<Vec<u8>>,
    },
    Head {
        #[serde(with = "epoch_number")]
        epoch: u64,
    },
    Pruned {
        #[serde(with = "epoch_number")]
        oldest: u64,
    },
    Rejected {
        reason: Rejection,
    },
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rejection {
    Expired,
    Early,
    BadRequest,
    Stale,
    Other,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    Accepted,
    Retry,
    Rebuild,
    Rejoin,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeviceRecord {
    version: u8,
    device_key: String,
    hpke_private: [u8; 32],
    hpke_public: [u8; 32],
    created_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Member {
    pub device_key: String,
    pub uid: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct GroupSummary {
    pub group: [u8; 32],
    pub conversation: String,
    #[serde(with = "epoch_number")]
    pub incarnation: u64,
    /// The conversation's live pin: its highest accepted incarnation.
    pub live: bool,
    pub history: bool,
    pub record: GroupRecord,
    pub members: Vec<Member>,
}

pub struct Agent {
    signer: Ed25519Signer,
    credential: Credential,
    provider: Provider,
    device: DeviceRecord,
    groups: BTreeMap<String, GroupRecord>,
    epochs: BTreeMap<String, EpochRecord>,
    outbox: BTreeMap<String, Outbox>,
    durable: Stores,
    ready: bool,
    poisoned: bool,
    voice_only: bool,
}

impl Agent {
    /// A separate provider for a call. Its changes/outbox belong only in memory,
    /// under the same device lease as the durable chat agent.
    pub fn new_voice(seed: [u8; 32], credential: Credential, now: i64) -> Result<Self> {
        let mut agent = Self::new(seed, credential, now)?;
        agent.voice_only = true;
        Ok(agent)
    }

    pub fn new(seed: [u8; 32], credential: Credential, now: i64) -> Result<Self> {
        let signer = Self::signer(seed, &credential)?;
        if !(1..=vc_proto::MAX_SAFE_INTEGER).contains(&now) {
            return Err(Invalid("time"));
        }
        let provider = Provider::default();
        let ikm = provider.rand().random_array().map_err(err("random"))?;
        let (hpke_private, hpke_public) = history::hpke_key_pair(&ikm);
        Ok(Self {
            device: DeviceRecord {
                version: 1,
                device_key: credential.device_key().into(),
                hpke_private,
                hpke_public,
                created_at: now,
            },
            signer,
            credential,
            provider,
            groups: BTreeMap::new(),
            epochs: BTreeMap::new(),
            outbox: BTreeMap::new(),
            durable: Stores::default(),
            ready: true,
            poisoned: false,
            voice_only: false,
        })
    }

    fn signer(seed: [u8; 32], credential: &Credential) -> Result<Ed25519Signer> {
        let signer = Ed25519Signer(SigningKey::from_bytes(&seed));
        if Credential::parse(credential.encode().as_bytes()).as_ref() != Some(credential)
            || credential.device_key() != b64(signer.0.verifying_key().as_bytes())
        {
            return Err(Invalid("device credential"));
        }
        Ok(signer)
    }

    pub fn load(seed: [u8; 32], credential: Credential, stores: Stores) -> Result<Self> {
        Self::load_store(seed, credential, stores, false)
    }
    /// Reload only the call's volatile checkpoint after a rejected operation.
    /// A process/worker restart must discard it and rejoin, never restore it.
    pub fn load_voice(seed: [u8; 32], credential: Credential, stores: Stores) -> Result<Self> {
        Self::load_store(seed, credential, stores, true)
    }
    fn load_store(seed: [u8; 32], credential: Credential, stores: Stores, voice_only: bool) -> Result<Self> {
        let signer = Self::signer(seed, &credential)?;
        let device: DeviceRecord =
            parse(stores.groups.get(b"@device".as_slice()).ok_or(Invalid("missing device record"))?)?;
        if device.version != 1
            || device.device_key != credential.device_key()
            || !(1..=vc_proto::MAX_SAFE_INTEGER).contains(&device.created_at)
            || device.hpke_public
                != x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(device.hpke_private)).to_bytes()
        {
            return Err(Invalid("device record"));
        }
        let provider = Provider::default();
        *provider.storage().values.write().expect("not poisoned") = stores.openmls.clone().into_iter().collect();
        fn records<T: serde::de::DeserializeOwned>(store: &Store, skip_device: bool) -> Result<BTreeMap<String, T>> {
            store
                .iter()
                .filter(|(k, _)| !skip_device || k.as_slice() != b"@device")
                .map(|(k, v)| Ok((String::from_utf8(k.clone()).map_err(err("record key"))?, parse(v)?)))
                .collect()
        }
        let agent = Self {
            signer,
            credential,
            provider,
            device,
            groups: records(&stores.groups, true)?,
            epochs: records(&stores.epochs, false)?,
            outbox: records(&stores.outbox, false)?,
            durable: stores,
            ready: false,
            poisoned: false,
            voice_only,
        };
        for key in agent.epochs.keys() {
            let (group, epoch) = key.rsplit_once('/').ok_or(Invalid("epoch key"))?;
            if !agent.groups.contains_key(group)
                || epoch.len() != 20
                || !epoch.bytes().all(|c| c.is_ascii_digit())
                || epoch.parse::<u64>().is_err()
            {
                return Err(Invalid("epoch key"));
            }
        }
        for key in agent.outbox.keys() {
            if !agent.groups.contains_key(key) {
                return Err(Invalid("outbox key"));
            }
        }
        for (key, record) in &agent.groups {
            let descriptor = SignedDescriptor::parse(&record.descriptor).ok_or(Invalid("stored descriptor"))?;
            if (descriptor.descriptor.kind == crate::Kind::Voice) != voice_only {
                return Err(Invalid("agent store kind"));
            }
            if *key != group_key(&descriptor.descriptor.server_key, &descriptor.group_id()) {
                return Err(Invalid("group key"));
            }
            if !valid_evidence(&record.evidence) {
                return Err(Invalid("stored evidence"));
            }
            if matches!(record.status, Status::Creating | Status::Active | Status::Joining) {
                let group = agent.mls(key)?;
                client_context(group.public_group().group_context(), &descriptor.descriptor.server_key)?;
                rules::tree(&descriptor, group.public_group().treesync().full_leaves().map(|(_, l)| l))?;
                if record.status == Status::Creating
                    && (group.epoch().as_u64() != 0
                        || record.checkpoint.is_some()
                        || descriptor.creator.device_key() != agent.credential.device_key()
                        || descriptor.creator.uid() != agent.credential.uid()
                        || agent.epochs.keys().any(|k| k.starts_with(&format!("{key}/"))))
                {
                    return Err(Invalid("stored creation"));
                }
                if record.status == Status::Active
                    && record.checkpoint.as_ref() != Some(&checkpoint(group.public_group().group_context())?)
                {
                    return Err(Invalid("stored checkpoint"));
                }
                if (group.pending_commit().is_some() || record.status == Status::Joining)
                    != agent.outbox.contains_key(key)
                {
                    return Err(Invalid("stored outbox"));
                }
            } else if agent.outbox.contains_key(key) {
                return Err(Invalid("inactive outbox"));
            }
        }
        for (key, out) in &agent.outbox {
            let record = agent.groups.get(key).ok_or(Invalid("orphan outbox"))?;
            let group = agent.mls(key)?;
            let context = if record.status == Status::Joining {
                group.public_group().group_context()
            } else {
                group.pending_commit().ok_or(Invalid("missing pending commit"))?.group_context()
            };
            if out.group.as_slice() != group.group_id().as_slice()
                || hash(&out.commit) != out.hash
                || out.epoch.checked_add(1) != Some(context.epoch().as_u64())
                || checkpoint(info(&out.group_info)?.group_context())? != checkpoint(context)?
            {
                return Err(Invalid("stored outbox"));
            }
        }
        Ok(agent)
    }

    fn snapshot(&self) -> Stores {
        fn records<T: Serialize>(map: &BTreeMap<String, T>) -> Store {
            map.iter().map(|(k, v)| (k.as_bytes().to_vec(), json(v))).collect()
        }
        let mut groups = records(&self.groups);
        groups.insert(b"@device".to_vec(), json(&self.device));
        Stores {
            openmls: self.provider.storage().values.read().expect("not poisoned").clone().into_iter().collect(),
            groups,
            epochs: records(&self.epochs),
            outbox: records(&self.outbox),
        }
    }

    /// A failed operation always exposes an empty change set and poisons the agent.
    pub fn take_changes(&mut self) -> Changes {
        if self.poisoned || !self.ready {
            return Changes::default();
        }
        let next = self.snapshot();
        let changes = Changes::between(&self.durable, &next);
        self.durable = next;
        self.ready = false;
        changes
    }

    fn operation<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        if self.poisoned {
            return Err(AgentError::Reload);
        }
        if self.ready {
            self.poisoned = true;
            return Err(AgentError::Unpersisted);
        }
        match f(self) {
            Ok(value) => {
                self.ready = true;
                Ok(value)
            }
            Err(error) => {
                self.poisoned = true;
                Err(error)
            }
        }
    }

    fn mls(&self, key: &str) -> Result<MlsGroup> {
        let record = self.groups.get(key).ok_or(Invalid("unknown group"))?;
        let descriptor = SignedDescriptor::parse(&record.descriptor).ok_or(Invalid("descriptor"))?;
        MlsGroup::load(self.provider.storage(), &GroupId::from_slice(&descriptor.group_id()))
            .map_err(err("load group"))?
            .ok_or(AgentError::Rejoin)
    }
    fn with_key(&self) -> CredentialWithKey {
        CredentialWithKey {
            credential: BasicCredential::new(self.credential.encode().into_bytes()).into(),
            signature_key: self.signer.0.verifying_key().as_bytes().to_vec().into(),
        }
    }
    fn lifetime(&self, now: i64) -> Result<Lifetime> {
        if !(0..=vc_proto::MAX_SAFE_INTEGER).contains(&now) || !self.credential.is_current(now, &()) {
            return Err(Invalid("certificate now"));
        }
        let start = now as u64 / 1000;
        let end = match &self.credential {
            Credential::Account(c) => (c.expires_at as u64 / 1000).min(start + 84 * 86400),
            _ => start + 84 * 86400,
        };
        if end <= start {
            return Err(Invalid("key package lifetime"));
        }
        Ok(Lifetime::init(start, end))
    }
    pub fn key_packages(&mut self, n: usize, now: i64) -> Result<Vec<Vec<u8>>> {
        if self.voice_only {
            return Err(Invalid("voice has no key packages"));
        }
        self.operation(|a| {
            if n > 20 {
                return Err(Invalid("key package count"));
            }
            (0..n).map(|_| a.package(now, false)).collect()
        })
    }
    pub fn last_resort_key_package(&mut self, now: i64) -> Result<Vec<u8>> {
        if self.voice_only {
            return Err(Invalid("voice has no key packages"));
        }
        self.operation(|a| a.package(now, true))
    }
    fn package(&self, now: i64, last_resort: bool) -> Result<Vec<u8>> {
        let mut builder =
            KeyPackage::builder().key_package_lifetime(self.lifetime(now)?).leaf_node_capabilities(capabilities());
        if last_resort {
            builder = builder.mark_as_last_resort();
        }
        builder
            .build(SUITE, &self.provider, &self.signer, self.with_key())
            .map_err(err("key package"))?
            .key_package()
            .tls_serialize_detached()
            .map_err(err("key package encoding"))
    }
    /// Browser synchronization inventory, including tentative creations and pins.
    pub fn inventory(&self, server: &str) -> Result<Vec<GroupSummary>> {
        let mut live = BTreeMap::new();
        for (_, record) in
            self.groups.iter().filter(|(k, r)| k.starts_with(&format!("{server}/")) && r.status != Status::Creating)
        {
            let d = SignedDescriptor::parse(&record.descriptor).ok_or(Invalid("descriptor"))?;
            let top = live.entry(d.descriptor.conversation.clone()).or_insert(d.descriptor.incarnation);
            *top = (*top).max(d.descriptor.incarnation);
        }
        self.groups
            .iter()
            .filter(|(key, _)| key.starts_with(&format!("{server}/")))
            .map(|(key, record)| {
                let d = SignedDescriptor::parse(&record.descriptor).ok_or(Invalid("descriptor"))?;
                let members = if matches!(record.status, Status::Active | Status::Joining | Status::Creating) {
                    self.mls(key)?
                        .public_group()
                        .treesync()
                        .full_leaves()
                        .map(|(_, leaf)| {
                            let c = leaf_credential(leaf).ok_or(Invalid("credential"))?;
                            Ok(Member { device_key: c.device_key().into(), uid: c.uid() })
                        })
                        .collect::<Result<Vec<_>>>()?
                } else {
                    Vec::new()
                };
                Ok(GroupSummary {
                    group: d.group_id(),
                    live: record.status != Status::Creating
                        && live.get(&d.descriptor.conversation) == Some(&d.descriptor.incarnation),
                    incarnation: d.descriptor.incarnation,
                    conversation: d.descriptor.conversation,
                    history: d.descriptor.history,
                    record: record.clone(),
                    members,
                })
            })
            .collect()
    }
    pub fn device_hpke_key(&self) -> DeviceHpkeKey {
        DeviceHpkeKey::sign(&self.signer.0, &self.device.hpke_public, self.device.created_at)
    }
    pub fn group(&self, server: &str, id: &[u8; 32]) -> Option<&GroupRecord> {
        self.groups.get(&group_key(server, id))
    }
    pub fn epoch(&self, server: &str, id: &[u8; 32], epoch: u64) -> Option<&EpochRecord> {
        self.epochs.get(&epoch_key(&group_key(server, id), epoch))
    }
    pub fn outbox(&self, server: &str, id: &[u8; 32]) -> Option<&Outbox> {
        self.outbox.get(&group_key(server, id))
    }

    /// This device's groups of one conversation: key, descriptor and status.
    fn pins(&self, server: &str, conversation: &str) -> Result<Vec<(String, SignedDescriptor, Status)>> {
        let prefix = format!("{server}/");
        let mut pins = Vec::new();
        for (key, record) in self.groups.iter().filter(|(k, _)| k.starts_with(&prefix)) {
            let d = SignedDescriptor::parse(&record.descriptor).ok_or(Invalid("descriptor"))?;
            if d.descriptor.conversation == conversation {
                pins.push((key.clone(), d, record.status));
            }
        }
        Ok(pins)
    }
    /// The live pin: the highest accepted incarnation (a creation is not
    /// accepted until it is settled).
    fn live(&self, server: &str, conversation: &str) -> Result<Option<SignedDescriptor>> {
        Ok(self
            .pins(server, conversation)?
            .into_iter()
            .filter(|(_, _, status)| *status != Status::Creating)
            .map(|(_, d, _)| d)
            .max_by_key(|d| d.descriptor.incarnation))
    }
    /// One group per conversation and incarnation.
    fn check_incarnation(&self, descriptor: &SignedDescriptor) -> Result<()> {
        let d = &descriptor.descriptor;
        if self
            .pins(&d.server_key, &d.conversation)?
            .iter()
            .any(|(_, old, _)| old.descriptor.incarnation == d.incarnation && old.group_id() != descriptor.group_id())
        {
            return Err(Invalid("group pin"));
        }
        Ok(())
    }
    /// Only the live pin may be joined. A later incarnation needs its chain
    /// accepted first, and without any pin only incarnation zero is a first
    /// contact.
    fn check_pin(&self, descriptor: &SignedDescriptor) -> Result<()> {
        self.check_incarnation(descriptor)?;
        let d = &descriptor.descriptor;
        let key = group_key(&d.server_key, &descriptor.group_id());
        if self.groups.get(&key).is_some_and(|r| r.status == Status::Superseded) {
            return Err(Invalid("superseded"));
        }
        match self.live(&d.server_key, &d.conversation)?.map(|live| live.descriptor.incarnation) {
            Some(live) if d.incarnation < live => Err(Invalid("superseded")),
            Some(live) if d.incarnation > live => Err(Invalid("restart chain")),
            None if d.incarnation > 0 => Err(Invalid("restart chain")),
            _ => Ok(()),
        }
    }
    /// Earlier incarnations are never live again: their MLS state, with every
    /// epoch's private keys, and outbox go; epoch keys and evidence stay.
    fn supersede(&mut self, server: &str, conversation: &str, live: u64) -> Result<()> {
        for (key, d, status) in self.pins(server, conversation)? {
            if d.descriptor.incarnation >= live || status == Status::Superseded {
                continue;
            }
            let group_id = GroupId::from_slice(&d.group_id());
            if let Some(mut group) = MlsGroup::load(self.provider.storage(), &group_id).map_err(err("load group"))? {
                group.delete(self.provider.storage()).map_err(err("delete group"))?;
            }
            self.provider.storage().purge_epoch_key_pairs(&group_id).map_err(err("delete keys"))?;
            self.outbox.remove(&key);
            let record = self.groups.get_mut(&key).ok_or(Invalid("record"))?;
            record.status = Status::Superseded;
            record.server_pending = false;
        }
        Ok(())
    }
    fn fresh(&self, key: &str, context: &GroupContext) -> Result<()> {
        if let Some(old) = self.groups.get(key).and_then(|r| r.checkpoint.as_ref()) {
            let next = checkpoint(context)?;
            if next.epoch < old.epoch || next.epoch == old.epoch && next.hash != old.hash {
                return Err(Invalid("freshness"));
            }
        }
        Ok(())
    }
    fn record(&mut self, descriptor: &SignedDescriptor, status: Status) -> String {
        let key = group_key(&descriptor.descriptor.server_key, &descriptor.group_id());
        let record = self.groups.entry(key.clone()).or_insert_with(|| GroupRecord {
            descriptor: descriptor.encode(),
            status,
            checkpoint: None,
            welcome_ids: BTreeSet::new(),
            server_pending: false,
            voice_removals: BTreeSet::new(),
            voice_needs_sync: false,
            evidence: BTreeMap::new(),
            own_commit_at: None,
        });
        record.status = status;
        record.server_pending = false;
        key
    }
    fn derived(&mut self, key: &str, group: &MlsGroup) -> Result<()> {
        let context = group.public_group().group_context();
        self.fresh(key, context)?;
        if self.voice_only {
            let record = self.groups.get_mut(key).ok_or(Invalid("group record"))?;
            record.status = Status::Active;
            record.checkpoint = Some(checkpoint(context)?);
            record.server_pending = false;
            return Ok(());
        }
        let exported = group
            .export_secret(self.provider.crypto(), "gwar chat v1", group.group_id().as_slice(), 32)
            .map_err(err("exporter"))?;
        let epoch = group.epoch().as_u64();
        self.epochs.insert(
            epoch_key(key, epoch),
            EpochRecord::Derived { key: exported.try_into().map_err(err("exporter length"))? },
        );
        let record = self.groups.get_mut(key).ok_or(Invalid("group record"))?;
        for (_, leaf) in group.public_group().treesync().full_leaves() {
            witness(record.evidence.entry(leaf_credential(leaf).expect("validated leaf").uid()).or_default(), epoch);
        }
        record.status = Status::Active;
        record.checkpoint = Some(checkpoint(context)?);
        record.server_pending = false;
        Ok(())
    }
    pub fn create(&mut self, descriptor: SignedDescriptor) -> Result<Vec<u8>> {
        self.operation(|a| {
            let descriptor = SignedDescriptor::parse(&descriptor.encode()).ok_or(Invalid("descriptor"))?;
            if descriptor.creator != a.credential || !a.credential.is_current(descriptor.descriptor.created_at, &()) {
                return Err(Invalid("creator"));
            }
            let d = &descriptor.descriptor;
            if (d.kind == crate::Kind::Voice) != a.voice_only {
                return Err(Invalid("agent store kind"));
            }
            if d.incarnation == 0 {
                a.check_pin(&descriptor)?;
            } else {
                // A restart candidate continues this device's live pin, one per predecessor.
                a.check_incarnation(&descriptor)?;
                if a.pins(&d.server_key, &d.conversation)?
                    .iter()
                    .any(|(_, p, _)| p.descriptor.incarnation == d.incarnation)
                    || !a.live(&d.server_key, &d.conversation)?.is_some_and(|live| successor(&live, &descriptor))
                {
                    return Err(Invalid("restart predecessor"));
                }
            }
            let key = group_key(&descriptor.descriptor.server_key, &descriptor.group_id());
            if a.groups.contains_key(&key) {
                return Err(Invalid("group already known"));
            }
            let extensions = Extensions::from_vec(vec![
                Extension::RequiredCapabilities(RequiredCapabilitiesExtension::new(
                    &[ExtensionType::Unknown(DESCRIPTOR_EXTENSION)],
                    &[],
                    &[CredentialType::Basic],
                )),
                Extension::Unknown(DESCRIPTOR_EXTENSION, UnknownExtension(descriptor.encode())),
                Extension::ExternalSenders(vec![ExternalSender::new(
                    fixed::<32>(&descriptor.descriptor.server_key).ok_or(Invalid("server key"))?.to_vec().into(),
                    BasicCredential::new(server_credential(&descriptor.descriptor.server_key)).into(),
                )]),
            ])
            .map_err(err("extensions"))?;
            let config = MlsGroupCreateConfig::builder()
                .ciphersuite(SUITE)
                .capabilities(capabilities())
                .lifetime(a.lifetime(descriptor.descriptor.created_at)?)
                .use_ratchet_tree_extension(!d.kind.separate_tree())
                .wire_format_policy(PURE_PLAINTEXT_WIRE_FORMAT_POLICY)
                .with_group_context_extensions(extensions)
                .build();
            let group = MlsGroup::new_with_group_id(
                &a.provider,
                &a.signer,
                &config,
                GroupId::from_slice(&descriptor.group_id()),
                a.with_key(),
            )
            .map_err(err("create group"))?;
            client_context(group.public_group().group_context(), &descriptor.descriptor.server_key)?;
            rules::tree(&descriptor, group.public_group().treesync().full_leaves().map(|(_, l)| l))?;
            a.record(&descriptor, Status::Creating);
            wire(group.export_group_info(a.provider.crypto(), &a.signer, true).map_err(err("group info"))?)
        })
    }

    /// Settles a creation with the group the server holds for the
    /// conversation: from the `mls.group.create` reply `{group}` or
    /// `conflict`, or after a restart from `mls.groups` (`None` when it lists
    /// none). Returns whether this device's group is that group; otherwise it
    /// is deleted, with its pin, so the device can join the winner or create
    /// the conversation again.
    pub fn settle(&mut self, server: &str, id: &[u8; 32], winner: Option<&[u8; 32]>) -> Result<bool> {
        self.operation(|a| {
            let key = group_key(server, id);
            if a.groups.get(&key).is_none_or(|r| r.status != Status::Creating) {
                return Err(Invalid("not creating"));
            }
            if winner == Some(id) {
                a.won(server, &key)?;
                return Ok(true);
            }
            // A creating group has no epoch keys, outbox or other members.
            let mut group = a.mls(&key)?;
            group.delete(a.provider.storage()).map_err(err("delete group"))?;
            a.groups.remove(&key);
            Ok(false)
        })
    }

    /// This device's creation is the group the server holds: it is active at
    /// epoch zero, and a won restart is live at once while its predecessor
    /// never is again.
    fn won(&mut self, server: &str, key: &str) -> Result<()> {
        let group = self.mls(key)?;
        self.derived(key, &group)?;
        let record = self.groups.get_mut(key).ok_or(Invalid("record"))?;
        let d = SignedDescriptor::parse(&record.descriptor).ok_or(Invalid("descriptor"))?.descriptor;
        record.own_commit_at = Some(d.created_at);
        self.supersede(server, &d.conversation, d.incarnation)
    }

    pub fn gate(&self, server: &str, id: &[u8; 32], now: i64, revocations: &dyn Revocations) -> Result<Gate> {
        current_time(now)?;
        if self.poisoned {
            return Err(AgentError::Reload);
        }
        let key = group_key(server, id);
        let record = self.groups.get(&key).ok_or(Invalid("unknown group"))?;
        let mut gate = Gate {
            blocked: Vec::new(),
            server_pending: record.server_pending || record.voice_needs_sync || !record.voice_removals.is_empty(),
            pending_commit: self.outbox.contains_key(&key),
            rejoin: false,
            status: record.status,
        };
        if record.status != Status::Active {
            gate.rejoin = matches!(record.status, Status::Removed | Status::Rejoin);
            return Ok(gate);
        }
        let group = self.mls(&key)?;
        for (index, leaf) in group.public_group().treesync().full_leaves() {
            if !leaf_credential(leaf).ok_or(Invalid("credential"))?.is_current(now, revocations) {
                gate.blocked.push(index.u32());
            }
        }
        for p in group.pending_proposals() {
            gate.server_pending = true;
            if let Proposal::Remove(remove) = p.proposal() {
                gate.rejoin |= remove.removed() == group.own_leaf_index();
            }
        }
        gate.rejoin |= gate.blocked.contains(&group.own_leaf_index().u32());
        Ok(gate)
    }
    pub fn sending_key(
        &self,
        server: &str,
        id: &[u8; 32],
        now: i64,
        revocations: &dyn Revocations,
    ) -> Result<(u64, [u8; 32])> {
        if self.voice_only {
            return Err(Invalid("voice has no chat keys"));
        }
        if self.ready {
            return Err(AgentError::Unpersisted);
        }
        if !self.gate(server, id, now, revocations)?.can_send() {
            return Err(AgentError::Blocked);
        }
        let epoch = self.mls(&group_key(server, id))?.epoch().as_u64();
        Ok((epoch, *self.epoch(server, id, epoch).and_then(EpochRecord::key).ok_or(Invalid("epoch key"))?))
    }
    pub fn pending(&mut self, server: &str, id: &[u8; 32], pending: bool) -> Result<()> {
        self.operation(|a| {
            a.groups.get_mut(&group_key(server, id)).ok_or(Invalid("unknown group"))?.server_pending = pending;
            Ok(())
        })
    }
    pub fn proposal(&mut self, server: &str, id: &[u8; 32], bytes: &[u8]) -> Result<()> {
        self.operation(|a| {
            let key = group_key(server, id);
            if a.groups.get(&key).is_none_or(|r| r.status != Status::Active) {
                return Err(AgentError::Blocked);
            }
            let mut group = a.mls(&key)?;
            a.process_proposal(&key, &mut group, bytes)
        })
    }
    fn process_proposal(&mut self, key: &str, group: &mut MlsGroup, bytes: &[u8]) -> Result<()> {
        let processed = group.process_message(&self.provider, protocol(bytes)?).map_err(err("proposal"))?;
        if !matches!(processed.sender(),Sender::External(index) if *index==SenderExtensionIndex::new(0)) {
            return Err(Invalid("proposal sender"));
        }
        let ProcessedMessageContent::ProposalMessage(proposal) = processed.into_content() else {
            return Err(Invalid("not a proposal"));
        };
        if !matches!(proposal.proposal(), Proposal::Remove(_)) {
            return Err(Invalid("external proposal type"));
        }
        let Proposal::Remove(remove) = proposal.proposal() else { unreachable!() };
        let target = remove.removed();
        if self.voice_only {
            if group.public_group().leaf(target).is_none() {
                return Err(Invalid("removal target"));
            }
            self.groups.get_mut(key).ok_or(Invalid("group record"))?.voice_removals.insert(target.u32());
        }
        group.store_pending_proposal(self.provider.storage(), *proposal).map_err(err("store proposal"))?;
        Ok(())
    }
    fn resolve_voice_removals(&mut self, key: &str, staged: &StagedCommit) {
        if let Some(record) = self.groups.get_mut(key) {
            for removal in staged.remove_proposals() {
                record.voice_removals.remove(&removal.remove_proposal().removed().u32());
            }
        }
    }
    fn check_staged(&self, group: &MlsGroup, staged: &StagedCommit, sender: &CommitSender, time: i64) -> Result<()> {
        rules::staged(group.public_group(), staged, sender, time, self.provider.crypto())?;
        for p in staged.queued_proposals() {
            if p.proposal_or_ref_type() == ProposalOrRefType::Reference
                && !matches!(p.sender(),Sender::External(i) if *i==SenderExtensionIndex::new(0))
            {
                return Err(Invalid("proposal reference sender"));
            }
            if matches!(p.sender(), Sender::External(_)) && !matches!(p.proposal(), Proposal::Remove(_)) {
                return Err(Invalid("external proposal type"));
            }
        }
        let descriptor = group
            .public_group()
            .group_context()
            .extensions()
            .unknown(DESCRIPTOR_EXTENSION)
            .and_then(|ext| SignedDescriptor::parse(&ext.0))
            .ok_or(Invalid("descriptor"))?;
        client_context(staged.group_context(), &descriptor.descriptor.server_key)?;
        // The public state of an eviction has no exported staged tree.
        let removed: BTreeSet<_> = staged.remove_proposals().map(|p| p.remove_proposal().removed()).collect();
        let path_index = match sender {
            CommitSender::Member(i) => Some(LeafNodeIndex::new(*i)),
            _ => None,
        };
        let mut leaves: Vec<LeafNode> = group
            .public_group()
            .treesync()
            .full_leaves()
            .filter(|(i, _)| {
                !removed.contains(i) && !(Some(*i) == path_index && staged.update_path_leaf_node().is_some())
            })
            .map(|(_, l)| l.clone())
            .collect();
        leaves.extend(staged.add_proposals().map(|p| p.add_proposal().key_package().leaf_node().clone()));
        leaves.extend(staged.update_path_leaf_node().cloned());
        rules::tree(&descriptor, leaves.iter())?;
        Ok(())
    }

    pub fn commit(
        &mut self,
        server: &str,
        id: &[u8; 32],
        adds: &[Vec<u8>],
        removes: &[u32],
        now: i64,
        revocations: &dyn Revocations,
    ) -> Result<Outbox> {
        self.operation(|a| {
            let key = group_key(server, id);
            let gate = a.gate(server, id, now, revocations)?;
            if gate.rejoin {
                return Err(AgentError::Rejoin);
            }
            if gate.status != Status::Active || a.outbox.contains_key(&key) {
                return Err(AgentError::Blocked);
            }
            if !a.credential.is_current(now, revocations) {
                return Err(AgentError::Blocked);
            }
            if a.groups[&key].server_pending {
                return Err(Invalid("fetch outstanding proposals first"));
            }
            let mut group = a.mls(&key)?;
            let descriptor = client_context(group.public_group().group_context(), server)?;
            let kind = descriptor.descriptor.kind;
            if kind.separate_tree() && !adds.is_empty() {
                return Err(Invalid("separate-tree groups join externally"));
            }
            group.set_configuration(a.provider.storage(), &join_config(kind)).map_err(err("group config"))?;
            let packages: Vec<KeyPackage> = adds
                .iter()
                .map(|bytes| {
                    if bytes.len() > 4096 {
                        return Err(Invalid("key package size"));
                    }
                    let package = KeyPackageIn::tls_deserialize_exact(bytes)
                        .map_err(err("key package encoding"))?
                        .validate(a.provider.crypto(), ProtocolVersion::Mls10)
                        .map_err(err("key package"))?;
                    let c = leaf_credential(package.leaf_node()).ok_or(Invalid("credential"))?;
                    if package.ciphersuite() != SUITE
                        || !c.is_current(now, revocations)
                        || package.life_time().not_after().saturating_mul(1000) < (now as u64).saturating_add(600_000)
                    {
                        return Err(Invalid("add not current"));
                    }
                    Ok(package)
                })
                .collect::<Result<_>>()?;
            let mut removals: BTreeSet<u32> = removes.iter().copied().collect();
            removals.extend(gate.blocked);
            group.set_aad(ds::commit_aad(now));
            let bundle = group
                .commit_builder()
                .propose_adds(packages)
                .propose_removals(removals.into_iter().map(LeafNodeIndex::new))
                .force_self_update(true)
                .leaf_node_parameters(LeafNodeParameters::builder().with_credential_with_key(a.with_key()).build())
                .load_psks(a.provider.storage())
                .map_err(err("commit PSKs"))?
                .create_group_info(true)
                .build(a.provider.rand(), a.provider.crypto(), &a.signer, |_| true)
                .map_err(err("build commit"))?
                .stage_commit(&a.provider)
                .map_err(err("stage commit"))?;
            a.check_staged(
                &group,
                group.pending_commit().ok_or(Invalid("pending commit"))?,
                &CommitSender::Member(group.own_leaf_index().u32()),
                now,
            )?;
            let out = a.bundle(id, group.epoch().as_u64(), bundle, now)?;
            a.outbox.insert(key, out.clone());
            Ok(out)
        })
    }
    fn bundle(&self, id: &[u8; 32], epoch: u64, bundle: CommitMessageBundle, now: i64) -> Result<Outbox> {
        let (commit, welcome, info) = bundle.into_messages();
        let commit = wire(commit)?;
        Ok(Outbox {
            group: *id,
            epoch,
            hash: hash(&commit),
            commit,
            welcome: welcome.map(wire).transpose()?,
            group_info: wire(info.ok_or(Invalid("missing group info"))?)?,
            committed_at: now,
        })
    }
    pub fn receive(&mut self, server: &str, id: &[u8; 32], commit: &[u8], proposals: &[Vec<u8>]) -> Result<()> {
        self.operation(|a| {
            let key = group_key(server, id);
            if a.outbox.contains_key(&key) {
                return Err(Invalid("reconcile pending commit first"));
            }
            a.receive_inner(&key, commit, proposals)
        })
    }
    fn receive_inner(&mut self, key: &str, commit: &[u8], proposals: &[Vec<u8>]) -> Result<()> {
        if self.groups.get(key).is_none_or(|r| r.status != Status::Active) {
            return Err(AgentError::Blocked);
        }
        let mut group = self.mls(key)?;
        for p in proposals {
            self.process_proposal(key, &mut group, p)?;
        }
        let processed = group.process_message(&self.provider, protocol(commit)?).map_err(err("commit"))?;
        let time = ds::commit_time(processed.aad()).ok_or(Invalid("commit time"))?;
        let sender = match processed.sender() {
            Sender::Member(i) => CommitSender::Member(i.u32()),
            Sender::NewMemberCommit => CommitSender::NewMember,
            _ => return Err(Invalid("commit sender")),
        };
        let ProcessedMessageContent::StagedCommitMessage(staged) = processed.into_content() else {
            return Err(Invalid("not a commit"));
        };
        self.check_staged(&group, &staged, &sender, time)?;
        self.resolve_voice_removals(key, &staged);
        let removed = staged.self_removed();
        if removed {
            // Validation is complete. Delete before advancing: OpenMLS's delete
            // removes only the current epoch's key pairs, and an eviction merge
            // advances the public epoch without deleting the old private keys.
            group.delete(self.provider.storage()).map_err(err("delete group"))?;
            self.groups.get_mut(key).ok_or(Invalid("record"))?.status = Status::Removed;
        } else {
            self.fresh(key, staged.group_context())?;
            group.merge_staged_commit(&self.provider, *staged).map_err(err("merge commit"))?;
            self.derived(key, &group)?;
        }
        Ok(())
    }
    pub fn reconcile(&mut self, server: &str, id: &[u8; 32], result: Reconcile) -> Result<Resolution> {
        self.operation(|a| {
            let key = group_key(server, id);
            let out = a.outbox.get(&key).ok_or(Invalid("no outbox"))?.clone();
            let joining = a.groups[&key].status == Status::Joining;
            match result {
                Reconcile::Entry { epoch, commit, proposals } => {
                    if epoch != out.epoch {
                        return Err(Invalid("log epoch"));
                    }
                    if hash(&commit) == out.hash {
                        let mut group = a.mls(&key)?;
                        if !joining {
                            a.resolve_voice_removals(&key, group.pending_commit().ok_or(Invalid("pending commit"))?);
                            group.merge_pending_commit(&a.provider).map_err(err("merge pending"))?;
                        }
                        a.derived(&key, &group)?;
                        a.outbox.remove(&key);
                        a.groups.get_mut(&key).ok_or(Invalid("record"))?.own_commit_at = Some(out.committed_at);
                        Ok(Resolution::Accepted)
                    } else if joining {
                        a.drop_mls(&key)?;
                        Ok(Resolution::Rejoin)
                    } else {
                        let mut group = a.mls(&key)?;
                        group.clear_pending_commit(a.provider.storage()).map_err(err("clear pending"))?;
                        a.outbox.remove(&key);
                        a.receive_inner(&key, &commit, &proposals)?;
                        Ok(if a.groups[&key].status == Status::Removed {
                            Resolution::Rejoin
                        } else {
                            Resolution::Rebuild
                        })
                    }
                }
                Reconcile::Head { epoch } => {
                    if epoch != out.epoch {
                        return Err(Invalid("log head"));
                    }
                    Ok(Resolution::Retry)
                }
                Reconcile::Pruned { oldest } => {
                    if oldest <= out.epoch {
                        return Err(Invalid("log oldest"));
                    }
                    a.drop_mls(&key)?;
                    Ok(Resolution::Rejoin)
                }
                Reconcile::Rejected { reason: Rejection::Expired } => {
                    if joining {
                        a.drop_mls(&key)?;
                        Ok(Resolution::Rejoin)
                    } else {
                        let mut group = a.mls(&key)?;
                        group.clear_pending_commit(a.provider.storage()).map_err(err("clear pending"))?;
                        a.outbox.remove(&key);
                        Ok(Resolution::Rebuild)
                    }
                }
                Reconcile::Rejected { .. } => Ok(Resolution::Retry),
            }
        })
    }
    /// Deletes everything this device holds for a group whose conversation
    /// no longer exists (a deleted channel): its MLS state with every epoch's
    /// keys, its outbox, epoch keys, evidence and pin.
    pub fn forget(&mut self, server: &str, id: &[u8; 32]) -> Result<()> {
        self.operation(|a| {
            let key = group_key(server, id);
            if !a.groups.contains_key(&key) {
                return Ok(());
            }
            let group_id = GroupId::from_slice(id);
            if let Some(mut group) = MlsGroup::load(a.provider.storage(), &group_id).map_err(err("load group"))? {
                group.delete(a.provider.storage()).map_err(err("delete group"))?;
            }
            a.provider.storage().purge_epoch_key_pairs(&group_id).map_err(err("delete keys"))?;
            a.outbox.remove(&key);
            a.epochs.retain(|k, _| !k.starts_with(&format!("{key}/")));
            a.groups.remove(&key);
            Ok(())
        })
    }
    /// Accepts a conversation's restarts (`mls.group_history`): each group
    /// with its encoded `SignedDescriptor`, by incarnation, starting at this
    /// device's live pin, or at incarnation zero without one. Every link must
    /// verify (signature, group id, server, conversation, consecutive
    /// incarnations naming their predecessor, unchanged fields, a creator
    /// certified at its creation time). New incarnations are pinned, the last
    /// becomes live, to be joined, and the earlier ones are superseded, all in
    /// this one operation. Returns the live group.
    pub fn accept_chain(
        &mut self,
        server: &str,
        conversation: &str,
        chain: &[([u8; 32], Vec<u8>)],
    ) -> Result<[u8; 32]> {
        self.operation(|a| {
            if a.voice_only || conversation.starts_with("voice:") {
                return Err(Invalid("voice has no incarnation chain"));
            }
            let chain = chain
                .iter()
                .map(|(group, bytes)| {
                    let d = SignedDescriptor::parse(bytes).ok_or(Invalid("chain descriptor"))?;
                    let x = &d.descriptor;
                    if d.group_id() != *group
                        || x.server_key != server
                        || x.conversation != conversation
                        || !d.creator.is_current(x.created_at, &())
                    {
                        return Err(Invalid("chain descriptor"));
                    }
                    Ok(d)
                })
                .collect::<Result<Vec<_>>>()?;
            let first = chain.first().ok_or(Invalid("chain anchor"))?;
            let anchored = match a.live(server, conversation)? {
                Some(live) => live.group_id() == first.group_id(),
                None => first.descriptor.incarnation == 0,
            };
            if !anchored {
                return Err(Invalid("chain anchor"));
            }
            if chain.windows(2).any(|w| !successor(&w[0], &w[1])) {
                return Err(Invalid("chain link"));
            }
            for d in &chain {
                a.check_incarnation(d)?;
                a.groups.entry(group_key(server, &d.group_id())).or_insert_with(|| GroupRecord {
                    descriptor: d.encode(),
                    status: Status::Rejoin,
                    checkpoint: None,
                    welcome_ids: BTreeSet::new(),
                    server_pending: false,
                    voice_removals: BTreeSet::new(),
                    voice_needs_sync: false,
                    evidence: BTreeMap::new(),
                    own_commit_at: None,
                });
            }
            let live = chain.last().expect("checked above");
            let key = group_key(server, &live.group_id());
            if a.groups[&key].status == Status::Creating {
                // This device's own creation, which the server holds: it won.
                a.won(server, &key)?;
            }
            // Earlier links, including a creation of this device the chain
            // continued past, are never live again.
            a.supersede(server, conversation, live.descriptor.incarnation)?;
            Ok(live.group_id())
        })
    }
    /// Whether a GroupInfo of `id`, verified as an external joiner would
    /// (tree, a member's signature, Gwar's rules; leaf lifetimes are skipped,
    /// as when joining, since an older GroupInfo may hold expired packages),
    /// is behind this device's checkpoint of that group: an earlier epoch, or
    /// another GroupContext for the same epoch. One of the observations that
    /// trigger a restart. `tree` is TLS-encoded; DMs ignore it and require an embedded tree.
    pub fn behind(&self, server: &str, id: &[u8; 32], bytes: &[u8], tree: Option<&[u8]>) -> Result<bool> {
        let info = info(bytes)?;
        let descriptor = client_context(info.group_context(), server)?;
        if descriptor.group_id() != *id {
            return Err(Invalid("group info group"));
        }
        let tree_in = info_tree(&info, descriptor.descriptor.kind, tree)?;
        let tree = tree_in
            .clone()
            .into_verified(SUITE, self.provider.crypto(), info.group_id())
            .map_err(err("ratchet tree"))?;
        rules::tree(&descriptor, tree.leaves())?;
        rules::voice_initial_tree(&descriptor, info.epoch().as_u64(), tree.leaves())?;
        let next = checkpoint(info.group_context())?;
        // Verification only: a throwaway provider, and nothing is committed.
        MlsGroup::external_commit_builder()
            .with_ratchet_tree(tree_in)
            .skip_lifetime_validation()
            .build_group(&Provider::default(), info, self.with_key())
            .map_err(err("group info"))?;
        let Some(old) = self.groups.get(&group_key(server, id)).and_then(|r| r.checkpoint.as_ref()) else {
            return Ok(false);
        };
        Ok(next.epoch < old.epoch || next.epoch == old.epoch && next.hash != old.hash)
    }
    fn drop_mls(&mut self, key: &str) -> Result<()> {
        self.mls(key)?.delete(self.provider.storage()).map_err(err("delete group"))?;
        self.outbox.remove(key);
        self.groups.get_mut(key).ok_or(Invalid("record"))?.status = Status::Rejoin;
        Ok(())
    }
    /// Joins using the embedded tree, or a separate TLS tree for channels/server chat.
    /// DMs ignore `tree` and require the embedded tree.
    pub fn join_external(
        &mut self,
        server: &str,
        bytes: &[u8],
        tree: Option<&[u8]>,
        now: i64,
        revocations: &dyn Revocations,
    ) -> Result<Outbox> {
        self.operation(|a| {
            current_time(now)?;
            let info = info(bytes)?;
            let descriptor = client_context(info.group_context(), server)?;
            if (descriptor.descriptor.kind == crate::Kind::Voice) != a.voice_only {
                return Err(Invalid("agent store kind"));
            }
            a.check_pin(&descriptor)?;
            let id = descriptor.group_id();
            let key = group_key(server, &id);
            if a.groups.get(&key).is_some_and(|r| r.status == Status::Creating) {
                return Err(AgentError::Blocked);
            }
            a.fresh(&key, info.group_context())?;
            if !a.credential.is_current(now, revocations) {
                return Err(AgentError::Blocked);
            }
            // A signed newer GroupInfo proves the source epoch is already
            // consumed. This permits recovery when its winning log entry can
            // no longer be replayed (e.g. expired Add packages). Equal/older
            // heads must retain the pending bytes for normal reconciliation.
            if a.outbox.get(&key).is_some_and(|out| info.epoch().as_u64() <= out.epoch) {
                return Err(Invalid("pending commit"));
            }
            if a.groups.get(&key).is_some_and(|r| {
                r.status == Status::Removed && r.checkpoint.as_ref().is_some_and(|c| info.epoch().as_u64() <= c.epoch)
            }) {
                return Err(Invalid("removed checkpoint"));
            }
            let old_checkpoint = checkpoint(info.group_context())?;
            let tree_in = info_tree(&info, descriptor.descriptor.kind, tree)?;
            let old_tree = tree_in
                .clone()
                .into_verified(SUITE, a.provider.crypto(), info.group_id())
                .map_err(err("ratchet tree"))?;
            rules::tree(&descriptor, old_tree.leaves())?;
            rules::voice_initial_tree(&descriptor, info.epoch().as_u64(), old_tree.leaves())?;
            if old_tree
                .leaves()
                .filter_map(leaf_credential)
                .any(|c| c.device_key() == a.credential.device_key() && c.uid() != a.credential.uid())
            {
                return Err(Invalid("external replacement identity"));
            }

            if a.groups.get(&key).is_some_and(|r| matches!(r.status, Status::Active | Status::Joining)) {
                a.drop_mls(&key)?;
            }
            let (mut group, bundle) = MlsGroup::external_commit_builder()
                .with_config(join_config(descriptor.descriptor.kind))
                .with_ratchet_tree(tree_in)
                .with_aad(ds::commit_aad(now))
                .skip_lifetime_validation()
                .build_group(&a.provider, info, a.with_key())
                .map_err(err("external group"))?
                .leaf_node_parameters(LeafNodeParameters::builder().with_capabilities(capabilities()).build())
                .load_psks(a.provider.storage())
                .map_err(err("external PSKs"))?
                .use_ratchet_tree_extension(!descriptor.descriptor.kind.separate_tree())
                .create_group_info(true)
                .build(a.provider.rand(), a.provider.crypto(), &a.signer, |_| true)
                .map_err(err("external commit"))?
                .finalize(&a.provider)
                .map_err(err("external finalize"))?;
            client_context(group.public_group().group_context(), server)?;
            rules::tree(&descriptor, group.public_group().treesync().full_leaves().map(|(_, l)| l))?;
            group
                .set_configuration(a.provider.storage(), &join_config(descriptor.descriptor.kind))
                .map_err(err("group config"))?;
            a.record(&descriptor, Status::Joining);
            let record = a.groups.get_mut(&key).ok_or(Invalid("record"))?;
            record.checkpoint = Some(old_checkpoint.clone());
            if a.voice_only {
                // A snapshot cannot distinguish missed removal/rejoin from a
                // self-update. Start a fresh membership trust interval, but do
                // not export until its complete current removal state is known.
                record.voice_removals.clear();
                record.voice_needs_sync = true;
            }
            let out = a.bundle(&id, old_checkpoint.epoch, bundle, now)?;
            a.outbox.insert(key, out.clone());
            Ok(out)
        })
    }
}

impl Agent {
    pub fn share_history(
        &mut self,
        server: &str,
        id: &[u8; 32],
        recipient: &DeviceHpkeKey,
        now: i64,
        revocations: &dyn Revocations,
    ) -> Result<Vec<Vec<u8>>> {
        if self.voice_only {
            return Err(Invalid("voice has no history"));
        }
        self.operation(|a| {
            if !a.gate(server, id, now, revocations)?.can_send() {
                return Err(AgentError::Blocked);
            }
            if !recipient.verify() {
                return Err(Invalid("recipient HPKE statement"));
            }
            let key = group_key(server, id);
            let group = a.mls(&key)?;
            let member = group
                .public_group()
                .treesync()
                .full_leaves()
                .filter_map(|(_, l)| leaf_credential(l))
                .find(|c| c.device_key() == recipient.device_key)
                .ok_or(Invalid("recipient is not a member"))?;
            let record = &a.groups[&key];
            let descriptor = SignedDescriptor::parse(&record.descriptor).ok_or(Invalid("descriptor"))?;
            let evidence = record.evidence.get(&member.uid()).map(Vec::as_slice).unwrap_or_default();
            let prefix = format!("{key}/");
            let entries: Vec<(u64, [u8; 32])> = a
                .epochs
                .iter()
                .filter_map(|(k, v)| {
                    let epoch = k.strip_prefix(&prefix)?.parse().ok()?;
                    if !descriptor.descriptor.history
                        && !(matches!(v, EpochRecord::Derived { .. }) && witnessed(evidence, epoch))
                    {
                        return None;
                    }
                    Some((epoch, *v.key()?))
                })
                .collect();
            let mut chunks = Vec::new();
            let mut rest = entries.as_slice();
            while let Some((first, _)) = rest.first() {
                let n = rest
                    .iter()
                    .take_while(|(e, _)| e.checked_sub(*first).is_some_and(|span| span < history::MAX_CHUNK_EPOCHS))
                    .count();
                let entries = &rest[..n];
                chunks.push(
                    Chunk::seal(
                        Header {
                            server_key: server.into(),
                            group_id: *id,
                            recipient_device_key: recipient.device_key.clone(),
                            recipient_hpke_key: recipient.hpke_key.clone(),
                            sender_device_key: a.credential.device_key().into(),
                            first_epoch: *first,
                            last_epoch: entries[n - 1].0,
                            created_at: now,
                        },
                        entries,
                        &a.signer.0,
                    )
                    .map_err(err("history seal"))?,
                );
                rest = &rest[n..];
            }
            Ok(chunks)
        })
    }
    pub fn import_history(&mut self, server: &str, id: &[u8; 32], bytes: &[u8]) -> Result<()> {
        if self.voice_only {
            return Err(Invalid("voice has no history"));
        }
        self.operation(|a| {
            let key = group_key(server, id);
            if a.groups.get(&key).is_none_or(|r| r.status != Status::Active) {
                return Err(AgentError::Blocked);
            }
            let chunk = Chunk::parse(bytes).ok_or(Invalid("history chunk"))?;
            if !chunk.is_for(server, id, a.credential.device_key(), &b64(&a.device.hpke_public)) || !chunk.verify() {
                return Err(Invalid("history recipient or signature"));
            }
            let group = a.mls(&key)?;
            let sender = group
                .public_group()
                .treesync()
                .full_leaves()
                .filter_map(|(_, l)| leaf_credential(l))
                .find(|c| c.device_key() == chunk.header.sender_device_key)
                .ok_or(Invalid("history sender is not a member"))?;
            for (epoch, value) in chunk.open(&a.device.hpke_private).map_err(err("history open"))? {
                let record = a
                    .epochs
                    .entry(epoch_key(&key, epoch))
                    .or_insert_with(|| EpochRecord::Imported { key: value, sender_uids: BTreeSet::new() });
                if let EpochRecord::Imported { key, sender_uids } = record {
                    if *key != value {
                        *record = EpochRecord::Conflicted;
                    } else {
                        sender_uids.insert(sender.uid());
                    }
                }
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WelcomeResult {
    pub group: [u8; 32],
    pub duplicate: bool,
}

impl Agent {
    /// Persist before acknowledging. A duplicate never consumes its key package again.
    pub fn welcome(&mut self, server: &str, id: &str, bytes: &[u8]) -> Result<WelcomeResult> {
        if self.voice_only {
            return Err(Invalid("voice has no Welcome"));
        }
        self.accept_welcome(server, id, bytes, None)
    }
    /// Recover membership whose log cannot be replayed. The caller establishes
    /// the log gap; the Welcome must authenticate this group at a newer epoch.
    /// Persist the entire result before acknowledging, as for `welcome`.
    pub fn replace_stale_welcome(
        &mut self,
        server: &str,
        group: &[u8; 32],
        id: &str,
        bytes: &[u8],
    ) -> Result<WelcomeResult> {
        self.accept_welcome(server, id, bytes, Some(group))
    }
    fn accept_welcome(
        &mut self,
        server: &str,
        id: &str,
        bytes: &[u8],
        replace: Option<&[u8; 32]>,
    ) -> Result<WelcomeResult> {
        if self.voice_only {
            return Err(Invalid("voice has no Welcome"));
        }
        self.operation(|a| {
            if id.is_empty() {
                return Err(Invalid("welcome id"));
            }
            for (key, record) in &a.groups {
                if key.starts_with(&format!("{server}/")) && record.welcome_ids.contains(id) {
                    let descriptor = SignedDescriptor::parse(&record.descriptor).ok_or(Invalid("descriptor"))?;
                    if replace.is_some_and(|group| *group != descriptor.group_id()) {
                        return Err(Invalid("welcome replacement group"));
                    }
                    return Ok(WelcomeResult { group: descriptor.group_id(), duplicate: true });
                }
            }
            handshake_size(bytes)?;
            let MlsMessageBodyIn::Welcome(welcome) =
                MlsMessageIn::tls_deserialize_exact(bytes).map_err(err("welcome encoding"))?.extract()
            else {
                return Err(Invalid("not a welcome"));
            };
            if welcome.ciphersuite() != SUITE {
                return Err(Invalid("ciphersuite"));
            }
            let builder = StagedWelcome::build_from_welcome(&a.provider, &join_config(crate::Kind::Dm), welcome)
                .map_err(err("welcome"))?
                .skip_lifetime_validation();
            let builder = if replace.is_some() { builder.replace_old_group() } else { builder };
            let staged = builder.build().map_err(err("staged welcome"))?;
            let descriptor = client_context(staged.group_context(), server)?;
            if descriptor.descriptor.kind.separate_tree() {
                return Err(Invalid("separate-tree groups join externally"));
            }
            a.check_pin(&descriptor)?;
            let key = group_key(server, &descriptor.group_id());
            a.fresh(&key, staged.group_context())?;
            if let Some(expected) = replace {
                if *expected != descriptor.group_id() {
                    return Err(Invalid("welcome replacement group"));
                }
                let old = a.groups.get(&key).ok_or(Invalid("welcome replacement membership"))?;
                if !matches!(old.status, Status::Active | Status::Joining)
                    || old.checkpoint.as_ref().is_none_or(|c| staged.group_context().epoch().as_u64() <= c.epoch)
                    || a.outbox.get(&key).is_some_and(|out| staged.group_context().epoch().as_u64() <= out.epoch)
                {
                    return Err(Invalid("welcome replacement epoch or status"));
                }
            } else if a
                .groups
                .get(&key)
                .is_some_and(|r| matches!(r.status, Status::Creating | Status::Active | Status::Joining))
            {
                return Err(Invalid("group already joined"));
            }
            if a.groups.get(&key).is_some_and(|r| {
                r.status == Status::Removed
                    && r.checkpoint.as_ref().is_some_and(|c| staged.group_context().epoch().as_u64() <= c.epoch)
            }) {
                return Err(Invalid("removed checkpoint"));
            }
            if !staged
                .own_leaf_node()
                .and_then(leaf_credential)
                .is_some_and(|c| c.device_key() == a.credential.device_key() && c.uid() == a.credential.uid())
            {
                return Err(Invalid("welcome identity"));
            }
            if replace.is_some() {
                a.drop_mls(&key)?;
            }
            let group = staged.into_group(&a.provider).map_err(err("welcome group"))?;
            rules::tree(&descriptor, group.public_group().treesync().full_leaves().map(|(_, l)| l))?;
            a.record(&descriptor, Status::Active);
            a.derived(&key, &group)?;
            let record = a.groups.get_mut(&key).ok_or(Invalid("record"))?;
            record.welcome_ids.insert(id.into());
            // The leaf came from a key package: no own commit made it, also on readmission.
            record.own_commit_at = None;
            Ok(WelcomeResult { group: descriptor.group_id(), duplicate: false })
        })
    }
}
