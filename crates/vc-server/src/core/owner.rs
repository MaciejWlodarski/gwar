//! The server's owner and the managers of private channels (docs/e2e-build.md, section 6,
//! "Owner chain (6b)" and "Managers (6b)"). The server keeps the chain of the current
//! generation and the newest anchor and list, and checks every change as a reader would.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use gwar_mls::Revocations;
use gwar_mls::owner::{Genesis, OwnerChain, SignedAnchor, SignedLink, SignedList};
use vc_proto::{ErrorCode, Event, Managers, OwnerInfo, OwnerLinkStatement, Permission, Response, Uid};

use super::{Core, Reply, err, now_ms, storage_error};
use crate::store::{ADMIN_GROUP, Store};

/// The owner chain and the newest manager pair of the current generation.
pub(super) struct OwnerState {
    pub(super) chain: OwnerChain,
    pub(super) pair: Option<(SignedAnchor, SignedList)>,
}

/// Devices revoked on Gwar Connect, as the server has learned them.
pub(super) struct StoredRevocations<'a>(pub(super) &'a Store);

impl Revocations for StoredRevocations<'_> {
    fn revoked_at(&self, account_key: &str, device_key: &str) -> Option<i64> {
        self.0.device_revoked_at(account_key, device_key).ok().flatten()
    }
}

/// Loads the stored chain and pair; refuses to start on a record that does not verify.
pub(super) fn load(store: &Store, server_key: &str) -> anyhow::Result<Option<OwnerState>> {
    let Some(genesis) = store.owner_genesis()? else { return Ok(None) };
    let genesis =
        Genesis { server_key: server_key.to_owned(), generation: genesis.generation, owner_uid: genesis.owner_uid };
    let links = store
        .owner_links(genesis.generation)?
        .into_iter()
        .map(|(statement, signature)| SignedLink::parse(&statement, &signature))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| anyhow::anyhow!("a stored owner link does not verify"))?;
    let chain =
        OwnerChain::verify(genesis, links).ok_or_else(|| anyhow::anyhow!("the stored owner chain does not verify"))?;
    let pair = match store.manager_pair(chain.generation())? {
        Some((anchor, list)) => Some(
            SignedAnchor::parse(&anchor)
                .zip(SignedList::parse(&list))
                .filter(|(a, l)| chain.list_valid(a, l))
                .ok_or_else(|| anyhow::anyhow!("the stored manager pair does not verify"))?,
        ),
        None => None,
    };
    Ok(Some(OwnerState { chain, pair }))
}

fn encoded(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

impl Core {
    pub(super) fn owner_info(&self) -> Option<OwnerInfo> {
        self.owner.as_ref().map(|o| OwnerInfo {
            uid: o.chain.owner(),
            generation: o.chain.generation(),
            seq: o.chain.seq(),
            head: o.chain.head(),
        })
    }

    /// Members holding `channel_edit` (Admin included), sorted.
    fn channel_editors(&self) -> anyhow::Result<Vec<Uid>> {
        let groups: Vec<_> = self
            .groups
            .values()
            .filter(|g| {
                g.permissions.contains(&Permission::ManageChannels)
                    || g.permissions.contains(&Permission::Administrator)
            })
            .map(|g| g.id)
            .collect();
        self.store.uids_in_groups(&groups)
    }

    /// What `Welcome.managers` and `managers.updated` report.
    pub(super) fn managers_view(&self) -> Option<Managers> {
        let owner = self.owner.as_ref()?;
        let editors = self.channel_editors().inspect_err(|e| tracing::warn!("store: {e:#}")).unwrap_or_default();
        let (confirmers, managers): (&[Uid], &[Uid]) = match &owner.pair {
            Some((a, l)) => (&a.anchor().confirmers, &l.list().managers),
            None => (&[], &[]),
        };
        let me = owner.chain.owner();
        let pending =
            editors.iter().filter(|u| **u != me && !confirmers.contains(u) && !managers.contains(u)).cloned().collect();
        let mut stale: Vec<Uid> = managers.iter().chain(confirmers).filter(|u| !editors.contains(u)).cloned().collect();
        stale.sort();
        Some(Managers {
            anchor: owner.pair.as_ref().map(|(a, _)| encoded(&a.encode())),
            list: owner.pair.as_ref().map(|(_, l)| encoded(&l.encode())),
            pending,
            stale,
        })
    }

    /// Tells everyone when the managers' view changed (a role, the owner or the pair).
    pub(super) fn reconcile_managers(&mut self, before: Option<Managers>) {
        let after = self.managers_view();
        if after != before
            && let Some(view) = after
        {
            self.broadcast(Event::ManagersUpdated(view), |_| true);
        }
    }

    pub(super) fn owner_chain(&self, generation: u64, after: u64) -> Reply {
        let Some(owner) = self.owner.as_ref().filter(|o| o.chain.generation() == generation) else {
            return Err(err(ErrorCode::NotFound, "no owner chain of that generation"));
        };
        let (links, more) = owner.chain.page(after);
        Ok(Response::OwnerChain {
            genesis: owner.chain.genesis().statement(),
            links: links
                .iter()
                .map(|l| OwnerLinkStatement { statement: l.link().statement(), signature: l.signature().to_owned() })
                .collect(),
            next: more.then(|| links.last().map_or(after, |l| l.link().seq)),
        })
    }

    pub(super) fn owner_transfer(&mut self, statement: &str, signature: &str) -> Reply {
        let Some(owner) = &self.owner else { return Err(err(ErrorCode::BadRequest, "this server has no owner")) };
        let link =
            SignedLink::parse(statement, signature).ok_or_else(|| err(ErrorCode::BadRequest, "invalid owner link"))?;
        // The client signed against a head and an anchor; if either moved, it re-signs.
        let anchor = owner.pair.as_ref().map(|(a, _)| a.hash());
        if link.link().previous_link_hash != owner.chain.head() || link.link().anchor_hash != anchor {
            return Err(err(ErrorCode::Conflict, "the owner record changed; sign again"));
        }
        let mut chain = owner.chain.clone();
        chain.push(link.clone()).ok_or_else(|| err(ErrorCode::BadRequest, "the link is not signed by the owner"))?;
        let new_owner = link.link().new_owner_uid.clone();
        let Some((user_id, _)) = self.store.user_by_uid(&new_owner).map_err(storage_error)? else {
            return Err(err(ErrorCode::NotFound, "the new owner is not a member of this server"));
        };
        let before = self.managers_view();
        let admissions = self.admissions();
        self.store
            .append_owner_link(chain.generation(), chain.seq(), statement, signature, &new_owner)
            .map_err(storage_error)?;
        self.owner.as_mut().expect("checked above").chain = chain;
        // Private channels admit the current owner (6c).
        self.refresh_all_private();
        // The new owner holds the Admin group (the old owner keeps their roles).
        let online: Vec<_> = self.sessions.values().filter(|s| s.user_id == user_id).map(|s| s.id).collect();
        for s in online {
            if let Some(session) = self.sessions.get_mut(&s)
                && !session.groups.contains(&ADMIN_GROUP)
            {
                session.groups.push(ADMIN_GROUP);
            }
            self.client_updated(s);
        }
        self.announce_member(&new_owner);
        self.reconcile_admissions(admissions);
        self.enforce_connect_everywhere();
        let info = self.owner_info().expect("an owner");
        self.broadcast(Event::OwnerUpdated(info), |_| true);
        self.reconcile_managers(before);
        Ok(Response::Empty {})
    }

    pub(super) fn managers_update(&mut self, anchor: &str, list: &str) -> Reply {
        let Some(owner) = &self.owner else { return Err(err(ErrorCode::BadRequest, "this server has no owner")) };
        let decode = |value: &str| URL_SAFE_NO_PAD.decode(value).ok();
        let anchor = decode(anchor)
            .and_then(|b| SignedAnchor::parse(&b))
            .ok_or_else(|| err(ErrorCode::BadRequest, "invalid confirmer anchor"))?;
        let list = decode(list)
            .and_then(|b| SignedList::parse(&b))
            .ok_or_else(|| err(ErrorCode::BadRequest, "invalid manager list"))?;
        // As a reader of the current chain would accept it, now.
        if !owner.chain.list_current(&anchor, &list, now_ms(), &StoredRevocations(&self.store)) {
            return Err(err(ErrorCode::BadRequest, "the anchor and list are not current under this server's owner"));
        }
        if let Some((a, l)) = &owner.pair {
            let anchor_ok = anchor.anchor().anchor_seq > a.anchor().anchor_seq
                || (anchor.anchor().anchor_seq == a.anchor().anchor_seq && anchor.hash() == a.hash());
            let list_ok =
                list.list().seq > l.list().seq || (list.list().seq == l.list().seq && list.hash() == l.hash());
            if !anchor_ok || !list_ok {
                return Err(err(ErrorCode::Conflict, "a newer anchor or list is already in place"));
            }
            if list.list().seq == l.list().seq {
                return Ok(Response::Empty {});
            }
        }
        let before = self.managers_view();
        let admissions = self.admissions();
        let generation = owner.chain.generation();
        self.store.set_manager_pair(generation, &anchor.encode(), &list.encode()).map_err(storage_error)?;
        self.owner.as_mut().expect("checked above").pair = Some((anchor, list));
        // Confirmers and managers are admitted to every encrypted private channel (6c).
        self.reconcile_admissions(admissions);
        self.enforce_connect_everywhere();
        self.reconcile_managers(before);
        Ok(Response::Empty {})
    }
}
