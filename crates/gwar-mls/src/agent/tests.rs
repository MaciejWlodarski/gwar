use super::*;
use crate::credential::tests::{certified, key, public};
use crate::{Descriptor, Kind, ds::Observer};

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}
fn server() -> String {
    public(&key(200))
}
fn device(account: u8, seed: u8, t: i64) -> Agent {
    let mut a = Agent::new([seed; 32], certified(&key(account), &key(seed), t - 86_400_000), t).unwrap();
    a.take_changes();
    a
}
fn descriptor(a: &Agent, b: &Agent, t: i64, dm: bool, history: bool) -> SignedDescriptor {
    let mut uids = [a.credential.uid(), b.credential.uid()];
    uids.sort();
    SignedDescriptor::sign(
        Descriptor {
            server_key: server(),
            conversation: if dm { format!("dm:{}:{}", uids[0], uids[1]) } else { "channel:1".into() },
            kind: if dm { Kind::Dm } else { Kind::Channel },
            incarnation: 0,
            previous_group_id: None,
            private: false,
            history,
            founding_owner: None,
            channel_group_id: None,
            creator_uid: a.credential.uid(),
            created_at: t,
            creation_nonce: b64(&[1; 16]),
        },
        a.credential.clone(),
        &a.signer.0,
    )
}
struct Fixture {
    a: Agent,
    b: Agent,
    observer: Observer,
    id: [u8; 32],
    info: Vec<u8>,
    t: i64,
}
fn fixture() -> Fixture {
    let t = now();
    let mut a = device(1, 11, t);
    let b = device(2, 21, t);
    let d = descriptor(&a, &b, t, true, true);
    let id = d.group_id();
    let info = a.create(d).unwrap();
    a.take_changes();
    let observer = Observer::create(&info, &server()).unwrap();
    assert!(a.settle(&server(), &id, Some(&id)).unwrap());
    a.take_changes();
    Fixture { a, b, observer, id, info, t }
}
fn accept(observer: Observer, out: &Outbox, t: i64) -> Observer {
    observer.commit(&out.commit, &out.group_info, out.welcome.as_deref(), t, t).unwrap().0
}
fn acknowledge(a: &mut Agent, id: &[u8; 32], out: &Outbox) {
    assert_eq!(
        a.reconcile(
            &server(),
            id,
            Reconcile::Entry { epoch: out.epoch, commit: out.commit.clone(), proposals: vec![] }
        )
        .unwrap(),
        Resolution::Accepted
    );
    a.take_changes();
}
fn joined() -> Fixture {
    let mut f = fixture();
    let out = f.b.join_external(&server(), &f.info, None, f.t, &()).unwrap();
    f.b.take_changes();
    f.observer = accept(f.observer, &out, f.t);
    acknowledge(&mut f.b, &f.id, &out);
    f.a.receive(&server(), &f.id, &out.commit, &[]).unwrap();
    f.a.take_changes();
    f.info = out.group_info;
    f
}
#[test]
fn external_join_waits_for_acceptance_and_exports_matching_keys() {
    let mut f = fixture();
    let out = f.b.join_external(&server(), &f.info, None, f.t, &()).unwrap();
    assert_eq!(f.b.group(&server(), &f.id).unwrap().status, Status::Joining);
    assert!(f.b.epoch(&server(), &f.id, 1).is_none());
    f.b.take_changes();
    assert_eq!(f.b.sending_key(&server(), &f.id, f.t, &()), Err(AgentError::Blocked));
    f.observer = accept(f.observer, &out, f.t);
    acknowledge(&mut f.b, &f.id, &out);
    f.a.receive(&server(), &f.id, &out.commit, &[]).unwrap();
    f.a.take_changes();
    assert_eq!(
        f.a.sending_key(&server(), &f.id, f.t, &()).unwrap(),
        f.b.sending_key(&server(), &f.id, f.t, &()).unwrap()
    );
}
#[test]
fn pending_commit_round_trip_and_reconciliation() {
    let mut f = joined();
    let old = f.a.sending_key(&server(), &f.id, f.t, &()).unwrap();
    let out = f.a.commit(&server(), &f.id, &[], &[], f.t, &()).unwrap();
    assert_eq!(f.a.mls(&group_key(&server(), &f.id)).unwrap().epoch().as_u64(), old.0);
    let mut stores = f.a.durable.clone();
    f.a.take_changes().apply(&mut stores);
    let mut a = Agent::load([11; 32], f.a.credential.clone(), stores.clone()).unwrap();
    assert_eq!(a.outbox(&server(), &f.id), Some(&out));
    assert!(a.take_changes().is_empty());
    for result in [
        Reconcile::Head { epoch: out.epoch },
        Reconcile::Rejected { reason: Rejection::Early },
        Reconcile::Rejected { reason: Rejection::BadRequest },
        Reconcile::Rejected { reason: Rejection::Stale },
        Reconcile::Rejected { reason: Rejection::Other },
    ] {
        assert_eq!(a.reconcile(&server(), &f.id, result).unwrap(), Resolution::Retry);
        assert!(a.take_changes().is_empty());
        assert_eq!(a.outbox(&server(), &f.id), Some(&out));
    }
    f.observer = accept(f.observer, &out, f.t);
    acknowledge(&mut a, &f.id, &out);
    assert_eq!(a.sending_key(&server(), &f.id, f.t, &()).unwrap().0, old.0 + 1);
    let loaded = Agent::load([11; 32], a.credential.clone(), a.durable.clone()).unwrap();
    assert_eq!(loaded.snapshot(), a.snapshot());
}
#[test]
fn losing_member_commit_is_discarded_and_winner_processed() {
    let mut f = joined();
    let losing = f.a.commit(&server(), &f.id, &[], &[], f.t, &()).unwrap();
    f.a.take_changes();
    let winner = f.b.commit(&server(), &f.id, &[], &[], f.t, &()).unwrap();
    f.b.take_changes();
    f.observer = accept(f.observer, &winner, f.t);
    assert_ne!(losing.hash, winner.hash);
    assert_eq!(
        f.a.reconcile(
            &server(),
            &f.id,
            Reconcile::Entry { epoch: winner.epoch, commit: winner.commit.clone(), proposals: vec![] }
        )
        .unwrap(),
        Resolution::Rebuild
    );
    f.a.take_changes();
    acknowledge(&mut f.b, &f.id, &winner);
    assert_eq!(
        f.a.sending_key(&server(), &f.id, f.t, &()).unwrap(),
        f.b.sending_key(&server(), &f.id, f.t, &()).unwrap()
    );
}
#[test]
fn expired_and_pruned_outboxes() {
    for pruned in [false, true] {
        let mut f = joined();
        let out = f.a.commit(&server(), &f.id, &[], &[], f.t, &()).unwrap();
        f.a.take_changes();
        let result = if pruned {
            Reconcile::Pruned { oldest: out.epoch + 1 }
        } else {
            Reconcile::Rejected { reason: Rejection::Expired }
        };
        assert_eq!(
            f.a.reconcile(&server(), &f.id, result).unwrap(),
            if pruned { Resolution::Rejoin } else { Resolution::Rebuild }
        );
        f.a.take_changes();
        assert!(f.a.outbox(&server(), &f.id).is_none());
        assert!(f.a.epoch(&server(), &f.id, 1).is_some());
        if pruned {
            assert!(f.a.mls(&group_key(&server(), &f.id)).is_err());
            assert!(f.a.durable.openmls.is_empty());
        } else {
            assert!(f.a.mls(&group_key(&server(), &f.id)).unwrap().pending_commit().is_none());
        }
    }
}
#[test]
fn losing_expired_or_pruned_external_join_is_deleted() {
    for case in 0..6 {
        // Cases 3..6 replace an existing membership via drop_mls first.
        let mut f = if case < 3 { fixture() } else { joined() };
        let epochs = f.b.durable.epochs.clone();
        let out = f.b.join_external(&server(), &f.info, None, f.t, &()).unwrap();
        f.b.take_changes();
        let winner = f.a.commit(&server(), &f.id, &[], &[], f.t, &()).unwrap();
        f.a.take_changes();
        let result = match case % 3 {
            0 => Reconcile::Rejected { reason: Rejection::Expired },
            1 => Reconcile::Entry { epoch: out.epoch, commit: winner.commit, proposals: vec![] },
            _ => Reconcile::Pruned { oldest: out.epoch + 1 },
        };
        assert_eq!(f.b.reconcile(&server(), &f.id, result).unwrap(), Resolution::Rejoin);
        f.b.take_changes();
        assert!(f.b.mls(&group_key(&server(), &f.id)).is_err());
        assert_eq!(f.b.durable.epochs, epochs);
        assert!(f.b.durable.openmls.is_empty());
        f.b.join_external(&server(), &winner.group_info, None, f.t, &()).unwrap();
    }
}
struct Revoked(String, i64);
impl Revocations for Revoked {
    fn revoked_at(&self, _: &str, device: &str) -> Option<i64> {
        (device == self.0).then_some(self.1)
    }
}
#[test]
fn revocation_blocks_sending_but_not_acceptance_and_removal_restores_it() {
    let mut f = joined();
    let revoked = Revoked(f.b.credential.device_key().into(), f.t);
    assert!(f.a.gate(&server(), &f.id, f.t, &()).unwrap().can_send());
    assert!(!f.a.gate(&server(), &f.id, f.t, &revoked).unwrap().can_send());
    let out = f.b.commit(&server(), &f.id, &[], &[], f.t, &()).unwrap();
    f.b.take_changes();
    f.observer = accept(f.observer, &out, f.t);
    acknowledge(&mut f.b, &f.id, &out);
    f.a.receive(&server(), &f.id, &out.commit, &[]).unwrap();
    f.a.take_changes();
    let checkpoint = f.b.group(&server(), &f.id).unwrap().checkpoint.clone();
    let epochs = f.b.durable.epochs.clone();
    let descriptor = f.b.group(&server(), &f.id).unwrap().descriptor.clone();
    let out = f.a.commit(&server(), &f.id, &[], &[], f.t, &revoked).unwrap();
    f.a.take_changes();
    f.observer = accept(f.observer, &out, f.t);
    acknowledge(&mut f.a, &f.id, &out);
    f.b.receive(&server(), &f.id, &out.commit, &[]).unwrap();
    f.b.take_changes();
    assert_eq!(f.b.group(&server(), &f.id).unwrap().status, Status::Removed);
    assert!(f.b.durable.openmls.is_empty());
    assert_eq!(f.b.durable.epochs, epochs);
    assert_eq!(f.b.group(&server(), &f.id).unwrap().descriptor, descriptor);
    f.b = Agent::load([21; 32], f.b.credential.clone(), f.b.durable.clone()).unwrap();
    assert_eq!(f.b.group(&server(), &f.id).unwrap().checkpoint, checkpoint);
    assert!(f.b.epoch(&server(), &f.id, 3).is_none());
    assert!(f.a.gate(&server(), &f.id, f.t, &revoked).unwrap().can_send());
    let rejoin = f.b.join_external(&server(), &out.group_info, None, f.t, &()).unwrap();
    f.b.take_changes();
    f.observer = accept(f.observer, &rejoin, f.t);
    acknowledge(&mut f.b, &f.id, &rejoin);
}
#[test]
fn own_server_proposal_requires_rejoin() {
    let mut f = joined();
    let index = f.observer.leaves().iter().find(|l| l.device_key == f.b.credential.device_key()).unwrap().index;
    let proposal = f.observer.propose_remove(&Ed25519Signer(key(200)), index).unwrap();
    f.b.proposal(&server(), &f.id, &proposal).unwrap();
    f.b.take_changes();
    let gate = f.b.gate(&server(), &f.id, f.t, &()).unwrap();
    assert!(gate.rejoin && gate.server_pending);
    assert_eq!(f.b.commit(&server(), &f.id, &[], &[], f.t, &()), Err(AgentError::Rejoin));
    assert!(f.b.take_changes().is_empty());
    let mut b = Agent::load([21; 32], f.b.credential.clone(), f.b.durable.clone()).unwrap();
    let out = b.join_external(&server(), &f.info, None, f.t, &()).unwrap();
    b.take_changes();
    f.observer = accept(f.observer, &out, f.t);
    acknowledge(&mut b, &f.id, &out);
}
#[test]
fn server_proposals_are_committed_by_reference() {
    let mut f = joined();
    let index = f.observer.leaves().iter().find(|l| l.device_key == f.b.credential.device_key()).unwrap().index;
    let proposal = f.observer.propose_remove(&Ed25519Signer(key(200)), index).unwrap();
    f.a.proposal(&server(), &f.id, &proposal).unwrap();
    f.a.take_changes();
    assert!(f.a.durable.openmls.keys().any(|k| k.starts_with(b"QueuedProposal")));
    assert!(!f.a.gate(&server(), &f.id, f.t, &()).unwrap().can_send());
    let out = f.a.commit(&server(), &f.id, &[], &[], f.t, &()).unwrap();
    f.a.take_changes();
    f.observer = accept(f.observer, &out, f.t);
    acknowledge(&mut f.a, &f.id, &out);
    f.b.receive(&server(), &f.id, &out.commit, &[proposal]).unwrap();
    f.b.take_changes();
    for a in [&f.a, &f.b] {
        assert!(
            !a.durable.openmls.keys().any(|k| k.starts_with(b"QueuedProposal") || k.starts_with(b"ProposalQueueRefs"))
        );
    }
    assert_eq!(f.b.group(&server(), &f.id).unwrap().status, Status::Removed);
}
#[test]
fn failed_operation_has_no_changes_and_reload_restores_pending_state() {
    let mut f = joined();
    let snapshot = f.a.durable.clone();
    assert!(f.a.receive(&server(), &f.id, b"bad", &[]).is_err());
    assert!(f.a.take_changes().is_empty());
    assert_eq!(f.a.key_packages(1, f.t), Err(AgentError::Reload));
    let mut a = Agent::load([11; 32], f.a.credential.clone(), snapshot).unwrap();
    a.commit(&server(), &f.id, &[], &[], f.t, &()).unwrap();
}
#[test]
fn freshness_rejects_rollback_and_same_epoch_fork() {
    let mut f = joined();
    let original = f.info.clone();
    let snapshot = f.a.durable.clone();
    let x = f.a.commit(&server(), &f.id, &[], &[], f.t, &()).unwrap();
    f.a.take_changes();
    acknowledge(&mut f.a, &f.id, &x);
    let mut fork = Agent::load([11; 32], f.a.credential.clone(), snapshot).unwrap();
    let y = fork.commit(&server(), &f.id, &[], &[], f.t, &()).unwrap();
    let durable = f.a.durable.clone();
    assert_eq!(f.a.join_external(&server(), &y.group_info, None, f.t, &()), Err(Invalid("freshness")));
    assert!(f.a.take_changes().is_empty());
    let mut a = Agent::load([11; 32], f.a.credential.clone(), durable).unwrap();
    assert_eq!(a.join_external(&server(), &original, None, f.t, &()), Err(Invalid("freshness")));
}
#[test]
fn history_import_conflicts_are_order_independent_and_derived_wins() {
    let mut f = joined();
    let hpke = f.b.device_hpke_key();
    let make = |value| {
        Chunk::seal(
            Header {
                server_key: server(),
                group_id: f.id,
                recipient_device_key: hpke.device_key.clone(),
                recipient_hpke_key: hpke.hpke_key.clone(),
                sender_device_key: f.a.credential.device_key().into(),
                first_epoch: 0,
                last_epoch: 1,
                created_at: f.t,
            },
            &[(0, [value; 32]), (1, [value; 32])],
            &f.a.signer.0,
        )
        .unwrap()
    };
    let chunks = [make(3), make(4)];
    let snapshot = f.b.durable.clone();
    let own = f.b.epoch(&server(), &f.id, 1).unwrap().clone();
    for order in [[0, 1], [1, 0]] {
        let mut b = Agent::load([21; 32], f.b.credential.clone(), snapshot.clone()).unwrap();
        for i in order {
            b.import_history(&server(), &f.id, &chunks[i]).unwrap();
            b.take_changes();
        }
        assert_eq!(b.epoch(&server(), &f.id, 0), Some(&EpochRecord::Conflicted));
        assert_eq!(b.epoch(&server(), &f.id, 1), Some(&own));
        b.import_history(&server(), &f.id, &chunks[0]).unwrap();
        b.take_changes();
        assert_eq!(b.epoch(&server(), &f.id, 0), Some(&EpochRecord::Conflicted));
    }
    let chunks = f.a.share_history(&server(), &f.id, &hpke, f.t, &()).unwrap();
    f.a.take_changes();
    for chunk in chunks {
        f.b.import_history(&server(), &f.id, &chunk).unwrap();
        f.b.take_changes();
    }
    assert_eq!(f.a.epoch(&server(), &f.id, 0).unwrap().key(), f.b.epoch(&server(), &f.id, 0).unwrap().key());
}
#[test]
fn key_package_lifetimes_and_last_resort() {
    let mut f = fixture();
    let packages = f.b.key_packages(2, f.t).unwrap();
    f.b.take_changes();
    for p in packages {
        let p = ds::key_package(&p).unwrap();
        assert!(!p.last_resort);
        if let Credential::Account(c) = &f.b.credential {
            assert!(p.not_after * 1000 <= c.expires_at as u64);
        }
    }
    let p = f.b.last_resort_key_package(f.t).unwrap();
    f.b.take_changes();
    assert!(ds::key_package(&p).unwrap().last_resort);
}

#[test]
fn dm_adds_devices_with_welcomes_and_deduplicates_after_crash() {
    let mut f = fixture();
    let mut b2 = device(2, 22, f.t);
    let mut a2 = device(1, 12, f.t);
    let mut packages = Vec::new();
    for a in [&mut f.b, &mut b2, &mut a2] {
        packages.extend(a.key_packages(1, f.t).unwrap());
        a.take_changes();
    }
    let out = f.a.commit(&server(), &f.id, &packages, &[], f.t, &()).unwrap();
    f.a.take_changes();
    f.observer = accept(f.observer, &out, f.t);
    acknowledge(&mut f.a, &f.id, &out);
    for a in [&mut f.b, &mut b2, &mut a2] {
        assert!(!a.welcome(&server(), "welcome-1", out.welcome.as_ref().unwrap()).unwrap().duplicate);
        a.take_changes();
        assert_eq!(
            a.sending_key(&server(), &f.id, f.t, &()).unwrap(),
            f.a.sending_key(&server(), &f.id, f.t, &()).unwrap()
        );
    }
    let mut b = Agent::load([21; 32], f.b.credential.clone(), f.b.durable.clone()).unwrap();
    assert!(b.welcome(&server(), "welcome-1", b"not decoded for a duplicate").unwrap().duplicate);
    assert!(b.take_changes().is_empty());
    assert_eq!(f.observer.leaves().len(), 4);
}
#[test]
fn rejected_welcome_rolls_back_consumed_key_package_by_reload() {
    let mut f = fixture();
    let packages = f.b.key_packages(1, f.t).unwrap();
    f.b.take_changes();
    let saved = f.b.durable.clone();
    let out = f.a.commit(&server(), &f.id, &packages, &[], f.t, &()).unwrap();
    f.a.take_changes();
    assert!(f.b.welcome(&public(&key(199)), "id", out.welcome.as_ref().unwrap()).is_err());
    assert!(f.b.take_changes().is_empty());
    let mut b = Agent::load([21; 32], f.b.credential.clone(), saved).unwrap();
    b.welcome(&server(), "id", out.welcome.as_ref().unwrap()).unwrap();
    b.take_changes();
    let mut loaded = Agent::load([21; 32], b.credential.clone(), b.durable.clone()).unwrap();
    assert_eq!(loaded.snapshot(), b.snapshot());
    assert!(loaded.take_changes().is_empty());
}
#[test]
fn welcome_acceptance_is_structural_and_gate_uses_current_revocations_and_expiry() {
    let mut f = fixture();
    let packages = f.b.key_packages(1, f.t).unwrap();
    f.b.take_changes();
    let out = f.a.commit(&server(), &f.id, &packages, &[], f.t, &()).unwrap();
    f.a.take_changes();
    f.observer = accept(f.observer, &out, f.t);
    f.b.welcome(&server(), "id", out.welcome.as_ref().unwrap()).unwrap();
    f.b.take_changes();
    assert!(!f.b.gate(&server(), &f.id, f.t, &Revoked(f.a.credential.device_key().into(), f.t)).unwrap().can_send());
    assert_eq!(f.b.gate(&server(), &f.id, f.t + 31 * 86_400_000, &()).unwrap().blocked.len(), 2);
}
#[test]
fn history_off_shares_only_derived_epochs_with_recipient_evidence() {
    let t = now();
    let mut a = device(1, 11, t);
    let mut b = device(2, 21, t);
    let d = descriptor(&a, &b, t, false, false);
    let id = d.group_id();
    let info = a.create(d).unwrap();
    a.take_changes();
    let observer = Observer::create(&info, &server()).unwrap();
    assert!(a.settle(&server(), &id, Some(&id)).unwrap());
    a.take_changes();
    let join = b.join_external(&server(), &info, None, t, &()).unwrap();
    b.take_changes();
    let observer = accept(observer, &join, t);
    acknowledge(&mut b, &id, &join);
    a.receive(&server(), &id, &join.commit, &[]).unwrap();
    a.take_changes();
    let k = group_key(&server(), &id);
    a.epochs.insert(
        epoch_key(&k, 2),
        EpochRecord::Imported { key: [2; 32], sender_uids: [b.credential.uid()].into_iter().collect() },
    );
    let chunks = a.share_history(&server(), &id, &b.device_hpke_key(), t, &()).unwrap();
    a.take_changes();
    let epochs: Vec<_> = chunks
        .iter()
        .flat_map(|bytes| Chunk::parse(bytes).unwrap().open(&b.device.hpke_private).unwrap())
        .map(|(e, _)| e)
        .collect();
    assert_eq!(epochs, [1]);
    assert_eq!(observer.epoch(), 1);
}
#[test]
fn history_chunks_cover_at_most_a_thousand_epochs_and_skip_conflicts() {
    let mut f = joined();
    let k = group_key(&server(), &f.id);
    for e in 2..2200 {
        f.a.epochs.insert(epoch_key(&k, e), EpochRecord::Derived { key: [7; 32] });
    }
    f.a.epochs.insert(epoch_key(&k, 2001), EpochRecord::Conflicted);
    let chunks = f.a.share_history(&server(), &f.id, &f.b.device_hpke_key(), f.t, &()).unwrap();
    f.a.take_changes();
    assert_eq!(chunks.len(), 3);
    let mut count = 0;
    for bytes in chunks {
        let c = Chunk::parse(&bytes).unwrap();
        assert!(bytes.len() <= history::MAX_CHUNK_BYTES);
        let entries = c.open(&f.b.device.hpke_private).unwrap();
        assert!(entries.len() <= 1000);
        assert!(!entries.iter().any(|(e, _)| *e == 2001));
        count += entries.len();
    }
    assert_eq!(count, 2199);
}
#[test]
fn blocked_history_share_and_nonmember_import_are_rejected() {
    let mut f = joined();
    let hpke = f.b.device_hpke_key();
    let revoked = Revoked(f.b.credential.device_key().into(), f.t);
    assert_eq!(f.a.share_history(&server(), &f.id, &hpke, f.t, &revoked), Err(AgentError::Blocked));
    assert!(f.a.take_changes().is_empty());
    let outsider = key(99);
    let bytes = Chunk::seal(
        Header {
            server_key: server(),
            group_id: f.id,
            recipient_device_key: hpke.device_key,
            recipient_hpke_key: hpke.hpke_key,
            sender_device_key: public(&outsider),
            first_epoch: 0,
            last_epoch: 0,
            created_at: f.t,
        },
        &[(0, [7; 32])],
        &outsider,
    )
    .unwrap();
    assert_eq!(f.b.import_history(&server(), &f.id, &bytes), Err(Invalid("history sender is not a member")));
    assert!(f.b.take_changes().is_empty());
}
fn raw_commit(
    a: &mut Agent,
    id: &[u8; 32],
    time: i64,
    packages: Vec<KeyPackage>,
    params: LeafNodeParameters,
    extensions: Option<Extensions<GroupContext>>,
) -> Outbox {
    let mut group = a.mls(&group_key(&server(), id)).unwrap();
    group.set_aad(ds::commit_aad(time));
    let mut builder =
        group.commit_builder().propose_adds(packages).force_self_update(true).leaf_node_parameters(params);
    if let Some(ext) = extensions {
        builder = builder.propose_group_context_extensions(ext).unwrap();
    }
    let bundle = builder
        .load_psks(a.provider.storage())
        .unwrap()
        .build(a.provider.rand(), a.provider.crypto(), &a.signer, |_| true)
        .unwrap()
        .stage_commit(&a.provider)
        .unwrap();
    a.bundle(id, group.epoch().as_u64(), bundle, time).unwrap()
}
#[test]
fn staged_commit_rejects_a_leaf_certified_only_after_its_time() {
    let mut f = joined();
    let mut c = device(2, 22, f.t);
    c.credential = certified(&key(2), &key(22), f.t - 1000);
    let bytes = c.key_packages(1, f.t).unwrap().pop().unwrap();
    let package = KeyPackageIn::tls_deserialize_exact(&bytes)
        .unwrap()
        .validate(f.a.provider.crypto(), ProtocolVersion::Mls10)
        .unwrap();
    let out = raw_commit(&mut f.a, &f.id, f.t - 10_000, vec![package], LeafNodeParameters::default(), None);
    assert_eq!(
        f.b.receive(&server(), &f.id, &out.commit, &[]),
        Err(AgentError::Rule(ds::DsError::Invalid("certificate at commit time")))
    );
    assert!(f.b.take_changes().is_empty());
}
#[test]
fn staged_commit_rejects_outsider_and_changed_update_identity() {
    for identity in [false, true] {
        let mut f = joined();
        let (packages, params) = if identity {
            let credential = certified(&key(2), &key(11), f.t - 1000);
            (
                vec![],
                LeafNodeParameters::builder()
                    .with_credential_with_key(CredentialWithKey {
                        credential: BasicCredential::new(credential.encode().into_bytes()).into(),
                        signature_key: key(11).verifying_key().as_bytes().to_vec().into(),
                    })
                    .build(),
            )
        } else {
            let mut outsider = device(3, 31, f.t);
            let bytes = outsider.key_packages(1, f.t).unwrap().pop().unwrap();
            (
                vec![
                    KeyPackageIn::tls_deserialize_exact(&bytes)
                        .unwrap()
                        .validate(f.a.provider.crypto(), ProtocolVersion::Mls10)
                        .unwrap(),
                ],
                LeafNodeParameters::default(),
            )
        };
        let out = raw_commit(&mut f.a, &f.id, f.t, packages, params, None);
        assert!(f.b.receive(&server(), &f.id, &out.commit, &[]).is_err());
        assert!(f.b.take_changes().is_empty());
    }
}
#[test]
fn staged_commit_rejects_context_changes() {
    for change in 0..3 {
        let mut f = joined();
        let group = f.a.mls(&group_key(&server(), &f.id)).unwrap();
        let mut ext = group.extensions().clone();
        let extension = match change {
            0 => Extension::ExternalSenders(vec![ExternalSender::new(
                key(199).verifying_key().as_bytes().to_vec().into(),
                BasicCredential::new(server_credential(&public(&key(199)))).into(),
            )]),
            1 => Extension::RequiredCapabilities(RequiredCapabilitiesExtension::new(
                &[ExtensionType::Unknown(DESCRIPTOR_EXTENSION), ExtensionType::LastResort],
                &[],
                &[CredentialType::Basic],
            )),
            _ => Extension::Unknown(DESCRIPTOR_EXTENSION, UnknownExtension(b"changed".to_vec())),
        };
        ext.add_or_replace(extension).unwrap();
        let out = raw_commit(&mut f.a, &f.id, f.t, vec![], LeafNodeParameters::default(), Some(ext));
        assert_eq!(
            f.b.receive(&server(), &f.id, &out.commit, &[]),
            Err(AgentError::Rule(ds::DsError::Invalid("group context changed")))
        );
        assert!(f.b.take_changes().is_empty());
    }
}
#[test]
fn pin_rejects_a_second_group_for_the_same_incarnation() {
    let mut f = joined();
    let mut a = device(1, 11, f.t);
    let mut d = descriptor(&a, &f.b, f.t, true, true);
    d.descriptor.creation_nonce = b64(&[9; 16]);
    let d = SignedDescriptor::sign(d.descriptor, a.credential.clone(), &a.signer.0);
    let info = a.create(d).unwrap();
    assert_eq!(f.b.join_external(&server(), &info, None, f.t, &()), Err(Invalid("group pin")));
}
#[test]
fn creation_change_set_restores_the_device_hpke_key() {
    let t = now();
    let c = certified(&key(1), &key(11), t - 1000);
    let mut a = Agent::new([11; 32], c.clone(), t).unwrap();
    let mut stores = Stores::default();
    a.take_changes().apply(&mut stores);
    let loaded = Agent::load([11; 32], c, stores).unwrap();
    assert_eq!(a.device_hpke_key(), loaded.device_hpke_key());
    assert!(a.device_hpke_key().verify());
}

#[test]
fn structural_leaf_rules_have_independent_regressions() {
    let f = fixture();
    let d = SignedDescriptor::parse(&f.a.group(&server(), &f.id).unwrap().descriptor).unwrap();
    let leaf = f.a.mls(&group_key(&server(), &f.id)).unwrap().own_leaf_node().unwrap().clone();
    assert_eq!(rules::tree(&d, [&leaf, &leaf].into_iter()), Err(ds::DsError::Invalid("two leaves for one device")));
    let make = |identity: Vec<u8>, capabilities: Capabilities| {
        KeyPackage::builder()
            .leaf_node_capabilities(capabilities)
            .build(
                SUITE,
                &f.a.provider,
                &f.a.signer,
                CredentialWithKey {
                    credential: BasicCredential::new(identity).into(),
                    signature_key: key(11).verifying_key().as_bytes().to_vec().into(),
                },
            )
            .unwrap()
            .key_package()
            .leaf_node()
            .clone()
    };
    let malformed = make(b"invalid identity".to_vec(), capabilities());
    assert_eq!(rules::tree(&d, [&malformed].into_iter()), Err(ds::DsError::Invalid("leaf credential")));
    let mismatch = make(f.b.credential.encode().into_bytes(), capabilities());
    assert_eq!(rules::tree(&d, [&mismatch].into_iter()), Err(ds::DsError::Invalid("leaf credential")));
    let missing = make(f.a.credential.encode().into_bytes(), Capabilities::default());
    assert_eq!(rules::tree(&d, [&missing].into_iter()), Err(ds::DsError::Invalid("leaf capabilities")));
    let outsider = make(certified(&key(3), &key(11), f.t - 1000).encode().into_bytes(), capabilities());
    assert_eq!(rules::tree(&d, [&outsider].into_iter()), Err(ds::DsError::Invalid("not a participant")));
}
#[test]
fn external_join_and_welcome_skip_expired_leaf_lifetimes() {
    let mut f = fixture();
    let sec = f.t as u64 / 1000;
    let package = KeyPackage::builder()
        // Valid for the commit even on a slow machine, expired for the welcome below.
        .key_package_lifetime(Lifetime::init(sec - 1, sec + 6))
        .leaf_node_capabilities(capabilities())
        .build(SUITE, &f.b.provider, &f.b.signer, f.b.with_key())
        .unwrap()
        .into_key_package();
    let out = raw_commit(&mut f.a, &f.id, f.t, vec![package], LeafNodeParameters::default(), None);
    f.observer = accept(f.observer, &out, f.t);
    std::thread::sleep(std::time::Duration::from_secs(7));
    f.b.welcome(&server(), "delayed", out.welcome.as_ref().unwrap()).unwrap();
    f.b.take_changes();
    let mut c = device(1, 12, f.t);
    c.join_external(&server(), &out.group_info, None, f.t, &()).unwrap();
    assert_eq!(f.b.behind(&server(), &f.id, &out.group_info, None), Ok(false), "observed despite the expired leaf");
}
#[test]
fn renewal_after_key_package_does_not_reject_welcome() {
    let mut f = fixture();
    let packages = f.b.key_packages(1, f.t).unwrap();
    f.b.take_changes();
    let out = f.a.commit(&server(), &f.id, &packages, &[], f.t, &()).unwrap();
    let renewed = certified(&key(2), &key(21), f.t);
    let mut b = Agent::load([21; 32], renewed, f.b.durable.clone()).unwrap();
    b.welcome(&server(), "renewed", out.welcome.as_ref().unwrap()).unwrap();
}
#[test]
fn removing_a_leaf_leaves_no_new_key_and_rejects_its_old_group_info() {
    let mut f = joined();
    let old = f.info.clone();
    let i = f.observer.leaves().iter().find(|l| l.device_key == f.b.credential.device_key()).unwrap().index;
    let out = f.a.commit(&server(), &f.id, &[], &[i], f.t, &()).unwrap();
    f.a.take_changes();
    f.b.receive(&server(), &f.id, &out.commit, &[]).unwrap();
    f.b.take_changes();
    assert_eq!(f.b.join_external(&server(), &old, None, f.t, &()), Err(Invalid("removed checkpoint")));
}
#[test]
fn pending_flag_blocks_even_before_the_proposals_are_fetched() {
    let mut f = joined();
    f.a.pending(&server(), &f.id, true).unwrap();
    f.a.take_changes();
    assert_eq!(f.a.sending_key(&server(), &f.id, f.t, &()), Err(AgentError::Blocked));
    assert_eq!(f.a.share_history(&server(), &f.id, &f.b.device_hpke_key(), f.t, &()), Err(AgentError::Blocked));
}
#[test]
fn ordinary_member_proposals_cannot_be_stored_for_reference() {
    let mut f = joined();
    let mut group = f.a.mls(&group_key(&server(), &f.id)).unwrap();
    let (proposal, _) = group.propose_remove_member(&f.a.provider, &f.a.signer, LeafNodeIndex::new(1)).unwrap();
    assert_eq!(f.b.proposal(&server(), &f.id, &wire(proposal).unwrap()), Err(Invalid("proposal sender")));
}
#[test]
fn joining_state_and_failed_storage_write_reload_round_trip() {
    let mut f = fixture();
    let before = f.b.durable.clone();
    let out = f.b.join_external(&server(), &f.info, None, f.t, &()).unwrap();
    let changes = f.b.take_changes();
    let mut rolled_back = Agent::load([21; 32], f.b.credential.clone(), before.clone()).unwrap();
    assert!(rolled_back.group(&server(), &f.id).is_none());
    rolled_back.join_external(&server(), &f.info, None, f.t, &()).unwrap();
    let mut after = before;
    changes.apply(&mut after);
    let mut recovered = Agent::load([21; 32], f.b.credential.clone(), after).unwrap();
    assert_eq!(recovered.outbox(&server(), &f.id), Some(&out));
    assert!(recovered.epoch(&server(), &f.id, 1).is_none());
    f.observer = accept(f.observer, &out, f.t);
    acknowledge(&mut recovered, &f.id, &out);
}

#[test]
fn corrupt_openmls_records_fail_load_without_panicking() {
    let f = joined();
    for label in [b"Tree".as_slice(), b"MlsGroupJoinConfig", b"MessageSecrets", b"GroupState"] {
        let mut stores = f.a.durable.clone();
        let (_, value) = stores.openmls.iter_mut().find(|(key, _)| key.starts_with(label)).unwrap();
        *value = vec![255];
        assert!(Agent::load([11; 32], f.a.credential.clone(), stores).is_err());
    }
}

#[test]
fn staged_commit_requires_signed_commit_time() {
    let mut f = joined();
    let mut group = f.a.mls(&group_key(&server(), &f.id)).unwrap();
    group.set_aad(b"unrecognized time".to_vec());
    let bundle = group.self_update(&f.a.provider, &f.a.signer, LeafNodeParameters::default()).unwrap();
    let out = f.a.bundle(&f.id, group.epoch().as_u64(), bundle, f.t).unwrap();
    assert_eq!(f.b.receive(&server(), &f.id, &out.commit, &[]), Err(Invalid("commit time")));
    assert!(f.b.take_changes().is_empty());
}

#[test]
fn changed_path_certificate_must_cover_commit_time() {
    let mut f = joined();
    let c = certified(&key(1), &key(11), f.t + 1000);
    let params = LeafNodeParameters::builder()
        .with_credential_with_key(CredentialWithKey {
            credential: BasicCredential::new(c.encode().into_bytes()).into(),
            signature_key: key(11).verifying_key().as_bytes().to_vec().into(),
        })
        .build();
    let out = raw_commit(&mut f.a, &f.id, f.t, vec![], params, None);
    assert_eq!(
        f.b.receive(&server(), &f.id, &out.commit, &[]),
        Err(AgentError::Rule(ds::DsError::Invalid("certificate at commit time")))
    );
    assert!(f.b.take_changes().is_empty());
}

#[test]
fn external_replacement_cannot_change_uid() {
    let mut f = joined();
    f.a.credential = certified(&key(2), &key(11), f.t - 1000);
    let (_, bundle) = MlsGroup::external_commit_builder()
        .with_config(join_config(Kind::Dm))
        .with_aad(ds::commit_aad(f.t))
        .skip_lifetime_validation()
        .build_group(&f.a.provider, info(&f.info).unwrap(), f.a.with_key())
        .unwrap()
        .leaf_node_parameters(LeafNodeParameters::builder().with_capabilities(capabilities()).build())
        .load_psks(f.a.provider.storage())
        .unwrap()
        .build(f.a.provider.rand(), f.a.provider.crypto(), &f.a.signer, |_| true)
        .unwrap()
        .finalize(&f.a.provider)
        .unwrap();
    let bytes = wire(bundle.into_commit()).unwrap();
    assert_eq!(
        f.b.receive(&server(), &f.id, &bytes, &[]),
        Err(AgentError::Rule(ds::DsError::Invalid("external removal")))
    );
    assert!(f.b.take_changes().is_empty());
}

#[test]
fn a_losing_creation_is_settled_away_then_joins_the_winner() {
    for welcome in [false, true] {
        let mut f = fixture();
        // Preserve a separate group, key packages and the device HPKE key exactly.
        let other = descriptor(&f.b, &f.a, f.t, false, false);
        let other_id = other.group_id();
        f.b.create(other).unwrap();
        f.b.take_changes();
        let packages = f.b.key_packages(1, f.t).unwrap();
        f.b.take_changes();
        let before = f.b.durable.clone();
        let losing = descriptor(&f.b, &f.a, f.t, true, true);
        let losing_id = losing.group_id();
        f.b.create(losing).unwrap();
        f.b.take_changes();
        let saved = f.b.durable.clone();
        assert_eq!(f.b.join_external(&server(), &f.info, None, f.t, &()), Err(Invalid("group pin")));
        f.b = Agent::load([21; 32], f.b.credential.clone(), saved).unwrap();
        assert!(!f.b.settle(&server(), &losing_id, Some(&f.id)).unwrap());
        f.b.take_changes();
        assert_eq!(f.b.durable, before);
        assert!(f.b.group(&server(), &other_id).is_some());
        f.b = Agent::load([21; 32], f.b.credential.clone(), f.b.durable.clone()).unwrap();
        if welcome {
            let out = f.a.commit(&server(), &f.id, &packages, &[], f.t, &()).unwrap();
            f.a.take_changes();
            f.observer = accept(f.observer, &out, f.t);
            acknowledge(&mut f.a, &f.id, &out);
            f.b.welcome(&server(), "winner", out.welcome.as_ref().unwrap()).unwrap();
            f.b.take_changes();
        } else {
            let out = f.b.join_external(&server(), &f.info, None, f.t, &()).unwrap();
            f.b.take_changes();
            f.observer = accept(f.observer, &out, f.t);
            acknowledge(&mut f.b, &f.id, &out);
            f.a.receive(&server(), &f.id, &out.commit, &[]).unwrap();
            f.a.take_changes();
        }
        assert_eq!(f.a.sending_key(&server(), &f.id, f.t, &()), f.b.sending_key(&server(), &f.id, f.t, &()));
    }
}

#[test]
fn settle_rejects_groups_that_are_not_creating_without_changes() {
    for case in 0..6 {
        let mut f = if case == 2 || case == 3 { joined() } else { fixture() };
        let agent = match case {
            0 => &mut f.b, // Unknown group.
            1 => {
                f.a.commit(&server(), &f.id, &[], &[], f.t, &()).unwrap();
                f.a.take_changes();
                &mut f.a // Creator at epoch zero, but with an outbox.
            }
            2 => &mut f.a, // Creator after epoch zero.
            3 => &mut f.b, // Joined by this device, created by another.
            4 => &mut f.a, // Accepted creation at epoch zero without an outbox.
            _ => {
                f.b.join_external(&server(), &f.info, None, f.t, &()).unwrap();
                f.b.take_changes();
                &mut f.b // Unaccepted external join.
            }
        };
        let before = agent.durable.clone();
        assert_eq!(agent.settle(&server(), &f.id, Some(&[9; 32])), Err(Invalid("not creating")));
        assert!(agent.take_changes().is_empty());
        assert_eq!(agent.durable, before);
    }
}

// Build a valid MLS group without Gwar's policy to exercise hostile incoming artifacts.
fn raw_create(a: &Agent, descriptor: &SignedDescriptor) -> MlsGroup {
    let extensions = Extensions::from_vec(vec![
        Extension::RequiredCapabilities(RequiredCapabilitiesExtension::new(
            &[ExtensionType::Unknown(DESCRIPTOR_EXTENSION)],
            &[],
            &[CredentialType::Basic],
        )),
        Extension::Unknown(DESCRIPTOR_EXTENSION, UnknownExtension(descriptor.encode())),
        Extension::ExternalSenders(vec![ExternalSender::new(
            key(200).verifying_key().as_bytes().to_vec().into(),
            BasicCredential::new(server_credential(&server())).into(),
        )]),
    ])
    .unwrap();
    MlsGroup::new_with_group_id(
        &a.provider,
        &a.signer,
        &MlsGroupCreateConfig::builder()
            .ciphersuite(SUITE)
            .capabilities(capabilities())
            .use_ratchet_tree_extension(true)
            .wire_format_policy(PURE_PLAINTEXT_WIRE_FORMAT_POLICY)
            .with_group_context_extensions(extensions)
            .build(),
        GroupId::from_slice(&descriptor.group_id()),
        a.with_key(),
    )
    .unwrap()
}

fn restart(a: &Agent, previous: &SignedDescriptor, nonce: u8) -> SignedDescriptor {
    let mut d = previous.descriptor.clone();
    d.incarnation += 1;
    d.previous_group_id = Some(b64(&previous.group_id()));
    d.creator_uid = a.credential.uid();
    d.creation_nonce = b64(&[nonce; 16]);
    SignedDescriptor::sign(d, a.credential.clone(), &a.signer.0)
}
fn link(d: &SignedDescriptor) -> ([u8; 32], Vec<u8>) {
    (d.group_id(), d.encode())
}
/// `a` restarts the joined fixture's group and wins; returns both descriptors and the successor's GroupInfo.
fn restarted() -> (Fixture, SignedDescriptor, SignedDescriptor, Vec<u8>) {
    let mut f = joined();
    let first = descriptor(&f.a, &f.b, f.t, true, true);
    let next = restart(&f.a, &first, 2);
    let info = f.a.create(next.clone()).unwrap();
    f.a.take_changes();
    Observer::create(&info, &server()).unwrap();
    assert!(f.a.settle(&server(), &next.group_id(), Some(&next.group_id())).unwrap());
    f.a.take_changes();
    (f, first, next, info)
}
#[test]
fn a_won_restart_is_live_and_supersedes_its_predecessor() {
    let (f, first, next, _) = restarted();
    let old = f.a.group(&server(), &first.group_id()).unwrap();
    assert_eq!(old.status, Status::Superseded);
    assert!(f.a.mls(&group_key(&server(), &first.group_id())).is_err(), "no MLS state is kept");
    assert!(f.a.epoch(&server(), &first.group_id(), 1).is_some(), "keys stay for reading");
    assert!(!f.a.gate(&server(), &first.group_id(), f.t, &()).unwrap().can_send());
    let inventory = f.a.inventory(&server()).unwrap();
    let live: Vec<_> = inventory.iter().map(|g| (g.group, g.incarnation, g.live)).collect();
    assert!(live.contains(&(first.group_id(), 0, false)) && live.contains(&(next.group_id(), 1, true)));
    assert!(f.a.sending_key(&server(), &next.group_id(), f.t, &()).is_ok());
    let loaded = Agent::load([11; 32], f.a.credential.clone(), f.a.durable.clone()).unwrap();
    assert_eq!(loaded.snapshot(), f.a.snapshot());
}
#[test]
fn a_restart_is_joined_only_after_its_chain_is_accepted() {
    let (mut f, first, next, info) = restarted();
    assert_eq!(f.b.join_external(&server(), &info, None, f.t, &()), Err(Invalid("restart chain")));
    let mut b = Agent::load([21; 32], f.b.credential.clone(), f.b.durable.clone()).unwrap();
    assert_eq!(
        b.accept_chain(&server(), &first.descriptor.conversation, &[link(&first), link(&next)]),
        Ok(next.group_id())
    );
    b.take_changes();
    assert_eq!(b.group(&server(), &first.group_id()).unwrap().status, Status::Superseded);
    assert_eq!(b.group(&server(), &next.group_id()).unwrap().status, Status::Rejoin);
    assert!(b.epoch(&server(), &first.group_id(), 1).is_some());
    let out = b.join_external(&server(), &info, None, f.t, &()).unwrap();
    b.take_changes();
    acknowledge(&mut b, &next.group_id(), &out);
    f.a.receive(&server(), &next.group_id(), &out.commit, &[]).unwrap();
    f.a.take_changes();
    assert_eq!(
        b.sending_key(&server(), &next.group_id(), f.t, &()).unwrap(),
        f.a.sending_key(&server(), &next.group_id(), f.t, &()).unwrap()
    );
    // The predecessor is never live again, and no sibling of the live pin can be made.
    assert_eq!(b.join_external(&server(), &f.info, None, f.t, &()), Err(Invalid("superseded")));
    let mut b = Agent::load([21; 32], b.credential.clone(), b.durable.clone()).unwrap();
    assert_eq!(b.create(restart(&b, &first, 3)), Err(Invalid("group pin")));
    let mut b = Agent::load([21; 32], b.credential.clone(), b.durable.clone()).unwrap();
    let mut fresh = descriptor(&b, &f.a, f.t, true, true).descriptor;
    fresh.creation_nonce = b64(&[4; 16]);
    assert_eq!(b.create(SignedDescriptor::sign(fresh, b.credential.clone(), &b.signer.0)), Err(Invalid("group pin")));
}
#[test]
fn a_welcome_into_a_restart_needs_its_chain() {
    let (f, first, next, _) = restarted();
    let creator = device(1, 11, f.t);
    let mut group = raw_create(&creator, &next);
    let mut b = Agent::load([21; 32], f.b.credential.clone(), f.b.durable.clone()).unwrap();
    let packages = b.key_packages(1, f.t).unwrap();
    b.take_changes();
    let package = KeyPackageIn::tls_deserialize_exact(&packages[0])
        .unwrap()
        .validate(b.provider.crypto(), ProtocolVersion::Mls10)
        .unwrap();
    let (_, welcome, _) = group.add_members(&creator.provider, &creator.signer, &[package]).unwrap();
    let welcome = wire(welcome).unwrap();
    assert_eq!(b.welcome(&server(), "w", &welcome), Err(Invalid("restart chain")));
    let mut b = Agent::load([21; 32], b.credential.clone(), b.durable.clone()).unwrap();
    b.accept_chain(&server(), &first.descriptor.conversation, &[link(&first), link(&next)]).unwrap();
    b.take_changes();
    assert!(!b.welcome(&server(), "w", &welcome).unwrap().duplicate);
    b.take_changes();
    assert_eq!(b.group(&server(), &next.group_id()).unwrap().status, Status::Active);
}
#[test]
fn a_new_device_accepts_a_chain_only_from_incarnation_zero() {
    let (f, first, next, info) = restarted();
    let mut c = device(2, 22, f.t);
    let conversation = first.descriptor.conversation.clone();
    assert_eq!(c.accept_chain(&server(), &conversation, &[link(&next)]), Err(Invalid("chain anchor")));
    let mut c = device(2, 22, f.t);
    assert_eq!(c.accept_chain(&server(), &conversation, &[link(&first), link(&next)]), Ok(next.group_id()));
    c.take_changes();
    assert_eq!(c.group(&server(), &first.group_id()).unwrap().status, Status::Superseded);
    c.join_external(&server(), &info, None, f.t, &()).unwrap();
}
#[test]
fn chain_links_must_verify() {
    let (f, first, next, _) = restarted();
    let conversation = first.descriptor.conversation.clone();
    let reject = |chain: Vec<([u8; 32], Vec<u8>)>, reason: &'static str| {
        let mut b = Agent::load([21; 32], f.b.credential.clone(), f.b.durable.clone()).unwrap();
        assert_eq!(b.accept_chain(&server(), &conversation, &chain), Err(Invalid(reason)));
        assert!(b.take_changes().is_empty());
    };
    let changed = |change: &dyn Fn(&mut Descriptor)| {
        let mut d = next.descriptor.clone();
        change(&mut d);
        link(&SignedDescriptor::sign(d, f.a.credential.clone(), &f.a.signer.0))
    };
    reject(vec![], "chain anchor");
    reject(vec![link(&next)], "chain anchor");
    reject(vec![link(&first), (first.group_id(), next.encode())], "chain descriptor");
    reject(vec![link(&first), changed(&|d| d.incarnation = 2)], "chain link");
    reject(vec![link(&first), changed(&|d| d.previous_group_id = Some(b64(&[9; 32])))], "chain link");
    reject(vec![link(&first), changed(&|d| d.server_key = public(&key(201)))], "chain descriptor");
    // A creator whose certificate did not cover the creation time.
    reject(vec![link(&first), changed(&|d| d.created_at = f.t - 2 * 86_400_000)], "chain descriptor");
    // Two restarts in a row, followed from the device's live pin.
    let third = restart(&f.a, &next, 5);
    let mut b = Agent::load([21; 32], f.b.credential.clone(), f.b.durable.clone()).unwrap();
    assert_eq!(
        b.accept_chain(&server(), &conversation, &[link(&first), link(&next), link(&third)]),
        Ok(third.group_id())
    );
    b.take_changes();
    assert_eq!(b.accept_chain(&server(), &conversation, &[link(&next), link(&third)]), Err(Invalid("chain anchor")));
}
#[test]
fn a_successor_keeps_the_conversations_fields() {
    let t = now();
    let (a, b) = (device(1, 11, t), device(2, 21, t));
    let channel = descriptor(&a, &b, t, false, true);
    assert!(successor(&channel, &restart(&b, &channel, 2)));
    let mut other = restart(&b, &channel, 2).descriptor;
    other.history = false;
    assert!(!successor(&channel, &SignedDescriptor::sign(other, b.credential.clone(), &b.signer.0)));
}
#[test]
fn a_creation_continues_only_the_live_pin_once() {
    let mut f = joined();
    let first = descriptor(&f.a, &f.b, f.t, true, true);
    let mut skipped = restart(&f.a, &first, 2).descriptor;
    skipped.incarnation = 2;
    assert_eq!(
        f.a.create(SignedDescriptor::sign(skipped, f.a.credential.clone(), &f.a.signer.0)),
        Err(Invalid("restart predecessor"))
    );
    let mut a = Agent::load([11; 32], f.a.credential.clone(), f.a.durable.clone()).unwrap();
    let next = restart(&a, &first, 2);
    a.create(next).unwrap();
    a.take_changes();
    // One candidate per predecessor, and the predecessor stays live until it is settled.
    assert_eq!(a.create(restart(&a, &first, 3)), Err(Invalid("group pin")));
    let a = Agent::load([11; 32], a.credential.clone(), a.durable.clone()).unwrap();
    assert!(a.sending_key(&server(), &f.id, f.t, &()).is_ok());
}
#[test]
fn group_infos_behind_the_checkpoint_or_forking_it_are_observed() {
    let mut f = joined();
    let ours = f.a.commit(&server(), &f.id, &[], &[], f.t, &()).unwrap();
    f.a.take_changes();
    let theirs = f.b.commit(&server(), &f.id, &[], &[], f.t, &()).unwrap();
    f.b.take_changes();
    acknowledge(&mut f.a, &f.id, &ours);
    assert_eq!(f.a.behind(&server(), &f.id, &f.info, None), Ok(true), "an earlier epoch");
    assert_eq!(f.a.behind(&server(), &f.id, &theirs.group_info, None), Ok(true), "the same epoch, another context");
    assert_eq!(f.a.behind(&server(), &f.id, &ours.group_info, None), Ok(false));
    assert_eq!(f.b.behind(&server(), &f.id, &f.info, None), Ok(false), "b has not moved past it");
    assert!(f.a.behind(&server(), &f.id, &ours.commit, None).is_err());
    // An observation counts only for the group it was asked for.
    assert_eq!(f.a.behind(&server(), &[9; 32], &f.info, None), Err(Invalid("group info group")));
}
#[test]
fn accepting_a_chain_through_this_devices_own_creation_settles_it() {
    let mut f = joined();
    let first = descriptor(&f.a, &f.b, f.t, true, true);
    let conversation = first.descriptor.conversation.clone();
    let next = restart(&f.a, &first, 2);
    f.a.create(next.clone()).unwrap();
    f.a.take_changes();
    // The server holds the creation, but its reply was lost.
    assert_eq!(f.a.accept_chain(&server(), &conversation, &[link(&first), link(&next)]), Ok(next.group_id()));
    f.a.take_changes();
    assert_eq!(f.a.group(&server(), &first.group_id()).unwrap().status, Status::Superseded);
    assert_eq!(f.a.group(&server(), &next.group_id()).unwrap().status, Status::Active);
    assert!(f.a.sending_key(&server(), &next.group_id(), f.t, &()).is_ok());
    assert_eq!(f.a.join_external(&server(), &f.info, None, f.t, &()), Err(Invalid("superseded")));
    let mut a = Agent::load([11; 32], f.a.credential.clone(), f.a.durable.clone()).unwrap();
    assert_eq!(a.settle(&server(), &next.group_id(), None), Err(Invalid("not creating")));

    // A creation the chain continued past is superseded with its MLS state.
    let mut f = joined();
    let first = descriptor(&f.a, &f.b, f.t, true, true);
    let next = restart(&f.a, &first, 2);
    f.a.create(next.clone()).unwrap();
    f.a.take_changes();
    let after = restart(&f.b, &next, 3);
    let chain = [link(&first), link(&next), link(&after)];
    assert_eq!(f.a.accept_chain(&server(), &conversation, &chain), Ok(after.group_id()));
    f.a.take_changes();
    assert_eq!(f.a.group(&server(), &next.group_id()).unwrap().status, Status::Superseded);
    assert!(f.a.mls(&group_key(&server(), &next.group_id())).is_err());
    assert_eq!(f.a.group(&server(), &after.group_id()).unwrap().status, Status::Rejoin);
    let live: Vec<_> = f.a.inventory(&server()).unwrap().into_iter().filter(|g| g.live).map(|g| g.group).collect();
    assert_eq!(live, vec![after.group_id()]);
}

#[test]
fn self_update_publishes_loaded_renewal_and_keeps_peer_gate_open_after_old_expiry() {
    let t = now();
    let mut a = device(1, 11, t);
    a.credential = certified(&key(1), &key(11), t - 29 * 86_400_000);
    let mut b = device(2, 21, t);
    let d = descriptor(&a, &b, t, true, true);
    let id = d.group_id();
    let info = a.create(d).unwrap();
    a.take_changes();
    let observer = Observer::create(&info, &server()).unwrap();
    assert!(a.settle(&server(), &id, Some(&id)).unwrap());
    a.take_changes();
    let out = b.join_external(&server(), &info, None, t, &()).unwrap();
    b.take_changes();
    let observer = accept(observer, &out, t);
    acknowledge(&mut b, &id, &out);
    a.receive(&server(), &id, &out.commit, &[]).unwrap();
    a.take_changes();
    let after_expiry = t + 86_400_000;
    assert!(!b.gate(&server(), &id, after_expiry, &()).unwrap().can_send());
    let renewed = certified(&key(1), &key(11), t);
    let mut a = Agent::load([11; 32], renewed.clone(), a.durable.clone()).unwrap();
    let out = a.commit(&server(), &id, &[], &[], t, &()).unwrap();
    a.take_changes();
    let observer = accept(observer, &out, t);
    acknowledge(&mut a, &id, &out);
    b.receive(&server(), &id, &out.commit, &[]).unwrap();
    b.take_changes();
    let group = b.mls(&group_key(&server(), &id)).unwrap();
    assert!(group.public_group().treesync().full_leaves().any(|(_, l)| leaf_credential(l) == Some(renewed.clone())));
    assert_eq!(observer.epoch(), 2);
    assert!(b.gate(&server(), &id, after_expiry, &()).unwrap().can_send());
}

#[test]
fn load_rejects_noncanonical_and_orphan_store_keys() {
    let f = joined();
    let group = group_key(&server(), &f.id);
    for suffix in [
        "10",
        "2",
        "",
        "0000000000000000000a",
        "000000000000000000000",
        "+0000000000000000001",
        "18446744073709551616",
        "00000000000000000001/extra",
    ] {
        let mut stores = f.a.durable.clone();
        stores.epochs.insert(format!("{group}/{suffix}").into_bytes(), json(&EpochRecord::Conflicted));
        assert!(matches!(Agent::load([11; 32], f.a.credential.clone(), stores), Err(Invalid("epoch key"))));
    }
    for which in 0..3 {
        for key in [b"unknown".to_vec(), format!("{group}/extra").into_bytes(), vec![255]] {
            let mut stores = f.a.durable.clone();
            match which {
                0 => {
                    let record = stores.groups[&group.as_bytes().to_vec()].clone();
                    stores.groups.insert(key, record);
                }
                1 => {
                    stores.epochs.insert(key, json(&EpochRecord::Conflicted));
                }
                _ => {
                    stores.outbox.insert(
                        key,
                        json(&Outbox {
                            group: f.id,
                            epoch: 0,
                            hash: [0; 32],
                            commit: vec![],
                            welcome: None,
                            group_info: vec![],
                            committed_at: 0,
                        }),
                    );
                }
            }
            assert!(Agent::load([11; 32], f.a.credential.clone(), stores).is_err());
        }
    }
    let mut stores = f.a.durable.clone();
    stores.epochs.insert(epoch_key(&group, u64::MAX).into_bytes(), json(&EpochRecord::Conflicted));
    assert!(Agent::load([11; 32], f.a.credential.clone(), stores).is_ok());
}

#[test]
fn load_validates_hpke_key_pair_and_positive_safe_statement_time() {
    let a = device(1, 11, now());
    for case in 0..6 {
        let mut stores = a.durable.clone();
        let mut record: DeviceRecord = parse(&stores.groups[b"@device".as_slice()]).unwrap();
        match case {
            0 => record.hpke_public[0] ^= 1,
            1 => record.hpke_private[1] ^= 1,
            2 => record.created_at = 0,
            3 => record.created_at = -1,
            4 => record.created_at = vc_proto::MAX_SAFE_INTEGER + 1,
            _ => record.created_at = i64::MAX,
        }
        stores.groups.insert(b"@device".to_vec(), json(&record));
        assert!(matches!(Agent::load([11; 32], a.credential.clone(), stores), Err(Invalid("device record"))));
    }
    let credential = Credential::Local { device_key: public(&key(11)) };
    for t in [0, -1, vc_proto::MAX_SAFE_INTEGER + 1] {
        assert!(matches!(Agent::new([11; 32], credential.clone(), t), Err(Invalid("time"))));
    }
    for t in [1, vc_proto::MAX_SAFE_INTEGER] {
        let mut a = Agent::new([11; 32], credential.clone(), t).unwrap();
        a.take_changes();
        let loaded = Agent::load([11; 32], credential.clone(), a.durable.clone()).unwrap();
        assert_eq!(loaded.device_hpke_key().created_at, t);
        assert!(loaded.device_hpke_key().verify());
    }
}

#[test]
fn creation_waits_for_persisted_server_acceptance() {
    let t = now();
    let mut a = device(1, 11, t);
    let b = device(2, 21, t);
    let d = descriptor(&a, &b, t, true, true);
    let id = d.group_id();
    let info = a.create(d).unwrap();
    let mut stores = a.durable.clone();
    a.take_changes().apply(&mut stores);
    let credential = a.credential.clone();
    let load = || Agent::load([11; 32], credential.clone(), stores.clone()).unwrap();
    a = load();
    let gate = a.gate(&server(), &id, t, &()).unwrap();
    assert_eq!(gate.status, Status::Creating);
    assert!(!gate.can_send() && !gate.rejoin && !gate.pending_commit);
    assert!(a.epoch(&server(), &id, 0).is_none());
    assert_eq!(a.sending_key(&server(), &id, t, &()), Err(AgentError::Blocked));
    assert_eq!(a.commit(&server(), &id, &[], &[], t, &()), Err(AgentError::Blocked));
    assert!(a.take_changes().is_empty());
    a = load();
    assert_eq!(a.share_history(&server(), &id, &b.device_hpke_key(), t, &()), Err(AgentError::Blocked));
    assert!(a.take_changes().is_empty());
    // Neither incoming traffic nor rejoining can silently accept a creation.
    a = load();
    assert_eq!(a.receive(&server(), &id, &[], &[]), Err(AgentError::Blocked));
    a = load();
    assert_eq!(a.proposal(&server(), &id, &[]), Err(AgentError::Blocked));
    a = load();
    assert_eq!(a.join_external(&server(), &info, None, t, &()), Err(AgentError::Blocked));
    a = load();
    assert_eq!(a.settle(&server(), &[0; 32], None), Err(Invalid("not creating")));
    assert!(a.take_changes().is_empty());
    a = load();
    assert!(a.settle(&server(), &id, Some(&id)).unwrap());
    assert_eq!(a.sending_key(&server(), &id, t, &()), Err(AgentError::Unpersisted));
    a.take_changes().apply(&mut stores);
    a = Agent::load([11; 32], credential.clone(), stores.clone()).unwrap();
    assert_eq!(a.group(&server(), &id).unwrap().status, Status::Active);
    assert_eq!(a.sending_key(&server(), &id, t, &()).unwrap().0, 0);
    assert_eq!(a.settle(&server(), &id, None), Err(Invalid("not creating")));
    assert!(a.take_changes().is_empty());
    a = Agent::load([11; 32], credential, stores).unwrap();
    assert_eq!(a.settle(&server(), &id, Some(&id)), Err(Invalid("not creating")));
}

#[test]
fn eviction_with_pending_commit_and_proposals_erases_only_target_mls_state() {
    let mut f = joined();
    // Keep another accepted group plus key packages, then compare the entire
    // OpenMLS store: deleting one group must preserve all unrelated records.
    let target_records = f.b.durable.openmls.clone();
    let d = descriptor(&f.b, &f.a, f.t, false, false);
    let other_id = d.group_id();
    f.b.create(d).unwrap();
    f.b.take_changes();
    assert!(f.b.settle(&server(), &other_id, Some(&other_id)).unwrap());
    f.b.take_changes();
    f.b.key_packages(2, f.t).unwrap();
    f.b.take_changes();
    let unrelated: Store =
        f.b.durable
            .openmls
            .iter()
            .filter(|(key, _)| !target_records.contains_key(*key))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
    let proposal = f.observer.propose_remove(&Ed25519Signer(key(200)), 1).unwrap();
    f.a.proposal(&server(), &f.id, &proposal).unwrap();
    f.a.take_changes();
    let winner = f.a.commit(&server(), &f.id, &[], &[], f.t, &()).unwrap();
    f.a.take_changes();
    f.observer = accept(f.observer, &winner, f.t);
    f.b.commit(&server(), &f.id, &[], &[], f.t, &()).unwrap();
    f.b.take_changes();
    let epochs = f.b.durable.epochs.clone();
    let record = f.b.group(&server(), &f.id).unwrap().clone();
    assert_eq!(
        f.b.reconcile(
            &server(),
            &f.id,
            Reconcile::Entry { epoch: winner.epoch, commit: winner.commit, proposals: vec![proposal] }
        )
        .unwrap(),
        Resolution::Rejoin
    );
    f.b.take_changes();
    assert_eq!(f.b.durable.openmls, unrelated);
    assert_eq!(f.b.durable.epochs, epochs);
    assert_eq!(f.b.group(&server(), &f.id).unwrap().descriptor, record.descriptor);
    assert_eq!(f.b.group(&server(), &f.id).unwrap().checkpoint, record.checkpoint);
}

#[test]
fn after_a_restart_a_creation_is_settled_with_the_servers_group_list() {
    let t = now();
    let mut a = device(1, 11, t);
    let mut b = device(2, 21, t);
    let d = descriptor(&a, &b, t, true, true);
    let id = d.group_id();
    let info = a.create(d).unwrap();
    let mut stores = a.durable.clone();
    a.take_changes().apply(&mut stores);
    let credential = a.credential.clone();
    // Never registered: `mls.groups` lists no group, so the creation goes and
    // the conversation can be created again.
    let mut a = Agent::load([11; 32], credential.clone(), stores.clone()).unwrap();
    assert!(!a.settle(&server(), &id, None).unwrap());
    let mut fresh = stores.clone();
    a.take_changes().apply(&mut fresh);
    let mut a = Agent::load([11; 32], credential.clone(), fresh).unwrap();
    assert!(a.group(&server(), &id).is_none());
    a.create(descriptor(&a, &b, t + 1, true, true)).unwrap();
    // Accepted, but the reply was lost and the peer joined meanwhile:
    // `mls.groups` lists this group, so the creation won.
    let observer = Observer::create(&info, &server()).unwrap();
    let join = b.join_external(&server(), &info, None, t, &()).unwrap();
    b.take_changes();
    accept(observer, &join, t);
    acknowledge(&mut b, &id, &join);
    let mut a = Agent::load([11; 32], credential, stores).unwrap();
    assert!(a.settle(&server(), &id, Some(&id)).unwrap());
    a.take_changes();
    a.receive(&server(), &id, &join.commit, &[]).unwrap();
    a.take_changes();
    assert_eq!(a.sending_key(&server(), &id, t, &()), b.sending_key(&server(), &id, t, &()));
}

#[test]
fn chat_uses_sending_gate_and_derived_or_imported_epoch_keys() {
    use crate::chat::{self, ChatError, Context};
    let mut f = fixture();
    let d = SignedDescriptor::parse(&f.a.group(&server(), &f.id).unwrap().descriptor).unwrap();
    let c = Context {
        server_key: server(),
        conversation: d.descriptor.conversation,
        group_id: f.id,
        epoch: 0,
        message_id: [7; 16],
        revision: 0,
    };
    let p = serde_json::to_vec(&serde_json::json!({
        "text": "hello", "mentions": [], "attachments": [],
        "signature": {
            "server_key": server(), "conversation": c.conversation,
            "group_id": b64(&f.id), "epoch": "0",
            "message_id": b64(&c.message_id), "revision": 0
        }
    }))
    .unwrap();
    let mut invalid_utf8 = p.clone();
    let text = invalid_utf8.windows(5).position(|w| w == b"hello").unwrap();
    invalid_utf8[text] = 0xff;
    assert_eq!(chat::seal(&f.a, &c, &invalid_utf8, f.t, &()), Err(ChatError::Context));
    let ciphertext = chat::seal(&f.a, &c, &p, f.t, &()).unwrap();
    assert_ne!(ciphertext, chat::seal(&f.a, &c, &p, f.t, &()).unwrap());
    assert_eq!(chat::open(&f.a, &c, &ciphertext).unwrap(), p);
    f.a.pending(&server(), &f.id, true).unwrap();
    f.a.take_changes();
    assert_eq!(chat::seal(&f.a, &c, &p, f.t, &()).unwrap_err(), ChatError::Agent(AgentError::Blocked));
    f.a.pending(&server(), &f.id, false).unwrap();
    f.a.take_changes();
    let out = f.b.join_external(&server(), &f.info, None, f.t, &()).unwrap();
    f.b.take_changes();
    f.observer = accept(f.observer, &out, f.t);
    acknowledge(&mut f.b, &f.id, &out);
    f.a.receive(&server(), &f.id, &out.commit, &[]).unwrap();
    f.a.take_changes();
    assert_eq!(chat::open(&f.b, &c, &ciphertext).unwrap_err(), ChatError::NoKey);
    assert_eq!(chat::seal(&f.a, &c, &p, f.t, &()).unwrap_err(), ChatError::Context);
    let chunks = f.a.share_history(&server(), &f.id, &f.b.device_hpke_key(), f.t, &()).unwrap();
    f.a.take_changes();
    for chunk in chunks {
        f.b.import_history(&server(), &f.id, &chunk).unwrap();
        f.b.take_changes();
    }
    assert_eq!(chat::open(&f.b, &c, &ciphertext).unwrap(), p);
    let mut wrong = c.clone();
    wrong.conversation.push('x');
    assert_eq!(chat::open(&f.b, &wrong, &ciphertext).unwrap_err(), ChatError::Context);
}

#[test]
fn pending_commit_recovery_requires_a_valid_strictly_newer_group_info() {
    for external in [false, true] {
        let mut f = joined();
        let pending = if external {
            f.a.join_external(&server(), &f.info, None, f.t, &()).unwrap()
        } else {
            f.a.commit(&server(), &f.id, &[], &[], f.t, &()).unwrap()
        };
        let mut durable = f.a.durable.clone();
        f.a.take_changes().apply(&mut durable);
        let reload = |stores| Agent::load([11; 32], f.a.credential.clone(), stores).unwrap();
        let mut a = reload(durable.clone());
        assert!(a.join_external(&server(), &f.info, None, f.t, &()).is_err());
        assert!(a.take_changes().is_empty());
        let mut a = reload(durable.clone());
        assert_eq!(a.outbox(&server(), &f.id).unwrap().hash, pending.hash);
        let winner = f.b.commit(&server(), &f.id, &[], &[], f.t, &()).unwrap();
        f.b.take_changes();
        f.observer = accept(f.observer, &winner, f.t);
        let mut invalid = winner.group_info.clone();
        let last = invalid.len() - 1;
        invalid[last] ^= 1;
        assert!(a.join_external(&server(), &invalid, None, f.t, &()).is_err());
        assert!(a.take_changes().is_empty());
        let mut a = reload(durable);
        let recovery = a.join_external(&server(), &winner.group_info, None, f.t, &()).unwrap();
        a.take_changes();
        assert_eq!(recovery.epoch, pending.epoch + 1);
        assert_eq!(a.group(&server(), &f.id).unwrap().status, Status::Joining);
        assert!(a.epoch(&server(), &f.id, recovery.epoch).is_none());
        assert!(a.sending_key(&server(), &f.id, f.t, &()).is_err());
        f.observer = accept(f.observer, &recovery, f.t);
        acknowledge(&mut a, &f.id, &recovery);
        assert_eq!(a.sending_key(&server(), &f.id, f.t, &()).unwrap().0, recovery.epoch + 1);
    }
}

// B remains at epoch 1 while A removes and readmits it at epochs 2 and 3.
fn offline_readmission() -> (Fixture, Vec<u8>) {
    let mut f = joined();
    let packages = f.b.key_packages(1, f.t).unwrap();
    f.b.take_changes();
    let out = f.a.commit(&server(), &f.id, &[], &[1], f.t, &()).unwrap();
    f.a.take_changes();
    f.observer = accept(f.observer, &out, f.t);
    acknowledge(&mut f.a, &f.id, &out);
    let out = f.a.commit(&server(), &f.id, &packages, &[], f.t, &()).unwrap();
    f.a.take_changes();
    f.observer = accept(f.observer, &out, f.t);
    acknowledge(&mut f.a, &f.id, &out);
    (f, out.welcome.unwrap())
}

#[test]
fn pruned_readmission_derives_welcome_epoch_and_survives_reload() {
    for pending in [false, true] {
        let (mut f, welcome) = offline_readmission();
        if pending {
            f.b.commit(&server(), &f.id, &[], &[], f.t, &()).unwrap();
            f.b.take_changes();
        }
        let old_key = f.b.epoch(&server(), &f.id, 1).unwrap().clone();
        let saved = f.b.durable.clone();
        assert!(f.b.welcome(&server(), "readmission", &welcome).is_err());
        f.b = Agent::load([21; 32], f.b.credential.clone(), saved).unwrap();
        assert!(!f.b.replace_stale_welcome(&server(), &f.id, "readmission", &welcome).unwrap().duplicate);
        f.b.take_changes();
        f.b = Agent::load([21; 32], f.b.credential.clone(), f.b.durable.clone()).unwrap();
        assert_eq!(f.b.sending_key(&server(), &f.id, f.t, &()), f.a.sending_key(&server(), &f.id, f.t, &()));
        assert!(matches!(f.b.epoch(&server(), &f.id, 3), Some(EpochRecord::Derived { .. })));
        assert_eq!(f.b.epoch(&server(), &f.id, 1), Some(&old_key));
        assert!(f.b.epoch(&server(), &f.id, 2).is_none());
        assert!(f.b.outbox(&server(), &f.id).is_none());
        assert!(f.b.replace_stale_welcome(&server(), &f.id, "readmission", b"").unwrap().duplicate);
        assert!(f.b.take_changes().is_empty());
    }
}

#[test]
fn stale_welcome_replacement_rejects_wrong_group_epoch_and_invalid_welcome() {
    for case in 0..6 {
        let (mut f, welcome) = offline_readmission();
        let key = group_key(&server(), &f.id);
        if case == 1 || case == 2 {
            let mut checkpoint = f.a.groups[&key].checkpoint.clone().unwrap();
            if case == 2 {
                checkpoint.epoch += 1;
            }
            f.b.groups.get_mut(&key).unwrap().checkpoint = Some(checkpoint);
        }
        if case == 5 {
            f.b.groups.get_mut(&key).unwrap().status = Status::Creating;
        }
        let expected_server = server();
        let result = f.b.replace_stale_welcome(
            if case == 4 { "wrong server" } else { &expected_server },
            if case == 0 { &[99; 32] } else { &f.id },
            "readmission",
            if case == 3 { b"invalid" } else { &welcome },
        );
        assert!(result.is_err(), "case {case}");
        assert!(f.b.take_changes().is_empty());
        // Rejection must leave the durable old membership and package usable.
        f.b = Agent::load([21; 32], f.b.credential.clone(), f.b.durable.clone()).unwrap();
        assert_eq!(f.b.group(&server(), &f.id).unwrap().checkpoint.as_ref().unwrap().epoch, 1);
        f.b.replace_stale_welcome(&server(), &f.id, "readmission", &welcome).unwrap();
    }
}
#[test]
fn evidence_records_epoch_ranges_per_uid_and_limits_history_off_sharing() {
    let t = now();
    let mut a = device(1, 11, t);
    let mut b = device(2, 21, t);
    let d = descriptor(&a, &b, t, false, false);
    let id = d.group_id();
    let info = a.create(d).unwrap();
    a.take_changes();
    let mut observer = Observer::create(&info, &server()).unwrap();
    assert!(a.settle(&server(), &id, Some(&id)).unwrap());
    a.take_changes();
    let join = b.join_external(&server(), &info, None, t, &()).unwrap();
    b.take_changes();
    observer = accept(observer, &join, t);
    acknowledge(&mut b, &id, &join);
    a.receive(&server(), &id, &join.commit, &[]).unwrap();
    a.take_changes();
    // B leaves at epoch 2 and rejoins at epoch 3.
    let remove = a.commit(&server(), &id, &[], &[1], t, &()).unwrap();
    a.take_changes();
    observer = accept(observer, &remove, t);
    acknowledge(&mut a, &id, &remove);
    let rejoin = b.join_external(&server(), &remove.group_info, Some(&observer.tree().unwrap().1), t, &()).unwrap();
    b.take_changes();
    accept(observer, &rejoin, t);
    acknowledge(&mut b, &id, &rejoin);
    a.receive(&server(), &id, &rejoin.commit, &[]).unwrap();
    a.take_changes();
    let range = |first, last| Interval { first, last };
    let evidence = &a.group(&server(), &id).unwrap().evidence;
    assert_eq!(evidence[&a.credential.uid()], [range(0, 3)]);
    assert_eq!(evidence[&b.credential.uid()], [range(1, 1), range(3, 3)]);
    assert_eq!(b.group(&server(), &id).unwrap().evidence[&a.credential.uid()], [range(1, 1), range(3, 3)]);
    let chunks = a.share_history(&server(), &id, &b.device_hpke_key(), t, &()).unwrap();
    a.take_changes();
    let epochs: Vec<_> = chunks
        .iter()
        .flat_map(|bytes| Chunk::parse(bytes).unwrap().open(&b.device.hpke_private).unwrap())
        .map(|(e, _)| e)
        .collect();
    assert_eq!(epochs, [1, 3]);
    // Stored evidence must stay sorted and disjoint.
    let mut stores = a.durable.clone();
    let key = group_key(&server(), &id);
    let mut record = a.groups[&key].clone();
    record.evidence.get_mut(&b.credential.uid()).unwrap().reverse();
    stores.groups.insert(key.into_bytes(), json(&record));
    assert_eq!(Agent::load([11; 32], a.credential.clone(), stores).err(), Some(Invalid("stored evidence")));
}
#[test]
fn own_commit_time_follows_accepted_own_commits_only() {
    let mut f = joined();
    let own = |a: &Agent, id| a.group(&server(), id).unwrap().own_commit_at;
    assert_eq!(own(&f.a, &f.id), Some(f.t));
    assert_eq!(own(&f.b, &f.id), Some(f.t));
    let out = f.a.commit(&server(), &f.id, &[], &[], f.t + 5, &()).unwrap();
    f.a.take_changes();
    assert_eq!(own(&f.a, &f.id), Some(f.t), "a pending commit is not accepted yet");
    acknowledge(&mut f.a, &f.id, &out);
    assert_eq!(own(&f.a, &f.id), Some(f.t + 5));
    f.b.receive(&server(), &f.id, &out.commit, &[]).unwrap();
    f.b.take_changes();
    assert_eq!(own(&f.b, &f.id), Some(f.t));
    let mut c = device(2, 23, f.t);
    let packages = c.key_packages(1, f.t).unwrap();
    c.take_changes();
    let add = f.a.commit(&server(), &f.id, &packages, &[], f.t + 6, &()).unwrap();
    f.a.take_changes();
    acknowledge(&mut f.a, &f.id, &add);
    c.welcome(&server(), "w", add.welcome.as_ref().unwrap()).unwrap();
    c.take_changes();
    assert_eq!(own(&c, &f.id), None);
}
#[test]
fn welcome_readmission_clears_own_commit_time() {
    let own = |a: &Agent, id| a.group(&server(), id).unwrap().own_commit_at;
    // Removed, then readmitted by an ordinary Welcome.
    let mut f = joined();
    assert_eq!(own(&f.b, &f.id), Some(f.t));
    let packages = f.b.key_packages(1, f.t).unwrap();
    f.b.take_changes();
    let removal = f.a.commit(&server(), &f.id, &[], &[1], f.t, &()).unwrap();
    f.a.take_changes();
    acknowledge(&mut f.a, &f.id, &removal);
    f.b.receive(&server(), &f.id, &removal.commit, &[]).unwrap();
    f.b.take_changes();
    let add = f.a.commit(&server(), &f.id, &packages, &[], f.t, &()).unwrap();
    f.a.take_changes();
    acknowledge(&mut f.a, &f.id, &add);
    assert!(!f.b.welcome(&server(), "back", add.welcome.as_ref().unwrap()).unwrap().duplicate);
    f.b.take_changes();
    let b = Agent::load([21; 32], f.b.credential.clone(), f.b.durable.clone()).unwrap();
    assert_eq!(own(&b, &f.id), None);
    // Readmitted while the device missed the removal.
    let (mut f, welcome) = offline_readmission();
    assert_eq!(own(&f.b, &f.id), Some(f.t));
    f.b.replace_stale_welcome(&server(), &f.id, "readmission", &welcome).unwrap();
    f.b.take_changes();
    let b = Agent::load([21; 32], f.b.credential.clone(), f.b.durable.clone()).unwrap();
    assert_eq!(own(&b, &f.id), None);
}
#[test]
fn forgetting_a_group_leaves_nothing_of_it() {
    let mut f = joined();
    let target = f.a.durable.openmls.clone();
    // Another group of the same device and its key packages must survive.
    let d = descriptor(&f.a, &f.b, f.t, false, true);
    let other = d.group_id();
    f.a.create(d).unwrap();
    f.a.take_changes();
    assert!(f.a.settle(&server(), &other, Some(&other)).unwrap());
    f.a.take_changes();
    f.a.key_packages(2, f.t).unwrap();
    f.a.take_changes();
    let unrelated: Store =
        f.a.durable
            .openmls
            .iter()
            .filter(|(k, _)| !target.contains_key(*k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
    let other_key = group_key(&server(), &other);
    let kept = |stores: &Stores| {
        (
            stores.groups.keys().filter(|k| k.starts_with(other_key.as_bytes())).count(),
            stores.epochs.keys().filter(|k| k.starts_with(other_key.as_bytes())).count(),
        )
    };
    let other_records = kept(&f.a.durable);
    // The target holds a stored proposal and an unacknowledged own commit in the outbox.
    let proposal = f.observer.propose_remove(&Ed25519Signer(key(200)), 1).unwrap();
    f.a.proposal(&server(), &f.id, &proposal).unwrap();
    f.a.take_changes();
    let out = f.a.commit(&server(), &f.id, &[], &[], f.t, &()).unwrap();
    f.a.take_changes();
    assert!(f.a.outbox(&server(), &f.id).is_some());
    let mut stores = f.a.durable.clone();
    f.a.forget(&server(), &f.id).unwrap();
    f.a.take_changes().apply(&mut stores);
    assert_eq!(stores, f.a.durable);
    let a = Agent::load([11; 32], f.a.credential.clone(), stores.clone()).unwrap();
    assert!(a.group(&server(), &f.id).is_none() && a.outbox(&server(), &f.id).is_none());
    assert_eq!(stores.openmls, unrelated, "only the other group's and the device's OpenMLS records");
    let target_key = group_key(&server(), &f.id);
    assert!(
        !stores
            .groups
            .keys()
            .chain(stores.epochs.keys())
            .chain(stores.outbox.keys())
            .any(|k| k.starts_with(target_key.as_bytes()))
    );
    assert_eq!(kept(&stores), other_records);
    assert!(a.sending_key(&server(), &other, f.t, &()).is_ok());
    // Forgetting is idempotent and the device can join again later.
    f.a.forget(&server(), &f.id).unwrap();
    assert!(f.a.take_changes().is_empty());
    let again = f.a.join_external(&server(), &out.group_info, None, f.t, &()).unwrap();
    assert_eq!(again.group, f.id);
}

fn channel_fixture(kind: Kind) -> Fixture {
    let t = now();
    let mut a = device(1, 11, t);
    let mut b = device(2, 21, t);
    if kind == Kind::Voice {
        a = Agent::new_voice([11; 32], a.credential.clone(), t).unwrap();
        b = Agent::new_voice([21; 32], b.credential.clone(), t).unwrap();
        a.take_changes();
        b.take_changes();
    }
    let mut d = descriptor(&a, &b, t, false, kind != Kind::Voice).descriptor;
    d.kind = kind;
    if kind == Kind::Server {
        d.conversation = "server".into();
    } else if kind == Kind::Voice {
        d.conversation = "voice:1".into();
        d.channel_group_id = Some(b64(&[9; 32]));
    }
    let d = SignedDescriptor::sign(d, a.credential.clone(), &a.signer.0);
    let id = d.group_id();
    let info = a.create(d).unwrap();
    a.take_changes();
    let observer = Observer::create(&info, &server()).unwrap();
    a.settle(&server(), &id, Some(&id)).unwrap();
    a.take_changes();
    Fixture { a, b, observer, id, info, t }
}

#[test]
fn separate_tree_join_updates_removal_and_behind() {
    for kind in [Kind::Channel, Kind::Server, Kind::Voice] {
        let mut f = channel_fixture(kind);
        assert!(info(&f.info).unwrap().extensions().ratchet_tree().is_some());
        let old_tree = f.observer.tree().unwrap().1;
        let out = f.a.commit(&server(), &f.id, &[], &[], f.t, &()).unwrap();
        f.a.take_changes();
        let parsed = info(&out.group_info).unwrap();
        assert_eq!(parsed.extensions().iter().count(), 1);
        assert!(parsed.extensions().external_pub().is_some());
        let mut forged = out.group_info.clone();
        *forged.last_mut().unwrap() ^= 1;
        let observer = Observer::load(&f.observer.save(), &server()).unwrap();
        assert!(matches!(
            observer.commit(&out.commit, &forged, None, f.t, f.t),
            Err(ds::DsError::Invalid("group info signature"))
        ));
        let before = f.observer.save();
        f.observer = accept(f.observer, &out, f.t);
        acknowledge(&mut f.a, &f.id, &out);
        // The same epoch's GroupInfo with the tree in it is refused: these groups leave it out.
        let group = f.a.mls(&group_key(&server(), &f.id)).unwrap();
        let with_tree = wire(group.export_group_info(f.a.provider.crypto(), &f.a.signer, true).unwrap()).unwrap();
        assert!(matches!(
            Observer::load(&before, &server()).unwrap().commit(&out.commit, &with_tree, None, f.t, f.t),
            Err(ds::DsError::Invalid("group info does not match the commit"))
        ));
        let (epoch, tree) = f.observer.tree().unwrap();
        assert_eq!(epoch, 1);
        assert_ne!(tree, old_tree);
        assert_eq!(f.a.behind(&server(), &f.id, &out.group_info, Some(&tree)), Ok(false));
        assert_eq!(f.a.behind(&server(), &f.id, &f.info, None), Ok(true));
        assert!(f.a.behind(&server(), &f.id, &out.group_info, Some(&old_tree)).is_err());
        let other_tree = channel_fixture(kind).observer.tree().unwrap().1;
        for supplied in [None, Some(old_tree.as_slice()), Some(other_tree.as_slice()), Some(&[0][..])] {
            let mut b = device(2, 21, f.t);
            assert!(b.join_external(&server(), &out.group_info, supplied, f.t, &()).is_err());
        }
        let mut forged = out.group_info.clone();
        *forged.last_mut().unwrap() ^= 1;
        assert!(f.a.behind(&server(), &f.id, &forged, Some(&tree)).is_err());
        let join = f.b.join_external(&server(), &out.group_info, Some(&tree), f.t, &()).unwrap();
        f.b.take_changes();
        assert!(info(&join.group_info).unwrap().extensions().ratchet_tree().is_none());
        f.observer = accept(f.observer, &join, f.t);
        acknowledge(&mut f.b, &f.id, &join);
        f.a.receive(&server(), &f.id, &join.commit, &[]).unwrap();
        f.a.take_changes();
        assert_eq!(f.a.behind(&server(), &f.id, &out.group_info, Some(&tree)), Ok(true));
        let update = f.b.commit(&server(), &f.id, &[], &[], f.t, &()).unwrap();
        f.b.take_changes();
        assert!(info(&update.group_info).unwrap().extensions().ratchet_tree().is_none());
        f.observer = accept(f.observer, &update, f.t);
        acknowledge(&mut f.b, &f.id, &update);
        f.a.receive(&server(), &f.id, &update.commit, &[]).unwrap();
        f.a.take_changes();
        let remove = f.a.commit(&server(), &f.id, &[], &[1], f.t, &()).unwrap();
        assert!(info(&remove.group_info).unwrap().extensions().ratchet_tree().is_none());
        accept(f.observer, &remove, f.t);
    }
}

#[test]
fn dm_requires_embedded_tree_and_ignores_supplied_tree() {
    let mut f = fixture();
    let out = f.a.commit(&server(), &f.id, &[], &[], f.t, &()).unwrap();
    f.a.take_changes();
    acknowledge(&mut f.a, &f.id, &out);
    let group = f.a.mls(&group_key(&server(), &f.id)).unwrap();
    let without = wire(group.export_group_info(f.a.provider.crypto(), &f.a.signer, false).unwrap()).unwrap();
    let tree = group.export_ratchet_tree().tls_serialize_detached().unwrap();
    assert!(Observer::create(&without, &server()).is_err());
    assert_eq!(f.a.behind(&server(), &f.id, &without, Some(&tree)), Err(Invalid("no ratchet tree")));
    assert!(f.observer.commit(&out.commit, &without, None, f.t, f.t).is_err());
    assert_eq!(f.a.behind(&server(), &f.id, &out.group_info, Some(&[0])), Ok(false));
    assert!(f.b.join_external(&server(), &out.group_info, Some(&[0]), f.t, &()).is_ok());
    let mut b = device(2, 21, f.t);
    assert_eq!(b.join_external(&server(), &without, Some(&tree), f.t, &()), Err(Invalid("no ratchet tree")));
}

#[test]
fn channel_key_package_adds_are_refused_by_agent_and_observer() {
    for kind in [Kind::Channel, Kind::Voice] {
        let mut f = channel_fixture(kind);
        let package = device(2, 21, f.t).key_packages(1, f.t).unwrap().remove(0);
        assert!(f.a.commit(&server(), &f.id, std::slice::from_ref(&package), &[], f.t, &()).is_err());
        // A nonconforming client can still construct an Add; shared rules must refuse it.
        let mut a =
            Agent::load_store([11; 32], f.a.credential.clone(), f.a.durable.clone(), kind == Kind::Voice).unwrap();
        let mut group = a.mls(&group_key(&server(), &f.id)).unwrap();
        let package = KeyPackageIn::tls_deserialize_exact(&package)
            .unwrap()
            .validate(a.provider.crypto(), ProtocolVersion::Mls10)
            .unwrap();
        group.set_configuration(a.provider.storage(), &join_config(Kind::Dm)).unwrap();
        group.set_aad(ds::commit_aad(f.t));
        let (commit, welcome, _) = group.add_members(&a.provider, &a.signer, &[package]).unwrap();
        group.merge_pending_commit(&a.provider).unwrap();
        // Without the tree, as these groups' GroupInfos are: only the Add itself is wrong.
        let gi = wire(group.export_group_info(a.provider.crypto(), &a.signer, false).unwrap()).unwrap();
        let welcome = wire(welcome).unwrap();
        assert!(matches!(
            f.observer.commit(&wire(commit).unwrap(), &gi, Some(&welcome), f.t, f.t),
            Err(ds::DsError::Invalid("separate-tree groups join externally"))
        ));
        // A Welcome beside a commit that adds nobody is refused too.
        let mut f2 = channel_fixture(kind);
        let update = f2.a.commit(&server(), &f2.id, &[], &[], f2.t, &()).unwrap();
        f2.a.take_changes();
        assert!(matches!(
            f2.observer.commit(&update.commit, &update.group_info, Some(&welcome), f2.t, f2.t),
            Err(ds::DsError::Invalid("channel and server groups take no Welcome"))
        ));
        f.b.take_changes();
        assert!(f.b.welcome(&server(), "channel", &welcome).is_err());
        a.take_changes();
    }
}

#[test]
// About seven minutes in debug: each join verifies the growing tree.
#[ignore = "350-device stress test"]
fn channel_tree_larger_than_handshake_limit() {
    let started = std::time::Instant::now();
    let mut f = channel_fixture(Kind::Channel);
    for i in 0..350u32 {
        let seed = hash(&i.to_be_bytes());
        let credential = certified(&key(2), &SigningKey::from_bytes(&seed), f.t - 86_400_000);
        let mut joiner = Agent::new(seed, credential, f.t).unwrap();
        joiner.take_changes();
        let (epoch, tree) = f.observer.tree().unwrap();
        assert_eq!(epoch, i as u64);
        if i == 349 {
            assert!(tree.len() > MAX_HANDSHAKE_BYTES, "{} bytes", tree.len());
        }
        let out = joiner.join_external(&server(), &f.info, Some(&tree), f.t, &()).unwrap();
        assert!(out.commit.len() < MAX_HANDSHAKE_BYTES);
        assert!(out.group_info.len() < MAX_HANDSHAKE_BYTES);
        assert!(out.welcome.is_none());
        f.observer = accept(f.observer, &out, f.t);
        f.info = out.group_info;
    }
    assert_eq!(f.observer.leaves().len(), 351);
    eprintln!("350 external joins: {:?}", started.elapsed());
}

#[test]
fn voice_uses_external_joins_parent_gates_and_no_durable_chat_keys() {
    let t = now();
    let mut parent = device(1, 11, t);
    let mut other_parent = device(2, 21, t);
    let d = descriptor(&parent, &other_parent, t, false, true);
    let parent_id = d.group_id();
    let parent_info = parent.create(d).unwrap();
    parent.take_changes();
    parent.settle(&server(), &parent_id, Some(&parent_id)).unwrap();
    parent.take_changes();
    let mut parent_observer = Observer::create(&parent_info, &server()).unwrap();
    let make_voice = |seed, credential| {
        let mut a = Agent::new_voice([seed; 32], credential, t).unwrap();
        a.take_changes();
        a
    };
    let mut a = make_voice(11, parent.credential.clone());
    let mut b = make_voice(21, other_parent.credential.clone());
    let mut desc = descriptor(&a, &b, t, false, false).descriptor;
    desc.kind = Kind::Voice;
    desc.conversation = "voice:1".into();
    desc.channel_group_id = Some(b64(&parent_id));
    let signed = SignedDescriptor::sign(desc, a.credential.clone(), &a.signer.0);
    let id = signed.group_id();
    let info = a.create(signed.clone()).unwrap();
    a.take_changes();
    assert_eq!(a.voice_keys(&server(), &id, &parent, t, &()), Err(AgentError::Blocked));
    let mut observer = Observer::create(&info, &server()).unwrap();
    let mut memory = a.durable.clone();
    assert!(Agent::load([11; 32], a.credential.clone(), memory.clone()).is_err());
    a = Agent::load_voice([11; 32], a.credential.clone(), memory.clone()).unwrap();
    a.settle(&server(), &id, Some(&id)).unwrap();
    a.take_changes().apply(&mut memory);
    let keys = a.voice_keys(&server(), &id, &parent, t, &()).unwrap();
    assert_eq!(keys.epoch, 0);
    assert_eq!(keys.members.len(), 1);
    assert!(a.epoch(&server(), &id, 0).is_none());
    assert!(a.group(&server(), &id).unwrap().evidence.is_empty());
    assert!(a.sending_key(&server(), &id, t, &()).is_err());
    assert!(a.share_history(&server(), &id, &b.device_hpke_key(), t, &()).is_err());
    assert!(a.import_history(&server(), &id, b"not history").is_err());
    assert!(a.key_packages(1, t).is_err());
    assert!(a.welcome(&server(), "delivery", b"not Welcome").is_err());
    let join = b.join_external(&server(), &info, None, t, &()).unwrap();
    b.take_changes();
    assert_eq!(b.voice_keys(&server(), &id, &other_parent, t, &()), Err(AgentError::Blocked));
    observer = accept(observer, &join, t);
    acknowledge(&mut b, &id, &join);
    a.receive(&server(), &id, &join.commit, &[]).unwrap();
    a.take_changes();
    // Historical voice acceptance is independent of the parent's current sync.
    assert_eq!(a.voice_keys(&server(), &id, &parent, t, &()), Err(AgentError::Blocked));
    assert_eq!(a.voice_parent_removals(&server(), &id, &parent, false), Err(AgentError::Blocked));
    let pj = other_parent.join_external(&server(), &parent_info, None, t, &()).unwrap();
    other_parent.take_changes();
    parent_observer = accept(parent_observer, &pj, t);
    acknowledge(&mut other_parent, &parent_id, &pj);
    parent.receive(&server(), &parent_id, &pj.commit, &[]).unwrap();
    parent.take_changes();
    assert!(a.voice_parent_removals(&server(), &id, &parent, true).unwrap().is_empty());
    assert_eq!(
        a.voice_keys(&server(), &id, &parent, t, &()).unwrap().members,
        b.voice_keys(&server(), &id, &other_parent, t, &()).unwrap().members
    );
    assert_eq!(observer.leaves().len(), 2);
    // A lost commit reply is settled from its exact bytes, keeping all old keys out of stores.
    let update = a.commit(&server(), &id, &[], &[], t, &()).unwrap();
    a.take_changes();
    observer = accept(observer, &update, t);
    let stored = a.durable.clone();
    a = Agent::load_voice([11; 32], a.credential.clone(), stored).unwrap();
    acknowledge(&mut a, &id, &update);
    assert_eq!(a.voice_keys(&server(), &id, &parent, t, &()).unwrap().epoch, observer.epoch());
    assert!(a.epochs.is_empty());
    // A parent removal gates current sending, without invalidating historical voice commits.
    let own = other_parent.mls(&group_key(&server(), &parent_id)).unwrap().own_leaf_index().u32();
    let remove = parent.commit(&server(), &parent_id, &[], &[own], t, &()).unwrap();
    parent.take_changes();
    parent_observer = accept(parent_observer, &remove, t);
    acknowledge(&mut parent, &parent_id, &remove);
    assert_eq!(parent_observer.leaves().len(), 1);
    assert_eq!(a.voice_keys(&server(), &id, &parent, t, &()), Err(AgentError::Blocked));
    assert_eq!(a.voice_parent_removals(&server(), &id, &parent, true).unwrap().len(), 1);
    assert_eq!(voice_kid(0x1_0000_0001, 7), 0x1_0000_0007);
    assert_eq!(voice_kid(u64::MAX, u32::MAX), u64::MAX);
}

#[test]
fn voice_rejects_add_chain_and_wrong_parent_binding() {
    let t = now();
    let parent = device(1, 11, t);
    let other = device(2, 21, t);
    let mut a = Agent::new_voice([11; 32], parent.credential.clone(), t).unwrap();
    a.take_changes();
    let mut d = descriptor(&a, &other, t, false, false).descriptor;
    d.kind = Kind::Voice;
    d.conversation = "voice:1".into();
    d.channel_group_id = Some(b64(&[9; 32]));
    let signed = SignedDescriptor::sign(d, a.credential.clone(), &a.signer.0);
    let id = signed.group_id();
    a.create(signed.clone()).unwrap();
    a.take_changes();
    a.settle(&server(), &id, Some(&id)).unwrap();
    a.take_changes();
    assert!(a.voice_keys(&server(), &id, &parent, t, &()).is_err());
    let snapshot = a.durable.clone();
    assert!(a.commit(&server(), &id, &[vec![0]], &[], t, &()).is_err());
    a = Agent::load_voice([11; 32], parent.credential.clone(), snapshot).unwrap();
    assert!(a.accept_chain(&server(), "voice:1", &[(id, signed.encode())]).is_err());
    let mut d = signed.descriptor;
    d.history = true;
    assert!(Descriptor::parse(&d.statement()).is_none());
    d.history = false;
    d.incarnation = 1;
    d.previous_group_id = Some(b64(&[3; 32]));
    assert!(Descriptor::parse(&d.statement()).is_none());
}
