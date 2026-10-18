//! The key archive on Gwar Connect (docs/e2e-build.md, section 7): device and recovery archive
//! keys, manifests (generations), records and key transfers. This module is the cryptography and
//! the reader's checks; what a device keeps (checkpoints, revocations, queues) is the caller's.

use ed25519_dalek::{Signer as _, SigningKey};
use openmls_rust_crypto::RustCrypto;
use openmls_traits::{
    crypto::OpenMlsCrypto,
    random::OpenMlsRand,
    types::{AeadType, HpkeAeadType, HpkeCiphertext, HpkeConfig, HpkeKdfType, HpkeKemType},
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use vc_proto::DeviceCertificate;

use crate::crypto::Crypto;
use crate::encoding::{b64, decimal, fixed, netstrings, parse_netstrings};

const HPKE: HpkeConfig = HpkeConfig(HpkeKemType::DhKem25519, HpkeKdfType::HkdfSha256, HpkeAeadType::AesGcm128);
/// At most this many recipient devices per generation.
pub const MAX_RECIPIENTS: usize = 64;
/// A record's plaintext limit.
pub const MAX_RECORD_PLAINTEXT: usize = 512;
/// A manifest's size limit (64 recipients, many revocations).
pub const MAX_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_CERTIFICATE_MS: i64 = 90 * 86_400_000;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ArchiveError {
    #[error("invalid {0}")]
    Invalid(&'static str),
    #[error("cannot open")]
    Open,
}
use ArchiveError::Invalid;
type Result<T> = std::result::Result<T, ArchiveError>;

fn hash(bytes: &[u8]) -> String {
    b64(&Sha256::digest(bytes))
}
fn random<const N: usize>() -> Result<[u8; N]> {
    RustCrypto::default().random_array().map_err(|_| Invalid("randomness"))
}
fn text(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes).map_err(|_| Invalid("text"))
}
fn signature_text(bytes: &[u8]) -> Result<String> {
    let s = text(bytes)?;
    fixed::<64>(s).ok_or(Invalid("signature"))?;
    Ok(s.to_owned())
}
fn public(key: &SigningKey) -> String {
    b64(key.verifying_key().as_bytes())
}
fn seal_to(recipient: &[u8; 32], info: &[u8], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    let sealed = Crypto::default().hpke_seal(HPKE, recipient, info, aad, plaintext).map_err(|_| Invalid("seal"))?;
    let mut bytes = sealed.kem_output.as_slice().to_vec();
    bytes.extend_from_slice(sealed.ciphertext.as_slice());
    Ok(bytes)
}
fn open_from(sealed: &[u8], secret: &[u8; 32], info: &[u8], aad: &[u8]) -> Result<[u8; 32]> {
    if sealed.len() < 32 {
        return Err(ArchiveError::Open);
    }
    let ciphertext =
        HpkeCiphertext { kem_output: sealed[..32].to_vec().into(), ciphertext: sealed[32..].to_vec().into() };
    let key = Crypto::default().hpke_open(HPKE, &ciphertext, secret, info, aad).map_err(|_| ArchiveError::Open)?;
    key.as_slice().try_into().map_err(|_| ArchiveError::Open)
}

/// An X25519 key pair (private scalar, public key).
pub fn x25519_pair(secret: [u8; 32]) -> ([u8; 32], [u8; 32]) {
    let secret = x25519_dalek::StaticSecret::from(secret);
    (secret.to_bytes(), x25519_dalek::PublicKey::from(&secret).to_bytes())
}
/// A fresh device archive key pair.
pub fn device_key_pair() -> Result<([u8; 32], [u8; 32])> {
    Ok(x25519_pair(random()?))
}
/// The recovery archive key pair of a recovery code (its 20 bytes) for this account.
pub fn recovery_key_pair(code: &[u8], account_key: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
    let mut secret = [0; 32];
    hkdf::Hkdf::<Sha256>::new(Some(account_key), code)
        .expand(b"gwar archive recovery v1", &mut secret)
        .expect("32-byte output");
    x25519_pair(secret)
}

/// A device certificate's wire form: its statement and the account signature.
pub fn certificate_bytes(c: &DeviceCertificate) -> Vec<u8> {
    netstrings(&[vc_proto::device_statement(c).as_bytes(), c.signature.as_bytes()])
}
fn parse_certificate(bytes: &[u8]) -> Result<DeviceCertificate> {
    let [statement, signature] = parse_netstrings(bytes, 2).ok_or(Invalid("certificate"))?[..] else {
        return Err(Invalid("certificate"));
    };
    let [header, account_key, device_key, issued_at, expires_at] = text(statement)?.split('\n').collect::<Vec<_>>()[..]
    else {
        return Err(Invalid("certificate"));
    };
    let c = DeviceCertificate {
        account_key: account_key.into(),
        device_key: device_key.into(),
        issued_at: decimal(issued_at).ok_or(Invalid("certificate"))? as i64,
        expires_at: decimal(expires_at).ok_or(Invalid("certificate"))? as i64,
        signature: signature_text(signature)?,
    };
    if header != "gwar device v1" || vc_proto::device_statement(&c) != text(statement)? {
        return Err(Invalid("certificate"));
    }
    Ok(c)
}
/// Signed by `account_key` for `device_key`, at most 90 days, covering `at`.
fn certificate_covers(c: &DeviceCertificate, account_key: &str, device_key: &str, at: i64) -> bool {
    c.account_key == account_key
        && c.device_key == device_key
        && c.issued_at <= at
        && at < c.expires_at
        && c.expires_at - c.issued_at <= MAX_CERTIFICATE_MS
        && vc_proto::signed_by(account_key, &vc_proto::device_statement(c), &c.signature)
}

/// A signed device revocation of this account (`gwar revoke v1`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revocation {
    pub device_key: String,
    pub revoked_at: i64,
    pub signature: String,
}
impl Revocation {
    pub fn statement(&self, account_key: &str) -> String {
        format!("gwar revoke v1\n{account_key}\n{}\n{}", self.device_key, self.revoked_at)
    }
    pub fn verify(&self, account_key: &str) -> bool {
        fixed::<32>(&self.device_key).is_some()
            && self.revoked_at >= 0
            && vc_proto::signed_by(account_key, &self.statement(account_key), &self.signature)
    }
    fn encode(&self, account_key: &str) -> Vec<u8> {
        netstrings(&[self.statement(account_key).as_bytes(), self.signature.as_bytes()])
    }
    fn parse(bytes: &[u8], account_key: &str) -> Result<Self> {
        let [statement, signature] = parse_netstrings(bytes, 2).ok_or(Invalid("revocation"))?[..] else {
            return Err(Invalid("revocation"));
        };
        let [header, account, device_key, revoked_at] = text(statement)?.split('\n').collect::<Vec<_>>()[..] else {
            return Err(Invalid("revocation"));
        };
        let r = Self {
            device_key: device_key.into(),
            revoked_at: decimal(revoked_at).ok_or(Invalid("revocation"))? as i64,
            signature: signature_text(signature)?,
        };
        if header != "gwar revoke v1" || account != account_key || r.statement(account_key) != text(statement)? {
            return Err(Invalid("revocation"));
        }
        Ok(r)
    }
}

/// A device's published archive key: `gwar archive device v1`, signed by the device key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceArchiveKey {
    pub account_key: String,
    pub device_key: String,
    pub x25519: [u8; 32],
    pub created_at: i64,
    pub signature: String,
}
impl DeviceArchiveKey {
    pub fn statement(&self) -> String {
        format!(
            "gwar archive device v1\n{}\n{}\n{}\n{}",
            self.account_key,
            self.device_key,
            b64(&self.x25519),
            self.created_at
        )
    }
    pub fn sign(account_key: &str, device: &SigningKey, x25519: [u8; 32], created_at: i64) -> Self {
        let mut k = Self {
            account_key: account_key.into(),
            device_key: public(device),
            x25519,
            created_at,
            signature: String::new(),
        };
        k.signature = b64(&device.sign(k.statement().as_bytes()).to_bytes());
        k
    }
    pub fn parse(statement: &str, signature: &str) -> Option<Self> {
        let [header, account_key, device_key, x25519, created_at] = statement.split('\n').collect::<Vec<_>>()[..]
        else {
            return None;
        };
        let k = Self {
            account_key: account_key.into(),
            device_key: device_key.into(),
            x25519: fixed::<32>(x25519)?,
            created_at: decimal(created_at)? as i64,
            signature: signature.into(),
        };
        (header == "gwar archive device v1"
            && fixed::<32>(&k.account_key).is_some()
            && k.statement() == statement
            && vc_proto::signed_by(&k.device_key, statement, signature))
        .then_some(k)
    }
}

/// The recovery archive key of one recovery code version: `gwar archive recovery v1`, signed by
/// the account key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryArchiveKey {
    pub account_key: String,
    pub version: u64,
    pub x25519: [u8; 32],
    pub created_at: i64,
    pub signature: String,
}
impl RecoveryArchiveKey {
    pub fn statement(&self) -> String {
        format!(
            "gwar archive recovery v1\n{}\n{}\n{}\n{}",
            self.account_key,
            self.version,
            b64(&self.x25519),
            self.created_at
        )
    }
    pub fn sign(account: &SigningKey, version: u64, x25519: [u8; 32], created_at: i64) -> Self {
        let mut k = Self { account_key: public(account), version, x25519, created_at, signature: String::new() };
        k.signature = b64(&account.sign(k.statement().as_bytes()).to_bytes());
        k
    }
    pub fn parse(statement: &str, signature: &str) -> Option<Self> {
        let [header, account_key, version, x25519, created_at] = statement.split('\n').collect::<Vec<_>>()[..] else {
            return None;
        };
        let k = Self {
            account_key: account_key.into(),
            version: decimal(version)?,
            x25519: fixed::<32>(x25519)?,
            created_at: decimal(created_at)? as i64,
            signature: signature.into(),
        };
        (header == "gwar archive recovery v1"
            && k.version >= 1
            && k.statement() == statement
            && vc_proto::signed_by(&k.account_key, statement, signature))
        .then_some(k)
    }
}

/// A manifest recipient: a device key and its archive key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Recipient {
    pub device_key: String,
    pub x25519: [u8; 32],
}

/// Where a generation key is sealed: a recipient device, or the recovery key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seat<'a> {
    Device(&'a str),
    Recovery,
}

/// What a device knows now, for [`Manifest::safe`].
pub struct Knowledge<'a> {
    /// Every revocation of the account it knows (device key → revoked at).
    pub revocations: &'a BTreeMap<String, i64>,
    /// Current, unrevoked devices of the account with their published archive keys.
    pub current: &'a BTreeMap<String, [u8; 32]>,
    /// The current recovery key version, if the account has one.
    pub recovery_version: Option<u64>,
    /// This device.
    pub me: &'a str,
}

/// A generation of the archive, verified as a reader would ([`Manifest::parse`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub account_key: String,
    pub generation: u64,
    pub previous_hash: Option<String>,
    pub creator: String,
    pub recipients: Vec<Recipient>,
    pub recovery_version: Option<u64>,
    pub created_at: i64,
    pub certificate: DeviceCertificate,
    pub revocations: Vec<Revocation>,
    sealed: Vec<Vec<u8>>,
    bytes: Vec<u8>,
}

fn recipients_text(recipients: &[Recipient]) -> String {
    if recipients.is_empty() {
        return "-".into();
    }
    recipients.iter().map(|r| format!("{}:{}", r.device_key, b64(&r.x25519))).collect::<Vec<_>>().join(",")
}
fn parse_recipients(value: &str) -> Option<Vec<Recipient>> {
    if value == "-" {
        return Some(Vec::new());
    }
    let recipients: Vec<Recipient> = value
        .split(',')
        .map(|r| {
            let (device_key, x25519) = r.split_once(':')?;
            fixed::<32>(device_key)?;
            Some(Recipient { device_key: device_key.into(), x25519: fixed::<32>(x25519)? })
        })
        .collect::<Option<_>>()?;
    recipients.windows(2).all(|w| w[0].device_key < w[1].device_key).then_some(recipients)
}
fn dash(value: Option<String>) -> String {
    value.unwrap_or_else(|| "-".into())
}

/// The fields a manifest's statement and its sealing header share.
struct ManifestFields<'a> {
    account_key: &'a str,
    generation: u64,
    previous_hash: Option<&'a str>,
    creator: &'a str,
    recipients: &'a [Recipient],
    recovery_version: Option<u64>,
    certificate_hash: &'a str,
    created_at: i64,
}
impl ManifestFields<'_> {
    /// The statement without `body_hash`: the AAD of every sealed key.
    fn header(&self) -> String {
        self.lines(None)
    }
    fn statement(&self, body_hash: &str) -> String {
        self.lines(Some(body_hash))
    }
    fn lines(&self, body_hash: Option<&str>) -> String {
        let mut lines = vec![
            "gwar archive manifest v1".to_owned(),
            self.account_key.to_owned(),
            self.generation.to_string(),
            dash(self.previous_hash.map(str::to_owned)),
            self.creator.to_owned(),
            recipients_text(self.recipients),
            dash(self.recovery_version.map(|v| v.to_string())),
            self.certificate_hash.to_owned(),
        ];
        lines.extend(body_hash.map(str::to_owned));
        lines.push(self.created_at.to_string());
        lines.join("\n")
    }
}
fn generation_info(account_key: &str, generation: u64) -> Vec<u8> {
    format!("gwar archive generation v1\n{account_key}\n{generation}").into_bytes()
}

impl Manifest {
    /// Creates generation `generation` with a fresh key, sealed to `recipients` (this creator
    /// among them) and the recovery key; returns the manifest's bytes and the generation key.
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        generation: u64,
        previous_hash: Option<&str>,
        creator: &SigningKey,
        certificate: &DeviceCertificate,
        recipients: &[Recipient],
        recovery: Option<&RecoveryArchiveKey>,
        revocations: &[Revocation],
        created_at: i64,
    ) -> Result<(Vec<u8>, [u8; 32])> {
        let account_key = certificate.account_key.as_str();
        let creator_key = public(creator);
        let mut recipients = recipients.to_vec();
        recipients.sort();
        recipients.dedup_by(|a, b| a.device_key == b.device_key);
        let mut revocations = revocations.to_vec();
        revocations.sort_by(|a, b| a.device_key.cmp(&b.device_key));
        revocations.dedup_by(|a, b| a.device_key == b.device_key);
        if generation == 0
            || (generation == 1) != previous_hash.is_none()
            || recipients.len() > MAX_RECIPIENTS
            || !recipients.iter().any(|r| r.device_key == creator_key)
            || !certificate_covers(certificate, account_key, &creator_key, created_at)
            || recovery.is_some_and(|r| r.account_key != account_key)
        {
            return Err(Invalid("manifest"));
        }
        let key: [u8; 32] = random()?;
        let certificate = certificate_bytes(certificate);
        let certificate_hash = hash(&certificate);
        let fields = ManifestFields {
            account_key,
            generation,
            previous_hash,
            creator: &creator_key,
            recipients: &recipients,
            recovery_version: recovery.map(|r| r.version),
            certificate_hash: &certificate_hash,
            created_at,
        };
        let (header, info) = (fields.header(), generation_info(account_key, generation));
        let mut sealed = Vec::new();
        for r in &recipients {
            sealed.push(seal_to(&r.x25519, &info, header.as_bytes(), &key)?);
        }
        if let Some(r) = recovery {
            sealed.push(seal_to(&r.x25519, &info, header.as_bytes(), &key)?);
        }
        let revocations_list: Vec<Vec<u8>> = revocations.iter().map(|r| r.encode(account_key)).collect();
        let body = netstrings(&[
            &netstrings(&revocations_list.iter().map(Vec::as_slice).collect::<Vec<_>>()),
            &netstrings(&sealed.iter().map(Vec::as_slice).collect::<Vec<_>>()),
        ]);
        let statement = fields.statement(&hash(&body));
        let signature = b64(&creator.sign(statement.as_bytes()).to_bytes());
        let bytes = netstrings(&[statement.as_bytes(), signature.as_bytes(), &certificate, &body]);
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(Invalid("manifest size"));
        }
        // What a reader accepts, the creator produced.
        Self::parse(&bytes, account_key)?;
        Ok((bytes, key))
    }

    /// Parses and checks a manifest of `account_key` as a reader must: canonical, signed by its
    /// creator, the embedded certificate covering its time, the creator a recipient, the body
    /// bound by its hash, every revocation verified. The chain (`previous_hash`), the recipients'
    /// published keys and the reader's own revocations are the caller's.
    pub fn parse(bytes: &[u8], account_key: &str) -> Result<Self> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(Invalid("manifest size"));
        }
        let [statement, signature, certificate, body] = parse_netstrings(bytes, 4).ok_or(Invalid("manifest"))?[..]
        else {
            return Err(Invalid("manifest"));
        };
        let statement_text = text(statement)?;
        let [
            header,
            account,
            generation,
            previous,
            creator,
            recipients,
            recovery,
            certificate_hash,
            body_hash,
            created_at,
        ] = statement_text.split('\n').collect::<Vec<_>>()[..]
        else {
            return Err(Invalid("manifest"));
        };
        let generation = decimal(generation).ok_or(Invalid("manifest"))?;
        let previous_hash = (previous != "-").then(|| previous.to_owned());
        let recovery_version = if recovery == "-" { None } else { Some(decimal(recovery).ok_or(Invalid("manifest"))?) };
        let recipients = parse_recipients(recipients).ok_or(Invalid("manifest recipients"))?;
        let created_at = decimal(created_at).ok_or(Invalid("manifest"))? as i64;
        let fields = ManifestFields {
            account_key,
            generation,
            previous_hash: previous_hash.as_deref(),
            creator,
            recipients: &recipients,
            recovery_version,
            certificate_hash,
            created_at,
        };
        if header != "gwar archive manifest v1"
            || account != account_key
            || generation == 0
            || (generation == 1) != previous_hash.is_none()
            || previous_hash.as_deref().is_some_and(|h| fixed::<32>(h).is_none())
            || recovery_version == Some(0)
            || fields.statement(body_hash) != statement_text
        {
            return Err(Invalid("manifest"));
        }
        if !vc_proto::signed_by(creator, statement_text, &signature_text(signature)?) {
            return Err(Invalid("manifest signature"));
        }
        if hash(certificate) != certificate_hash {
            return Err(Invalid("manifest certificate"));
        }
        let certificate = parse_certificate(certificate)?;
        if !certificate_covers(&certificate, account_key, creator, created_at) {
            return Err(Invalid("manifest certificate"));
        }
        if recipients.is_empty()
            || recipients.len() > MAX_RECIPIENTS
            || !recipients.iter().any(|r| r.device_key == creator)
        {
            return Err(Invalid("manifest recipients"));
        }
        if hash(body) != body_hash {
            return Err(Invalid("manifest body"));
        }
        let [revocations, sealed] = parse_netstrings(body, 2).ok_or(Invalid("manifest body"))?[..] else {
            return Err(Invalid("manifest body"));
        };
        let revocations = parse_list(revocations)?
            .into_iter()
            .map(|r| Revocation::parse(r, account_key))
            .collect::<Result<Vec<_>>>()?;
        if revocations.windows(2).any(|w| w[0].device_key >= w[1].device_key)
            || revocations.iter().any(|r| !r.verify(account_key))
        {
            return Err(Invalid("manifest revocations"));
        }
        let sealed: Vec<Vec<u8>> = parse_list(sealed)?.into_iter().map(<[u8]>::to_vec).collect();
        if sealed.len() != recipients.len() + usize::from(recovery_version.is_some()) {
            return Err(Invalid("manifest sealed keys"));
        }
        Ok(Self {
            account_key: account_key.into(),
            generation,
            previous_hash,
            creator: creator.into(),
            recipients,
            recovery_version,
            created_at,
            certificate,
            revocations,
            sealed,
            bytes: bytes.to_vec(),
        })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    /// What the next generation's `previous_hash` names, and what checkpoints keep.
    pub fn hash(&self) -> String {
        hash(&self.bytes)
    }
    fn fields(&self) -> (String, Vec<u8>) {
        let certificate_hash = hash(&certificate_bytes(&self.certificate));
        let fields = ManifestFields {
            account_key: &self.account_key,
            generation: self.generation,
            previous_hash: self.previous_hash.as_deref(),
            creator: &self.creator,
            recipients: &self.recipients,
            recovery_version: self.recovery_version,
            certificate_hash: &certificate_hash,
            created_at: self.created_at,
        };
        (fields.header(), generation_info(&self.account_key, self.generation))
    }
    /// The generation key, opened by a recipient device's or the recovery archive key.
    pub fn open(&self, seat: Seat<'_>, secret: &[u8; 32]) -> Result<[u8; 32]> {
        let index = match seat {
            Seat::Device(device) => {
                self.recipients.iter().position(|r| r.device_key == device).ok_or(ArchiveError::Open)?
            }
            Seat::Recovery if self.recovery_version.is_some() => self.recipients.len(),
            Seat::Recovery => return Err(ArchiveError::Open),
        };
        let (header, info) = self.fields();
        open_from(&self.sealed[index], secret, &info, header.as_bytes())
    }
    /// Whether the creator was revoked, by what the reader knows, before it created this.
    pub fn creator_revoked(&self, revocations: &BTreeMap<String, i64>) -> bool {
        revocations.get(&self.creator).is_some_and(|&at| at <= self.created_at)
    }
    /// Safe for uploads by what this device knows now (docs/e2e-build.md, section 7, "Safe
    /// generation"): it carries every known revocation; its creator and every recipient are
    /// current devices with the archive keys it names; it is sealed to the current recovery
    /// version (or none without one); this device is a recipient.
    pub fn safe(&self, k: &Knowledge<'_>) -> bool {
        let carried: BTreeSet<&str> = self.revocations.iter().map(|r| r.device_key.as_str()).collect();
        k.revocations.keys().all(|d| carried.contains(d.as_str()))
            && k.current.contains_key(&self.creator)
            && self.recipients.iter().all(|r| k.current.get(&r.device_key) == Some(&r.x25519))
            && self.recovery_version == k.recovery_version
            && self.recipients.iter().any(|r| r.device_key == k.me)
    }
}

fn parse_list(bytes: &[u8]) -> Result<Vec<&[u8]>> {
    // A netstring list of any length: parse one at a time.
    let mut items = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let colon = rest.iter().position(|&b| b == b':').ok_or(Invalid("list"))?;
        let length = decimal(text(&rest[..colon])?).ok_or(Invalid("list"))? as usize;
        let end = colon.checked_add(1 + length).ok_or(Invalid("list"))?;
        if rest.get(end) != Some(&b',') {
            return Err(Invalid("list"));
        }
        items.push(&rest[colon + 1..end]);
        rest = &rest[end + 1..];
    }
    // Canonical: re-encoding gives the same bytes.
    if netstrings(&items) != bytes {
        return Err(Invalid("list"));
    }
    Ok(items)
}

/// One epoch key in the archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordKey {
    pub server: String,
    pub group: [u8; 32],
    pub epoch: u64,
    pub key: [u8; 32],
}
impl RecordKey {
    fn json(&self) -> String {
        format!(
            r#"{{"server":"{}","group":"{}","epoch":"{}","key":"{}"}}"#,
            self.server,
            b64(&self.group),
            self.epoch,
            b64(&self.key)
        )
    }
    fn parse(bytes: &[u8]) -> Result<Self> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            server: String,
            group: String,
            epoch: String,
            key: String,
        }
        let w: Wire = serde_json::from_slice(bytes).map_err(|_| Invalid("record"))?;
        let r = Self {
            server: w.server,
            group: fixed::<32>(&w.group).ok_or(Invalid("record"))?,
            epoch: decimal(&w.epoch).ok_or(Invalid("record"))?,
            key: fixed::<32>(&w.key).ok_or(Invalid("record"))?,
        };
        if fixed::<32>(&r.server).is_none() || r.json().as_bytes() != bytes {
            return Err(Invalid("record"));
        }
        Ok(r)
    }
}
fn record_aad(account_key: &str, generation: u64, record_id: &[u8; 16]) -> String {
    format!("gwar archive record v1\n{account_key}\n{generation}\n{}", b64(record_id))
}
/// A new record: a random id and `nonce ‖ AES-256-GCM(generation key, …)`.
pub fn seal_record(
    generation_key: &[u8; 32],
    account_key: &str,
    generation: u64,
    record: &RecordKey,
) -> Result<([u8; 16], Vec<u8>)> {
    let plaintext = record.json();
    if plaintext.len() > MAX_RECORD_PLAINTEXT || fixed::<32>(&record.server).is_none() {
        return Err(Invalid("record"));
    }
    let (id, nonce): ([u8; 16], [u8; 12]) = (random()?, random()?);
    let sealed = RustCrypto::default()
        .aead_encrypt(
            AeadType::Aes256Gcm,
            generation_key,
            plaintext.as_bytes(),
            &nonce,
            record_aad(account_key, generation, &id).as_bytes(),
        )
        .map_err(|_| Invalid("record"))?;
    let mut data = nonce.to_vec();
    data.extend(sealed);
    Ok((id, data))
}
/// A record's key, bound to its account, generation and id.
pub fn open_record(
    generation_key: &[u8; 32],
    account_key: &str,
    generation: u64,
    record_id: &[u8; 16],
    data: &[u8],
) -> Result<RecordKey> {
    if data.len() < 28 {
        return Err(ArchiveError::Open);
    }
    let plaintext = RustCrypto::default()
        .aead_decrypt(
            AeadType::Aes256Gcm,
            generation_key,
            &data[12..],
            &data[..12],
            record_aad(account_key, generation, record_id).as_bytes(),
        )
        .map_err(|_| ArchiveError::Open)?;
    RecordKey::parse(&plaintext)
}

/// Who a transfer is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferRecipient {
    Device { device_key: String, x25519: [u8; 32] },
    Recovery { version: u64 },
}
impl TransferRecipient {
    fn text(&self) -> String {
        match self {
            Self::Device { device_key, x25519 } => format!("device:{device_key}:{}", b64(x25519)),
            Self::Recovery { version } => format!("recovery:{version}"),
        }
    }
    fn parse(value: &str) -> Option<Self> {
        if let Some(rest) = value.strip_prefix("device:") {
            let (device_key, x25519) = rest.split_once(':')?;
            fixed::<32>(device_key)?;
            return Some(Self::Device { device_key: device_key.into(), x25519: fixed::<32>(x25519)? });
        }
        let version = decimal(value.strip_prefix("recovery:")?)?;
        (version >= 1).then_some(Self::Recovery { version })
    }
}

/// A key transfer of one generation key to one recipient, verified as a reader would.
#[derive(Debug, Clone, PartialEq)]
pub struct Transfer {
    pub account_key: String,
    pub generation: u64,
    pub manifest_hash: String,
    pub sender: String,
    pub recipient: TransferRecipient,
    pub created_at: i64,
    pub certificate: DeviceCertificate,
    sealed: Vec<u8>,
}
#[allow(clippy::too_many_arguments)]
fn transfer_lines(
    account_key: &str,
    generation: u64,
    manifest_hash: &str,
    sender: &str,
    recipient: &TransferRecipient,
    certificate_hash: &str,
    sealed_hash: Option<&str>,
    created_at: i64,
) -> String {
    let mut lines = vec![
        "gwar archive transfer v1".to_owned(),
        account_key.to_owned(),
        generation.to_string(),
        manifest_hash.to_owned(),
        sender.to_owned(),
        recipient.text(),
        certificate_hash.to_owned(),
    ];
    lines.extend(sealed_hash.map(str::to_owned));
    lines.push(created_at.to_string());
    lines.join("\n")
}
fn transfer_info(account_key: &str, generation: u64) -> Vec<u8> {
    format!("gwar archive transfer v1\n{account_key}\n{generation}").into_bytes()
}
impl Transfer {
    /// Seals `generation_key` of `manifest` to `recipient` (its archive key `x25519`), signed by
    /// a sender device with its current certificate.
    pub fn create(
        manifest: &Manifest,
        generation_key: &[u8; 32],
        sender: &SigningKey,
        certificate: &DeviceCertificate,
        recipient: TransferRecipient,
        x25519: &[u8; 32],
        created_at: i64,
    ) -> Result<Vec<u8>> {
        let sender_key = public(sender);
        let account_key = manifest.account_key.as_str();
        if !certificate_covers(certificate, account_key, &sender_key, created_at)
            || matches!(&recipient, TransferRecipient::Device { x25519: k, .. } if k != x25519)
        {
            return Err(Invalid("transfer"));
        }
        let certificate = certificate_bytes(certificate);
        let certificate_hash = hash(&certificate);
        let manifest_hash = manifest.hash();
        let header = transfer_lines(
            account_key,
            manifest.generation,
            &manifest_hash,
            &sender_key,
            &recipient,
            &certificate_hash,
            None,
            created_at,
        );
        let sealed =
            seal_to(x25519, &transfer_info(account_key, manifest.generation), header.as_bytes(), generation_key)?;
        let statement = transfer_lines(
            account_key,
            manifest.generation,
            &manifest_hash,
            &sender_key,
            &recipient,
            &certificate_hash,
            Some(&hash(&sealed)),
            created_at,
        );
        let signature = b64(&sender.sign(statement.as_bytes()).to_bytes());
        let bytes = netstrings(&[statement.as_bytes(), signature.as_bytes(), &certificate, &sealed]);
        Self::parse(&bytes, account_key)?;
        Ok(bytes)
    }

    /// Parses and checks a transfer of `account_key`: canonical, signed by its sender, the
    /// embedded certificate covering its time, the sealed key bound by its hash. Whether the
    /// manifest is one the reader accepted, and the sender's revocation, are the caller's.
    pub fn parse(bytes: &[u8], account_key: &str) -> Result<Self> {
        let [statement, signature, certificate, sealed] = parse_netstrings(bytes, 4).ok_or(Invalid("transfer"))?[..]
        else {
            return Err(Invalid("transfer"));
        };
        let statement_text = text(statement)?;
        let [header, account, generation, manifest_hash, sender, recipient, certificate_hash, sealed_hash, created_at] =
            statement_text.split('\n').collect::<Vec<_>>()[..]
        else {
            return Err(Invalid("transfer"));
        };
        let generation = decimal(generation).ok_or(Invalid("transfer"))?;
        let recipient = TransferRecipient::parse(recipient).ok_or(Invalid("transfer recipient"))?;
        let created_at = decimal(created_at).ok_or(Invalid("transfer"))? as i64;
        if header != "gwar archive transfer v1"
            || account != account_key
            || generation == 0
            || fixed::<32>(manifest_hash).is_none()
            || transfer_lines(
                account_key,
                generation,
                manifest_hash,
                sender,
                &recipient,
                certificate_hash,
                Some(sealed_hash),
                created_at,
            ) != statement_text
        {
            return Err(Invalid("transfer"));
        }
        if !vc_proto::signed_by(sender, statement_text, &signature_text(signature)?) {
            return Err(Invalid("transfer signature"));
        }
        if hash(certificate) != certificate_hash || hash(sealed) != sealed_hash {
            return Err(Invalid("transfer binding"));
        }
        let certificate = parse_certificate(certificate)?;
        if !certificate_covers(&certificate, account_key, sender, created_at) {
            return Err(Invalid("transfer certificate"));
        }
        Ok(Self {
            account_key: account_key.into(),
            generation,
            manifest_hash: manifest_hash.into(),
            sender: sender.into(),
            recipient,
            created_at,
            certificate,
            sealed: sealed.to_vec(),
        })
    }
    /// The generation key, opened with the recipient's archive key.
    pub fn open(&self, secret: &[u8; 32]) -> Result<[u8; 32]> {
        let certificate_hash = hash(&certificate_bytes(&self.certificate));
        let header = transfer_lines(
            &self.account_key,
            self.generation,
            &self.manifest_hash,
            &self.sender,
            &self.recipient,
            &certificate_hash,
            None,
            self.created_at,
        );
        open_from(&self.sealed, secret, &transfer_info(&self.account_key, self.generation), header.as_bytes())
    }
}

#[cfg(test)]
mod tests;
