use super::*;
use crate::credential::tests::{certified, key, public};
use ed25519_dalek::Signer as _;

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}
/// Device `seed` of account 1.
fn device(seed: u8, t: i64) -> Agent {
    let mut a = Agent::new([seed; 32], certified(&key(1), &key(seed), t - 1000), t).unwrap();
    a.take_changes();
    a
}
fn entry(a: &Agent, t: i64) -> ArchiveDevice {
    let (statement, signature) = a.archive_device_key(t).unwrap();
    let crate::Credential::Account(c) = &a.credential else { unreachable!() };
    ArchiveDevice {
        statement,
        signature,
        certificate: Some(ArchiveCertificate {
            issued_at: c.issued_at,
            expires_at: c.expires_at,
            signature: c.signature.clone(),
        }),
    }
}
#[allow(clippy::too_many_arguments)]
fn sync(
    a: &mut Agent,
    devices: &[ArchiveDevice],
    recovery: &[(String, String)],
    revocations: &[ArchiveRevocation],
    manifests: &[Vec<u8>],
    transfers: &[Vec<u8>],
    code: Option<[u8; 32]>,
    t: i64,
) -> ArchiveState {
    let state = a.archive_sync(devices, recovery, revocations, manifests, transfers, code, t).unwrap();
    a.take_changes();
    state
}

#[test]
fn devices_share_the_archive_through_generations_transfers_and_the_recovery_code() {
    let t = now();
    let server = public(&key(200));
    let mut laptop = device(11, t);
    let mut phone = device(12, t);
    let devices = vec![entry(&laptop, t)];
    // No generation yet: the laptop creates the first one.
    assert_eq!(sync(&mut laptop, &devices, &[], &[], &[], &[], None, t).safe, None);
    let g1 = laptop.archive_create_manifest(t, &[]).unwrap();
    assert_eq!(sync(&mut laptop, &devices, &[], &[], std::slice::from_ref(&g1), &[], None, t).safe, Some(1));
    // An epoch key goes up once.
    let epoch = format!("{}/{}/{:020}", server, b64(&[3; 32]), 7);
    laptop.epochs.insert(epoch.clone(), EpochRecord::Derived { key: [9; 32], archived: false });
    let upload = laptop.archive_seal(100, t, &[]).unwrap().unwrap();
    assert_eq!((upload.generation, upload.records.len()), (1, 1));
    laptop.archive_uploaded(&upload.epochs).unwrap();
    laptop.take_changes();
    assert!(laptop.archive_seal(100, t, &[]).unwrap().unwrap().records.is_empty());

    // The phone joins: it cannot read generation 1 until the laptop sends a transfer.
    let devices = vec![entry(&laptop, t), entry(&phone, t)];
    let phone_key = phone.credential.device_key().to_owned();
    assert!(sync(&mut phone, &devices, &[], &[], std::slice::from_ref(&g1), &[], None, t).opened.is_empty());
    assert_eq!(sync(&mut laptop, &devices, &[], &[], &[], &[], None, t).waiting, vec![phone_key.clone()]);
    let transfers = laptop.archive_transfers_for(&phone_key, t, &[]).unwrap();
    assert_eq!(transfers.len(), 1);
    laptop.archive_transfers_sent(&transfers.iter().map(|(tag, _)| tag.clone()).collect::<Vec<_>>()).unwrap();
    laptop.take_changes();
    let bytes: Vec<Vec<u8>> = transfers.into_iter().map(|(_, b)| b).collect();
    assert_eq!(sync(&mut phone, &devices, &[], &[], &[], &bytes, None, t).opened, vec![1]);
    let records: Vec<(u64, String, String)> =
        upload.records.iter().map(|(id, data)| (1, id.clone(), data.clone())).collect();
    assert_eq!(phone.archive_import(&records).unwrap(), 1);
    phone.take_changes();
    // Its group is unknown to the phone: the key waits there, never creating a group.
    assert_eq!(phone.archive.as_ref().unwrap().held.len(), 1);
    assert!(phone.epochs.is_empty());

    // A recovery code: generation 1 is not sealed to it, so it is unsafe and generation 2 follows.
    let code = [5u8; 20];
    let (_, recovery_public) = archive::recovery_key_pair(&code, &key(1).verifying_key().to_bytes());
    let recovery_key = RecoveryArchiveKey::sign(&key(1), 1, recovery_public, t);
    let recovery = vec![(recovery_key.statement(), recovery_key.signature.clone())];
    assert_eq!(sync(&mut laptop, &devices, &recovery, &[], &[], &[], None, t).safe, None);
    let g2 = laptop.archive_create_manifest(t, &[]).unwrap();
    assert_eq!(sync(&mut laptop, &devices, &recovery, &[], std::slice::from_ref(&g2), &[], None, t).safe, Some(2));
    // A new device with only the recovery code opens generation 2.
    let mut fresh = device(13, t);
    let mut all = devices.clone();
    all.push(entry(&fresh, t));
    let state = sync(&mut fresh, &all, &recovery, &[], &[g1.clone(), g2.clone()], &[], None, t);
    assert!(state.opened.is_empty());
    // The code given later opens the generation it accepted before.
    assert_eq!(
        sync(
            &mut fresh,
            &all,
            &recovery,
            &[],
            &[],
            &[],
            Some(archive::recovery_key_pair(&code, &key(1).verifying_key().to_bytes()).0),
            t
        )
        .opened,
        vec![2]
    );

    // Revoking the phone makes generation 2 unsafe; generation 3 leaves it out.
    let revoked_at = t + 1;
    let statement = format!("gwar revoke v1\n{}\n{phone_key}\n{revoked_at}", public(&key(1)));
    let revocation = ArchiveRevocation {
        device_key: phone_key.clone(),
        revoked_at,
        signature: b64(&key(1).sign(statement.as_bytes()).to_bytes()),
    };
    let only_laptop = vec![entry(&laptop, t), entry(&phone, t)];
    assert_eq!(
        sync(&mut laptop, &only_laptop, &recovery, std::slice::from_ref(&revocation), &[], &[], None, t + 2).safe,
        None
    );
    let g3 = laptop.archive_create_manifest(t + 2, &[]).unwrap();
    assert_eq!(
        sync(&mut laptop, &only_laptop, &recovery, &[], std::slice::from_ref(&g3), &[], None, t + 2).safe,
        Some(3)
    );
    let m3 = Manifest::parse(&g3, &public(&key(1))).unwrap();
    assert!(m3.recipients.iter().all(|r| r.device_key != phone_key));
    assert!(m3.revocations.iter().any(|r| r.device_key == phone_key));

    // A different manifest for a generation it saw is refused; the state reloads.
    let other = laptop.archive_create_manifest(t + 2, &[]).unwrap();
    // (The test's epoch has no group record, which loading requires: leave it out.)
    let mut stores = laptop.durable.clone();
    stores.epochs.clear();
    let mut copy = Agent::load([11; 32], laptop.credential.clone(), stores).unwrap();
    assert_eq!(copy.archive.as_ref().unwrap().checkpoints.len(), 3);
    let forked = Manifest::parse(&other, &public(&key(1))).unwrap();
    assert_eq!(forked.generation, 4, "the next number");
    assert!(
        copy.archive_sync(&only_laptop, &recovery, &[], std::slice::from_ref(&g1), &[], None, t + 2).is_ok(),
        "the same manifest again changes nothing"
    );
}

fn revoke(device_key: &str, revoked_at: i64) -> ArchiveRevocation {
    let statement = format!("gwar revoke v1\n{}\n{device_key}\n{revoked_at}", public(&key(1)));
    ArchiveRevocation {
        device_key: device_key.into(),
        revoked_at,
        signature: b64(&key(1).sign(statement.as_bytes()).to_bytes()),
    }
}
fn certificate(a: &Agent) -> DeviceCertificate {
    let crate::Credential::Account(c) = &a.credential else { unreachable!() };
    c.clone()
}

#[test]
fn revocations_carried_by_manifests_and_expiring_certificates_narrow_recipients() {
    let t = now();
    let mut laptop = device(11, t);
    let mut phone = device(12, t);
    // The tablet's certificate expires ten seconds from now.
    let mut tablet = Agent::new([14; 32], certified(&key(1), &key(14), t - 30 * 86_400_000 + 10_000), t).unwrap();
    tablet.take_changes();
    let tablet_key = tablet.credential.device_key().to_owned();
    let devices = vec![entry(&laptop, t), entry(&phone, t), entry(&tablet, t)];
    sync(&mut laptop, &devices, &[], &[], &[], &[], None, t);
    let g1 = laptop.archive_create_manifest(t, &[]).unwrap();
    assert_eq!(sync(&mut laptop, &devices, &[], &[], std::slice::from_ref(&g1), &[], None, t).safe, Some(1));
    // Rechecked when sealing: once the tablet's certificate expires, generation 1 is unsafe.
    assert!(laptop.archive_seal(100, t, &[]).unwrap().is_some());
    assert!(laptop.archive_seal(100, t + 20_000, &[]).unwrap().is_none());

    // The phone learns of the tablet's revocation and makes generation 2 carrying it.
    let revocation = revoke(&tablet_key, t + 1);
    sync(&mut phone, &devices, &[], std::slice::from_ref(&revocation), std::slice::from_ref(&g1), &[], None, t + 2);
    let g2 = phone.archive_create_manifest(t + 2, &[]).unwrap();
    // The laptop's feed lacks the revocation; the manifest's revocation still leaves the tablet
    // out of transfers and new generations.
    let state = sync(&mut laptop, &devices, &[], &[], std::slice::from_ref(&g2), &[], None, t + 2);
    assert_eq!(state.safe, Some(2));
    assert!(state.waiting.is_empty());
    assert!(laptop.archive_transfers_for(&tablet_key, t + 2, &[]).is_err());
    let g3 = Manifest::parse(&laptop.archive_create_manifest(t + 3, &[]).unwrap(), &public(&key(1))).unwrap();
    assert!(g3.recipients.iter().all(|r| r.device_key != tablet_key));

    // A reader refuses a manifest whose creator its own carried revocations revoked.
    let r = revoke(&tablet_key, t + 1);
    let carried = Revocation { device_key: r.device_key, revoked_at: r.revoked_at, signature: r.signature };
    let m1 = Manifest::parse(&g1, &public(&key(1))).unwrap();
    let recipients: Vec<Recipient> = m1.recipients.clone();
    let (forged, _) = Manifest::create(
        2,
        Some(&m1.hash()),
        &tablet.signer.0,
        &certificate(&tablet),
        &recipients,
        None,
        std::slice::from_ref(&carried),
        t + 2,
    )
    .unwrap();
    let mut fresh = device(13, t);
    let mut all = devices.clone();
    all.push(entry(&fresh, t));
    assert_eq!(
        fresh.archive_sync(&all, &[], &[], &[g1.clone(), forged], &[], None, t + 2),
        Err(AgentError::Invalid("a manifest by a revoked device"))
    );
}

#[test]
fn a_device_keeps_the_first_archive_key_of_each_device() {
    let t = now();
    let mut laptop = device(11, t);
    let phone = device(12, t);
    let phone_key = phone.credential.device_key().to_owned();
    let both = [entry(&laptop, t), entry(&phone, t)];
    sync(&mut laptop, &both, &[], &[], &[], &[], None, t);
    // Connect later shows another archive key for the phone: the first stays, and the phone is
    // no recipient while Connect shows the other.
    let other = DeviceArchiveKey::sign(&public(&key(1)), &phone.signer.0, [7; 32], t);
    let mut swapped = entry(&phone, t);
    (swapped.statement, swapped.signature) = (other.statement(), other.signature.clone());
    let both = [both[0].clone(), swapped];
    sync(&mut laptop, &both, &[], &[], &[], &[], None, t);
    let record = laptop.archive.as_ref().unwrap();
    assert_eq!(record.devices[&phone_key].x25519, phone.device.hpke_public);
    assert!(!record.current(t).contains_key(&phone_key));
    let g1 = Manifest::parse(&laptop.archive_create_manifest(t, &[]).unwrap(), &public(&key(1))).unwrap();
    assert!(g1.recipients.iter().all(|r| r.device_key != phone_key));
}

#[test]
fn transfers_are_owed_per_generation_and_recipient() {
    let t = now();
    let mut laptop = device(11, t);
    let phone = device(12, t);
    let phone_key = phone.credential.device_key().to_owned();
    let alone = vec![entry(&laptop, t)];
    sync(&mut laptop, &alone, &[], &[], &[], &[], None, t);
    let g1 = laptop.archive_create_manifest(t, &[]).unwrap();
    sync(&mut laptop, &alone, &[], &[], std::slice::from_ref(&g1), &[], None, t);
    let code = [5u8; 20];
    let (_, recovery_public) = archive::recovery_key_pair(&code, &key(1).verifying_key().to_bytes());
    let recovery_key = RecoveryArchiveKey::sign(&key(1), 1, recovery_public, t);
    let recovery = vec![(recovery_key.statement(), recovery_key.signature.clone())];
    // Generation 1 is not sealed to the new recovery key: it is owed a transfer.
    assert_eq!(sync(&mut laptop, &alone, &recovery, &[], &[], &[], None, t).waiting, vec!["recovery:1".to_owned()]);
    let g2 = laptop.archive_create_manifest(t, &[]).unwrap();
    sync(&mut laptop, &alone, &recovery, &[], std::slice::from_ref(&g2), &[], None, t);

    let both = vec![entry(&laptop, t), entry(&phone, t)];
    let state = sync(&mut laptop, &both, &recovery, &[], &[], &[], None, t);
    assert_eq!(state.waiting, vec![phone_key.clone(), "recovery:1".to_owned()]);
    let transfers = laptop.archive_transfers_for(&phone_key, t, &[]).unwrap();
    assert_eq!(
        transfers.iter().map(|(tag, _)| tag.clone()).collect::<Vec<_>>(),
        [format!("1/{phone_key}"), format!("2/{phone_key}")]
    );
    // Only the first reached Connect: the phone still waits for the second.
    laptop.archive_transfers_sent(&[transfers[0].0.clone()]).unwrap();
    laptop.take_changes();
    let state = sync(&mut laptop, &both, &recovery, &[], &[], &[], None, t);
    assert!(state.waiting.contains(&phone_key));
    assert_eq!(laptop.archive_transfers_for(&phone_key, t, &[]).unwrap().len(), 1);
    // A transfer on Connect counts, whoever sent it.
    let recovery_transfers = laptop.archive_transfers_for("recovery:1", t, &[]).unwrap();
    let bytes: Vec<Vec<u8>> = recovery_transfers.into_iter().map(|(_, b)| b).collect();
    let state = sync(&mut laptop, &both, &recovery, &[], &[], &bytes, None, t);
    assert_eq!(state.waiting, vec![phone_key]);
}

#[test]
fn held_keys_keep_conflicts_and_settle_when_the_group_appears() {
    let t = now();
    let server = public(&key(200));
    let mut laptop = device(11, t);
    let devices = vec![entry(&laptop, t)];
    sync(&mut laptop, &devices, &[], &[], &[], &[], None, t);
    let g1 = laptop.archive_create_manifest(t, &[]).unwrap();
    sync(&mut laptop, &devices, &[], &[], std::slice::from_ref(&g1), &[], None, t);
    let descriptor = crate::SignedDescriptor::sign(
        crate::Descriptor {
            server_key: server.clone(),
            conversation: "channel:1".into(),
            kind: crate::Kind::Channel,
            incarnation: 0,
            previous_group_id: None,
            private: false,
            history: true,
            founding_owner: None,
            channel_group_id: None,
            creator_uid: laptop.credential.uid(),
            created_at: t,
            creation_nonce: b64(&[1; 16]),
        },
        laptop.credential.clone(),
        &laptop.signer.0,
    );
    let group = descriptor.group_id();
    let generation_key = laptop.archive.as_ref().unwrap().keys[&1];
    let account = public(&key(1));
    let record = |epoch: u64, value: u8| {
        let entry = RecordKey { server: server.clone(), group, epoch, key: [value; 32] };
        let (id, data) = archive::seal_record(&generation_key, &account, 1, &entry).unwrap();
        (1, b64(&id), b64(&data))
    };
    // Epoch 5 once; epoch 6 twice with different keys, in one import and then again.
    assert_eq!(laptop.archive_import(&[record(5, 1), record(6, 2), record(6, 3)]).unwrap(), 3);
    laptop.take_changes();
    laptop.archive_import(&[record(6, 2)]).unwrap();
    laptop.take_changes();
    let held = &laptop.archive.as_ref().unwrap().held;
    assert_eq!(held.len(), 2);
    assert!(held.values().any(|h| *h == Held::Conflicted));
    assert!(laptop.epochs.is_empty());
    // An unsettled creation holds no keys; once it settles, its held keys move into its epochs
    // in the same change.
    laptop.create(descriptor).unwrap();
    laptop.take_changes();
    assert!(laptop.epochs.is_empty());
    assert!(laptop.settle(&server, &group, Some(&group)).unwrap());
    let group_text = group_key(&server, &group);
    assert!(matches!(
        laptop.epochs.get(&epoch_key(&group_text, 5)),
        Some(EpochRecord::Imported { key, archived: true, .. }) if *key == [1; 32]
    ));
    assert_eq!(laptop.epochs.get(&epoch_key(&group_text, 6)), Some(&EpochRecord::Conflicted));
    assert!(laptop.archive.as_ref().unwrap().held.is_empty());
    laptop.take_changes();
    let reloaded = Agent::load([11; 32], laptop.credential.clone(), laptop.durable.clone()).unwrap();
    assert_eq!(reloaded.epochs.get(&epoch_key(&group_text, 6)), Some(&EpochRecord::Conflicted));
}

#[test]
fn a_stored_archive_record_is_checked_on_load() {
    let t = now();
    let mut laptop = device(11, t);
    let devices = vec![entry(&laptop, t)];
    sync(&mut laptop, &devices, &[], &[], &[], &[], None, t);
    let g1 = laptop.archive_create_manifest(t, &[]).unwrap();
    sync(&mut laptop, &devices, &[], &[], std::slice::from_ref(&g1), &[], None, t);
    let g2 = laptop.archive_create_manifest(t, &[]).unwrap();
    sync(&mut laptop, &devices, &[], &[], std::slice::from_ref(&g2), &[], None, t);
    let stored: serde_json::Value = serde_json::from_slice(&laptop.durable.groups[KEY]).unwrap();
    let load = |edit: &dyn Fn(&mut serde_json::Value)| {
        let mut value = stored.clone();
        edit(&mut value);
        let mut stores = laptop.durable.clone();
        stores.groups.insert(KEY.to_vec(), serde_json::to_vec(&value).unwrap());
        Agent::load([11; 32], laptop.credential.clone(), stores).map(|_| ())
    };
    assert_eq!(load(&|_| {}), Ok(()));
    let invalid = Err(AgentError::Invalid("stored archive record"));
    assert_eq!(load(&|v| v["checkpoints"]["2"] = v["checkpoints"]["1"].clone()), invalid);
    assert_eq!(
        load(&|v| {
            v["checkpoints"].as_object_mut().unwrap().remove("1");
            v["manifests"].as_object_mut().unwrap().remove("1");
        }),
        invalid
    );
    assert_eq!(load(&|v| v["manifests"].as_object_mut().unwrap().remove("2").map(|_| ()).unwrap()), invalid);
    assert_eq!(load(&|v| v["keys"]["3"] = v["keys"]["1"].clone()), invalid);
    assert_eq!(load(&|v| v["sent"] = serde_json::json!(["9/x"])), invalid);
    assert_eq!(load(&|v| v["account_key"] = public(&key(2)).into()), invalid);
}

#[test]
fn unsigned_certificate_times_are_bounded_before_any_arithmetic() {
    let t = now();
    let laptop = device(11, t);
    let mut forged = entry(&laptop, t);
    forged.certificate = Some(ArchiveCertificate { issued_at: i64::MIN, expires_at: t + 1, signature: "x".into() });
    let crate::Credential::Account(c) = &laptop.credential else { unreachable!() };
    assert!(!certificate_current(
        &c.account_key,
        laptop.credential.device_key(),
        forged.certificate.as_ref().unwrap(),
        t
    ));
    let mut reader = device(12, t);
    sync(&mut reader, &[forged], &[], &[], &[], &[], None, t);
    assert!(reader.archive.as_ref().unwrap().current(t).is_empty());
}

#[test]
fn uploads_transfers_and_new_generations_judge_the_feed_now() {
    let t = now();
    let mut laptop = device(11, t);
    let phone = device(12, t);
    let tablet = device(14, t);
    let tablet_key = tablet.credential.device_key().to_owned();
    let devices = vec![entry(&laptop, t), entry(&phone, t)];
    sync(&mut laptop, &devices, &[], &[], &[], &[], None, t);
    let g1 = laptop.archive_create_manifest(t, &[]).unwrap();
    sync(&mut laptop, &devices, &[], &[], std::slice::from_ref(&g1), &[], None, t);
    let all = vec![entry(&laptop, t), entry(&phone, t), entry(&tablet, t)];
    sync(&mut laptop, &all, &[], &[], &[], &[], None, t);
    assert_eq!(laptop.archive_transfers_for(&tablet_key, t, &[]).unwrap().len(), 1);
    // The feed revokes the tablet before the next sync: no transfer to it, generation 1 is unsafe
    // and the next generation leaves it out.
    let revoked = [revoke(&tablet_key, t)];
    assert!(laptop.archive_transfers_for(&tablet_key, t, &revoked).is_err());
    assert!(laptop.archive_seal(100, t, &revoked).unwrap().is_none());
    let g2 = Manifest::parse(&laptop.archive_create_manifest(t, &revoked).unwrap(), &public(&key(1))).unwrap();
    assert!(g2.recipients.iter().all(|r| r.device_key != tablet_key));
    assert!(g2.revocations.iter().any(|r| r.device_key == tablet_key));
}
