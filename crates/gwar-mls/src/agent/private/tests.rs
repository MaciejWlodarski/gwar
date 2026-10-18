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
    vc_proto::uid_for_key(key(account).verifying_key().as_bytes())
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
    assert!(owner.settle(&server(), &id, Some(&id)).unwrap());
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
    owner.settle(&server(), &id, Some(&id)).unwrap();
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
