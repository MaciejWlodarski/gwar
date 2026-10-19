use super::*;
use crate::agent::{Reconcile, Resolution, Status};
use crate::credential::tests::{certified, key, public};
use crate::ds::Observer;
use crate::owner::{Anchor, Genesis, ManagerList, SignedAnchor};
use crate::{Descriptor, Kind};

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}
fn server() -> String {
    public(&key(200))
}
fn uid(account: u8) -> Uid {
    gwar_proto::uid_for_key(key(account).verifying_key().as_bytes())
}
/// Device `seed` of account `account`, with the owner chain of account 1 pinned.
fn device(account: u8, seed: u8, t: i64, chain: &OwnerChain) -> Agent {
    let mut a = Agent::new([seed; 32], certified(&key(account), &key(seed), t - 86_400_000), t).unwrap();
    a.take_changes();
    a.check_owner(&server(), &chain.genesis().statement(), vec![], None).unwrap();
    a.take_changes();
    a
}
/// The owner (account 1) names `confirmers` and `managers`; `seq` numbers both.
fn pair(chain: &OwnerChain, owner: &Agent, seq: u64, managers: &[u8], t: i64) -> (SignedAnchor, Vec<u8>) {
    let anchor = SignedAnchor::sign(
        Anchor {
            server_key: server(),
            generation: 1,
            owner_head: chain.head(),
            anchor_seq: seq,
            confirmers: vec![],
            signed_at: t,
        },
        &key(1),
    );
    let mut managers: Vec<Uid> = managers.iter().map(|&m| uid(m)).collect();
    managers.sort();
    let list = owner
        .sign_manager_list(ManagerList {
            server_key: server(),
            generation: 1,
            anchor_hash: anchor.hash(),
            seq,
            managers,
            signer_uid: uid(1),
            signed_at: t,
        })
        .unwrap();
    (anchor, list)
}
fn accept_pair(a: &mut Agent, pair: &(SignedAnchor, Vec<u8>), t: i64) {
    a.accept_managers(&server(), &pair.0.encode(), &pair.1, t, &()).unwrap();
    a.take_changes();
}
fn descriptor(creator: &Agent, t: i64) -> SignedDescriptor {
    SignedDescriptor::sign(
        Descriptor {
            server_key: server(),
            conversation: "channel:7".into(),
            kind: Kind::Channel,
            incarnation: 0,
            previous_group_id: None,
            private: true,
            history: true,
            founding_owner: Some(uid(1)),
            channel_group_id: None,
            creator_uid: creator.credential.uid(),
            created_at: t,
            creation_nonce: b64(&[3; 16]),
        },
        creator.credential.clone(),
        &creator.signer.0,
    )
}
fn ack(a: &mut Agent, id: &[u8; 32], out: &Outbox) {
    let entry = Reconcile::Entry { epoch: out.epoch, commit: out.commit.clone(), proposals: vec![] };
    assert_eq!(a.reconcile(&server(), id, entry).unwrap(), Resolution::Accepted);
    a.take_changes();
}

struct Private {
    t: i64,
    chain: OwnerChain,
    owner: Agent,
    member: Agent,
    observer: Observer,
    id: [u8; 32],
    /// The newest GroupInfo.
    info: Vec<u8>,
}

/// The owner creates a private channel's group admitting account 2, whose device joins.
fn private() -> Private {
    let t = now();
    let chain = OwnerChain::new(Genesis { server_key: server(), generation: 1, owner_uid: uid(1) });
    let mut owner = device(1, 11, t, &chain);
    let mut member = device(2, 21, t, &chain);
    let first = pair(&chain, &owner, 1, &[], t);
    accept_pair(&mut owner, &first, t);
    accept_pair(&mut member, &first, t);
    let d = descriptor(&owner, t);
    let id = d.group_id();
    assert_eq!(owner.create(d.clone()), Err(Invalid("a private channel's group needs its members")));
    let info = owner.create_private(d, &[uid(2)], t, &()).unwrap();
    owner.take_changes();
    assert!(owner.settle_private(&server(), &id, Some(&id), t, &()).unwrap());
    owner.take_changes();
    let mut observer = Observer::create_with(&info, &server(), Some(chain.clone())).unwrap();
    let tree = observer.tree().unwrap().1;
    let out = member.join_external(&server(), &info, Some(&tree), t, &()).unwrap();
    member.take_changes();
    observer = observer.commit(&out.commit, &out.group_info, None, t, t).unwrap().0;
    ack(&mut member, &id, &out);
    owner.receive(&server(), &id, &out.commit, &[]).unwrap();
    owner.take_changes();
    Private { t, chain, owner, member, observer, id, info: out.group_info }
}

fn authority(a: &Agent, id: &[u8; 32], t: i64) -> Option<Authority> {
    a.gate(&server(), id, t, &()).unwrap().access
}

#[test]
fn a_private_group_admits_only_its_lists_and_gates_on_current_authority() {
    let mut p = private();
    for a in [&p.owner, &p.member] {
        assert_eq!(authority(a, &p.id, p.t), Some(Authority::Current));
        assert!(a.gate(&server(), &p.id, p.t, &()).unwrap().can_send());
    }
    // An outsider cannot join: its own leaf breaks the membership rule.
    let mut outsider = device(3, 31, p.t, &p.chain);
    accept_pair(&mut outsider, &pair(&p.chain, &p.owner, 1, &[], p.t), p.t);
    let (_, tree) = p.observer.tree().unwrap();
    let info = p.info.clone();
    assert_eq!(
        outsider.join_external(&server(), &info, Some(&tree), p.t, &()),
        Err(AgentError::Rule(crate::ds::DsError::Invalid("not admitted to the private channel")))
    );
    // A device that pinned no owner, or accepted no managers, waits.
    let mut unpinned = Agent::new([41; 32], certified(&key(4), &key(41), p.t - 1), p.t).unwrap();
    unpinned.take_changes();
    assert_eq!(
        unpinned.join_external(&server(), &info, Some(&tree), p.t, &()),
        Err(Invalid("private channel without an owner pin"))
    );
    let mut no_pair = device(2, 22, p.t, &p.chain);
    assert_eq!(no_pair.join_external(&server(), &info, Some(&tree), p.t, &()), Err(AgentError::Waiting));
    // A plain member cannot sign a member list.
    let mut copy = Agent::load([21; 32], p.member.credential.clone(), p.member.durable.clone()).unwrap();
    assert_eq!(copy.commit_access(&server(), &p.id, Some(&[uid(2), uid(3)]), p.t, &()), Err(AgentError::Waiting));
    // The persisted state reloads.
    let reloaded = Agent::load([21; 32], p.member.credential.clone(), p.member.durable.clone()).unwrap();
    assert_eq!(authority(&reloaded, &p.id, p.t), Some(Authority::Current));
    p.member.take_changes();
}

#[test]
fn an_access_edit_removes_excluded_leaves_and_advances_the_checkpoint() {
    let mut p = private();
    // The owner drops account 2: one commit with the new list and the removal.
    let out = p.owner.commit_access(&server(), &p.id, Some(&[]), p.t, &()).unwrap();
    p.owner.take_changes();
    p.observer = p.observer.commit(&out.commit, &out.group_info, None, p.t, p.t).unwrap().0;
    ack(&mut p.owner, &p.id, &out);
    p.member.receive(&server(), &p.id, &out.commit, &[]).unwrap();
    p.member.take_changes();
    assert_eq!(p.member.group(&server(), &p.id).unwrap().status, Status::Removed);
    assert_eq!(p.owner.observe_access(&server(), &p.id, p.t, &()).unwrap(), Some(Authority::Current));
    p.owner.take_changes();
    assert_eq!(p.owner.group(&server(), &p.id).unwrap().access.as_ref().map(|c| c.seq), Some(2));
    // Nothing to commit without a change.
    assert_eq!(p.owner.commit_access(&server(), &p.id, None, p.t, &()), Err(Invalid("nothing to commit")));
}

#[test]
fn a_newer_manager_pair_is_carried_into_the_group() {
    let mut p = private();
    // The owner accepts a newer pair (account 2 becomes a manager): the group carries the older one.
    let newer = pair(&p.chain, &p.owner, 2, &[2], p.t);
    accept_pair(&mut p.owner, &newer, p.t);
    assert_eq!(authority(&p.owner, &p.id, p.t), Some(Authority::Carry));
    assert!(!p.owner.gate(&server(), &p.id, p.t, &()).unwrap().can_send());
    // The member, which has not accepted it yet, still sees its own pair current.
    assert_eq!(authority(&p.member, &p.id, p.t), Some(Authority::Current));
    let out = p.owner.commit_access(&server(), &p.id, None, p.t, &()).unwrap();
    p.owner.take_changes();
    p.observer = p.observer.commit(&out.commit, &out.group_info, None, p.t, p.t).unwrap().0;
    ack(&mut p.owner, &p.id, &out);
    assert_eq!(authority(&p.owner, &p.id, p.t), Some(Authority::Current));
    // The member processes it, then waits until it accepts the pair too.
    p.member.receive(&server(), &p.id, &out.commit, &[]).unwrap();
    p.member.take_changes();
    assert_eq!(authority(&p.member, &p.id, p.t), Some(Authority::Wait));
    accept_pair(&mut p.member, &newer, p.t);
    // The carried member list is the one it accepted: current at once.
    assert_eq!(authority(&p.member, &p.id, p.t), Some(Authority::Current));
}

#[test]
fn a_new_member_list_counts_once_it_is_recorded() {
    let mut p = private();
    let out = p.owner.commit_access(&server(), &p.id, Some(&[uid(2), uid(3)]), p.t, &()).unwrap();
    p.owner.take_changes();
    p.observer = p.observer.commit(&out.commit, &out.group_info, None, p.t, p.t).unwrap().0;
    p.member.receive(&server(), &p.id, &out.commit, &[]).unwrap();
    p.member.take_changes();
    // Not usable until recorded (current on first sight): a later commit cannot then roll it back.
    assert_eq!(authority(&p.member, &p.id, p.t), Some(Authority::Wait));
    assert!(p.member.sending_key(&server(), &p.id, p.t, &()).is_err());
    assert_eq!(p.member.observe_access(&server(), &p.id, p.t, &()).unwrap(), Some(Authority::Current));
    p.member.take_changes();
    assert_eq!(p.member.group(&server(), &p.id).unwrap().access.as_ref().map(|c| c.seq), Some(2));
}

#[test]
fn a_removed_member_cannot_rejoin_the_removed_epoch_from_a_tombstone() {
    let mut p = private();
    let before = p.info.clone();
    let out = p.owner.commit_access(&server(), &p.id, Some(&[]), p.t, &()).unwrap();
    p.owner.take_changes();
    p.observer = p.observer.commit(&out.commit, &out.group_info, None, p.t, p.t).unwrap().0;
    p.member.receive(&server(), &p.id, &out.commit, &[]).unwrap();
    p.member.take_changes();
    // The removing commit's lists still become the checkpoint: no rejoin under the older list.
    assert!(p.member.group(&server(), &p.id).unwrap().removal_access.is_some());
    assert_eq!(p.member.observe_access(&server(), &p.id, p.t, &()).unwrap(), None);
    p.member.take_changes();
    let record = p.member.group(&server(), &p.id).unwrap();
    assert_eq!((record.access.as_ref().map(|c| c.seq), record.removal_access.is_none()), (Some(2), true));
    p.member.forget(&server(), &p.id).unwrap();
    p.member.take_changes();
    assert!(p.member.group(&server(), &p.id).unwrap().removed);
    let tree = p.observer.tree().unwrap().1;
    assert_eq!(p.member.join_external(&server(), &before, Some(&tree), p.t, &()), Err(Invalid("removed checkpoint")));
}

#[test]
fn a_transfer_without_the_anchor_leaves_the_group_waiting() {
    let mut p = private();
    // Ownership moves to account 5 without handing the anchor over: it is no longer current.
    let link = crate::owner::OwnerLink {
        server_key: server(),
        generation: 1,
        seq: 1,
        previous_link_hash: p.chain.head(),
        previous_owner_account_key: public(&key(1)),
        new_owner_uid: uid(5),
        anchor_hash: None,
        signed_at: p.t,
    };
    let link = crate::owner::SignedLink::sign(link, &key(1));
    let links = vec![(link.link().statement(), link.signature().to_owned())];
    p.member.check_owner(&server(), &p.chain.genesis().statement(), links, None).unwrap();
    p.member.take_changes();
    assert_eq!(authority(&p.member, &p.id, p.t), Some(Authority::Wait));
}

#[test]
fn a_forgotten_group_keeps_its_access_checkpoint() {
    let mut p = private();
    let out = p.owner.commit_access(&server(), &p.id, Some(&[uid(2)]), p.t, &()).unwrap();
    p.owner.take_changes();
    let old_info = p.info.clone();
    p.observer = p.observer.commit(&out.commit, &out.group_info, None, p.t, p.t).unwrap().0;
    ack(&mut p.owner, &p.id, &out);
    p.member.receive(&server(), &p.id, &out.commit, &[]).unwrap();
    p.member.take_changes();
    assert_eq!(p.member.observe_access(&server(), &p.id, p.t, &()).unwrap(), Some(Authority::Current));
    p.member.take_changes();
    p.member.forget(&server(), &p.id).unwrap();
    p.member.take_changes();
    let tombstone = p.member.group(&server(), &p.id).unwrap();
    assert_eq!((tombstone.status, tombstone.access.as_ref().map(|c| c.seq)), (Status::Forgotten, Some(2)));
    // Shown again, it never goes back to the earlier list (or epoch); the current one is joined.
    let (_, tree) = p.observer.tree().unwrap();
    let mut copy = Agent::load([21; 32], p.member.credential.clone(), p.member.durable.clone()).unwrap();
    assert!(copy.join_external(&server(), &old_info, Some(&tree), p.t, &()).is_err());
    let info = out.group_info.clone();
    assert!(p.member.join_external(&server(), &info, Some(&tree), p.t, &()).is_ok());
}

#[test]
fn a_former_owners_leaf_is_removed_without_renewing_the_lists() {
    let t = now();
    let chain = OwnerChain::new(Genesis { server_key: server(), generation: 1, owner_uid: uid(1) });
    let mut owner = device(1, 11, t, &chain);
    let mut manager = device(2, 21, t, &chain);
    // Account 2 is a manager; it signs the member list after the owner creates the group.
    let first = pair(&chain, &owner, 1, &[2], t);
    accept_pair(&mut owner, &first, t);
    accept_pair(&mut manager, &first, t);
    let d = descriptor(&owner, t);
    let id = d.group_id();
    let info = owner.create_private(d, &[], t, &()).unwrap();
    owner.take_changes();
    owner.settle_private(&server(), &id, Some(&id), t, &()).unwrap();
    owner.take_changes();
    let mut observer = Observer::create_with(&info, &server(), Some(chain.clone())).unwrap();
    let out = manager.join_external(&server(), &info, Some(&observer.tree().unwrap().1), t, &()).unwrap();
    manager.take_changes();
    observer = observer.commit(&out.commit, &out.group_info, None, t, t).unwrap().0;
    ack(&mut manager, &id, &out);
    let out = manager.commit_access(&server(), &id, Some(&[uid(3)]), t, &()).unwrap();
    manager.take_changes();
    observer = observer.commit(&out.commit, &out.group_info, None, t, t).unwrap().0;
    ack(&mut manager, &id, &out);
    assert_eq!(manager.observe_access(&server(), &id, t, &()).unwrap(), Some(Authority::Current));
    manager.take_changes();
    // Ownership moves to account 5 with the anchor: the former owner's leaf is no longer admitted.
    let link = crate::owner::OwnerLink {
        server_key: server(),
        generation: 1,
        seq: 1,
        previous_link_hash: chain.head(),
        previous_owner_account_key: public(&key(1)),
        new_owner_uid: uid(5),
        anchor_hash: Some(first.0.hash()),
        signed_at: t,
    };
    let link = crate::owner::SignedLink::sign(link, &key(1));
    let links = vec![(link.link().statement(), link.signature().to_owned())];
    manager.check_owner(&server(), &chain.genesis().statement(), links, None).unwrap();
    manager.take_changes();
    let Some(Authority::Remove { leaves }) = authority(&manager, &id, t) else { panic!("a removal is due") };
    assert_eq!(leaves.len(), 1);
    // The manager list was signed by the former owner, yet the removal needs no renewal.
    let out = manager.commit_access(&server(), &id, None, t, &()).unwrap();
    manager.take_changes();
    observer.commit(&out.commit, &out.group_info, None, t, t).unwrap();
    ack(&mut manager, &id, &out);
    assert_eq!(authority(&manager, &id, t), Some(Authority::Current));
}

#[test]
fn a_private_channels_call_exports_keys_only_with_the_parents_current_authority() {
    let mut p = private();
    let mut call = Agent::new_voice([11; 32], p.owner.credential.clone(), p.t).unwrap();
    call.take_changes();
    let d = SignedDescriptor::sign(
        Descriptor {
            server_key: server(),
            conversation: "voice:7".into(),
            kind: Kind::Voice,
            incarnation: 0,
            previous_group_id: None,
            private: false,
            history: false,
            founding_owner: None,
            channel_group_id: Some(b64(&p.id)),
            creator_uid: uid(1),
            created_at: p.t,
            creation_nonce: b64(&[5; 16]),
        },
        call.credential.clone(),
        &call.signer.0,
    );
    let voice = d.group_id();
    call.create(d).unwrap();
    call.take_changes();
    call.settle(&server(), &voice, Some(&voice)).unwrap();
    call.take_changes();
    assert!(call.voice_keys(&server(), &voice, &p.owner, p.t, &()).is_ok());
    // A newer pair the parent does not carry yet holds the call.
    let newer = pair(&p.chain, &p.owner, 2, &[], p.t);
    accept_pair(&mut p.owner, &newer, p.t);
    assert_eq!(call.voice_keys(&server(), &voice, &p.owner, p.t, &()).map(|_| ()), Err(AgentError::Blocked));
}

#[test]
fn a_manager_outside_the_group_offers_lists_a_member_commits() {
    let mut p = private();
    // Account 3 becomes a manager; its device is not in the group.
    let newer = pair(&p.chain, &p.owner, 2, &[3], p.t);
    accept_pair(&mut p.owner, &newer, p.t);
    accept_pair(&mut p.member, &newer, p.t);
    let mut manager = device(3, 31, p.t, &p.chain);
    accept_pair(&mut manager, &newer, p.t);
    let tree = p.observer.tree().unwrap().1;
    let offer = manager
        .offer_access(&server(), &p.info, Some(&p.observer.tree().unwrap().1), Some(&[uid(2), uid(4)]), p.t, &())
        .unwrap();
    // A plain member cannot offer: it holds no authority.
    assert_eq!(p.member.offer_access(&server(), &p.info, Some(&tree), None, p.t, &()), Err(AgentError::Waiting));
    let out = p.member.adopt_offer(&server(), &p.id, &offer, p.t, &()).unwrap();
    p.member.take_changes();
    p.observer = p.observer.commit(&out.commit, &out.group_info, None, p.t, p.t).unwrap().0;
    ack(&mut p.member, &p.id, &out);
    assert_eq!(p.member.observe_access(&server(), &p.id, p.t, &()).unwrap(), Some(Authority::Current));
    p.member.take_changes();
    assert_eq!(p.member.group(&server(), &p.id).unwrap().access.as_ref().map(|c| c.seq), Some(2));
    // Another list at the same seq is refused.
    let mut copy = Agent::load([21; 32], p.member.credential.clone(), p.member.durable.clone()).unwrap();
    let stale = manager.offer_access(&server(), &p.info, Some(&tree), None, p.t, &()).unwrap();
    assert_eq!(copy.adopt_offer(&server(), &p.id, &stale, p.t, &()).map(|_| ()), Err(AgentError::Waiting));
}

/// The successor of `previous` (incarnation `incarnation`), created by `creator`.
fn successor_of(creator: &Agent, previous: &[u8; 32], incarnation: u64, t: i64) -> SignedDescriptor {
    let mut d = descriptor(creator, t).descriptor;
    d.incarnation = incarnation;
    d.previous_group_id = Some(b64(previous));
    d.creation_nonce = b64(&[incarnation as u8 + 10; 16]);
    SignedDescriptor::sign(d, creator.credential.clone(), &creator.signer.0)
}
fn incarnations(ds: &[&SignedDescriptor]) -> Vec<([u8; 32], Vec<u8>)> {
    ds.iter().map(|d| (d.group_id(), d.encode())).collect()
}

#[test]
fn a_private_restart_is_adopted_only_once_its_tip_gives_current_authority() {
    let mut p = private();
    let first = SignedDescriptor::parse(&p.owner.group(&server(), &p.id).unwrap().descriptor).unwrap();
    // The owner accepts a newer pair and restarts the channel under it.
    let newer = pair(&p.chain, &p.owner, 2, &[], p.t);
    accept_pair(&mut p.owner, &newer, p.t);
    let next = successor_of(&p.owner, &p.id, 1, p.t);
    let info = p.owner.create_private(next.clone(), &[uid(2)], p.t, &()).unwrap();
    p.owner.take_changes();
    assert!(p.owner.settle_private(&server(), &next.group_id(), Some(&next.group_id()), p.t, &()).unwrap());
    p.owner.take_changes();
    let tree = Observer::create_with(&info, &server(), Some(p.chain.clone())).unwrap().tree().unwrap().1;
    let chain = incarnations(&[&first, &next]);
    // A plain restart chain is refused for a private conversation.
    let mut copy = Agent::load([21; 32], p.member.credential.clone(), p.member.durable.clone()).unwrap();
    assert!(copy.accept_chain(&server(), "channel:7", &chain).is_err());
    // The member has not accepted the newer pair: nothing changes but the pending tip.
    let pending = p.member.accept_private_chain(&server(), "channel:7", &chain, &info, Some(&tree), p.t, &()).unwrap();
    p.member.take_changes();
    assert!(!pending.adopted);
    assert_eq!(p.member.group(&server(), &p.id).unwrap().status, Status::Active);
    assert!(p.member.group(&server(), &next.group_id()).is_none());
    assert!(p.member.group(&server(), &p.id).unwrap().pending_tip.is_some());
    // Once it accepts the pair, the chain is adopted: the successor is live, the first superseded.
    accept_pair(&mut p.member, &newer, p.t);
    let adopted = p.member.accept_private_chain(&server(), "channel:7", &chain, &info, Some(&tree), p.t, &()).unwrap();
    p.member.take_changes();
    assert_eq!((adopted.adopted, adopted.group), (true, next.group_id()));
    assert_eq!(p.member.group(&server(), &p.id).unwrap().status, Status::Superseded);
    assert_eq!(p.member.group(&server(), &next.group_id()).unwrap().access.as_ref().map(|c| c.seq), Some(1));
}

#[test]
fn an_authority_restarts_from_a_pending_tip() {
    let mut p = private();
    let first = SignedDescriptor::parse(&p.owner.group(&server(), &p.id).unwrap().descriptor).unwrap();
    // A second device of the owner joined under the first pair only.
    let mut second = device(1, 12, p.t, &p.chain);
    accept_pair(&mut second, &pair(&p.chain, &p.owner, 1, &[], p.t), p.t);
    let newer = pair(&p.chain, &p.owner, 2, &[], p.t);
    accept_pair(&mut p.owner, &newer, p.t);
    let next = successor_of(&p.owner, &p.id, 1, p.t);
    let info = p.owner.create_private(next.clone(), &[uid(2)], p.t, &()).unwrap();
    p.owner.take_changes();
    let tree = Observer::create_with(&info, &server(), Some(p.chain.clone())).unwrap().tree().unwrap().1;
    // The second device pins the first incarnation, then sees the restart pending.
    let base = incarnations(&[&first]);
    let tree0 = p.observer.tree().unwrap().1;
    assert!(
        second.accept_private_chain(&server(), "channel:7", &base, &p.info, Some(&tree0), p.t, &()).unwrap().adopted
    );
    second.take_changes();
    let chain = incarnations(&[&first, &next]);
    assert!(
        !second.accept_private_chain(&server(), "channel:7", &chain, &info, Some(&tree), p.t, &()).unwrap().adopted
    );
    second.take_changes();
    // It creates a successor of the pending tip, which is live once it wins.
    let after = successor_of(&second, &next.group_id(), 2, p.t);
    second.create_private(after.clone(), &[uid(2)], p.t, &()).unwrap();
    second.take_changes();
    assert!(second.settle_private(&server(), &after.group_id(), Some(&after.group_id()), p.t, &()).unwrap());
    second.take_changes();
    assert_eq!(second.group(&server(), &p.id).unwrap().status, Status::Superseded);
    assert_eq!(second.group(&server(), &after.group_id()).unwrap().status, Status::Active);
}

#[test]
fn adopting_a_chain_records_the_newer_list_and_recovers_without_any_pin() {
    let mut p = private();
    let first = SignedDescriptor::parse(&p.owner.group(&server(), &p.id).unwrap().descriptor).unwrap();
    // The owner edits access; the member has not processed it, then accepts the live pin's GroupInfo.
    let out = p.owner.commit_access(&server(), &p.id, Some(&[uid(2), uid(3)]), p.t, &()).unwrap();
    p.owner.take_changes();
    p.observer = p.observer.commit(&out.commit, &out.group_info, None, p.t, p.t).unwrap().0;
    let tree = p.observer.tree().unwrap().1;
    let adopted = p
        .member
        .accept_private_chain(&server(), "channel:7", &incarnations(&[&first]), &out.group_info, Some(&tree), p.t, &())
        .unwrap();
    p.member.take_changes();
    assert!(adopted.adopted);
    assert_eq!(p.member.group(&server(), &p.id).unwrap().access.as_ref().map(|c| c.seq), Some(2));
    // A device without any pin, whose accepted pair is older than the restart's, keeps the tip
    // pending on the chain's first link and can restart from it.
    let newer = pair(&p.chain, &p.owner, 2, &[], p.t);
    let mut fresh = device(1, 13, p.t, &p.chain);
    accept_pair(&mut fresh, &pair(&p.chain, &p.owner, 1, &[], p.t), p.t);
    accept_pair(&mut p.owner, &newer, p.t);
    let next = successor_of(&p.owner, &p.id, 1, p.t);
    let info = p.owner.create_private(next.clone(), &[uid(2)], p.t, &()).unwrap();
    p.owner.take_changes();
    let next_tree = Observer::create_with(&info, &server(), Some(p.chain.clone())).unwrap().tree().unwrap().1;
    let chain = incarnations(&[&first, &next]);
    assert!(
        !fresh.accept_private_chain(&server(), "channel:7", &chain, &info, Some(&next_tree), p.t, &()).unwrap().adopted
    );
    fresh.take_changes();
    assert!(fresh.group(&server(), &p.id).unwrap().pending_tip.is_some());
    assert!(fresh.create_private(successor_of(&fresh, &next.group_id(), 2, p.t), &[uid(2)], p.t, &()).is_ok());
}

#[test]
fn a_restart_created_before_losing_authority_stays_pending_when_it_wins() {
    let mut p = private();
    let next = successor_of(&p.owner, &p.id, 1, p.t);
    p.owner.create_private(next.clone(), &[uid(2)], p.t, &()).unwrap();
    p.owner.take_changes();
    // A newer pair is accepted before the server's reply: the creation no longer carries it.
    let newer = pair(&p.chain, &p.owner, 2, &[], p.t);
    accept_pair(&mut p.owner, &newer, p.t);
    assert!(!p.owner.settle_private(&server(), &next.group_id(), Some(&next.group_id()), p.t, &()).unwrap());
    p.owner.take_changes();
    let live = p.owner.group(&server(), &p.id).unwrap();
    assert_eq!((live.status, live.pending_tip.is_some()), (Status::Active, true));
    assert!(p.owner.group(&server(), &next.group_id()).is_none());
}
