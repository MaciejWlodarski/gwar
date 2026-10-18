use super::*;
use crate::credential::tests::{key, public};

const NOW: i64 = 1_800_000_000_000;

fn account() -> SigningKey {
    key(1)
}
fn account_key() -> String {
    public(&account())
}
fn certificate(device: &SigningKey, issued_at: i64) -> DeviceCertificate {
    let mut c = DeviceCertificate {
        account_key: account_key(),
        device_key: public(device),
        issued_at,
        expires_at: issued_at + 30 * 86_400_000,
        signature: String::new(),
    };
    c.signature = b64(&account().sign(vc_proto::device_statement(&c).as_bytes()).to_bytes());
    c
}
struct Device {
    signing: SigningKey,
    secret: [u8; 32],
    public: [u8; 32],
}
fn device(seed: u8) -> Device {
    let (secret, public) = x25519_pair([seed; 32]);
    Device { signing: key(seed), secret, public }
}
fn recipient(d: &Device) -> Recipient {
    Recipient { device_key: public(&d.signing), x25519: d.public }
}
fn revocation(device: &SigningKey, at: i64) -> Revocation {
    let mut r = Revocation { device_key: public(device), revoked_at: at, signature: String::new() };
    r.signature = b64(&account().sign(r.statement(&account_key()).as_bytes()).to_bytes());
    r
}

#[test]
fn archive_keys_are_signed_and_canonical() {
    let a = device(11);
    let k = DeviceArchiveKey::sign(&account_key(), &a.signing, a.public, NOW);
    assert_eq!(DeviceArchiveKey::parse(&k.statement(), &k.signature), Some(k.clone()));
    assert_eq!(DeviceArchiveKey::parse(&k.statement().replace("v1", "v2"), &k.signature), None);
    let (secret, public) = recovery_key_pair(&[7; 20], &account().verifying_key().to_bytes());
    assert_eq!(recovery_key_pair(&[7; 20], &account().verifying_key().to_bytes()), (secret, public));
    assert_ne!(recovery_key_pair(&[8; 20], &account().verifying_key().to_bytes()).1, public);
    let r = RecoveryArchiveKey::sign(&account(), 1, public, NOW);
    assert_eq!(RecoveryArchiveKey::parse(&r.statement(), &r.signature), Some(r.clone()));
    // Signed by a device key, not the account key: refused.
    let forged =
        RecoveryArchiveKey { signature: b64(&a.signing.sign(r.statement().as_bytes()).to_bytes()), ..r.clone() };
    assert_eq!(RecoveryArchiveKey::parse(&forged.statement(), &forged.signature), None);
}

#[test]
fn a_manifest_seals_its_key_to_recipients_and_the_recovery_key_and_binds_every_ciphertext() {
    let (a, b, c) = (device(11), device(12), device(13));
    let (recovery_secret, recovery_public) = recovery_key_pair(&[7; 20], &account().verifying_key().to_bytes());
    let recovery = RecoveryArchiveKey::sign(&account(), 1, recovery_public, NOW);
    let (bytes, generation_key) = Manifest::create(
        1,
        None,
        &a.signing,
        &certificate(&a.signing, NOW - 1),
        &[recipient(&b), recipient(&a)],
        Some(&recovery),
        &[revocation(&c.signing, NOW - 10)],
        NOW,
    )
    .unwrap();
    let m = Manifest::parse(&bytes, &account_key()).unwrap();
    assert_eq!(m.recovery_version, Some(1));
    assert_eq!(m.open(Seat::Device(&public(&a.signing)), &a.secret).unwrap(), generation_key);
    assert_eq!(m.open(Seat::Device(&public(&b.signing)), &b.secret).unwrap(), generation_key);
    assert_eq!(m.open(Seat::Recovery, &recovery_secret).unwrap(), generation_key);
    assert!(m.open(Seat::Device(&public(&c.signing)), &c.secret).is_err());
    // Swapping one sealed key (Connect knows the public keys) breaks the body hash.
    let [statement, signature, cert, body] = parse_netstrings(&bytes, 4).unwrap()[..] else { panic!() };
    let [revocations, _sealed] = parse_netstrings(body, 2).unwrap()[..] else { panic!() };
    let forged_sealed = seal_to(&b.public, b"x", b"y", &[0; 32]).unwrap();
    let forged_body = netstrings(&[revocations, &netstrings(&[&forged_sealed, &forged_sealed, &forged_sealed])]);
    let forged = netstrings(&[statement, signature, cert, &forged_body]);
    assert_eq!(Manifest::parse(&forged, &account_key()), Err(Invalid("manifest body")));
    // Another account, or a certificate that does not cover its time, is refused.
    assert!(Manifest::parse(&bytes, &public(&key(2))).is_err());
    assert!(
        Manifest::create(1, None, &a.signing, &certificate(&a.signing, NOW + 1), &[recipient(&a)], None, &[], NOW)
            .is_err()
    );
    // The creator must be a recipient.
    assert!(
        Manifest::create(1, None, &a.signing, &certificate(&a.signing, NOW - 1), &[recipient(&b)], None, &[], NOW)
            .is_err()
    );
    // A chain: generation 2 names generation 1.
    let (next, _) = Manifest::create(
        2,
        Some(&m.hash()),
        &a.signing,
        &certificate(&a.signing, NOW - 1),
        &[recipient(&a)],
        None,
        &[],
        NOW,
    )
    .unwrap();
    assert_eq!(Manifest::parse(&next, &account_key()).unwrap().previous_hash, Some(m.hash()));
}

#[test]
fn a_generation_is_safe_only_by_what_the_device_knows() {
    let (a, b, c) = (device(11), device(12), device(13));
    let (bytes, _) = Manifest::create(
        1,
        None,
        &a.signing,
        &certificate(&a.signing, NOW - 1),
        &[recipient(&a), recipient(&b)],
        None,
        &[revocation(&c.signing, NOW - 10)],
        NOW,
    )
    .unwrap();
    let m = Manifest::parse(&bytes, &account_key()).unwrap();
    let current: BTreeMap<String, [u8; 32]> = [&a, &b].iter().map(|d| (public(&d.signing), d.public)).collect();
    let known: BTreeMap<String, i64> = [(public(&c.signing), NOW - 10)].into();
    let me = public(&b.signing);
    let k = |revocations, current, recovery_version, me| Knowledge { revocations, current, recovery_version, me };
    assert!(m.safe(&k(&known, &current, None, &me)));
    // A revocation it does not carry; a recipient no longer current; another recovery version;
    // this device not a recipient: unsafe.
    let more: BTreeMap<String, i64> = [(public(&c.signing), NOW - 10), (public(&key(14)), NOW)].into();
    assert!(!m.safe(&k(&more, &current, None, &me)));
    let fewer: BTreeMap<String, [u8; 32]> = [(public(&a.signing), a.public)].into();
    let a_key = public(&a.signing);
    assert!(!m.safe(&k(&known, &fewer, None, &a_key)));
    assert!(!m.safe(&k(&known, &current, Some(1), &me)));
    let c_key = public(&c.signing);
    assert!(!m.safe(&k(&known, &current, None, &c_key)));
    // The creator revoked before it created the manifest.
    assert!(m.creator_revoked(&[(public(&a.signing), NOW - 1)].into()));
    assert!(!m.creator_revoked(&[(public(&a.signing), NOW + 1)].into()));
}

#[test]
fn records_are_bound_to_their_account_generation_and_id() {
    let key: [u8; 32] = [5; 32];
    let record = RecordKey { server: public(&key_of(200)), group: [3; 32], epoch: 42, key: [9; 32] };
    let (id, data) = seal_record(&key, &account_key(), 1, &record).unwrap();
    assert_eq!(open_record(&key, &account_key(), 1, &id, &data).unwrap(), record);
    assert!(open_record(&key, &account_key(), 2, &id, &data).is_err());
    assert!(open_record(&key, &account_key(), 1, &[0; 16], &data).is_err());
    assert!(open_record(&key, &public(&key_of(2)), 1, &id, &data).is_err());
    // Oversized data is refused before decryption.
    assert_eq!(open_record(&key, &account_key(), 1, &id, &vec![0; MAX_RECORD_BYTES + 1]), Err(ArchiveError::Open));
}
fn key_of(seed: u8) -> SigningKey {
    key(seed)
}

#[test]
fn a_transfer_gives_one_generation_key_to_one_recipient() {
    let (a, b) = (device(11), device(12));
    let (bytes, generation_key) =
        Manifest::create(1, None, &a.signing, &certificate(&a.signing, NOW - 1), &[recipient(&a)], None, &[], NOW)
            .unwrap();
    let m = Manifest::parse(&bytes, &account_key()).unwrap();
    let to = TransferRecipient::Device { device_key: public(&b.signing), x25519: b.public };
    let t = Transfer::create(
        &m,
        &generation_key,
        &a.signing,
        &certificate(&a.signing, NOW - 1),
        to.clone(),
        &b.public,
        NOW,
    )
    .unwrap();
    let parsed = Transfer::parse(&t, &account_key()).unwrap();
    assert_eq!((parsed.manifest_hash.as_str(), &parsed.recipient), (m.hash().as_str(), &to));
    assert_eq!(parsed.open(&b.secret).unwrap(), generation_key);
    assert!(parsed.open(&a.secret).is_err());
    // The sealed key is bound by its hash.
    let [statement, signature, cert, _] = parse_netstrings(&t, 4).unwrap()[..] else { panic!() };
    let forged = netstrings(&[statement, signature, cert, &seal_to(&b.public, b"x", b"y", &[0; 32]).unwrap()]);
    assert_eq!(Transfer::parse(&forged, &account_key()), Err(Invalid("transfer binding")));
    // Size before parsing, and no extra fields.
    assert_eq!(Transfer::parse(&b"0:,".repeat(MAX_TRANSFER_BYTES), &account_key()), Err(Invalid("transfer size")));
    let extra = [t.as_slice(), b"0:,"].concat();
    assert_eq!(Transfer::parse(&extra, &account_key()), Err(Invalid("transfer")));
    // A recovery recipient.
    let (secret, public_key) = recovery_key_pair(&[7; 20], &account().verifying_key().to_bytes());
    let t = Transfer::create(
        &m,
        &generation_key,
        &a.signing,
        &certificate(&a.signing, NOW - 1),
        TransferRecipient::Recovery { version: 2 },
        &public_key,
        NOW,
    )
    .unwrap();
    assert_eq!(Transfer::parse(&t, &account_key()).unwrap().open(&secret).unwrap(), generation_key);
}
