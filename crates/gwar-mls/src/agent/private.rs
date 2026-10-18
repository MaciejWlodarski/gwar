//! Encrypted private channel groups on the device (docs/e2e-build.md, section 6, "Encrypted
//! private channel groups (6c)"): the owner chain this device pinned checks every tree and commit,
//! a per-group checkpoint of the newest member list it accepted guards against rollback, and
//! current authority (a)–(d) gates sending.

use openmls::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use vc_proto::Uid;

use super::{Agent, AgentError, Invalid, Outbox, Result, Status, client_context, ds, err, group_key, join_config};
use crate::access::{ACCESS_EXTENSION, Access, ChannelMembers, MAX_ACCESS_BYTES, MAX_MEMBERS, SignedMembers};
use crate::encoding::{b64, fixed};
use crate::owner::OwnerChain;
use crate::signer::leaf_credential;
use crate::{Revocations, SignedDescriptor, rules};

/// The newest member list this device accepted for one private group: lists are ordered by
/// generation, then seq; the same position needs the same list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessCheckpoint {
    pub generation: u64,
    pub seq: u64,
    pub hash: String,
}

impl AccessCheckpoint {
    pub(super) fn of(m: &SignedMembers) -> Self {
        Self { generation: m.members().generation, seq: m.members().seq, hash: m.hash() }
    }

    fn position(m: &SignedMembers) -> (u64, u64) {
        (m.members().generation, m.members().seq)
    }

    /// `m` is this very list.
    fn is(&self, m: &SignedMembers) -> bool {
        (self.generation, self.seq) == Self::position(m) && self.hash == m.hash()
    }

    /// `m` comes after this list.
    fn before(&self, m: &SignedMembers) -> bool {
        (self.generation, self.seq) < Self::position(m)
    }
}

/// Current authority of this device in a private group ((a)–(d)), and what it can do otherwise.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum Authority {
    /// (a)–(d) hold: the group may be used.
    Current,
    /// The group carries an older manager pair than this device accepted, under a member list
    /// whose signer the accepted pair still authorizes: commit the accepted pair
    /// ([`Agent::commit_access`] without members), with the removals it requires.
    Carry,
    /// The lists are current, but the tree holds leaves they exclude: commit their removal
    /// ([`Agent::commit_access`] without members).
    Remove { leaves: Vec<u32> },
    /// Nothing this device can repair (no accepted pair, lists it never saw current, older than
    /// its checkpoints, or a signer no longer authorized): wait for a manager.
    Wait,
}

/// The extensions of a private group: the descriptor's set with the access lists.
pub(super) fn required_extensions(private: bool) -> Vec<ExtensionType> {
    let mut required = vec![ExtensionType::Unknown(crate::DESCRIPTOR_EXTENSION)];
    if private {
        required.push(ExtensionType::Unknown(ACCESS_EXTENSION));
    }
    required
}

impl Agent {
    /// The owner chain this device pinned for `server`.
    pub(super) fn owner_chain(&self, server: &str) -> Result<Option<OwnerChain>> {
        self.servers.get(server).map(|r| r.chain(server)).transpose()
    }

    /// Gwar's tree rule for `descriptor`'s group with the lists of `context`: a private channel's
    /// leaves must be admitted by them under the pinned owner chain, and the group fails closed
    /// without a pin.
    pub(super) fn tree_rule<'a>(
        &self,
        descriptor: &SignedDescriptor,
        context: &GroupContext,
        leaves: impl Iterator<Item = &'a LeafNode>,
    ) -> Result<()> {
        if !rules::is_private(descriptor) {
            return Ok(rules::tree(descriptor, leaves)?);
        }
        let chain = self
            .owner_chain(&descriptor.descriptor.server_key)?
            .ok_or(Invalid("private channel without an owner pin"))?;
        let access = rules::access(context).ok_or(Invalid("access lists"))?;
        Ok(rules::tree_with(descriptor, leaves, Some(rules::Private { access: &access, chain: &chain }))?)
    }

    /// A member list signed by this device for `group`.
    fn sign_members(
        &self,
        chain: &OwnerChain,
        group: &[u8; 32],
        seq: u64,
        members: &[Uid],
        at: i64,
    ) -> Result<SignedMembers> {
        let mut members = members.to_vec();
        members.sort();
        members.dedup();
        if members.len() > MAX_MEMBERS || members.iter().any(|m| fixed::<20>(m).is_none()) {
            return Err(Invalid("members"));
        }
        if seq == 0 || seq > vc_proto::MAX_SAFE_INTEGER as u64 {
            return Err(Invalid("member list seq"));
        }
        let statement = ChannelMembers {
            server_key: chain.genesis().server_key.clone(),
            generation: chain.generation(),
            group_id: b64(group),
            seq,
            members,
            signer_uid: self.credential.uid(),
            signed_at: at,
        };
        ChannelMembers::parse(&statement.statement()).ok_or(Invalid("member list"))?;
        Ok(SignedMembers::sign(statement, self.credential.clone(), &self.signer.0))
    }

    /// The initial lists of a private group this device creates: the newest manager pair it
    /// accepted and a member list bound to the group at seq 1, signed by this device, which must
    /// be the owner, a confirmer or a manager under that pair, now.
    pub(super) fn initial_access(
        &self,
        descriptor: &SignedDescriptor,
        members: &[Uid],
        now: i64,
        revocations: &dyn Revocations,
    ) -> Result<Access> {
        let server = &descriptor.descriptor.server_key;
        let record = self.servers.get(server).ok_or(AgentError::Waiting)?;
        let chain = record.chain(server)?;
        let (anchor, list) = record.pair()?.ok_or(AgentError::Waiting)?;
        let id = descriptor.group_id();
        let access = Access::new(anchor, list, self.sign_members(&chain, &id, 1, members, now)?);
        if !access.current(&chain, &id, now, revocations) {
            return Err(AgentError::Waiting);
        }
        if access.encode().len() > MAX_ACCESS_BYTES {
            return Err(Invalid("access lists too large"));
        }
        Ok(access)
    }

    /// A newcomer joins a private group only with (a)–(c) as of now: the GroupInfo carries the
    /// newest manager pair this device accepted and a member list current now and not older than
    /// its checkpoint (a tombstone keeps it). Otherwise it waits for a channel manager.
    /// Returns the member list to record as accepted (before the join is sent).
    pub(super) fn newcomer(
        &self,
        key: &str,
        descriptor: &SignedDescriptor,
        context: &GroupContext,
        now: i64,
        revocations: &dyn Revocations,
    ) -> Result<Option<AccessCheckpoint>> {
        if !rules::is_private(descriptor) {
            return Ok(None);
        }
        let server = &descriptor.descriptor.server_key;
        let record = self.servers.get(server).ok_or(AgentError::Waiting)?;
        let chain = record.chain(server)?;
        let (anchor, list) = record.pair()?.ok_or(AgentError::Waiting)?;
        let access = rules::access(context).ok_or(Invalid("access lists"))?;
        let members = access.members();
        let checkpoint = self.groups.get(key).and_then(|r| r.access.as_ref());
        if access.anchor().hash() != anchor.hash()
            || access.list().hash() != list.hash()
            || checkpoint.is_some_and(|cp| !cp.is(members) && !cp.before(members))
            || !access.current(&chain, &descriptor.group_id(), now, revocations)
        {
            return Err(AgentError::Waiting);
        }
        Ok(Some(AccessCheckpoint::of(members)))
    }

    /// Current authority in the group at `key` (`None` outside private channels). Pure: a member
    /// list current now and newer than the checkpoint counts as accepted on first sight
    /// ([`Agent::observe_access`] records it).
    pub(super) fn authority(
        &self,
        key: &str,
        group: &MlsGroup,
        now: i64,
        revocations: &dyn Revocations,
    ) -> Result<Option<Authority>> {
        let record = self.groups.get(key).ok_or(Invalid("unknown group"))?;
        let descriptor = SignedDescriptor::parse(&record.descriptor).ok_or(Invalid("descriptor"))?;
        if !rules::is_private(&descriptor) {
            return Ok(None);
        }
        let server = &descriptor.descriptor.server_key;
        let Some(server_record) = self.servers.get(server) else { return Ok(Some(Authority::Wait)) };
        let chain = server_record.chain(server)?;
        let Some((anchor, list)) = server_record.pair()? else { return Ok(Some(Authority::Wait)) };
        let access = rules::access(group.public_group().group_context()).ok_or(Invalid("access lists"))?;
        let id = descriptor.group_id();
        let members = access.members();
        // (a) the newest pair this device accepted; (b), (c) the member list recorded as the newest
        // accepted (current on first sight, see `advance_access`), still under an anchor the
        // pinned head holds and signed by someone these lists authorize under the current owner.
        let pair_ok = access.anchor().hash() == anchor.hash() && access.list().hash() == list.hash();
        let list_ok = record.access.as_ref().is_some_and(|cp| cp.is(members))
            && chain.anchor_current(access.anchor())
            && access.signer_authorized(&chain, true);
        let not_older = record.access.as_ref().is_none_or(|cp| cp.is(members) || cp.before(members));
        if pair_ok && list_ok {
            // (d) no leaf the lists exclude (the owner being only the current one).
            let leaves = excluded(&access, &chain, group);
            return Ok(Some(if leaves.is_empty() { Authority::Current } else { Authority::Remove { leaves } }));
        }
        // An older pair in the group, under a member list the accepted pair still authorizes.
        let position = |a: &crate::owner::SignedAnchor, l: &crate::owner::SignedList| {
            (a.anchor().generation, a.anchor().anchor_seq, l.list().seq)
        };
        let older_pair = position(access.anchor(), access.list()) < position(&anchor, &list);
        let carried = Access::new(anchor, list, members.clone());
        if !pair_ok && older_pair && not_older && carried.current(&chain, &id, now, revocations) {
            return Ok(Some(Authority::Carry));
        }
        Ok(Some(Authority::Wait))
    }

    /// Records the group's member list as the newest accepted when it is current now and newer
    /// than the checkpoint; returns the current authority.
    pub fn observe_access(
        &mut self,
        server: &str,
        id: &[u8; 32],
        now: i64,
        revocations: &dyn Revocations,
    ) -> Result<Option<Authority>> {
        self.operation(|a| {
            super::current_time(now)?;
            let key = group_key(server, id);
            let Some(record) = a.groups.get(&key) else { return Ok(None) };
            if record.status != Status::Active {
                // A removal's lists, once current under the accepted pair, still raise the checkpoint.
                if let Some(bytes) = record.removal_access.clone() {
                    let access = Access::parse(&bytes).ok_or(Invalid("removal access"))?;
                    if a.advance_with(&key, &access, now, revocations)? {
                        a.groups.get_mut(&key).ok_or(Invalid("record"))?.removal_access = None;
                    }
                }
                return Ok(None);
            }
            let group = a.mls(&key)?;
            a.advance_access(&key, &group, now, revocations)?;
            a.authority(&key, &group, now, revocations)
        })
    }

    pub(super) fn advance_access(
        &mut self,
        key: &str,
        group: &MlsGroup,
        now: i64,
        revocations: &dyn Revocations,
    ) -> Result<()> {
        let record = self.groups.get(key).ok_or(Invalid("unknown group"))?;
        let descriptor = SignedDescriptor::parse(&record.descriptor).ok_or(Invalid("descriptor"))?;
        if !rules::is_private(&descriptor) {
            return Ok(());
        }
        let access = rules::access(group.public_group().group_context()).ok_or(Invalid("access lists"))?;
        self.advance_with(key, &access, now, revocations)?;
        Ok(())
    }

    /// Records `access`'s member list as the newest accepted when it is newer than the checkpoint,
    /// carries exactly the newest pair this device accepted, and is current now. Returns whether
    /// it is (now) the checkpoint or older than it: either way it has nothing more to give.
    fn advance_with(&mut self, key: &str, access: &Access, now: i64, revocations: &dyn Revocations) -> Result<bool> {
        let record = self.groups.get(key).ok_or(Invalid("unknown group"))?;
        let descriptor = SignedDescriptor::parse(&record.descriptor).ok_or(Invalid("descriptor"))?;
        let server = &descriptor.descriptor.server_key;
        let Some(server_record) = self.servers.get(server) else { return Ok(false) };
        let chain = server_record.chain(server)?;
        let Some((anchor, list)) = server_record.pair()? else { return Ok(false) };
        let members = access.members();
        if record.access.as_ref().is_some_and(|cp| !cp.before(members)) {
            return Ok(true);
        }
        // Only under the newest pair this device accepted: a signer that pair no longer names
        // cannot raise the checkpoint through an older pair still in the group.
        if access.anchor().hash() == anchor.hash()
            && access.list().hash() == list.hash()
            && access.current(&chain, &descriptor.group_id(), now, revocations)
        {
            self.groups.get_mut(key).ok_or(Invalid("record"))?.access = Some(AccessCheckpoint::of(members));
            return Ok(true);
        }
        Ok(false)
    }

    /// Commits new access lists to a private group: with `members`, a member list signed by this
    /// device at the next seq (an access edit); without, the group's own member list carried under
    /// the newest manager pair this device accepted (a repair). Either way the accepted pair goes
    /// in, and every leaf the new lists exclude is removed in the same commit. When the lists stay
    /// the same, it only removes.
    pub fn commit_access(
        &mut self,
        server: &str,
        id: &[u8; 32],
        members: Option<&[Uid]>,
        now: i64,
        revocations: &dyn Revocations,
    ) -> Result<Outbox> {
        self.commit_lists(server, id, Lists::Members(members), now, revocations)
    }

    /// Commits lists a channel authority offered (`mls.access.offered`): they must carry the newest
    /// pair this device accepted, be current now, bound to this group, and no older than its
    /// checkpoint or the group's own list. The same commit removes the leaves they exclude.
    pub fn adopt_offer(
        &mut self,
        server: &str,
        id: &[u8; 32],
        extension: &[u8],
        now: i64,
        revocations: &dyn Revocations,
    ) -> Result<Outbox> {
        self.commit_lists(server, id, Lists::Offer(extension), now, revocations)
    }

    /// Fresh lists for a private group this device's user has authority over but cannot commit to
    /// (not a member, or the lists are no longer current): the newest pair it accepted and a member
    /// list it signs at the next seq after the group's and its own checkpoint, with `members` or the
    /// group's members. The group's lists come from its GroupInfo, verified as an external joiner
    /// would (tree, a member's signature, Gwar's rules) and valid as history (bound to the group,
    /// signed by an authority of their own pair, each signer certified when it signed). Sent as
    /// `mls.access.offer`; a member commits it.
    pub fn offer_access(
        &self,
        server: &str,
        group_info: &[u8],
        tree: Option<&[u8]>,
        members: Option<&[Uid]>,
        now: i64,
        revocations: &dyn Revocations,
    ) -> Result<Vec<u8>> {
        super::current_time(now)?;
        let (descriptor, context) = self.verified_info(server, group_info, tree)?;
        if !rules::is_private(&descriptor) {
            return Err(Invalid("not a private channel"));
        }
        let id = descriptor.group_id();
        let record = self.servers.get(server).ok_or(AgentError::Waiting)?;
        let chain = record.chain(server)?;
        let (anchor, list) = record.pair()?.ok_or(AgentError::Waiting)?;
        let shown = rules::access(&context).ok_or(Invalid("access lists"))?;
        if !shown.valid_when_signed(&chain, &id) {
            return Err(Invalid("the group's access lists"));
        }
        let own = self.groups.get(&group_key(server, &id)).and_then(|r| r.access.as_ref()).map_or(0, |c| c.seq);
        let seq = shown.members().members().seq.max(own).checked_add(1).ok_or(Invalid("member list seq"))?;
        let members = members.map_or_else(|| shown.members().members().members.clone(), <[Uid]>::to_vec);
        let access = Access::new(anchor, list, self.sign_members(&chain, &id, seq, &members, now)?);
        if !access.current(&chain, &id, now, revocations) {
            return Err(AgentError::Waiting);
        }
        let encoded = access.encode();
        if encoded.len() > MAX_ACCESS_BYTES {
            return Err(Invalid("access lists too large"));
        }
        Ok(encoded)
    }

    /// A GroupInfo verified as an external joiner would: its tree, Gwar's rules (with a private
    /// channel's lists) and a member's signature. Returns its descriptor and GroupContext.
    fn verified_info(
        &self,
        server: &str,
        bytes: &[u8],
        tree: Option<&[u8]>,
    ) -> Result<(SignedDescriptor, GroupContext)> {
        let info = super::info(bytes)?;
        let descriptor = client_context(info.group_context(), server)?;
        let context = info.group_context().clone();
        let tree_in = super::info_tree(&info, descriptor.descriptor.kind, tree)?;
        let verified = tree_in
            .clone()
            .into_verified(crate::SUITE, self.provider.crypto(), info.group_id())
            .map_err(err("ratchet tree"))?;
        self.tree_rule(&descriptor, &context, verified.leaves())?;
        // Verification only: a throwaway provider, and nothing is committed.
        MlsGroup::external_commit_builder()
            .with_ratchet_tree(tree_in)
            .skip_lifetime_validation()
            .build_group(&crate::crypto::Provider::default(), info, self.with_key())
            .map_err(err("group info"))?;
        Ok((descriptor, context))
    }

    /// Whether this device's own creation of a private group still holds authority (no time at
    /// hand: the pair it carries is the newest accepted one, its anchor is current under the pinned
    /// head and its member list is signed by an authority under the current owner). Other groups:
    /// always.
    pub(super) fn own_creation_authorized(&self, key: &str) -> Result<bool> {
        let record = self.groups.get(key).ok_or(Invalid("record"))?;
        let descriptor = SignedDescriptor::parse(&record.descriptor).ok_or(Invalid("descriptor"))?;
        if !rules::is_private(&descriptor) {
            return Ok(true);
        }
        let server = &descriptor.descriptor.server_key;
        let Some(server_record) = self.servers.get(server) else { return Ok(false) };
        let chain = server_record.chain(server)?;
        let Some((anchor, list)) = server_record.pair()? else { return Ok(false) };
        let group = self.mls(key)?;
        let access = rules::access(group.public_group().group_context()).ok_or(Invalid("access lists"))?;
        Ok(access.anchor().hash() == anchor.hash()
            && access.list().hash() == list.hash()
            && chain.anchor_current(access.anchor())
            && access.signer_authorized(&chain, true))
    }

    /// The verified pending tip of a private conversation, kept on its live pin.
    pub(super) fn pending_tip(&self, server: &str, conversation: &str) -> Result<Option<SignedDescriptor>> {
        let Some(live) = self.live(server, conversation)? else { return Ok(None) };
        let record = self.groups.get(&group_key(server, &live.group_id())).ok_or(Invalid("live pin"))?;
        record
            .pending_tip
            .as_ref()
            .map(|bytes| SignedDescriptor::parse(bytes).ok_or(Invalid("stored pending tip")))
            .transpose()
    }

    /// Accepts a private conversation's restarts (6c, "Restarts", staged adoption): the chain is
    /// verified as [`Agent::accept_chain`] does, then its final successor's current GroupInfo
    /// (`group_info`, `tree`) must give (a)–(c) as of now, as for a newcomer. Only then are the pins,
    /// the live incarnation and the superseding written. Otherwise nothing changes but the verified
    /// tip, kept as pending on the live pin (`adopted` false): the device waits for a channel
    /// manager, as a newcomer does.
    #[allow(clippy::too_many_arguments)]
    pub fn accept_private_chain(
        &mut self,
        server: &str,
        conversation: &str,
        chain: &[([u8; 32], Vec<u8>)],
        group_info: &[u8],
        tree: Option<&[u8]>,
        now: i64,
        revocations: &dyn Revocations,
    ) -> Result<ChainAdoption> {
        self.operation(|a| {
            super::current_time(now)?;
            let chain = a.verify_chain(server, conversation, chain)?;
            let tip = chain.last().ok_or(Invalid("chain anchor"))?.clone();
            if !rules::is_private(&tip) {
                return Err(Invalid("not a private conversation"));
            }
            let (descriptor, context) = a.verified_info(server, group_info, tree)?;
            if descriptor.group_id() != tip.group_id() {
                return Err(Invalid("the final successor's GroupInfo"));
            }
            // Its lists must be valid history (bound to it, signed by an authority of their span),
            // even to be kept as pending: a fabricated tip must not become a restart's predecessor.
            let pin_chain = a.owner_chain(server)?.ok_or(AgentError::Waiting)?;
            let access = rules::access(&context).ok_or(Invalid("access lists"))?;
            if !access.valid_when_signed(&pin_chain, &tip.group_id()) {
                return Err(Invalid("the final successor's access lists"));
            }
            let key = group_key(server, &tip.group_id());
            match a.newcomer(&key, &tip, &context, now, revocations) {
                Ok(checkpoint) => {
                    let group = a.adopt_chain(server, conversation, &chain)?;
                    // The list seen current now is the newest accepted (newcomer refused older ones).
                    if checkpoint.is_some() {
                        a.groups.get_mut(&key).ok_or(Invalid("record"))?.access = checkpoint;
                    }
                    Ok(ChainAdoption { group, adopted: true })
                }
                Err(AgentError::Waiting) => {
                    // Without any pin, the chain's verified first link is pinned (not joined) to
                    // carry the pending tip, so an authority can still restart from it.
                    let live = match a.live(server, conversation)? {
                        Some(live) => live,
                        None => {
                            let first = chain.first().ok_or(Invalid("chain anchor"))?.clone();
                            a.adopt_chain(server, conversation, std::slice::from_ref(&first))?;
                            first
                        }
                    };
                    if live.group_id() != tip.group_id() {
                        a.groups
                            .get_mut(&group_key(server, &live.group_id()))
                            .ok_or(Invalid("live pin"))?
                            .pending_tip = Some(tip.encode());
                    }
                    Ok(ChainAdoption { group: tip.group_id(), adopted: false })
                }
                Err(error) => Err(error),
            }
        })
    }

    /// The member list of a private group this device holds, for renewal and the interface.
    pub fn access_view(&self, server: &str, id: &[u8; 32]) -> Result<Option<AccessView>> {
        let key = group_key(server, id);
        let Some(record) = self.groups.get(&key) else { return Ok(None) };
        if record.status != Status::Active {
            return Ok(None);
        }
        let group = self.mls(&key)?;
        let Some(access) = rules::access(group.public_group().group_context()) else { return Ok(None) };
        let m = access.members().members();
        let (signer_device, signer_account, signer_expires_at) = match access.members().signer() {
            crate::Credential::Account(c) => (c.device_key.clone(), Some(c.account_key.clone()), Some(c.expires_at)),
            other => (other.device_key().to_owned(), None, None),
        };
        Ok(Some(AccessView {
            seq: m.seq,
            members: m.members.clone(),
            signer_uid: m.signer_uid.clone(),
            signer_device,
            signer_account,
            signer_expires_at,
            signed_at: m.signed_at,
        }))
    }

    fn commit_lists(
        &mut self,
        server: &str,
        id: &[u8; 32],
        lists: Lists<'_>,
        now: i64,
        revocations: &dyn Revocations,
    ) -> Result<Outbox> {
        self.operation(|a| {
            let key = group_key(server, id);
            let gate = a.gate(server, id, now, revocations)?;
            if gate.rejoin {
                return Err(AgentError::Rejoin);
            }
            if gate.status != Status::Active
                || a.outbox.contains_key(&key)
                || !a.credential.is_current(now, revocations)
            {
                return Err(AgentError::Blocked);
            }
            if a.groups[&key].server_pending {
                return Err(Invalid("fetch outstanding proposals first"));
            }
            let mut group = a.mls(&key)?;
            let descriptor = client_context(group.public_group().group_context(), server)?;
            if !rules::is_private(&descriptor) {
                return Err(Invalid("not a private channel"));
            }
            a.advance_access(&key, &group, now, revocations)?;
            let record = a.servers.get(server).ok_or(AgentError::Waiting)?;
            let chain = record.chain(server)?;
            let (anchor, list) = record.pair()?.ok_or(AgentError::Waiting)?;
            let current = rules::access(group.public_group().group_context()).ok_or(Invalid("access lists"))?;
            let checkpoint = a.groups[&key].access.clone();
            let members_none = matches!(lists, Lists::Members(None));
            let older = |m: &SignedMembers| checkpoint.as_ref().is_some_and(|c| !c.is(m) && !c.before(m));
            let access = match lists {
                // The next seq after the list this device accepted: a list it never accepted (never
                // current, or history only) sets no floor.
                Lists::Members(Some(m)) => {
                    let seq = checkpoint.as_ref().ok_or(AgentError::Waiting)?.seq;
                    let members =
                        a.sign_members(&chain, id, seq.checked_add(1).ok_or(Invalid("member list seq"))?, m, now)?;
                    Access::new(anchor, list, members)
                }
                // Never carry a member list older than the one this device accepted.
                Lists::Members(None) if older(current.members()) => return Err(AgentError::Waiting),
                Lists::Members(None) => Access::new(anchor, list, current.members().clone()),
                Lists::Offer(bytes) => {
                    let offered = Access::parse(bytes).ok_or(Invalid("offered access lists"))?;
                    let (m, c) = (offered.members(), current.members());
                    let behind = (m.members().generation, m.members().seq) < (c.members().generation, c.members().seq);
                    if offered.anchor().hash() != anchor.hash()
                        || offered.list().hash() != list.hash()
                        || older(m)
                        || behind
                    {
                        return Err(AgentError::Waiting);
                    }
                    offered
                }
            };
            // A removal-only repair keeps the lists the device holds current authority under (they
            // need not be renewable, e.g. a list a former owner signed); anything new must be current.
            let removal_only =
                members_none && matches!(gate.access, Some(Authority::Remove { .. })) && access == current;
            if !removal_only && !access.current(&chain, id, now, revocations) {
                return Err(AgentError::Waiting);
            }
            let encoded = access.encode();
            if encoded.len() > MAX_ACCESS_BYTES {
                return Err(Invalid("access lists too large"));
            }
            let mut removals: BTreeSet<u32> = excluded(&access, &chain, &group).into_iter().collect();
            removals.extend(gate.blocked);
            let changed = access != current;
            if !changed && removals.is_empty() {
                return Err(Invalid("nothing to commit"));
            }
            group
                .set_configuration(a.provider.storage(), &join_config(descriptor.descriptor.kind))
                .map_err(err("group config"))?;
            group.set_aad(ds::commit_aad(now));
            // The proposal carries the whole set: every other extension is copied as it is.
            let extensions: Vec<Extension> = group
                .public_group()
                .group_context()
                .extensions()
                .iter()
                .map(|e| match e.extension_type() {
                    ExtensionType::Unknown(ACCESS_EXTENSION) => {
                        Extension::Unknown(ACCESS_EXTENSION, UnknownExtension(encoded.clone()))
                    }
                    _ => e.clone(),
                })
                .collect();
            let mut builder = group.commit_builder();
            if changed {
                builder = builder
                    .propose_group_context_extensions(Extensions::from_vec(extensions).map_err(err("extensions"))?)
                    .map_err(err("access proposal"))?;
            }
            let bundle = builder
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
                &ds::CommitSender::Member(group.own_leaf_index().u32()),
                now,
            )?;
            let out = a.bundle(id, group.epoch().as_u64(), bundle, now)?;
            a.outbox.insert(key, out.clone());
            Ok(out)
        })
    }
}

/// The outcome of [`Agent::accept_private_chain`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ChainAdoption {
    /// The final successor.
    pub group: [u8; 32],
    /// False: kept as pending, waiting for a channel manager.
    pub adopted: bool,
}

/// A private group's member list as this device holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AccessView {
    pub seq: u64,
    pub members: Vec<Uid>,
    pub signer_uid: Uid,
    pub signer_device: String,
    pub signer_account: Option<String>,
    pub signer_expires_at: Option<i64>,
    pub signed_at: i64,
}

/// Where `commit_lists` takes its member list from.
enum Lists<'a> {
    /// A new list with these members, signed by this device, or (`None`) the group's own.
    Members(Option<&'a [Uid]>),
    /// An offered extension.
    Offer(&'a [u8]),
}

/// Leaves whose users the lists do not admit now (the owner being only the current one).
fn excluded(access: &Access, chain: &OwnerChain, group: &MlsGroup) -> Vec<u32> {
    group
        .public_group()
        .treesync()
        .full_leaves()
        .filter(|(_, leaf)| leaf_credential(leaf).is_none_or(|c| !access.admits_now(chain, &c.uid())))
        .map(|(index, _)| index.u32())
        .collect()
}

#[cfg(test)]
mod tests;
