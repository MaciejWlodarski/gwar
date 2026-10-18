//! Encrypted private channels on the server (docs/e2e-build.md, section 6, 6c): who their groups
//! admit, the creator's authority and the live checkpoints of their member lists.

use std::collections::BTreeSet;

use gwar_mls::access::Access;
use gwar_mls::ds::Observer;
use gwar_mls::owner::OwnerChain;
use vc_proto::{ChannelId, ErrorBody, ErrorCode, Uid};

use super::{Core, err, mls::channel_id, now_ms, storage_error};

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
