//! The device's pins of a server's owner and managers (docs/e2e-build.md, section 6, 6b):
//! the record `{server}/@server` holds the verified owner chain and the newest current
//! anchor and list this device accepted, which are its checkpoints.

use serde::{Deserialize, Serialize};
use vc_proto::Uid;

use super::{Agent, Invalid, Result};
use crate::Revocations;
use crate::owner::{Genesis, ManagerList, OwnerChain, SignedAnchor, SignedLink, SignedList};

pub(super) const SUFFIX: &str = "/@server";

/// A server's owner pin and manager checkpoints, written atomically.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerRecord {
    pub genesis: String,
    /// `(statement, signature)` of every link of the pinned generation.
    pub links: Vec<(String, String)>,
    /// The newest accepted anchor and list (their envelopes), both current when accepted.
    pub anchor: Option<Vec<u8>>,
    pub list: Option<Vec<u8>>,
}

/// What a device learned from the server's owner chain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum OwnerCheck {
    /// First contact: pinned (trust on first use, or the owner an invite named).
    Pinned,
    Unchanged,
    /// The pinned chain grew: ownership moved.
    Extended,
    /// Not an extension of the pin (another chain, a lower seq or generation): disconnect.
    Changed,
    /// An owner reset: a neutral notice, accepted only explicitly ([`Agent::accept_generation`]).
    NewGeneration {
        generation: u64,
        owner: Uid,
    },
}

/// What a device made of the server's newest anchor and list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ManagersCheck {
    Accepted,
    Unchanged,
    /// Not current under the pinned chain (a former owner, an expired signer, not a confirmer).
    NotCurrent,
    /// Not current, but its anchor is, and names this device's user owner or confirmer: this
    /// device may renew the list ([`Agent::next_list`] with `managers` `None`).
    Renewable,
    /// Older than, or different at the same seq from, what this device accepted.
    Rollback,
}

/// The pinned owner and the managers this device accepted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OwnerView {
    pub generation: u64,
    pub seq: u64,
    pub head: String,
    pub owner: Uid,
    pub confirmers: Vec<Uid>,
    pub managers: Vec<Uid>,
    /// Who signed the accepted list, and when.
    pub list_signer: Option<Uid>,
    pub list_signed_at: Option<i64>,
    /// When the list signer's device certificate expires (`None`: a local identity, or no list).
    pub list_signer_expires_at: Option<i64>,
    pub anchor_seq: Option<u64>,
    pub list_seq: Option<u64>,
}

fn chain_of(server: &str, genesis: &str, links: &[(String, String)]) -> Option<OwnerChain> {
    let genesis = Genesis::parse(genesis).filter(|g| g.server_key == server)?;
    let links = links.iter().map(|(s, sig)| SignedLink::parse(s, sig)).collect::<Option<Vec<_>>>()?;
    OwnerChain::verify(genesis, links)
}

impl ServerRecord {
    fn chain(&self, server: &str) -> Result<OwnerChain> {
        chain_of(server, &self.genesis, &self.links).ok_or(Invalid("stored owner chain"))
    }

    fn pair(&self) -> Result<Option<(SignedAnchor, SignedList)>> {
        match (&self.anchor, &self.list) {
            (Some(a), Some(l)) => Ok(Some((
                SignedAnchor::parse(a).ok_or(Invalid("stored anchor"))?,
                SignedList::parse(l).ok_or(Invalid("stored list"))?,
            ))),
            (None, None) => Ok(None),
            _ => Err(Invalid("stored manager pair")),
        }
    }

    /// Checks a loaded record: its chain verifies and its pair is valid history of it.
    pub(super) fn validate(&self, server: &str) -> Result<()> {
        let chain = self.chain(server)?;
        if let Some((a, l)) = self.pair()?
            && !chain.list_valid(&a, &l)
        {
            return Err(Invalid("stored manager pair"));
        }
        Ok(())
    }
}

impl Agent {
    /// The pinned owner and accepted managers of `server`, if any.
    pub fn owner_view(&self, server: &str) -> Result<Option<OwnerView>> {
        let Some(record) = self.servers.get(server) else { return Ok(None) };
        let chain = record.chain(server)?;
        let pair = record.pair()?;
        Ok(Some(OwnerView {
            generation: chain.generation(),
            seq: chain.seq(),
            head: chain.head(),
            owner: chain.owner(),
            confirmers: pair.as_ref().map(|(a, _)| a.anchor().confirmers.clone()).unwrap_or_default(),
            managers: pair.as_ref().map(|(_, l)| l.list().managers.clone()).unwrap_or_default(),
            list_signer: pair.as_ref().map(|(_, l)| l.list().signer_uid.clone()),
            list_signed_at: pair.as_ref().map(|(_, l)| l.list().signed_at),
            list_signer_expires_at: pair.as_ref().and_then(|(_, l)| match l.signer() {
                crate::Credential::Account(c) => Some(c.expires_at),
                crate::Credential::Local { .. } => None,
            }),
            anchor_seq: pair.as_ref().map(|(a, _)| a.anchor().anchor_seq),
            list_seq: pair.as_ref().map(|(_, l)| l.list().seq),
        }))
    }

    /// Checks the server's owner chain against the pin: pins on first contact (refusing a chain
    /// whose owner is not the one `invite_owner` names), follows extensions, and reports anything
    /// else without changing the pin.
    pub fn check_owner(
        &mut self,
        server: &str,
        genesis: &str,
        links: Vec<(String, String)>,
        invite_owner: Option<&str>,
    ) -> Result<OwnerCheck> {
        self.operation(|a| {
            // A chain that does not verify is no owner record of this server: the caller disconnects.
            let Some(chain) = chain_of(server, genesis, &links) else { return Ok(OwnerCheck::Changed) };
            let Some(record) = a.servers.get(server) else {
                if invite_owner.is_some_and(|owner| owner != chain.owner()) {
                    return Ok(OwnerCheck::Changed);
                }
                let record = ServerRecord { genesis: genesis.to_owned(), links, anchor: None, list: None };
                a.servers.insert(server.to_owned(), record);
                return Ok(OwnerCheck::Pinned);
            };
            let pinned = record.chain(server)?;
            if chain.generation() > pinned.generation() {
                return Ok(OwnerCheck::NewGeneration { generation: chain.generation(), owner: chain.owner() });
            }
            // The same generation, reaching the pinned head at the pinned seq.
            let extends = chain.generation() == pinned.generation()
                && chain.seq() >= pinned.seq()
                && chain.position(&pinned.head()) == Some(pinned.seq() as usize);
            if !extends {
                return Ok(OwnerCheck::Changed);
            }
            if chain.seq() == pinned.seq() {
                return Ok(OwnerCheck::Unchanged);
            }
            a.servers.get_mut(server).expect("present").links = links;
            Ok(OwnerCheck::Extended)
        })
    }

    /// Accepts a new owner generation the user confirmed: re-pins and clears every checkpoint of
    /// the old one, in one write.
    pub fn accept_generation(&mut self, server: &str, genesis: &str, links: Vec<(String, String)>) -> Result<()> {
        self.operation(|a| {
            let chain = chain_of(server, genesis, &links).ok_or(Invalid("owner chain"))?;
            if let Some(record) = a.servers.get(server)
                && record.chain(server)?.generation() >= chain.generation()
            {
                return Err(Invalid("not a newer generation"));
            }
            a.servers.insert(
                server.to_owned(),
                ServerRecord { genesis: genesis.to_owned(), links, anchor: None, list: None },
            );
            Ok(())
        })
    }

    /// Accepts the server's newest anchor and list when they are current now under the pinned
    /// chain and no older than the checkpoints.
    pub fn accept_managers(
        &mut self,
        server: &str,
        anchor: &[u8],
        list: &[u8],
        now: i64,
        revocations: &dyn Revocations,
    ) -> Result<ManagersCheck> {
        self.operation(|a| {
            let Some(record) = a.servers.get(server) else { return Ok(ManagersCheck::NotCurrent) };
            let chain = record.chain(server)?;
            let (Some(new_anchor), Some(new_list)) = (SignedAnchor::parse(anchor), SignedList::parse(list)) else {
                return Ok(ManagersCheck::NotCurrent);
            };
            // Current now, even when it is the pair already accepted: a replayed pair whose signer
            // expired, was revoked or lost authority confers nothing (the checkpoint stays).
            let current = chain.list_current(&new_anchor, &new_list, now, revocations);
            if let Some((old_anchor, old_list)) = record.pair()? {
                let (a_seq, l_seq) = (new_anchor.anchor().anchor_seq, new_list.list().seq);
                let anchor_ok = a_seq > old_anchor.anchor().anchor_seq
                    || (a_seq == old_anchor.anchor().anchor_seq && new_anchor.hash() == old_anchor.hash());
                let list_ok =
                    l_seq > old_list.list().seq || (l_seq == old_list.list().seq && new_list.hash() == old_list.hash());
                if !anchor_ok || !list_ok {
                    return Ok(ManagersCheck::Rollback);
                }
                if !current {
                    return Ok(lapsed(a, &chain, &new_anchor));
                }
                if l_seq == old_list.list().seq {
                    return Ok(ManagersCheck::Unchanged);
                }
            }
            if !current {
                return Ok(lapsed(a, &chain, &new_anchor));
            }
            let record = a.servers.get_mut(server).expect("present");
            record.anchor = Some(anchor.to_vec());
            record.list = Some(list.to_vec());
            Ok(ManagersCheck::Accepted)
        })
    }

    /// The next confirmer anchor's statement under the pinned head, for the owner's account key
    /// to sign. Sorted, deduplicated confirmers; the owner is never one.
    /// The seq is beyond both the accepted anchor and `server_anchor` (the server's, when it is valid
    /// as history), so a fresh device of a new owner can replace an inherited anchor. With a
    /// checkpoint only a current server anchor counts: a former owner's history cannot raise it.
    pub fn next_anchor(
        &self,
        server: &str,
        confirmers: &[Uid],
        signed_at: i64,
        server_anchor: Option<&[u8]>,
    ) -> Result<String> {
        let record = self.servers.get(server).ok_or(Invalid("no owner pin"))?;
        let chain = record.chain(server)?;
        let accepted = record.pair()?.map(|(a, _)| a);
        let mut confirmers = confirmers.to_vec();
        confirmers.sort();
        confirmers.dedup();
        if confirmers.contains(&chain.owner()) || confirmers.len() > crate::owner::MAX_CONFIRMERS {
            return Err(Invalid("confirmers"));
        }
        let server_seq = server_anchor
            .and_then(SignedAnchor::parse)
            .filter(|a| chain.anchor_valid(a))
            .filter(|a| accepted.is_none() || chain.anchor_current(a))
            .map_or(0, |a| a.anchor().anchor_seq);
        let seq = successor(accepted.as_ref().map_or(0, |a| a.anchor().anchor_seq).max(server_seq))?;
        Ok(crate::owner::Anchor {
            server_key: server.to_owned(),
            generation: chain.generation(),
            owner_head: chain.head(),
            anchor_seq: seq,
            confirmers,
            signed_at,
        }
        .statement())
    }

    /// The anchor a new statement may be signed under: `supplied` when it is current under the pin
    /// and no older than the accepted one (same seq only with the same statement), else the
    /// accepted anchor.
    fn signing_anchor(&self, server: &str, supplied: Option<&[u8]>) -> Result<(OwnerChain, SignedAnchor)> {
        let record = self.servers.get(server).ok_or(Invalid("no owner pin"))?;
        let chain = record.chain(server)?;
        let accepted = record.pair()?.map(|(a, _)| a);
        let anchor = match supplied {
            Some(bytes) => {
                let anchor = SignedAnchor::parse(bytes).ok_or(Invalid("anchor"))?;
                if let Some(old) = &accepted {
                    let seq = anchor.anchor().anchor_seq;
                    if seq < old.anchor().anchor_seq || (seq == old.anchor().anchor_seq && anchor.hash() != old.hash())
                    {
                        return Err(Invalid("an anchor older than the accepted one"));
                    }
                }
                anchor
            }
            None => accepted.ok_or(Invalid("no anchor"))?,
        };
        if !chain.anchor_current(&anchor) {
            return Err(Invalid("anchor not current"));
        }
        Ok((chain, anchor))
    }

    /// The next manager list's statement under `anchor` (the accepted one when `None`), signed
    /// by this device ([`Agent::sign_manager_list`]), which must be the current owner's or a
    /// confirmer's. `managers` `None` renews: it keeps the server's list (`server_list`, when it
    /// verifies as history under a trusted anchor and is no older than the accepted list), else the
    /// accepted one. Its seq follows both. Trusted anchors are the signing and the accepted one; the
    /// server's own (`server_anchor`) only on a device without a checkpoint, which inherits it.
    #[allow(clippy::too_many_arguments)]
    pub fn next_list(
        &self,
        server: &str,
        anchor: Option<&[u8]>,
        managers: Option<&[Uid]>,
        signed_at: i64,
        server_anchor: Option<&[u8]>,
        server_list: Option<&[u8]>,
        revocations: &dyn Revocations,
    ) -> Result<String> {
        let (chain, anchor) = self.signing_anchor(server, anchor)?;
        let me = self.credential.uid();
        if me != chain.owner() && !anchor.anchor().confirmers.contains(&me) {
            return Err(Invalid("only the owner or a confirmer signs the list"));
        }
        let record = self.servers.get(server).expect("checked by signing_anchor");
        let pair = record.pair()?;
        // A superseded anchor never vouches for a list (its former confirmers could sign any seq),
        // and a replayed older list never renews removed managers.
        let shown = server_anchor.and_then(SignedAnchor::parse).filter(|a| pair.is_none() && chain.anchor_valid(a));
        let trusted: Vec<&SignedAnchor> =
            [Some(&anchor), pair.as_ref().map(|(a, _)| a), shown.as_ref()].into_iter().flatten().collect();
        // With a checkpoint, a newer list counts only when its signer still holds authority under
        // its anchor (the current owner or a confirmer): a former owner's later list does not.
        let verified = server_list
            .and_then(SignedList::parse)
            .filter(|l| {
                trusted.iter().any(|a| {
                    let signer = &l.list().signer_uid;
                    a.hash() == l.list().anchor_hash
                        && chain.list_valid(a, l)
                        && (pair.is_none() || *signer == chain.owner() || a.anchor().confirmers.contains(signer))
                })
            })
            .filter(|l| {
                pair.as_ref().is_none_or(|(_, old)| {
                    let (seq, old_seq) = (l.list().seq, old.list().seq);
                    seq > old_seq || (seq == old_seq && l.hash() == old.hash())
                })
            });
        let accepted = pair.as_ref().map(|(_, l)| l.list().clone());
        let floor = accepted.as_ref().map_or(0, |l| l.seq).max(verified.as_ref().map_or(0, |l| l.list().seq));
        // Renewal copies the membership of a list this device has not accepted only when its signer
        // device is current now (an expired or revoked device key could sign anything); its seq
        // still counts, or the renewal could not follow it.
        let members_from = verified.as_ref().filter(|l| {
            pair.as_ref().is_none_or(|(_, old)| l.hash() == old.hash() || l.signer().is_current(signed_at, revocations))
        });
        let mut managers = match managers {
            Some(m) => m.to_vec(),
            None => members_from
                .map(|l| l.list().managers.clone())
                .or_else(|| accepted.as_ref().map(|l| l.managers.clone()))
                .ok_or(Invalid("no list to renew"))?,
        };
        managers.sort();
        managers.dedup();
        managers.retain(|m| !anchor.anchor().confirmers.contains(m) && *m != chain.owner());
        Ok(ManagerList {
            server_key: server.to_owned(),
            generation: chain.generation(),
            anchor_hash: anchor.hash(),
            seq: successor(floor)?,
            managers,
            signer_uid: me,
            signed_at,
        }
        .statement())
    }

    /// The statement of a link moving ownership to `new_owner`, for the current owner's account key
    /// (`account_key`) to sign. It hands over the anchor this device accepted, which must be the
    /// server's current one (`server_anchor`): a server showing another anchor is refused.
    pub fn transfer_statement(
        &self,
        server: &str,
        account_key: &str,
        new_owner: &str,
        server_anchor: Option<&[u8]>,
        signed_at: i64,
    ) -> Result<String> {
        let record = self.servers.get(server).ok_or(Invalid("no owner pin"))?;
        let chain = record.chain(server)?;
        let accepted = record.pair()?.map(|(a, _)| a);
        let shown = server_anchor.map(|b| SignedAnchor::parse(b).ok_or(Invalid("anchor"))).transpose()?;
        let anchor_hash = match (accepted, shown) {
            (Some(accepted), Some(shown)) if accepted.hash() == shown.hash() => Some(accepted.hash()),
            (Some(_), _) => return Err(Invalid("the server's anchor is not the one this device accepted")),
            (None, Some(shown)) if chain.anchor_current(&shown) => Some(shown.hash()),
            (None, Some(_)) => return Err(Invalid("anchor not current")),
            (None, None) => None,
        };
        let link = crate::owner::OwnerLink {
            server_key: server.to_owned(),
            generation: chain.generation(),
            seq: chain.seq() + 1,
            previous_link_hash: chain.head(),
            previous_owner_account_key: account_key.to_owned(),
            new_owner_uid: new_owner.to_owned(),
            anchor_hash,
            signed_at,
        };
        let statement = link.statement();
        crate::owner::OwnerLink::parse(&statement).ok_or(Invalid("owner link"))?;
        Ok(statement)
    }

    /// Signs a manager list with this device; its signer must be this device's uid.
    pub fn sign_manager_list(&self, list: ManagerList) -> Result<Vec<u8>> {
        if list.signer_uid != self.credential.uid() {
            return Err(Invalid("list signer"));
        }
        Ok(SignedList::sign(list, self.credential.clone(), &self.signer.0).encode())
    }
}

/// A pair that is not current: renewable by this device when its anchor is current and names this
/// device's user the owner or a confirmer. An inherited anchor naming the new owner a confirmer
/// needs a new anchor instead: a list under it would not be current either.
fn lapsed(agent: &Agent, chain: &OwnerChain, anchor: &SignedAnchor) -> ManagersCheck {
    let (me, confirmers) = (agent.credential.uid(), &anchor.anchor().confirmers);
    if chain.anchor_current(anchor)
        && !confirmers.contains(&chain.owner())
        && (me == chain.owner() || confirmers.contains(&me))
    {
        ManagersCheck::Renewable
    } else {
        ManagersCheck::NotCurrent
    }
}

/// The next sequence number, within the protocol's bound.
fn successor(seq: u64) -> Result<u64> {
    (seq < vc_proto::MAX_SAFE_INTEGER as u64).then_some(seq + 1).ok_or(Invalid("sequence exhausted"))
}

/// Splits a stored key into the server of a server record.
pub(super) fn record_server(key: &str) -> Option<&str> {
    key.strip_suffix(SUFFIX)
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::credential::tests::{certified, key, public};
    use crate::owner::{Anchor, OwnerLink};

    const NOW: i64 = 1_800_000_000_000;

    fn uid(seed: u8) -> Uid {
        vc_proto::uid_for_key(key(seed).verifying_key().as_bytes())
    }
    fn server() -> String {
        public(&key(200))
    }
    /// Device `seed + 50` of account `seed`.
    fn agent(seed: u8) -> Agent {
        let mut a = Agent::new([seed + 50; 32], certified(&key(seed), &key(seed + 50), NOW - 1), NOW).unwrap();
        a.take_changes();
        a
    }
    fn genesis(generation: u64, owner: u8) -> Genesis {
        Genesis { server_key: server(), generation, owner_uid: uid(owner) }
    }
    fn wire(chain: &OwnerChain) -> (String, Vec<(String, String)>) {
        let links = chain.links().iter().map(|l| (l.link().statement(), l.signature().to_owned())).collect();
        (chain.genesis().statement(), links)
    }
    fn moved(chain: &OwnerChain, from: u8, to: u8, anchor: Option<&SignedAnchor>) -> OwnerChain {
        let link = OwnerLink {
            server_key: server(),
            generation: chain.generation(),
            seq: chain.seq() + 1,
            previous_link_hash: chain.head(),
            previous_owner_account_key: public(&key(from)),
            new_owner_uid: uid(to),
            anchor_hash: anchor.map(SignedAnchor::hash),
            signed_at: NOW,
        };
        let mut next = chain.clone();
        next.push(SignedLink::sign(link, &key(from))).unwrap();
        next
    }
    fn anchor(chain: &OwnerChain, owner: &SigningKey, seq: u64, confirmers: &[u8]) -> SignedAnchor {
        let mut confirmers: Vec<Uid> = confirmers.iter().map(|&c| uid(c)).collect();
        confirmers.sort();
        let a = Anchor {
            server_key: server(),
            generation: chain.generation(),
            owner_head: chain.head(),
            anchor_seq: seq,
            confirmers,
            signed_at: NOW,
        };
        SignedAnchor::sign(a, owner)
    }
    fn list(chain: &OwnerChain, anchor: &SignedAnchor, seq: u64, signer: &Agent) -> Vec<u8> {
        let l = ManagerList {
            server_key: server(),
            generation: chain.generation(),
            anchor_hash: anchor.hash(),
            seq,
            managers: vec![],
            signer_uid: signer.credential.uid(),
            signed_at: NOW,
        };
        signer.sign_manager_list(l).unwrap()
    }
    fn check(a: &mut Agent, chain: &OwnerChain, invite: Option<&str>) -> OwnerCheck {
        let (g, l) = wire(chain);
        let result = a.check_owner(&server(), &g, l, invite).unwrap();
        a.take_changes();
        result
    }

    #[test]
    fn the_pin_follows_only_extensions_and_survives_a_reload() {
        let mut device = agent(9);
        let chain = OwnerChain::new(genesis(1, 1));
        // An invite naming another owner pins nothing.
        assert_eq!(check(&mut device, &chain, Some(&uid(2))), OwnerCheck::Changed);
        assert!(device.owner_view(&server()).unwrap().is_none());
        assert_eq!(check(&mut device, &chain, Some(&uid(1))), OwnerCheck::Pinned);
        assert_eq!(check(&mut device, &chain, None), OwnerCheck::Unchanged);
        let longer = moved(&chain, 1, 2, None);
        assert_eq!(check(&mut device, &longer, None), OwnerCheck::Extended);
        // Back to the shorter chain, or another one at the same seq: changed, pin kept.
        assert_eq!(check(&mut device, &chain, None), OwnerCheck::Changed);
        assert_eq!(check(&mut device, &moved(&chain, 1, 3, None), None), OwnerCheck::Changed);
        assert_eq!(check(&mut device, &OwnerChain::new(genesis(1, 4)), None), OwnerCheck::Changed);
        let reloaded = Agent::load([59; 32], device.credential.clone(), device.durable.clone()).unwrap();
        let view = reloaded.owner_view(&server()).unwrap().unwrap();
        assert_eq!((view.owner, view.seq), (uid(2), 1));
    }

    #[test]
    fn a_new_generation_is_reported_and_accepted_only_explicitly() {
        let mut device = agent(9);
        let chain = OwnerChain::new(genesis(1, 1));
        check(&mut device, &chain, None);
        let x = anchor(&chain, &key(1), 1, &[]);
        let owner = agent(1);
        device.accept_managers(&server(), &x.encode(), &list(&chain, &x, 1, &owner), NOW, &()).unwrap();
        device.take_changes();
        let reset = OwnerChain::new(genesis(2, 5));
        assert_eq!(check(&mut device, &reset, None), OwnerCheck::NewGeneration { generation: 2, owner: uid(5) });
        assert_eq!(device.owner_view(&server()).unwrap().unwrap().generation, 1, "not applied");
        let (g, l) = wire(&reset);
        device.accept_generation(&server(), &g, l.clone()).unwrap();
        device.take_changes();
        let view = device.owner_view(&server()).unwrap().unwrap();
        assert_eq!((view.generation, view.owner, view.list_seq), (2, uid(5), None), "checkpoints cleared");
        assert!(device.accept_generation(&server(), &g, l).is_err(), "only a newer generation");
    }

    #[test]
    fn managers_are_accepted_only_current_and_never_rolled_back() {
        let mut device = agent(9);
        let chain = OwnerChain::new(genesis(1, 1));
        check(&mut device, &chain, None);
        let (owner, confirmer, stranger) = (agent(1), agent(5), agent(7));
        let x = anchor(&chain, &key(1), 1, &[5]);
        let mut accept = |anchor: &SignedAnchor, list: &[u8]| {
            let result = device.accept_managers(&server(), &anchor.encode(), list, NOW, &()).unwrap();
            device.take_changes();
            result
        };
        let first = list(&chain, &x, 1, &owner);
        assert_eq!(accept(&x, &first), ManagersCheck::Accepted);
        assert_eq!(accept(&x, &first), ManagersCheck::Unchanged);
        assert_eq!(accept(&x, &list(&chain, &x, 2, &stranger)), ManagersCheck::NotCurrent);
        assert_eq!(accept(&x, &list(&chain, &x, 2, &confirmer)), ManagersCheck::Accepted);
        assert_eq!(accept(&x, &first), ManagersCheck::Rollback);
        assert_eq!(accept(&x, &list(&chain, &x, 2, &owner)), ManagersCheck::Rollback, "same seq, other list");
        let reloaded = Agent::load([59; 32], device.credential.clone(), device.durable.clone()).unwrap();
        let view = reloaded.owner_view(&server()).unwrap().unwrap();
        assert_eq!((view.confirmers, view.list_seq, view.list_signer), (vec![uid(5)], Some(2), Some(uid(5))));
        // Only the device's own uid signs.
        let foreign = ManagerList {
            server_key: server(),
            generation: 1,
            anchor_hash: x.hash(),
            seq: 3,
            managers: vec![],
            signer_uid: uid(1),
            signed_at: NOW,
        };
        assert!(confirmer.sign_manager_list(foreign).is_err());
    }

    #[test]
    fn the_agent_builds_the_statements_its_owner_and_devices_sign() {
        let mut owner = agent(1);
        let chain = OwnerChain::new(genesis(1, 1));
        check(&mut owner, &chain, None);
        // The owner's browser signs the anchor with the account key; the device signs the list.
        let statement = owner.next_anchor(&server(), &[uid(5), uid(5)], NOW, None).unwrap();
        let signature = crate::encoding::b64(&ed25519_dalek::Signer::sign(&key(1), statement.as_bytes()).to_bytes());
        let anchor = SignedAnchor::from_parts(&statement, &public(&key(1)), &signature).unwrap();
        assert_eq!(anchor.anchor().confirmers, vec![uid(5)]);
        assert!(owner.next_anchor(&server(), &[uid(1)], NOW, None).is_err(), "the owner is no confirmer");
        let list_statement =
            owner.next_list(&server(), Some(&anchor.encode()), Some(&[uid(5), uid(7)]), NOW, None, None, &()).unwrap();
        let list = owner.sign_manager_list(ManagerList::parse(&list_statement).unwrap()).unwrap();
        assert_eq!(SignedList::parse(&list).unwrap().list().managers, vec![uid(7)], "confirmers are not repeated");
        assert_eq!(
            owner.accept_managers(&server(), &anchor.encode(), &list, NOW, &()).unwrap(),
            ManagersCheck::Accepted
        );
        owner.take_changes();
        // The next list continues the sequence under the accepted anchor.
        let next =
            ManagerList::parse(&owner.next_list(&server(), None, Some(&[]), NOW, None, None, &()).unwrap()).unwrap();
        assert_eq!((next.seq, next.anchor_hash), (2, anchor.hash()));
        // A transfer link the chain accepts, handing over the anchor.
        let link = owner.transfer_statement(&server(), &public(&key(1)), &uid(9), Some(&anchor.encode()), NOW).unwrap();
        let signature = crate::encoding::b64(&ed25519_dalek::Signer::sign(&key(1), link.as_bytes()).to_bytes());
        let mut moved = chain.clone();
        moved.push(SignedLink::parse(&link, &signature).unwrap()).unwrap();
        assert!(moved.anchor_current(&anchor));
    }

    #[test]
    fn replays_builders_and_renewal_hold_against_a_dishonest_server() {
        let mut owner = agent(1);
        let chain = OwnerChain::new(genesis(1, 1));
        check(&mut owner, &chain, None);
        let sign = |statement: &str| {
            crate::encoding::b64(&ed25519_dalek::Signer::sign(&key(1), statement.as_bytes()).to_bytes())
        };
        let anchor_at = |owner: &Agent, confirmers: &[Uid]| {
            let statement = owner.next_anchor(&server(), confirmers, NOW, None).unwrap();
            SignedAnchor::from_parts(&statement, &public(&key(1)), &sign(&statement)).unwrap()
        };
        let a1 = anchor_at(&owner, &[uid(5)]);
        let l1 = owner
            .sign_manager_list(
                ManagerList::parse(
                    &owner.next_list(&server(), Some(&a1.encode()), Some(&[]), NOW, None, None, &()).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(owner.accept_managers(&server(), &a1.encode(), &l1, NOW, &()).unwrap(), ManagersCheck::Accepted);
        owner.take_changes();
        // The same pair, replayed once its signer's device is revoked, confers nothing.
        struct Revoked;
        impl Revocations for Revoked {
            fn revoked_at(&self, _: &str, _: &str) -> Option<i64> {
                Some(NOW - 1)
            }
        }
        assert_eq!(
            owner.accept_managers(&server(), &a1.encode(), &l1, NOW, &Revoked).unwrap(),
            ManagersCheck::Renewable,
            "not current, and the owner may renew it"
        );
        let mut member = agent(7);
        check(&mut member, &chain, None);
        assert_eq!(
            member.accept_managers(&server(), &a1.encode(), &l1, NOW, &Revoked).unwrap(),
            ManagersCheck::NotCurrent
        );
        owner.take_changes();
        // A2 removes the confirmer; afterwards the server's replayed A1 signs nothing.
        let a2 = anchor_at(&owner, &[]);
        let l2 = owner
            .sign_manager_list(
                ManagerList::parse(
                    &owner.next_list(&server(), Some(&a2.encode()), Some(&[]), NOW, None, None, &()).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(owner.accept_managers(&server(), &a2.encode(), &l2, NOW, &()).unwrap(), ManagersCheck::Accepted);
        owner.take_changes();
        assert!(
            owner.next_list(&server(), Some(&a1.encode()), Some(&[]), NOW, None, None, &()).is_err(),
            "an older anchor"
        );
        assert!(owner.transfer_statement(&server(), &public(&key(1)), &uid(9), Some(&a1.encode()), NOW).is_err());
        assert!(
            owner.transfer_statement(&server(), &public(&key(1)), &uid(9), None, NOW).is_err(),
            "the anchor hidden"
        );
        assert!(owner.transfer_statement(&server(), &public(&key(1)), &uid(9), Some(&a2.encode()), NOW).is_ok());
        // A confirmer no longer on the anchor cannot sign a list.
        let mut former = agent(5);
        check(&mut former, &chain, None);
        former.accept_managers(&server(), &a2.encode(), &l2, NOW, &()).unwrap();
        former.take_changes();
        assert!(former.next_list(&server(), None, Some(&[]), NOW, None, None, &()).is_err());
        // A fresh device of the owner renews from the server's list, beyond its seq.
        let mut fresh = Agent::new([90; 32], certified(&key(1), &key(90), NOW - 1), NOW).unwrap();
        fresh.take_changes();
        check(&mut fresh, &chain, None);
        let next = ManagerList::parse(
            &fresh.next_list(&server(), Some(&a2.encode()), None, NOW, Some(&a2.encode()), Some(&l2), &()).unwrap(),
        )
        .unwrap();
        assert_eq!(next.seq, 3);
        // With a checkpoint, a replayed older list renews nothing of its own (its seq neither), and
        // the server's superseded anchor vouches for no list: the accepted list is kept.
        let renew = |owner: &Agent, server_anchor: &SignedAnchor, server_list: &[u8]| {
            ManagerList::parse(
                &owner
                    .next_list(&server(), None, None, NOW, Some(&server_anchor.encode()), Some(server_list), &())
                    .unwrap(),
            )
            .unwrap()
        };
        let removed = owner
            .sign_manager_list(ManagerList {
                managers: vec![uid(8)],
                ..ManagerList::parse(&owner.next_list(&server(), None, Some(&[]), NOW, None, None, &()).unwrap())
                    .unwrap()
            })
            .unwrap();
        assert_eq!(
            owner.accept_managers(&server(), &a2.encode(), &removed, NOW, &()).unwrap(),
            ManagersCheck::Accepted
        );
        owner.take_changes();
        let kept = renew(&owner, &a2, &l2);
        assert_eq!((kept.seq, kept.managers), (4, vec![uid(8)]), "l2 (seq 2) is older than the accepted seq 3");
        let mut c = agent(5);
        check(&mut c, &chain, None);
        c.accept_managers(&server(), &a1.encode(), &l1, NOW, &()).unwrap();
        c.take_changes();
        let high = c
            .sign_manager_list(ManagerList {
                seq: vc_proto::MAX_SAFE_INTEGER as u64 - 1,
                ..ManagerList::parse(&c.next_list(&server(), None, Some(&[]), NOW, None, None, &()).unwrap()).unwrap()
            })
            .unwrap();
        assert_eq!(renew(&owner, &a1, &high).seq, 4, "a former confirmer's list under the superseded anchor");
        assert!(
            c.next_list(
                &server(),
                None,
                Some(&[]),
                NOW,
                Some(&a1.encode()),
                Some(&{
                    c.sign_manager_list(ManagerList {
                        seq: vc_proto::MAX_SAFE_INTEGER as u64,
                        ..ManagerList::parse(&c.next_list(&server(), None, Some(&[]), NOW, None, None, &()).unwrap())
                            .unwrap()
                    })
                    .unwrap()
                }),
                &()
            )
            .is_err(),
            "beyond the protocol's bound"
        );
    }

    #[test]
    fn a_former_owner_cannot_steer_the_new_owners_renewal() {
        // 1 hands anchor X (no confirmers) over to 2; 1's list under X is no longer current.
        let former = agent(1);
        let chain = OwnerChain::new(genesis(1, 1));
        let x = anchor(&chain, &key(1), 1, &[]);
        let l1 = list(&chain, &x, 1, &former);
        let moved = moved(&chain, 1, 2, Some(&x));
        let mut new_owner = agent(2);
        check(&mut new_owner, &moved, None);
        assert_eq!(new_owner.accept_managers(&server(), &x.encode(), &l1, NOW, &()).unwrap(), ManagersCheck::Renewable);
        new_owner.take_changes();
        let renewed = new_owner
            .sign_manager_list(
                ManagerList::parse(
                    &new_owner
                        .next_list(&server(), Some(&x.encode()), None, NOW, Some(&x.encode()), Some(&l1), &())
                        .unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(
            new_owner.accept_managers(&server(), &x.encode(), &renewed, NOW, &()).unwrap(),
            ManagersCheck::Accepted
        );
        new_owner.take_changes();
        // 1 signs a later list under X (valid as history): renewal keeps the accepted membership.
        let injected = former
            .sign_manager_list(ManagerList {
                server_key: server(),
                generation: 1,
                anchor_hash: x.hash(),
                seq: 7,
                managers: vec![uid(8)],
                signer_uid: uid(1),
                signed_at: NOW,
            })
            .unwrap();
        let next = ManagerList::parse(
            &new_owner.next_list(&server(), None, None, NOW, Some(&x.encode()), Some(&injected), &()).unwrap(),
        )
        .unwrap();
        assert_eq!((next.seq, next.managers), (3, vec![]));
        // A list by another device of the owner, later revoked: its seq counts, its members do not.
        let other = Agent::new([91; 32], certified(&key(2), &key(91), NOW - 1), NOW).unwrap();
        let by_other = other
            .sign_manager_list(ManagerList {
                server_key: server(),
                generation: 1,
                anchor_hash: x.hash(),
                seq: 5,
                managers: vec![uid(8)],
                signer_uid: uid(2),
                signed_at: NOW,
            })
            .unwrap();
        struct Revoked(String);
        impl Revocations for Revoked {
            fn revoked_at(&self, _: &str, device: &str) -> Option<i64> {
                (device == self.0).then_some(NOW - 1)
            }
        }
        let revoked = Revoked(public(&key(91)));
        let renew = |revocations: &dyn Revocations| {
            ManagerList::parse(
                &new_owner
                    .next_list(&server(), None, None, NOW, Some(&x.encode()), Some(&by_other), revocations)
                    .unwrap(),
            )
            .unwrap()
        };
        assert_eq!((renew(&()).seq, renew(&()).managers), (6, vec![uid(8)]));
        assert_eq!((renew(&revoked).seq, renew(&revoked).managers), (6, vec![]));
        // Nor of a device whose certificate has expired.
        let expired =
            Agent::new([92; 32], certified(&key(2), &key(92), NOW - 31 * 86_400_000), NOW - 31 * 86_400_000).unwrap();
        let by_expired = expired
            .sign_manager_list(ManagerList {
                server_key: server(),
                generation: 1,
                anchor_hash: x.hash(),
                seq: 5,
                managers: vec![uid(8)],
                signer_uid: uid(2),
                signed_at: NOW,
            })
            .unwrap();
        let next = ManagerList::parse(
            &new_owner.next_list(&server(), None, None, NOW, Some(&x.encode()), Some(&by_expired), &()).unwrap(),
        )
        .unwrap();
        assert_eq!((next.seq, next.managers), (6, vec![]));
    }

    #[test]
    fn a_fresh_device_of_a_new_owner_replaces_an_inherited_anchor_beyond_its_seqs() {
        // The owner (1) names 5 a confirmer, then moves ownership to 5, handing the anchor over.
        let mut owner = agent(1);
        let chain = OwnerChain::new(genesis(1, 1));
        check(&mut owner, &chain, None);
        let x = anchor(&chain, &key(1), 1, &[5]);
        let lx = list(&chain, &x, 4, &owner);
        let moved = moved(&chain, 1, 5, Some(&x));
        // 5's fresh device cannot accept the inherited pair (the owner is no confirmer) ...
        let mut fresh = agent(5);
        check(&mut fresh, &moved, None);
        assert_eq!(fresh.accept_managers(&server(), &x.encode(), &lx, NOW, &()).unwrap(), ManagersCheck::NotCurrent);
        fresh.take_changes();
        // ... so it signs a replacement anchor and list beyond the server's seqs.
        let statement = fresh.next_anchor(&server(), &[], NOW, Some(&x.encode())).unwrap();
        assert_eq!(Anchor::parse(&statement).unwrap().anchor_seq, 2);
        let replacement = SignedAnchor::from_parts(
            &statement,
            &public(&key(5)),
            &crate::encoding::b64(&ed25519_dalek::Signer::sign(&key(5), statement.as_bytes()).to_bytes()),
        )
        .unwrap();
        let next = fresh
            .next_list(&server(), Some(&replacement.encode()), None, NOW, Some(&x.encode()), Some(&lx), &())
            .unwrap();
        let next = ManagerList::parse(&next).unwrap();
        assert_eq!((next.seq, next.managers), (5, vec![]));
    }
}
