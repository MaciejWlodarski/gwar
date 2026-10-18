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

// Golden vectors (docs/e2e-build.md, section 7, "Tests and acceptance"): statements and
// signatures are deterministic; the manifest, record and transfer were made once (their HPKE and
// nonces are random) and must keep parsing and opening the same.
const DEVICE_STATEMENT: &str = "gwar archive device v1\niojj3XQJ8ZX9UtstPLpdcspnCb8dlBIb83SIAbQPb1w\nZr5-Myx6RTMyvZ0Kf32wVfXF7xoGraZtmLOftoEMRzo\nc7LYt2qptTZgAyvI9di-46OuTjs6f9Sa3oH3NHo0qmg\n1800000000000";
const DEVICE_SIGNATURE: &str = "IZ9Xue9TRNjoJ67VfGwlHOT_xqxBYyz9pvoN56Ha437NL4c1s5VeKMJCyxiCFzk5WvQP6io6nUpOCrvBhRQnAA";
const RECOVERY_SECRET: &str = "wsE9Zu0C9ZRa32xyamWkiuQn3t4P6Kp31hBB6J6aOsA";
const RECOVERY_STATEMENT: &str = "gwar archive recovery v1\niojj3XQJ8ZX9UtstPLpdcspnCb8dlBIb83SIAbQPb1w\n1\nAdYez8CTRak_2wmVbKJzGXGL61WgXF0wZg7rPPOCjWc\n1800000000000";
const RECOVERY_SIGNATURE: &str =
    "_J0wCKw7P2hrrn3GC_GS9zHyXHCWydG8vYLNmAAerNp1TqZP6Z7CbpaIKeTvLK78SNRelyfSaLPiN-Weo_cgAA";
const REVOCATION_SIGNATURE: &str =
    "YJXz-eci-w_FlUIlkmv1eP__uNAZNSSV9sI2XKd05yBg214bpg_GnVxy-TnAxtOWKATs-fDw1yeStjxuyj3rAw";
const MANIFEST: &str = concat!(
    "Mzk2Omd3YXIgYXJjaGl2ZSBtYW5pZmVzdCB2MQppb2pqM1hRSjhaWDlVdHN0UExwZGNzcG5DYjhkbEJJYjgzU0lBYlFQYjF3",
    "CjEKLQpacjUtTXl4NlJUTXl2WjBLZjMyd1ZmWEY3eG9HcmFadG1MT2Z0b0VNUnpvCkMxRTYyYlNTUUJYS0NRTHRCNUJFMDZ4",
    "ZHZzSXdid2FVakJEYWpyYmpueTA6bDhPeEMwMXNFenA0NmwzTUhQWkNIVC1CcmpleDlpak9GTXB2em5jdzh6TSxacjUtTXl4",
    "NlJUTXl2WjBLZjMyd1ZmWEY3eG9HcmFadG1MT2Z0b0VNUnpvOmM3TFl0MnFwdFRaZ0F5dkk5ZGktNDZPdVRqczZmOVNhM29I",
    "M05IbzBxbWcKMQpLN1p0bHgxcVhyUjVORUxPaVVQZ01oZUtWY1loWkw3U3k1azR0dGxwbXRzClBNWmNwTlIzUWlrT043elR0",
    "aHNTSURFRFVWLV9GVHFfN2pZWnJwX3lUZ1EKMTgwMDAwMDAwMDAwMCw4NjpDQ0VHT0s4NFZ1R3JOYUdlcE5GY19vSXZTekk3",
    "eVFid1lFd09ralM5dUhQX2FfUkhfN3FvZmJWd1FxLWlvRlFDUXJKNW5vY09MbWMtUnd2azhGX3REZywyMjU6MTMwOmd3YXIg",
    "ZGV2aWNlIHYxCmlvamozWFFKOFpYOVV0c3RQTHBkY3NwbkNiOGRsQkliODNTSUFiUVBiMXcKWnI1LU15eDZSVE15dlowS2Yz",
    "MndWZlhGN3hvR3JhWnRtTE9mdG9FTVJ6bwoxNzk5OTk5OTk5OTk5CjE4MDI1OTE5OTk5OTksODY6S2dUQ0hubjFlMjNoY2ZP",
    "REZhS1NiVjB6bXFQazlhZmxNbGZYbEZtVXQ0a2hnUlRLMnFXT1JROVBsR2JnMTd2U0tHMkNzNFRkRWVZb05PZGhTdHV5Qmcs",
    "LDQ3ODoyMTY6MjExOjExNjpnd2FyIHJldm9rZSB2MQppb2pqM1hRSjhaWDlVdHN0UExwZGNzcG5DYjhkbEJJYjgzU0lBYlFQ",
    "YjF3CmthS0tDM1E0RlpPazJVYVZlU0NKSnFfSXJZTElnNXQyUkRXYm5ycWFTem8KMTc5OTk5OTk5OTk5MCw4NjpZSlh6LWVj",
    "aS13X0ZsVUlsa212MWVQX191TkFaTlNTVjlzSTJYS2QwNXlCZzIxNGJwZ19HblZ4eS1UbkF4dE9XS0FUcy1mRHcxeWVTdGp4",
    "dXlqM3JBdywsLDI1Mjo4MDrOxO9-q2pp_-VoE3_ngi4iUSs71s1FBOuY415P3XDWY7dRFqG4vj8suxqTF8g8dn7H5u0xJPac",
    "4y7iI-onslI19-AsLLLfr7XvL7m-CvJALCw4MDpCvSvKPTfgM-kxrlB_wNq0DQGlfQOAlR26PNKrSTVXeAHsIXeozoS1eFpx",
    "k2sB_qluQZv0tCKaJOyXkKS9jkueZNUJbarRJL8rm03ZEs2_aiw4MDrMtOP354wX6X9wp6a6Xexfzq3Qzn85yUWAY7TWNVXx",
    "ShoKrMeFu_3sDYDoIfkhWbMB1zAtzWL3-0atdLOyitcNrxmaYw2h1nzPTO3VbH7zIywsLA"
);
const MANIFEST_HASH: &str = "88R13kz5H1VZiD-c_biQoUHqmK80sisMZ0QG2oz2YBo";
const GENERATION_KEY: &str = "TzUPvCHn-y7RUzptyTGZjfkGB1QQWCpIM6GD-xREfy0";
const RECORD_ID: &str = "iylmaVjXGRZjJxBLe_4jAQ";
const RECORD_DATA: &str = concat!(
    "YHytEk8vPDSD9-dXsDo3wM0UbESX_oGfKBuinYEEREoooB2cc4QcwPnfFDLHjFfMk7WqNOyr5sq5ODFNRmaF8eZ9FonU7eDD",
    "2oA7tcuA9lDPI-lHnuVrsDLF7K3GMlNz-YX1USycMc0e94MN2Ok0cnLvSPHr0X-A1Z7sXQLfgbQhCG5dzyfjWsiognpBNFTF",
    "IaJqLGbA7DpNy86s2hhVO_w4S7vm82uK-07zw1u4syujlYwJTzI2YvIKbS3y3IyDlo5c6knGQ4YYuqY"
);
const TRANSFER: &str = concat!(
    "MjcxOmd3YXIgYXJjaGl2ZSB0cmFuc2ZlciB2MQppb2pqM1hRSjhaWDlVdHN0UExwZGNzcG5DYjhkbEJJYjgzU0lBYlFQYjF3",
    "CjEKODhSMTNrejVIMVZaaUQtY19iaVFvVUhxbUs4MHNpc01aMFFHMm96MllCbwpacjUtTXl4NlJUTXl2WjBLZjMyd1ZmWEY3",
    "eG9HcmFadG1MT2Z0b0VNUnpvCnJlY292ZXJ5OjEKSzdadGx4MXFYclI1TkVMT2lVUGdNaGVLVmNZaFpMN1N5NWs0dHRscG10",
    "cwo0QkltaUR6Q0gxZk5ROW0zdXpMRFM3R2E0WTNHcmp6ZHZCWldGakNLU1ZrCjE4MDAwMDAwMDAwMDAsODY6Y2EwY3ViOEk0",
    "S3p3ejJvMkVXRUFrTm1idHRUXzduZkVkREkxTFFUWnplRjNJWndOS2dCVEdBTjlvT2dNaUJrTy1ZMTVHLU92djJ2QzJ0R0ZY",
    "bWt2Q1EsMjI1OjEzMDpnd2FyIGRldmljZSB2MQppb2pqM1hRSjhaWDlVdHN0UExwZGNzcG5DYjhkbEJJYjgzU0lBYlFQYjF3",
    "ClpyNS1NeXg2UlRNeXZaMEtmMzJ3VmZYRjd4b0dyYVp0bUxPZnRvRU1Sem8KMTc5OTk5OTk5OTk5OQoxODAyNTkxOTk5OTk5",
    "LDg2OktnVENIbm4xZTIzaGNmT0RGYUtTYlYwem1xUGs5YWZsTWxmWGxGbVV0NGtoZ1JUSzJxV09SUTlQbEdiZzE3dlNLRzJD",
    "czRUZEVlWW9OT2RoU3R1eUJnLCw4MDrW6tkXLY_HuQtWjJdBHQ4CdGYapAz0r-qFIklG-tRLdkcjWPEoEfxsaULJgG7oElJV",
    "DEXNX9LtFi2e_d252Wn1LXHFax1c8oBt2HHZBjth1yw"
);

#[test]
fn golden_vectors() {
    let (a, b, c) = (device(11), device(12), device(13));
    let k = DeviceArchiveKey::sign(&account_key(), &a.signing, a.public, NOW);
    assert_eq!((k.statement().as_str(), k.signature.as_str()), (DEVICE_STATEMENT, DEVICE_SIGNATURE));
    let (secret, public_key) = recovery_key_pair(&[7; 20], &account().verifying_key().to_bytes());
    assert_eq!(b64(&secret), RECOVERY_SECRET);
    let r = RecoveryArchiveKey::sign(&account(), 1, public_key, NOW);
    assert_eq!((r.statement().as_str(), r.signature.as_str()), (RECOVERY_STATEMENT, RECOVERY_SIGNATURE));
    assert_eq!(revocation(&c.signing, NOW - 10).signature, REVOCATION_SIGNATURE);

    let manifest = fixed_bytes(MANIFEST);
    let m = Manifest::parse(&manifest, &account_key()).unwrap();
    assert_eq!(m.hash(), MANIFEST_HASH);
    assert_eq!((m.generation, m.recovery_version, m.created_at), (1, Some(1), NOW));
    assert_eq!(m.recipients, {
        let mut r = vec![recipient(&a), recipient(&b)];
        r.sort();
        r
    });
    assert_eq!(m.revocations, vec![revocation(&c.signing, NOW - 10)]);
    let generation_key = fixed::<32>(GENERATION_KEY).unwrap();
    assert_eq!(m.open(Seat::Device(&public(&a.signing)), &a.secret).unwrap(), generation_key);
    assert_eq!(m.open(Seat::Device(&public(&b.signing)), &b.secret).unwrap(), generation_key);
    assert_eq!(m.open(Seat::Recovery, &secret).unwrap(), generation_key);

    let record =
        open_record(&generation_key, &account_key(), 1, &fixed::<16>(RECORD_ID).unwrap(), &fixed_bytes(RECORD_DATA));
    assert_eq!(record.unwrap(), RecordKey { server: public(&key_of(200)), group: [3; 32], epoch: 42, key: [9; 32] });

    let t = Transfer::parse(&fixed_bytes(TRANSFER), &account_key()).unwrap();
    assert_eq!(
        (t.generation, t.manifest_hash.as_str(), &t.recipient),
        (1, MANIFEST_HASH, &TransferRecipient::Recovery { version: 1 })
    );
    assert_eq!(t.open(&secret).unwrap(), generation_key);
}
fn fixed_bytes(text: &str) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(text).unwrap()
}
