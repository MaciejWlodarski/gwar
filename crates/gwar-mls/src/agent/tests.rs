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
    let out = f.b.join_external(&server(), &f.info, f.t, &()).unwrap();
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
    let out = f.b.join_external(&server(), &f.info, f.t, &()).unwrap();
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
        } else {
            assert!(f.a.mls(&group_key(&server(), &f.id)).unwrap().pending_commit().is_none());
        }
    }
}
#[test]
fn losing_or_expired_external_join_is_deleted() {
    for expired in [false, true] {
        let mut f = fixture();
        f.b.join_external(&server(), &f.info, f.t, &()).unwrap();
        f.b.take_changes();
        let winner = f.a.commit(&server(), &f.id, &[], &[], f.t, &()).unwrap();
        f.a.take_changes();
        let result = if expired {
            Reconcile::Rejected { reason: Rejection::Expired }
        } else {
            Reconcile::Entry { epoch: 0, commit: winner.commit, proposals: vec![] }
        };
        assert_eq!(f.b.reconcile(&server(), &f.id, result).unwrap(), Resolution::Rejoin);
        f.b.take_changes();
        assert!(f.b.mls(&group_key(&server(), &f.id)).is_err());
        assert!(f.b.epochs.is_empty());
        f.b.join_external(&server(), &winner.group_info, f.t, &()).unwrap();
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
    let out = f.a.commit(&server(), &f.id, &[], &[], f.t, &revoked).unwrap();
    f.a.take_changes();
    f.observer = accept(f.observer, &out, f.t);
    acknowledge(&mut f.a, &f.id, &out);
    f.b.receive(&server(), &f.id, &out.commit, &[]).unwrap();
    f.b.take_changes();
    assert_eq!(f.b.group(&server(), &f.id).unwrap().status, Status::Removed);
    assert_eq!(f.b.group(&server(), &f.id).unwrap().checkpoint, checkpoint);
    assert!(f.b.epoch(&server(), &f.id, 3).is_none());
    assert!(f.a.gate(&server(), &f.id, f.t, &revoked).unwrap().can_send());
    let rejoin = f.b.join_external(&server(), &out.group_info, f.t, &()).unwrap();
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
    let out = b.join_external(&server(), &f.info, f.t, &()).unwrap();
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
    assert!(!f.a.gate(&server(), &f.id, f.t, &()).unwrap().can_send());
    let out = f.a.commit(&server(), &f.id, &[], &[], f.t, &()).unwrap();
    f.a.take_changes();
    f.observer = accept(f.observer, &out, f.t);
    acknowledge(&mut f.a, &f.id, &out);
    f.b.receive(&server(), &f.id, &out.commit, &[proposal]).unwrap();
    f.b.take_changes();
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
    assert_eq!(f.a.join_external(&server(), &y.group_info, f.t, &()), Err(Invalid("freshness")));
    assert!(f.a.take_changes().is_empty());
    let mut a = Agent::load([11; 32], f.a.credential.clone(), durable).unwrap();
    assert_eq!(a.join_external(&server(), &original, f.t, &()), Err(Invalid("freshness")));
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
    let join = b.join_external(&server(), &info, t, &()).unwrap();
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
        f.a.epochs.insert(epoch_key(&k, e), EpochRecord::Derived { key: [7; 32], uids: BTreeSet::new() });
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
    a.bundle(id, group.epoch().as_u64(), bundle).unwrap()
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
    assert_eq!(f.b.join_external(&server(), &info, f.t, &()), Err(Invalid("group pin")));
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
        .key_package_lifetime(Lifetime::init(sec - 1, sec + 2))
        .leaf_node_capabilities(capabilities())
        .build(SUITE, &f.b.provider, &f.b.signer, f.b.with_key())
        .unwrap()
        .into_key_package();
    let out = raw_commit(&mut f.a, &f.id, f.t, vec![package], LeafNodeParameters::default(), None);
    f.observer = accept(f.observer, &out, f.t);
    std::thread::sleep(std::time::Duration::from_secs(3));
    f.b.welcome(&server(), "delayed", out.welcome.as_ref().unwrap()).unwrap();
    f.b.take_changes();
    let mut c = device(1, 12, f.t);
    c.join_external(&server(), &out.group_info, f.t, &()).unwrap();
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
    assert_eq!(f.b.join_external(&server(), &old, f.t, &()), Err(Invalid("removed checkpoint")));
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
    let out = f.b.join_external(&server(), &f.info, f.t, &()).unwrap();
    let changes = f.b.take_changes();
    let mut rolled_back = Agent::load([21; 32], f.b.credential.clone(), before.clone()).unwrap();
    assert!(rolled_back.group(&server(), &f.id).is_none());
    rolled_back.join_external(&server(), &f.info, f.t, &()).unwrap();
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
    let out = f.a.bundle(&f.id, group.epoch().as_u64(), bundle).unwrap();
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
        .with_config(join_config())
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
