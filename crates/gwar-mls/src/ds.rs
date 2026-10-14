//! What the server checks: key packages, and every group followed as an OpenMLS
//! `PublicGroup` (docs/e2e-build.md, "What the server uses").
//!
//! The observer holds no secret. It verifies each `PublicMessage` handshake,
//! merges commits, and applies Gwar's structural rules; certificate currency
//! (expiry, revocation) is left to clients.

use std::collections::BTreeSet;

use openmls::prelude::{
    tls_codec::{Deserialize as _, Serialize as _},
    *,
};
use openmls_rust_crypto::{MemoryStorage, RustCrypto};
use vc_proto::Uid;

use crate::{
    DESCRIPTOR_EXTENSION, SUITE,
    credential::Credential,
    descriptor::{Kind, SignedDescriptor, dm_participants},
    encoding::{b64, fixed},
    signer::{Ed25519Signer, leaf_credential, server_credential},
};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DsError {
    /// Not for the observer's current epoch: someone else's commit came first.
    #[error("stale epoch")]
    Stale,
    #[error("invalid: {0}")]
    Invalid(&'static str),
}

use DsError::Invalid;

fn invalid<E>(reason: &'static str) -> impl FnOnce(E) -> DsError {
    move |_| Invalid(reason)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPackageInfo {
    pub device_key: String,
    pub uid: Uid,
    /// `KeyPackageRef`, as named in Welcome messages.
    pub reference: Vec<u8>,
    /// Unix seconds.
    pub not_after: u64,
    pub last_resort: bool,
}

/// Verifies a key package a device publishes for itself.
pub fn key_package(bytes: &[u8]) -> Result<KeyPackageInfo, DsError> {
    let crypto = RustCrypto::default();
    let package = KeyPackageIn::tls_deserialize_exact(bytes)
        .map_err(invalid("key package encoding"))?
        .validate(&crypto, ProtocolVersion::Mls10)
        .map_err(invalid("key package"))?;
    if package.ciphersuite() != SUITE {
        return Err(Invalid("ciphersuite"));
    }
    let leaf = package.leaf_node();
    if !leaf.capabilities().extensions().contains(&ExtensionType::Unknown(DESCRIPTOR_EXTENSION)) {
        return Err(Invalid("capabilities"));
    }
    let credential = leaf_credential(leaf).ok_or(Invalid("credential"))?;
    Ok(KeyPackageInfo {
        device_key: credential.device_key().to_owned(),
        uid: credential.uid(),
        reference: package.hash_ref(&crypto).map_err(invalid("key package ref"))?.as_slice().to_vec(),
        not_after: package.life_time().not_after(),
        last_resort: package.last_resort(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leaf {
    pub index: u32,
    pub device_key: String,
    pub uid: Uid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitSender {
    Member(u32),
    /// An external commit: the device joined by itself.
    NewMember,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    pub epoch: u64,
    pub sender: CommitSender,
    pub added: Vec<Leaf>,
    pub removed: Vec<Leaf>,
    /// Device keys the Welcome is for: exactly the devices added by key package.
    pub welcome_recipients: Vec<String>,
}

fn group_info_in(bytes: &[u8]) -> Result<openmls::messages::group_info::VerifiableGroupInfo, DsError> {
    match MlsMessageIn::tls_deserialize_exact(bytes).map_err(invalid("group info encoding"))?.extract() {
        MlsMessageBodyIn::GroupInfo(info) => Ok(info),
        _ => Err(Invalid("not a group info")),
    }
}

/// Verifies a GroupInfo with its ratchet tree: tree, signature, and the
/// GroupContext it carries. Returns the observer it describes.
fn verified_group_info(bytes: &[u8]) -> Result<(MemoryStorage, PublicGroup), DsError> {
    let info = group_info_in(bytes)?;
    let tree = info.extensions().ratchet_tree().ok_or(Invalid("no ratchet tree"))?.ratchet_tree().clone();
    let storage = MemoryStorage::default();
    let (group, _) = PublicGroup::from_external(&RustCrypto::default(), &storage, tree, info, ProposalStore::new())
        .map_err(invalid("group info"))?;
    Ok((storage, group))
}

/// One group, as the server follows it.
pub struct Observer {
    storage: MemoryStorage,
    group: PublicGroup,
    descriptor: SignedDescriptor,
    server_key: String,
}

impl Observer {
    /// Starts following a new group from its first GroupInfo.
    pub fn create(group_info: &[u8], server_key: &str) -> Result<Self, DsError> {
        let (storage, group) = verified_group_info(group_info)?;
        let observer = Self::from_parts(storage, group, server_key)?;
        observer.check_tree()?;
        Ok(observer)
    }

    fn from_parts(storage: MemoryStorage, group: PublicGroup, server_key: &str) -> Result<Self, DsError> {
        let context = group.group_context();
        if context.ciphersuite() != SUITE {
            return Err(Invalid("ciphersuite"));
        }
        let extensions = context.extensions();
        let descriptor = extensions
            .unknown(DESCRIPTOR_EXTENSION)
            .and_then(|ext| SignedDescriptor::parse(&ext.0))
            .ok_or(Invalid("descriptor"))?;
        if descriptor.group_id().as_slice() != context.group_id().as_slice()
            || descriptor.descriptor.server_key != server_key
        {
            return Err(Invalid("descriptor"));
        }
        let required = extensions.required_capabilities().ok_or(Invalid("required capabilities"))?;
        if !required.extension_types().contains(&ExtensionType::Unknown(DESCRIPTOR_EXTENSION)) {
            return Err(Invalid("required capabilities"));
        }
        let senders = extensions.external_senders().ok_or(Invalid("external senders"))?;
        let server = fixed::<32>(server_key).ok_or(Invalid("server key"))?;
        let expected =
            ExternalSender::new(server.to_vec().into(), BasicCredential::new(server_credential(server_key)).into());
        if senders.as_slice() != [expected] {
            return Err(Invalid("external senders"));
        }
        Ok(Self { storage, group, descriptor, server_key: server_key.to_owned() })
    }

    /// Restores an observer saved with [`Observer::save`].
    pub fn load(state: &[u8], server_key: &str) -> Result<Self, DsError> {
        let (group_id, rest) = state.split_at_checked(32).ok_or(Invalid("observer state"))?;
        let storage = MemoryStorage::default();
        *storage.values.write().expect("not poisoned") = decode_values(rest).ok_or(Invalid("observer state"))?;
        let group = PublicGroup::load(&storage, &GroupId::from_slice(group_id))
            .map_err(invalid("observer state"))?
            .ok_or(Invalid("observer state"))?;
        Self::from_parts(storage, group, server_key)
    }

    pub fn save(&self) -> Vec<u8> {
        let mut out = self.group.group_id().as_slice().to_vec();
        let values = self.storage.values.read().expect("not poisoned");
        let mut entries: Vec<_> = values.iter().collect();
        entries.sort();
        for (key, value) in entries {
            for field in [key, value] {
                out.extend_from_slice(&(field.len() as u32).to_be_bytes());
                out.extend_from_slice(field);
            }
        }
        out
    }

    pub fn group_id(&self) -> [u8; 32] {
        self.descriptor.group_id()
    }

    pub fn epoch(&self) -> u64 {
        self.group.group_context().epoch().as_u64()
    }

    pub fn descriptor(&self) -> &SignedDescriptor {
        &self.descriptor
    }

    pub fn leaves(&self) -> Vec<Leaf> {
        self.group
            .members()
            .map(|m| {
                let credential = Credential::parse(
                    BasicCredential::try_from(m.credential).expect("checked on every commit").identity(),
                )
                .expect("checked on every commit");
                Leaf { index: m.index.u32(), device_key: credential.device_key().to_owned(), uid: credential.uid() }
            })
            .collect()
    }

    /// Gwar's rules on the whole tree: well-formed credentials, one leaf per
    /// device key, and the membership rule of the conversation's kind.
    fn check_tree(&self) -> Result<(), DsError> {
        let mut devices = BTreeSet::new();
        let participants = dm_participants(&self.descriptor.descriptor.conversation);
        for (_, leaf) in self.group.treesync().full_leaves() {
            let credential = leaf_credential(leaf).ok_or(Invalid("leaf credential"))?;
            if !leaf.capabilities().extensions().contains(&ExtensionType::Unknown(DESCRIPTOR_EXTENSION)) {
                return Err(Invalid("leaf capabilities"));
            }
            if !devices.insert(credential.device_key().to_owned()) {
                return Err(Invalid("two leaves for one device"));
            }
            if self.descriptor.descriptor.kind == Kind::Dm
                && !participants.is_some_and(|uids| uids.contains(&credential.uid().as_str()))
            {
                return Err(Invalid("not a participant"));
            }
        }
        Ok(())
    }

    fn leaf(&self, index: LeafNodeIndex) -> Option<Leaf> {
        let credential = leaf_credential(self.group.leaf(index)?)?;
        Some(Leaf { index: index.u32(), device_key: credential.device_key().to_owned(), uid: credential.uid() })
    }

    /// Processes and merges the next commit, checking the GroupInfo its sender
    /// uploads for the new epoch and the Welcome for the devices it adds.
    /// Consumes the observer: on error, load the saved state again.
    pub fn commit(
        mut self,
        commit: &[u8],
        group_info: &[u8],
        welcome: Option<&[u8]>,
    ) -> Result<(Self, Accepted), DsError> {
        let message = MlsMessageIn::tls_deserialize_exact(commit)
            .map_err(invalid("commit encoding"))?
            .try_into_protocol_message()
            .map_err(invalid("not a handshake"))?;
        let ProtocolMessage::PublicMessage(public) = &message else {
            return Err(Invalid("not a public message"));
        };
        if public.content_type() != ContentType::Commit {
            return Err(Invalid("not a commit"));
        }
        if message.group_id().as_slice() != self.group.group_id().as_slice() {
            return Err(Invalid("another group"));
        }
        if message.epoch() != self.group.group_context().epoch() {
            return Err(DsError::Stale);
        }
        let crypto = RustCrypto::default();
        let processed = self.group.process_message(&crypto, message).map_err(invalid("commit"))?;
        let sender = match processed.sender() {
            Sender::Member(index) => CommitSender::Member(index.u32()),
            Sender::NewMemberCommit => CommitSender::NewMember,
            _ => return Err(Invalid("commit sender")),
        };
        let ProcessedMessageContent::StagedCommitMessage(staged) = processed.into_content() else {
            return Err(Invalid("not a commit"));
        };
        let unchanged =
            |ty| staged.group_context().extensions().unknown(ty) == self.group.group_context().extensions().unknown(ty);
        if !unchanged(DESCRIPTOR_EXTENSION)
            || staged.group_context().extensions().external_senders()
                != self.group.group_context().extensions().external_senders()
            || staged.group_context().extensions().required_capabilities()
                != self.group.group_context().extensions().required_capabilities()
        {
            return Err(Invalid("group context changed"));
        }
        // An update keeps the leaf's uid and device key.
        let keeps_identity = |index: LeafNodeIndex, leaf: &LeafNode| {
            let old = self.leaf(index);
            let new = leaf_credential(leaf);
            old.is_some_and(|old| new.is_some_and(|new| old.uid == new.uid() && old.device_key == new.device_key()))
        };
        if let CommitSender::Member(index) = sender
            && let Some(leaf) = staged.update_path_leaf_node()
            && !keeps_identity(LeafNodeIndex::new(index), leaf)
        {
            return Err(Invalid("update changes identity"));
        }
        // Members update their leaf in the commit's path; proposals other than the
        // server's removals are inline, and an Update proposal cannot be.
        if staged.update_proposals().next().is_some() {
            return Err(Invalid("update proposal"));
        }
        let removed: Vec<Leaf> =
            staged.remove_proposals().filter_map(|r| self.leaf(r.remove_proposal().removed())).collect();
        let mut welcome_for = BTreeSet::new();
        let mut added_devices = Vec::new();
        for add in staged.add_proposals() {
            let package = add.add_proposal().key_package();
            welcome_for.insert(package.hash_ref(&crypto).map_err(invalid("key package ref"))?.as_slice().to_vec());
            added_devices.push(leaf_credential(package.leaf_node()).ok_or(Invalid("leaf credential"))?);
        }
        let external = match sender {
            CommitSender::NewMember => Some(
                leaf_credential(staged.update_path_leaf_node().ok_or(Invalid("external commit path"))?)
                    .ok_or(Invalid("leaf credential"))?,
            ),
            CommitSender::Member(_) => None,
        };
        self.group.merge_commit(&self.storage, *staged).map_err(invalid("merge"))?;
        self.check_tree()?;

        let (_, uploaded) = verified_group_info(group_info)?;
        if uploaded.group_context() != self.group.group_context()
            || uploaded.confirmation_tag() != self.group.confirmation_tag()
        {
            return Err(Invalid("group info does not match the commit"));
        }

        let welcome_recipients = match welcome {
            None if welcome_for.is_empty() => Vec::new(),
            None => return Err(Invalid("missing welcome")),
            Some(bytes) => {
                let MlsMessageBodyIn::Welcome(welcome) =
                    MlsMessageIn::tls_deserialize_exact(bytes).map_err(invalid("welcome encoding"))?.extract()
                else {
                    return Err(Invalid("not a welcome"));
                };
                let named: BTreeSet<Vec<u8>> =
                    welcome.secrets().iter().map(|s| s.new_member().as_slice().to_vec()).collect();
                if named != welcome_for || welcome.secrets().len() != named.len() || welcome.ciphersuite() != SUITE {
                    return Err(Invalid("welcome recipients"));
                }
                added_devices.iter().map(|c| c.device_key().to_owned()).collect()
            }
        };
        let leaves = self.leaves();
        let find = |device: &str| leaves.iter().find(|l| l.device_key == device).cloned();
        let mut added: Vec<Leaf> = added_devices.iter().filter_map(|c| find(c.device_key())).collect();
        added.extend(external.iter().filter_map(|c| find(c.device_key())));
        let accepted = Accepted { epoch: self.epoch(), sender, added, removed, welcome_recipients };
        Ok((self, accepted))
    }

    /// An external `Remove` proposal for `leaf` in the current epoch, signed by
    /// the server key and kept so that a commit may include it by reference.
    pub fn propose_remove(&mut self, server: &Ed25519Signer, leaf: u32) -> Result<Vec<u8>, DsError> {
        if b64(server.0.verifying_key().as_bytes()) != self.server_key {
            return Err(Invalid("server key"));
        }
        let index = LeafNodeIndex::new(leaf);
        if self.group.leaf(index).is_none() {
            return Err(Invalid("no such leaf"));
        }
        let proposal = ExternalProposal::new_remove::<openmls_rust_crypto::OpenMlsRustCrypto>(
            index,
            self.group.group_id().clone(),
            self.group.group_context().epoch(),
            server,
            SenderExtensionIndex::new(0),
        )
        .map_err(invalid("proposal"))?;
        let bytes = proposal.tls_serialize_detached().map_err(invalid("proposal encoding"))?;
        let message = MlsMessageIn::tls_deserialize_exact(&bytes)
            .map_err(invalid("proposal encoding"))?
            .try_into_protocol_message()
            .map_err(invalid("proposal"))?;
        let processed = self.group.process_message(&RustCrypto::default(), message).map_err(invalid("proposal"))?;
        let ProcessedMessageContent::ProposalMessage(queued) = processed.into_content() else {
            return Err(Invalid("proposal"));
        };
        self.group.add_proposal(&self.storage, *queued).map_err(invalid("proposal store"))?;
        Ok(bytes)
    }
}

/// Length-prefixed key/value pairs written by [`Observer::save`].
fn decode_values(mut input: &[u8]) -> Option<std::collections::HashMap<Vec<u8>, Vec<u8>>> {
    fn take(input: &mut &[u8]) -> Option<Vec<u8>> {
        let (len, rest) = input.split_first_chunk::<4>()?;
        let (field, rest) = rest.split_at_checked(u32::from_be_bytes(*len) as usize)?;
        *input = rest;
        Some(field.to_vec())
    }
    let mut values = std::collections::HashMap::new();
    while !input.is_empty() {
        let key = take(&mut input)?;
        values.insert(key, take(&mut input)?);
    }
    Some(values)
}
