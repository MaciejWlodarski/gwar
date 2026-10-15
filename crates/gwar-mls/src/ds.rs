//! What the server checks: key packages, and every group followed as an OpenMLS
//! `PublicGroup` (docs/e2e-build.md, "What the server uses").
//!
//! The observer holds no secret. It verifies each `PublicMessage` handshake,
//! merges commits, and applies Gwar's structural rules, the same ones clients
//! apply: leaves a commit introduces or changes are certified at the commit's
//! time; expiry and revocation later on are handled by removal.

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
    descriptor::SignedDescriptor,
    encoding::b64,
    signer::{Ed25519Signer, leaf_credential},
};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DsError {
    /// Not for the observer's current epoch: someone else's commit came first.
    #[error("stale epoch")]
    Stale,
    /// The commit's time is too far behind the server's clock: it can never
    /// be accepted, so its sender builds a new one.
    #[error("commit expired")]
    Expired,
    /// The commit's time is too far ahead of the server's clock: it may be
    /// accepted later, so its sender keeps it.
    #[error("commit too early")]
    Early,
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
    /// The time its sender signed into the commit (Unix ms).
    pub committed_at: i64,
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

/// Verifies a new group's first GroupInfo with its ratchet tree: tree,
/// signature, lifetimes and the GroupContext it carries.
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
        let descriptor = crate::rules::context(group.group_context(), server_key)?;
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

    /// The current epoch and its TLS-encoded public ratchet tree.
    pub fn tree(&self) -> Result<(u64, Vec<u8>), DsError> {
        Ok((self.epoch(), self.group.export_ratchet_tree().tls_serialize_detached().map_err(invalid("tree encoding"))?))
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
        crate::rules::tree(&self.descriptor, self.group.treesync().full_leaves().map(|(_, leaf)| leaf))
    }

    /// Server removal proposals this commit references, in commit order.
    /// Processes on a disposable copy; the observer is unchanged.
    pub fn referenced_removals(&self, commit: &[u8]) -> Result<Vec<u32>, DsError> {
        let copy = Self::load(&self.save(), &self.server_key)?;
        let message = MlsMessageIn::tls_deserialize_exact(commit)
            .map_err(invalid("commit encoding"))?
            .try_into_protocol_message()
            .map_err(invalid("not a handshake"))?;
        let processed = copy.group.process_message(&RustCrypto::default(), message).map_err(invalid("commit"))?;
        let ProcessedMessageContent::StagedCommitMessage(staged) = processed.into_content() else {
            return Err(Invalid("not a commit"));
        };
        Ok(staged
            .queued_proposals()
            .filter_map(|p| match (p.sender(), p.proposal_or_ref_type(), p.proposal()) {
                (Sender::External(_), ProposalOrRefType::Reference, Proposal::Remove(r)) => Some(r.removed().u32()),
                _ => None,
            })
            .collect())
    }

    /// Processes and merges the next commit, checking the GroupInfo its sender
    /// uploads for the new epoch and the Welcome for the devices it adds.
    /// Consumes the observer: on error, load the saved state again.
    ///
    /// `now` is the server's clock and `previous` the time of the group's last
    /// commit (both Unix ms); see [`commit_time`].
    pub fn commit(
        mut self,
        commit: &[u8],
        group_info: &[u8],
        welcome: Option<&[u8]>,
        now: i64,
        previous: i64,
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
        // A commit too old for the clock can never be accepted, whatever else
        // OpenMLS would reject it for now (such as an Add whose key package ran
        // out meanwhile), so its sender learns to build a new one. The time is
        // read before the signature is checked; it is checked again below.
        if peek_commit_time(commit).is_some_and(|t| now.saturating_sub(t) > MAX_CLOCK_SKEW_MS) {
            return Err(DsError::Expired);
        }
        let crypto = RustCrypto::default();
        let processed = self.group.process_message(&crypto, message).map_err(invalid("commit"))?;
        let committed_at = commit_time(processed.aad()).ok_or(Invalid("commit time"))?;
        if now.saturating_sub(committed_at) > MAX_CLOCK_SKEW_MS {
            return Err(DsError::Expired);
        }
        if committed_at.saturating_sub(now) > MAX_CLOCK_SKEW_MS {
            return Err(DsError::Early);
        }
        if committed_at < previous {
            return Err(Invalid("commit time"));
        }
        let sender = match processed.sender() {
            Sender::Member(index) => CommitSender::Member(index.u32()),
            Sender::NewMemberCommit => CommitSender::NewMember,
            _ => return Err(Invalid("commit sender")),
        };
        let ProcessedMessageContent::StagedCommitMessage(staged) = processed.into_content() else {
            return Err(Invalid("not a commit"));
        };
        let crate::rules::Transition { removed, welcome_for, added_devices, external } =
            crate::rules::staged(&self.group, &staged, &sender, committed_at, &crypto)?;
        self.group.merge_commit(&self.storage, *staged).map_err(invalid("merge"))?;
        self.check_tree()?;

        self.check_group_info(group_info)?;

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
        let accepted = Accepted { epoch: self.epoch(), committed_at, sender, added, removed, welcome_recipients };
        Ok((self, accepted))
    }

    /// The GroupInfo a committer uploads for the new epoch must describe exactly
    /// the merged state: GroupContext, confirmation tag and, in direct messages,
    /// the tree (channel and server groups leave it out), signed by a member.
    /// Leaf lifetimes are not checked again (docs/e2e-build.md, "Lifetimes").
    fn check_group_info(&self, bytes: &[u8]) -> Result<(), DsError> {
        let info = group_info_in(bytes)?;
        let mismatch = Invalid("group info does not match the commit");
        let extensions = info.extensions();
        let tree = extensions.ratchet_tree();
        match tree {
            None if self.descriptor.descriptor.kind.separate_tree() => {}
            None => return Err(Invalid("no ratchet tree")),
            Some(_) if self.descriptor.descriptor.kind.separate_tree() => return Err(mismatch),
            Some(tree)
                if tree.ratchet_tree().tls_serialize_detached().ok()
                    == self.group.export_ratchet_tree().tls_serialize_detached().ok() => {}
            _ => return Err(mismatch),
        }
        if extensions.external_pub().is_none() || extensions.iter().count() != 1 + usize::from(tree.is_some()) {
            return Err(mismatch);
        }
        // GroupInfoTBS = group_context ‖ extensions ‖ confirmation_tag ‖ signer (u32).
        let mut expected = self.group.group_context().tls_serialize_detached().map_err(invalid("encoding"))?;
        expected.extend(extensions.tls_serialize_detached().map_err(invalid("encoding"))?);
        expected.extend(self.group.confirmation_tag().tls_serialize_detached().map_err(invalid("encoding"))?);
        let payload = info.unsigned_payload().map_err(invalid("encoding"))?;
        let signer = payload
            .strip_prefix(expected.as_slice())
            .and_then(|rest| <[u8; 4]>::try_from(rest).ok())
            .map(u32::from_be_bytes)
            .ok_or(mismatch)?;
        let leaf = self.group.leaf(LeafNodeIndex::new(signer)).ok_or(Invalid("group info signer"))?;
        let key = OpenMlsSignaturePublicKey::from_signature_key(leaf.signature_key().clone(), SignatureScheme::ED25519);
        info.verify_no_out(&RustCrypto::default(), &key).map_err(invalid("group info signature"))
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

/// How far a commit's signed time may be from the server's clock.
pub const MAX_CLOCK_SKEW_MS: i64 = 5 * 60 * 1000;

const COMMIT_TIME_HEADER: &str = "gwar commit v1\n";

/// The authenticated data a committer puts on every commit.
pub fn commit_aad(committed_at: i64) -> Vec<u8> {
    format!("{COMMIT_TIME_HEADER}{committed_at}").into_bytes()
}

/// The commit time of a serialized `PublicMessage`, read from its framing
/// (RFC 9420 6.1) without verifying it.
fn peek_commit_time(message: &[u8]) -> Option<i64> {
    // MLS variable-length integer (RFC 9420 2.1.2).
    fn varint(input: &mut &[u8]) -> Option<usize> {
        let first = *input.first()?;
        let size = [1, 2, 4].get(usize::from(first >> 6)).copied()?;
        let bytes = input.get(..size)?;
        let value = bytes[1..].iter().fold(usize::from(first & 0x3f), |v, &b| v << 8 | usize::from(b));
        *input = &input[size..];
        Some(value)
    }
    let mut input = message.get(4..)?; // version, wire format
    let group_id = varint(&mut input)?;
    input = input.get(group_id + 8..)?; // group id, epoch
    let sender = match input.first()? {
        1 | 2 => 5, // member or external: type and u32 index
        3 | 4 => 1, // new member: type only
        _ => return None,
    };
    input = input.get(sender..)?;
    let len = varint(&mut input)?;
    commit_time(input.get(..len)?)
}

/// The time in a commit's authenticated data (`gwar commit v1\n{unix ms}`).
pub fn commit_time(aad: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(aad).ok()?.strip_prefix(COMMIT_TIME_HEADER)?;
    crate::encoding::decimal(text).map(|ms| ms as i64)
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
