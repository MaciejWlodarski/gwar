//! Encrypted private channels on the server (docs/e2e-build.md, section 6, 6c): who their groups
//! admit, the creator's authority and the live checkpoints of their member lists.

use std::collections::BTreeSet;

use gwar_mls::access::{Access, MAX_ACCESS_BYTES};
use gwar_mls::ds::Observer;
use gwar_mls::owner::OwnerChain;
use gwar_proto::{ChannelId, ErrorBody, ErrorCode, Uid};

use super::{Core, Reply, err, mls::channel_id, now_ms, storage_error};
use gwar_proto::{Event, MlsEvent, Response, SessionId};

impl Core {
    /// The owner chain a private channel's lists are checked against.
    pub(super) fn current_chain(&self) -> Option<OwnerChain> {
        self.owner.as_ref().map(|o| o.chain.clone())
    }

    /// The owner, confirmers and managers of the server's newest pair: always admitted, so that
    /// they can join and repair any private channel.
    pub(super) fn private_authorities(&self) -> BTreeSet<Uid> {
        let Some(owner) = &self.owner else { return BTreeSet::new() };
        let mut all = BTreeSet::from([owner.chain.owner()]);
        if let Some((anchor, list)) = &owner.pair {
            all.extend(anchor.anchor().confirmers.iter().cloned());
            all.extend(list.list().managers.iter().cloned());
        }
        all
    }

    pub(super) fn is_encrypted_private(&self, channel: ChannelId) -> bool {
        self.channels.get(&channel).is_some_and(|c| c.private && c.encrypted)
    }

    /// Whether an encrypted private channel admits `uid` now: its live group's lists (with the
    /// current owner) or the server's newest authorities.
    pub(super) fn private_admits(&self, channel: ChannelId, uid: &str) -> bool {
        self.private_authorities().contains(uid)
            || self.private_access.get(&channel).is_some_and(|set| set.contains(uid))
    }

    /// Recomputes who a private channel's live group admits from its current lists.
    pub(super) fn refresh_private(&mut self, channel: ChannelId) {
        let set = (|| -> anyhow::Result<BTreeSet<Uid>> {
            let Some(row) = self.store.mls_live_group(&format!("channel:{channel}"))? else {
                return Ok(BTreeSet::new());
            };
            let chain = self.current_chain().ok_or_else(|| anyhow::anyhow!("no owner"))?;
            let observer = Observer::load(&row.observer, &self.server_key)?.with_owner(Some(chain.clone()));
            Ok(observer.access().map(|a| admitted_now(&a, &chain)).unwrap_or_default())
        })()
        .inspect_err(|e| tracing::warn!(channel, "private channel access: {e:#}"))
        .unwrap_or_default();
        self.private_access.insert(channel, set);
    }

    /// Every encrypted private channel's admitted set, at startup.
    pub(super) fn refresh_all_private(&mut self) {
        let channels: Vec<_> = self.channels.values().filter(|c| c.private && c.encrypted).map(|c| c.id).collect();
        for channel in channels {
            self.refresh_private(channel);
        }
    }

    /// A private channel's first group: created by the owner, a confirmer or a manager, under the
    /// server's newest pair, with a member list bound to it at seq 1, current now.
    pub(super) fn check_private_create(&self, uid: &str, observer: &Observer) -> Result<(), ErrorBody> {
        let refuse = |why: &str| Err(err(ErrorCode::Forbidden, why));
        let (Some(owner), Some(chain)) = (&self.owner, self.current_chain()) else {
            return refuse("a private channel needs the server's owner and managers");
        };
        if !self.private_authorities().contains(uid) {
            return refuse("only the owner, a confirmer or a manager creates a private channel's group");
        }
        let access = observer.access().ok_or_else(|| err(ErrorCode::BadRequest, "no access lists"))?;
        if observer.descriptor().descriptor.founding_owner.as_deref() != Some(chain.owner().as_str())
            || !same_pair(owner.pair.as_ref(), &access)
            || access.members().members().seq != 1
            || !access.current(&chain, &observer.group_id(), now_ms(), &super::owner::StoredRevocations(&self.store))
        {
            return Err(err(ErrorCode::BadRequest, "the access lists are not current under this server's managers"));
        }
        Ok(())
    }

    /// A live commit that replaces a private channel's lists: under the server's newest pair, current
    /// now, and newer than the group's checkpoint (the same seq only with the same list). Returns the
    /// checkpoint to store.
    pub(super) fn check_private_commit(
        &self,
        group: &str,
        before: Option<&Access>,
        after: &Observer,
    ) -> Result<Option<(u64, String)>, ErrorBody> {
        let Some(access) = after.access() else { return Ok(None) };
        if before.is_some_and(|b| b.encode() == access.encode()) {
            return Ok(None);
        }
        let conflict = |why: &str| Err(err(ErrorCode::Conflict, why));
        let (Some(owner), Some(chain)) = (&self.owner, self.current_chain()) else {
            return conflict("this server has no owner");
        };
        if !same_pair(owner.pair.as_ref(), &access)
            || !access.current(&chain, &after.group_id(), now_ms(), &super::owner::StoredRevocations(&self.store))
        {
            return conflict("the access lists are not current under this server's managers");
        }
        let (seq, hash) = (access.members().members().seq, access.members().hash());
        if let Some((old_seq, old_hash)) = self.store.mls_access_checkpoint(group).map_err(storage_error)?
            && (seq < old_seq || (seq == old_seq && hash != old_hash))
        {
            return conflict("an older member list");
        }
        Ok(Some((seq, hash)))
    }

    /// `mls.access.offer`: a channel authority that cannot commit to a private channel's live group
    /// offers fresh lists, which go to the group's readers for a member to commit (6c, "Offers").
    /// The server checks them as it would a commit: its newest pair, a current member list signed
    /// by the offering user, bound to the group and no older than the checkpoint. It grants nothing.
    pub(super) fn mls_access_offer(&mut self, session: SessionId, group: &str, extension: &str) -> Reply {
        let row = self.mls_group_for(session, group)?;
        if self.private_channel_of(&row.conversation).is_none() {
            return Err(super::mls::bad("not a private channel's group"));
        }
        if self
            .store
            .mls_live_group(&row.conversation)
            .map_err(storage_error)?
            .is_none_or(|live| live.group != row.group)
        {
            return Err(err(ErrorCode::Conflict, "not the conversation's live group"));
        }
        let bytes = super::mls::blob(extension, MAX_ACCESS_BYTES)?;
        let access = Access::parse(&bytes).ok_or_else(|| super::mls::bad("invalid access lists"))?;
        let id: [u8; 32] = gwar_proto::decode_fixed::<32>(group).ok_or_else(|| super::mls::bad("invalid group id"))?;
        let (Some(owner), Some(chain)) = (&self.owner, self.current_chain()) else {
            return Err(err(ErrorCode::Conflict, "this server has no owner"));
        };
        if access.members().members().signer_uid != self.sessions[&session].uid {
            return Err(err(ErrorCode::Forbidden, "the offering user signs the member list"));
        }
        if !same_pair(owner.pair.as_ref(), &access)
            || !access.current(&chain, &id, now_ms(), &super::owner::StoredRevocations(&self.store))
        {
            return Err(err(ErrorCode::Conflict, "the access lists are not current under this server's managers"));
        }
        let (seq, hash) = (access.members().members().seq, access.members().hash());
        if let Some((old_seq, old_hash)) = self.store.mls_access_checkpoint(group).map_err(storage_error)?
            && (seq < old_seq || (seq == old_seq && hash != old_hash))
        {
            return Err(err(ErrorCode::Conflict, "an older member list"));
        }
        let readers = self.mls_readers(&row.conversation);
        self.broadcast(Event::Mls(MlsEvent::AccessOffered { group: group.into(), extension: extension.into() }), |s| {
            readers.contains(&s.id)
        });
        Ok(Response::Empty {})
    }

    /// The private channel a group belongs to, if any.
    pub(super) fn private_channel_of(&self, conversation: &str) -> Option<ChannelId> {
        channel_id(conversation).filter(|&c| self.is_encrypted_private(c))
    }
}

/// Members, confirmers, managers and the current owner.
fn admitted_now(access: &Access, chain: &OwnerChain) -> BTreeSet<Uid> {
    let mut all = BTreeSet::from([chain.owner()]);
    all.extend(access.anchor().anchor().confirmers.iter().cloned());
    all.extend(access.list().list().managers.iter().cloned());
    all.extend(access.members().members().members.iter().cloned());
    all
}

fn same_pair(pair: Option<&(gwar_mls::owner::SignedAnchor, gwar_mls::owner::SignedList)>, access: &Access) -> bool {
    pair.is_some_and(|(a, l)| a.hash() == access.anchor().hash() && l.hash() == access.list().hash())
}
