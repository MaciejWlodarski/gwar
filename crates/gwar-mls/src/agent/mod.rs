//! One device's MLS state. Persist `take_changes()` atomically after every
//! operation, before sending its output. On any error, discard and reload.

pub mod epoch_number;
mod storage;
pub use storage::{Change, Changes, Store, Stores};

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
fn join_config() -> MlsGroupJoinConfig {
    MlsGroupJoinConfig::builder()
        .use_ratchet_tree_extension(true)
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
    Active,
    Joining,
    Removed,
    Rejoin,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupRecord {
    pub descriptor: Vec<u8>,
    pub status: Status,
    pub checkpoint: Option<Checkpoint>,
    pub welcome_ids: BTreeSet<String>,
    pub server_pending: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum EpochRecord {
    Derived { key: [u8; 32], uids: BTreeSet<String> },
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
}

impl Agent {
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
            if *key != group_key(&descriptor.descriptor.server_key, &descriptor.group_id()) {
                return Err(Invalid("group key"));
            }
            if matches!(record.status, Status::Active | Status::Joining) {
                let group = agent.mls(key)?;
                client_context(group.public_group().group_context(), &descriptor.descriptor.server_key)?;
                rules::tree(&descriptor, group.public_group().treesync().full_leaves().map(|(_, l)| l))?;
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
        self.operation(|a| {
            if n > 20 {
                return Err(Invalid("key package count"));
            }
            (0..n).map(|_| a.package(now, false)).collect()
        })
    }
    pub fn last_resort_key_package(&mut self, now: i64) -> Result<Vec<u8>> {
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

    fn check_pin(&self, descriptor: &SignedDescriptor) -> Result<()> {
        let d = &descriptor.descriptor;
        for record in self.groups.values() {
            let old = SignedDescriptor::parse(&record.descriptor).ok_or(Invalid("descriptor"))?;
            let o = &old.descriptor;
            if (o.server_key.as_str(), o.conversation.as_str(), o.incarnation)
                == (d.server_key.as_str(), d.conversation.as_str(), d.incarnation)
                && old.group_id() != descriptor.group_id()
            {
                return Err(Invalid("group pin"));
            }
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
        });
        record.status = status;
        record.server_pending = false;
        key
    }
    fn derived(&mut self, key: &str, group: &MlsGroup) -> Result<()> {
        let context = group.public_group().group_context();
        self.fresh(key, context)?;
        let exported = group
            .export_secret(self.provider.crypto(), "gwar chat v1", group.group_id().as_slice(), 32)
            .map_err(err("exporter"))?;
        let uids = group
            .public_group()
            .treesync()
            .full_leaves()
            .map(|(_, l)| leaf_credential(l).expect("validated leaf").uid())
            .collect();
        self.epochs.insert(
            epoch_key(key, group.epoch().as_u64()),
            EpochRecord::Derived { key: exported.try_into().map_err(err("exporter length"))?, uids },
        );
        let record = self.groups.get_mut(key).ok_or(Invalid("group record"))?;
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
            a.check_pin(&descriptor)?;
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
                .use_ratchet_tree_extension(true)
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
            a.record(&descriptor, Status::Active);
            a.derived(&key, &group)?;
            wire(group.export_group_info(a.provider.crypto(), &a.signer, true).map_err(err("group info"))?)
        })
    }

    /// Discard this device's losing creation before joining the server's winner.
    pub fn abandon(&mut self, server: &str, id: &[u8; 32]) -> Result<()> {
        self.operation(|a| {
            let key = group_key(server, id);
            let record = a.groups.get(&key).ok_or(Invalid("unknown group"))?;
            let descriptor = SignedDescriptor::parse(&record.descriptor).ok_or(Invalid("descriptor"))?;
            let mut group = a.mls(&key)?;
            if record.status != Status::Active
                || descriptor.creator.device_key() != a.credential.device_key()
                || descriptor.creator.uid() != a.credential.uid()
                || group.epoch().as_u64() != 0
                || a.outbox.contains_key(&key)
            {
                return Err(Invalid("cannot abandon"));
            }
            group.delete(a.provider.storage()).map_err(err("delete group"))?;
            a.groups.remove(&key);
            let prefix = format!("{key}/");
            let epochs: Vec<_> = a.epochs.keys().filter(|k| k.starts_with(&prefix)).cloned().collect();
            for epoch in epochs {
                a.epochs.remove(&epoch);
            }
            Ok(())
        })
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
            server_pending: record.server_pending,
            pending_commit: self.outbox.contains_key(&key),
            rejoin: false,
            status: record.status,
        };
        if record.status != Status::Active {
            gate.rejoin = record.status != Status::Joining;
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
            let mut group = a.mls(&group_key(server, id))?;
            a.process_proposal(&mut group, bytes)
        })
    }
    fn process_proposal(&self, group: &mut MlsGroup, bytes: &[u8]) -> Result<()> {
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
        group.store_pending_proposal(self.provider.storage(), *proposal).map_err(err("store proposal"))
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
            let out = a.bundle(id, group.epoch().as_u64(), bundle)?;
            a.outbox.insert(key, out.clone());
            Ok(out)
        })
    }
    fn bundle(&self, id: &[u8; 32], epoch: u64, bundle: CommitMessageBundle) -> Result<Outbox> {
        let (commit, welcome, info) = bundle.into_messages();
        let commit = wire(commit)?;
        Ok(Outbox {
            group: *id,
            epoch,
            hash: hash(&commit),
            commit,
            welcome: welcome.map(wire).transpose()?,
            group_info: wire(info.ok_or(Invalid("missing group info"))?)?,
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
        let mut group = self.mls(key)?;
        for p in proposals {
            self.process_proposal(&mut group, p)?;
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
        let removed = staged.self_removed();
        if !removed {
            self.fresh(key, staged.group_context())?;
        }
        group.merge_staged_commit(&self.provider, *staged).map_err(err("merge commit"))?;
        if removed {
            group.delete(self.provider.storage()).map_err(err("delete group"))?;
            self.groups.get_mut(key).ok_or(Invalid("record"))?.status = Status::Removed;
        } else {
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
                            group.merge_pending_commit(&a.provider).map_err(err("merge pending"))?;
                        }
                        a.derived(&key, &group)?;
                        a.outbox.remove(&key);
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
    fn drop_mls(&mut self, key: &str) -> Result<()> {
        self.mls(key)?.delete(self.provider.storage()).map_err(err("delete group"))?;
        self.outbox.remove(key);
        self.groups.get_mut(key).ok_or(Invalid("record"))?.status = Status::Rejoin;
        Ok(())
    }
    pub fn join_external(
        &mut self,
        server: &str,
        bytes: &[u8],
        now: i64,
        revocations: &dyn Revocations,
    ) -> Result<Outbox> {
        self.operation(|a| {
            current_time(now)?;
            let info = info(bytes)?;
            let descriptor = client_context(info.group_context(), server)?;
            a.check_pin(&descriptor)?;
            let id = descriptor.group_id();
            let key = group_key(server, &id);
            a.fresh(&key, info.group_context())?;
            if !a.credential.is_current(now, revocations) {
                return Err(AgentError::Blocked);
            }
            if a.outbox.contains_key(&key) {
                return Err(Invalid("pending commit"));
            }
            if a.groups.get(&key).is_some_and(|r| {
                r.status == Status::Removed && r.checkpoint.as_ref().is_some_and(|c| info.epoch().as_u64() <= c.epoch)
            }) {
                return Err(Invalid("removed checkpoint"));
            }
            let old_checkpoint = checkpoint(info.group_context())?;
            let old_tree = info
                .extensions()
                .ratchet_tree()
                .ok_or(Invalid("no ratchet tree"))?
                .ratchet_tree()
                .clone()
                .into_verified(SUITE, a.provider.crypto(), info.group_id())
                .map_err(err("ratchet tree"))?;
            rules::tree(&descriptor, old_tree.leaves())?;
            if old_tree
                .leaves()
                .filter_map(leaf_credential)
                .any(|c| c.device_key() == a.credential.device_key() && c.uid() != a.credential.uid())
            {
                return Err(Invalid("external replacement identity"));
            }

            if a.groups.get(&key).is_some_and(|r| r.status == Status::Active) {
                a.drop_mls(&key)?;
            }
            let (group, bundle) = MlsGroup::external_commit_builder()
                .with_config(join_config())
                .with_aad(ds::commit_aad(now))
                .skip_lifetime_validation()
                .build_group(&a.provider, info, a.with_key())
                .map_err(err("external group"))?
                .leaf_node_parameters(LeafNodeParameters::builder().with_capabilities(capabilities()).build())
                .load_psks(a.provider.storage())
                .map_err(err("external PSKs"))?
                .build(a.provider.rand(), a.provider.crypto(), &a.signer, |_| true)
                .map_err(err("external commit"))?
                .finalize(&a.provider)
                .map_err(err("external finalize"))?;
            client_context(group.public_group().group_context(), server)?;
            rules::tree(&descriptor, group.public_group().treesync().full_leaves().map(|(_, l)| l))?;
            a.record(&descriptor, Status::Joining);
            a.groups.get_mut(&key).ok_or(Invalid("record"))?.checkpoint = Some(old_checkpoint.clone());
            let out = a.bundle(&id, old_checkpoint.epoch, bundle)?;
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
            let descriptor = SignedDescriptor::parse(&a.groups[&key].descriptor).ok_or(Invalid("descriptor"))?;
            let prefix = format!("{key}/");
            let entries: Vec<(u64, [u8; 32])> = a
                .epochs
                .iter()
                .filter_map(|(k, v)| {
                    let epoch = k.strip_prefix(&prefix)?.parse().ok()?;
                    if !descriptor.descriptor.history
                        && !matches!(v,EpochRecord::Derived{uids,..} if uids.contains(&member.uid()))
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
        self.operation(|a| {
            if id.is_empty() {
                return Err(Invalid("welcome id"));
            }
            for (key, record) in &a.groups {
                if key.starts_with(&format!("{server}/")) && record.welcome_ids.contains(id) {
                    let descriptor = SignedDescriptor::parse(&record.descriptor).ok_or(Invalid("descriptor"))?;
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
            let staged = StagedWelcome::build_from_welcome(&a.provider, &join_config(), welcome)
                .map_err(err("welcome"))?
                .skip_lifetime_validation()
                .build()
                .map_err(err("staged welcome"))?;
            let descriptor = client_context(staged.group_context(), server)?;
            a.check_pin(&descriptor)?;
            let key = group_key(server, &descriptor.group_id());
            a.fresh(&key, staged.group_context())?;
            if a.groups.get(&key).is_some_and(|r| matches!(r.status, Status::Active | Status::Joining)) {
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
            let group = staged.into_group(&a.provider).map_err(err("welcome group"))?;
            rules::tree(&descriptor, group.public_group().treesync().full_leaves().map(|(_, l)| l))?;
            a.record(&descriptor, Status::Active);
            a.derived(&key, &group)?;
            a.groups.get_mut(&key).ok_or(Invalid("record"))?.welcome_ids.insert(id.into());
            Ok(WelcomeResult { group: descriptor.group_id(), duplicate: false })
        })
    }
}
