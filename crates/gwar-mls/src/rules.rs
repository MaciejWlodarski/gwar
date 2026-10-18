//! Structural acceptance rules shared by the observer and device agent.

use crate::{
    Credential, DESCRIPTOR_EXTENSION, SUITE, SignedDescriptor,
    access::{ACCESS_EXTENSION, Access},
    descriptor::{Kind, dm_participants},
    ds::{CommitSender, DsError, Leaf},
    encoding::fixed,
    owner::OwnerChain,
    signer::{leaf_credential, server_credential},
};
use DsError::Invalid;
use openmls::prelude::tls_codec::Serialize as _;
use openmls::prelude::*;
use openmls_traits::crypto::OpenMlsCrypto;
use std::collections::BTreeSet;
fn invalid<E>(reason: &'static str) -> impl FnOnce(E) -> DsError {
    move |_| Invalid(reason)
}

pub(crate) fn context(context: &GroupContext, server_key: &str) -> Result<SignedDescriptor, DsError> {
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
    if !required.extension_types().contains(&ExtensionType::Unknown(DESCRIPTOR_EXTENSION))
        || !required.credential_types().contains(&CredentialType::Basic)
    {
        return Err(Invalid("required capabilities"));
    }
    // A private channel's group carries its access lists and requires them of every leaf; no other
    // group carries them (docs/e2e-build.md, section 6, 6c).
    let required_access = required.extension_types().contains(&ExtensionType::Unknown(ACCESS_EXTENSION));
    if is_private(&descriptor) {
        if !required_access || access(context).is_none() {
            return Err(Invalid("private channel access"));
        }
    } else if required_access || extensions.unknown(ACCESS_EXTENSION).is_some() {
        return Err(Invalid("access lists outside a private channel"));
    }
    let senders = extensions.external_senders().ok_or(Invalid("external senders"))?;
    let server = fixed::<32>(server_key).ok_or(Invalid("server key"))?;
    let expected =
        ExternalSender::new(server.to_vec().into(), BasicCredential::new(server_credential(server_key)).into());
    if senders.as_slice() != [expected] {
        return Err(Invalid("external senders"));
    }
    Ok(descriptor)
}

/// A private channel's group: an encrypted channel created private.
pub(crate) fn is_private(descriptor: &SignedDescriptor) -> bool {
    descriptor.descriptor.kind == Kind::Channel && descriptor.descriptor.private
}

/// The access lists of a GroupContext, when it carries well-formed ones.
pub(crate) fn access(context: &GroupContext) -> Option<Access> {
    context.extensions().unknown(ACCESS_EXTENSION).and_then(|ext| Access::parse(&ext.0))
}

/// What the membership rule of a private channel needs: its lists and the owner chain.
#[derive(Clone, Copy)]
pub(crate) struct Private<'a> {
    pub access: &'a Access,
    pub chain: &'a OwnerChain,
}

pub(crate) fn tree<'a>(
    descriptor: &SignedDescriptor,
    leaves: impl Iterator<Item = &'a LeafNode>,
) -> Result<(), DsError> {
    tree_with(descriptor, leaves, None)
}

/// The tree rule; for a private channel every leaf advertises `0xff01` and is admitted by the lists
/// (as history: the owner is any owner of the anchor's span). Without its lists a private channel
/// fails closed.
pub(crate) fn tree_with<'a>(
    descriptor: &SignedDescriptor,
    leaves: impl Iterator<Item = &'a LeafNode>,
    private: Option<Private<'_>>,
) -> Result<(), DsError> {
    if is_private(descriptor) && private.is_none() {
        return Err(Invalid("private channel without its owner record"));
    }
    let mut devices = BTreeSet::new();
    let participants = dm_participants(&descriptor.descriptor.conversation);
    for leaf in leaves {
        let credential = leaf_credential(leaf).ok_or(Invalid("leaf credential"))?;
        if !leaf.capabilities().extensions().contains(&ExtensionType::Unknown(DESCRIPTOR_EXTENSION)) {
            return Err(Invalid("leaf capabilities"));
        }
        if !devices.insert(credential.device_key().to_owned()) {
            return Err(Invalid("two leaves for one device"));
        }
        if descriptor.descriptor.kind == Kind::Dm
            && !participants.is_some_and(|uids| uids.contains(&credential.uid().as_str()))
        {
            return Err(Invalid("not a participant"));
        }
        if let Some(p) = private.filter(|_| is_private(descriptor)) {
            if !leaf.capabilities().extensions().contains(&ExtensionType::Unknown(ACCESS_EXTENSION)) {
                return Err(Invalid("leaf capabilities"));
            }
            if !p.access.admits(p.chain, &credential.uid()) {
                return Err(Invalid("not admitted to the private channel"));
            }
        }
    }
    Ok(())
}

/// Epoch-zero voice snapshots must actually be the descriptor creator's singleton.
/// Later snapshots can have any structurally valid membership.
pub(crate) fn voice_initial_tree<'a>(
    descriptor: &SignedDescriptor,
    epoch: u64,
    mut leaves: impl Iterator<Item = &'a LeafNode>,
) -> Result<(), DsError> {
    if descriptor.descriptor.kind == Kind::Voice && epoch == 0 {
        let creator = leaves.next().and_then(leaf_credential).ok_or(Invalid("voice creator"))?;
        if leaves.next().is_some()
            || creator.device_key() != descriptor.creator.device_key()
            || creator.uid() != descriptor.descriptor.creator_uid
        {
            return Err(Invalid("voice creator"));
        }
    }
    Ok(())
}

pub(crate) fn leaf(group: &PublicGroup, index: LeafNodeIndex) -> Option<Leaf> {
    let c = leaf_credential(group.leaf(index)?)?;
    Some(Leaf { index: index.u32(), device_key: c.device_key().into(), uid: c.uid() })
}

pub(crate) struct Transition {
    pub removed: Vec<Leaf>,
    pub welcome_for: BTreeSet<Vec<u8>>,
    pub added_devices: Vec<Credential>,
    pub external: Option<Credential>,
}

pub(crate) fn staged(
    group: &PublicGroup,
    staged: &StagedCommit,
    sender: &CommitSender,
    committed_at: i64,
    crypto: &impl OpenMlsCrypto,
) -> Result<Transition, DsError> {
    staged_with(group, staged, sender, committed_at, crypto, None)
}

/// A commit's structure. In a private channel's group a member may replace `0xff01` alone with a
/// `GroupContextExtensions` proposal (never in an external commit); the new lists must be valid as
/// history at the commit's time, seqs are not compared (docs/e2e-build.md, section 6, "Changes").
/// The caller then applies the tree rule to the merged tree with the new lists.
pub(crate) fn staged_with(
    group: &PublicGroup,
    staged: &StagedCommit,
    sender: &CommitSender,
    committed_at: i64,
    crypto: &impl OpenMlsCrypto,
    owner: Option<&OwnerChain>,
) -> Result<Transition, DsError> {
    let unchanged =
        |ty| staged.group_context().extensions().unknown(ty) == group.group_context().extensions().unknown(ty);
    let current = group
        .group_context()
        .extensions()
        .unknown(DESCRIPTOR_EXTENSION)
        .and_then(|ext| SignedDescriptor::parse(&ext.0))
        .ok_or(Invalid("descriptor"))?;
    let private = is_private(&current);
    let access_changed = !unchanged(ACCESS_EXTENSION);
    if access_changed {
        let chain = owner.ok_or(Invalid("private channel without its owner record"))?;
        let group_id: [u8; 32] = current.group_id();
        let fresh = access(staged.group_context()).ok_or(Invalid("access lists"))?;
        if !private || matches!(sender, CommitSender::NewMember) || !fresh.valid(chain, &group_id, committed_at) {
            return Err(Invalid("access lists"));
        }
        // The proposal carries the whole set: every other extension stays byte-identical.
        let others = |e: &GroupContext| -> Vec<Vec<u8>> {
            e.extensions()
                .iter()
                .filter(|x| x.extension_type() != ExtensionType::Unknown(ACCESS_EXTENSION))
                .map(|x| x.tls_serialize_detached().unwrap_or_default())
                .collect()
        };
        if others(staged.group_context()) != others(group.group_context()) {
            return Err(Invalid("group context changed"));
        }
    }
    if !unchanged(DESCRIPTOR_EXTENSION)
        || staged.group_context().extensions().external_senders()
            != group.group_context().extensions().external_senders()
        || staged.group_context().extensions().required_capabilities()
            != group.group_context().extensions().required_capabilities()
    {
        return Err(Invalid("group context changed"));
    }
    // An update keeps the leaf's uid and device key.
    let keeps_identity = |index: LeafNodeIndex, leaf: &LeafNode| {
        let old = self::leaf(group, index);
        let new = leaf_credential(leaf);
        old.is_some_and(|old| new.is_some_and(|new| old.uid == new.uid() && old.device_key == new.device_key()))
    };
    if let CommitSender::Member(index) = sender
        && let Some(leaf) = staged.update_path_leaf_node()
        && !keeps_identity(LeafNodeIndex::new(*index), leaf)
    {
        return Err(Invalid("update changes identity"));
    }
    // Members update their leaf in the commit's path, so the only proposals
    // are adds, removals and an external join's init.
    let allowed: &[ProposalType] = if access_changed {
        &[ProposalType::Add, ProposalType::Remove, ProposalType::GroupContextExtensions]
    } else {
        &[ProposalType::Add, ProposalType::Remove, ProposalType::ExternalInit]
    };
    if staged.queued_proposals().any(|p| !allowed.contains(&p.proposal().proposal_type())) {
        return Err(Invalid("proposal type"));
    }
    let removed: Vec<Leaf> =
        staged.remove_proposals().filter_map(|r| leaf(group, r.remove_proposal().removed())).collect();
    let descriptor = current;
    if descriptor.descriptor.kind.separate_tree() && staged.add_proposals().next().is_some() {
        return Err(Invalid("separate-tree groups join externally"));
    }
    let mut welcome_for = BTreeSet::new();
    let mut added_devices = Vec::new();
    for add in staged.add_proposals() {
        let package = add.add_proposal().key_package();
        welcome_for.insert(package.hash_ref(crypto).map_err(invalid("key package ref"))?.as_slice().to_vec());
        added_devices.push(leaf_credential(package.leaf_node()).ok_or(Invalid("leaf credential"))?);
    }
    let external = match sender {
        CommitSender::NewMember => Some(
            leaf_credential(staged.update_path_leaf_node().ok_or(Invalid("external commit path"))?)
                .ok_or(Invalid("leaf credential"))?,
        ),
        CommitSender::Member(_) => None,
    };
    // A joining device may only replace its own earlier leaf.
    if let Some(joiner) = &external
        && (removed.len() > 1 || removed.iter().any(|l| l.device_key != joiner.device_key() || l.uid != joiner.uid()))
    {
        return Err(Invalid("external removal"));
    }
    // Every leaf the commit introduces or changes is certified at its time;
    // later expiry or revocation is handled by removal, not here.
    let path = match sender {
        CommitSender::Member(_) => staged.update_path_leaf_node().map(leaf_credential),
        CommitSender::NewMember => None,
    };
    let mut introduced = added_devices.iter().chain(&external);
    if path.as_ref().is_some_and(|c| !c.as_ref().is_some_and(|c| c.is_current(committed_at, &())))
        || introduced.any(|c| !c.is_current(committed_at, &()))
    {
        return Err(Invalid("certificate at commit time"));
    }
    Ok(Transition { removed, welcome_for, added_devices, external })
}
