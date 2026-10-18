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
    code: Option<&[u8]>,
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
    let g1 = laptop.archive_create_manifest(t).unwrap();
    assert_eq!(sync(&mut laptop, &devices, &[], &[], std::slice::from_ref(&g1), &[], None, t).safe, Some(1));
    // An epoch key goes up once.
    let epoch = format!("{}/{}/{:020}", server, b64(&[3; 32]), 7);
    laptop.epochs.insert(epoch.clone(), EpochRecord::Derived { key: [9; 32], archived: false });
    let upload = laptop.archive_seal(100).unwrap().unwrap();
    assert_eq!((upload.generation, upload.records.len()), (1, 1));
    laptop.archive_uploaded(&upload.epochs).unwrap();
    laptop.take_changes();
    assert!(laptop.archive_seal(100).unwrap().unwrap().records.is_empty());

    // The phone joins: it cannot read generation 1 until the laptop sends a transfer.
    let devices = vec![entry(&laptop, t), entry(&phone, t)];
    let phone_key = phone.credential.device_key().to_owned();
    assert!(sync(&mut phone, &devices, &[], &[], std::slice::from_ref(&g1), &[], None, t).opened.is_empty());
    assert_eq!(sync(&mut laptop, &devices, &[], &[], &[], &[], None, t).waiting_devices, vec![phone_key.clone()]);
    let transfers = laptop.archive_transfers_for(&phone_key, t).unwrap();
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
    let g2 = laptop.archive_create_manifest(t).unwrap();
    assert_eq!(sync(&mut laptop, &devices, &recovery, &[], std::slice::from_ref(&g2), &[], None, t).safe, Some(2));
    // A new device with only the recovery code opens generation 2.
    let mut fresh = device(13, t);
    let mut all = devices.clone();
    all.push(entry(&fresh, t));
    let state = sync(&mut fresh, &all, &recovery, &[], &[g1.clone(), g2.clone()], &[], Some(&code), t);
    assert_eq!(state.opened, vec![2]);

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
    let g3 = laptop.archive_create_manifest(t + 2).unwrap();
    assert_eq!(
        sync(&mut laptop, &only_laptop, &recovery, &[], std::slice::from_ref(&g3), &[], None, t + 2).safe,
        Some(3)
    );
    let m3 = Manifest::parse(&g3, &public(&key(1))).unwrap();
    assert!(m3.recipients.iter().all(|r| r.device_key != phone_key));
    assert!(m3.revocations.iter().any(|r| r.device_key == phone_key));

    // A different manifest for a generation it saw is refused; the state reloads.
    let other = laptop.archive_create_manifest(t + 2).unwrap();
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
