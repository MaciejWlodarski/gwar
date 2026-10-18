//! The device side of the key archive (docs/e2e-build.md, section 7, "Client"): checkpoints of
//! the account's manifests, the revocations, devices and recovery key it knows, the generation
//! keys it opened, records to upload and import, and transfers. Connect is reached by the caller.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use vc_proto::DeviceCertificate;

use super::{Agent, AgentError, EpochRecord, Invalid, Result, Status, epoch_key, group_key};
use crate::archive::{
    self, DeviceArchiveKey, Knowledge, Manifest, Recipient, RecordKey, RecoveryArchiveKey, Revocation, Seat, Transfer,
    TransferRecipient,
};
use crate::encoding::{b64, fixed};

pub(super) const KEY: &[u8] = b"@archive";

/// What this device keeps of its account's archive, stored as `@archive`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveRecord {
    pub account_key: String,
    /// `(generation, manifest hash)` of every generation it accepted (1 to the newest).
    pub checkpoints: BTreeMap<u64, String>,
    /// The accepted manifests, for the safe-generation rule and transfers.
    manifests: BTreeMap<u64, Vec<u8>>,
    /// Generation keys it opened.
    keys: BTreeMap<u64, [u8; 32]>,
    /// Every revocation of the account it learned (from Connect's feed and manifests).
    revocations: BTreeMap<String, (i64, String)>,
    /// The account's devices: the first archive key statement it verified per device key (a
    /// later, different one is ignored) and the certificate Connect showed at the last sync.
    devices: BTreeMap<String, KnownDevice>,
    /// The newest recovery archive key it verified (statement, signature).
    recovery: Option<(String, String)>,
    /// Keys of groups this device holds no record of yet, by epoch key.
    held: BTreeMap<String, Held>,
    /// Transfers sent or seen on Connect, as `generation/recipient`.
    sent: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KnownDevice {
    statement: String,
    signature: String,
    x25519: [u8; 32],
    certificate: Option<ArchiveCertificate>,
}

/// An archive key kept for a group this device holds no record of yet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Held {
    Key([u8; 32]),
    /// The archive held different keys for the epoch.
    Conflicted,
}

/// A device entry of `GET /v1/archive/devices`.
#[derive(Clone, Debug, Deserialize)]
pub struct ArchiveDevice {
    pub statement: String,
    pub signature: String,
    /// Its current certificate (`issued_at`, `expires_at`, `signature`), if Connect has one.
    pub certificate: Option<ArchiveCertificate>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveCertificate {
    pub issued_at: i64,
    pub expires_at: i64,
    pub signature: String,
}
/// A revocation of `GET /v1/revocations` for this account.
#[derive(Clone, Debug, Deserialize)]
pub struct ArchiveRevocation {
    pub device_key: String,
    pub revoked_at: i64,
    pub signature: String,
}

/// The result of [`Agent::archive_sync`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ArchiveState {
    /// The newest generation it accepted.
    pub newest: Option<u64>,
    /// The newest generation safe for uploads, if any (otherwise it creates one).
    pub safe: Option<u64>,
    /// Generations it can open.
    pub opened: Vec<u64>,
    /// Recipients this device owes transfers of generations it opened: current devices of the
    /// account (device keys) and the current recovery key (`recovery:{version}`).
    pub waiting: Vec<String>,
    /// The current recovery version, if any.
    pub recovery_version: Option<u64>,
}

/// Records sealed for upload, and the epochs they hold (marked once Connect stores them).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ArchiveUpload {
    pub generation: u64,
    pub records: Vec<(String, String)>,
    pub epochs: Vec<String>,
}

fn certificate_current(account_key: &str, device_key: &str, c: &ArchiveCertificate, now: i64) -> bool {
    let certificate = DeviceCertificate {
        account_key: account_key.into(),
        device_key: device_key.into(),
        issued_at: c.issued_at,
        expires_at: c.expires_at,
        signature: c.signature.clone(),
    };
    // Unsigned until the signature is checked: bounded before any arithmetic.
    let valid = 1..=vc_proto::MAX_SAFE_INTEGER;
    valid.contains(&c.issued_at)
        && valid.contains(&c.expires_at)
        && c.issued_at <= now
        && now < c.expires_at
        && c.expires_at.checked_sub(c.issued_at).is_some_and(|d| d <= 90 * 86_400_000)
        && vc_proto::signed_by(account_key, &vc_proto::device_statement(&certificate), &c.signature)
}

impl ArchiveRecord {
    fn empty(account_key: String) -> Self {
        Self {
            account_key,
            checkpoints: BTreeMap::new(),
            manifests: BTreeMap::new(),
            keys: BTreeMap::new(),
            revocations: BTreeMap::new(),
            devices: BTreeMap::new(),
            recovery: None,
            held: BTreeMap::new(),
            sent: BTreeSet::new(),
        }
    }
    fn revoked(&self) -> BTreeMap<String, i64> {
        self.revocations.iter().map(|(d, (at, _))| (d.clone(), *at)).collect()
    }
    fn recovery_key(&self) -> Option<RecoveryArchiveKey> {
        self.recovery.as_ref().and_then(|(s, sig)| RecoveryArchiveKey::parse(s, sig))
    }
    fn learn(&mut self, r: &Revocation) {
        self.revocations.entry(r.device_key.clone()).or_insert((r.revoked_at, r.signature.clone()));
    }
    fn newest(&self) -> Option<u64> {
        self.checkpoints.keys().next_back().copied()
    }
    fn manifest(&self, generation: u64) -> Result<Manifest> {
        let bytes = self.manifests.get(&generation).ok_or(Invalid("stored manifest"))?;
        Manifest::parse(bytes, &self.account_key).map_err(|_| Invalid("stored manifest"))
    }
    /// Devices usable as recipients now: a certificate current at `now`, no known revocation.
    fn current(&self, now: i64) -> BTreeMap<String, [u8; 32]> {
        self.devices
            .iter()
            .filter(|(d, k)| {
                !self.revocations.contains_key(*d)
                    && k.certificate.as_ref().is_some_and(|c| certificate_current(&self.account_key, d, c, now))
            })
            .map(|(d, k)| (d.clone(), k.x25519))
            .collect()
    }
    /// The newest generation safe for uploads by what this device knows at `now`.
    fn safe(&self, me: &str, now: i64) -> Result<Option<u64>> {
        let revocations = self.revoked();
        let current = self.current(now);
        let k = Knowledge {
            revocations: &revocations,
            current: &current,
            recovery_version: self.recovery_key().map(|r| r.version),
            me,
        };
        for generation in self.manifests.keys().rev() {
            if self.keys.contains_key(generation) && self.manifest(*generation)?.safe(&k) {
                return Ok(Some(*generation));
            }
        }
        Ok(None)
    }
    /// Recipients owed transfers by this device at `now`, with the generations each lacks.
    fn owed(&self, me: &str, now: i64) -> Result<BTreeMap<String, Vec<u64>>> {
        let current = self.current(now);
        let mut owed = BTreeMap::new();
        if !current.contains_key(me) {
            return Ok(owed);
        }
        let recovery = self.recovery_key().map(|k| k.version);
        for generation in self.keys.keys() {
            let m = self.manifest(*generation)?;
            let mut lacking: Vec<String> = current
                .keys()
                .filter(|d| d.as_str() != me && !m.recipients.iter().any(|r| r.device_key == **d))
                .cloned()
                .collect();
            if let Some(version) = recovery.filter(|v| m.recovery_version != Some(*v)) {
                lacking.push(format!("recovery:{version}"));
            }
            for recipient in lacking {
                if !self.sent.contains(&format!("{generation}/{recipient}")) {
                    owed.entry(recipient).or_insert_with(Vec::new).push(*generation);
                }
            }
        }
        Ok(owed)
    }
    /// Checks a stored record: this account; a chain of manifests from generation 1 matching
    /// the checkpoints; keys, transfers and held keys referring to what it holds; every stored
    /// statement verifying.
    fn validate(&self, account_key: &str) -> Result<()> {
        let bad = || Invalid("stored archive record");
        if self.account_key != account_key
            || !self.checkpoints.keys().copied().eq(1..=self.checkpoints.len() as u64)
            || !self.manifests.keys().eq(self.checkpoints.keys())
            || !self.keys.keys().all(|g| self.manifests.contains_key(g))
        {
            return Err(bad());
        }
        for (generation, hash) in &self.checkpoints {
            let m = self.manifest(*generation)?;
            let previous = generation.checked_sub(1).and_then(|g| self.checkpoints.get(&g));
            if m.generation != *generation || m.hash() != *hash || m.previous_hash.as_ref() != previous {
                return Err(bad());
            }
        }
        for (device_key, (revoked_at, signature)) in &self.revocations {
            let r =
                Revocation { device_key: device_key.clone(), revoked_at: *revoked_at, signature: signature.clone() };
            if !r.verify(account_key) {
                return Err(bad());
            }
        }
        for (device_key, d) in &self.devices {
            let k = DeviceArchiveKey::parse(&d.statement, &d.signature).ok_or_else(bad)?;
            if k.account_key != account_key || k.device_key != *device_key || k.x25519 != d.x25519 {
                return Err(bad());
            }
        }
        if self.recovery.is_some() && self.recovery_key().is_none_or(|k| k.account_key != account_key) {
            return Err(bad());
        }
        let sent = |tag: &String| {
            tag.split_once('/')
                .and_then(|(g, _)| g.parse::<u64>().ok())
                .is_some_and(|g| self.manifests.contains_key(&g))
        };
        if !self.sent.iter().all(sent) || !self.held.keys().all(|k| k.rsplit_once('/').is_some()) {
            return Err(bad());
        }
        Ok(())
    }
}

impl Agent {
    fn account_certificate(&self) -> Result<&DeviceCertificate> {
        match &self.credential {
            crate::Credential::Account(c) => Ok(c),
            crate::Credential::Local { .. } => Err(Invalid("local identities have no archive")),
        }
    }

    /// This device's archive key statement (its history HPKE key serves as its archive key),
    /// signed by its device key, for `PUT /v1/archive/device`.
    pub fn archive_device_key(&self, now: i64) -> Result<(String, String)> {
        let c = self.account_certificate()?;
        let k = DeviceArchiveKey::sign(&c.account_key, &self.signer.0, self.device.hpke_public, now);
        Ok((k.statement(), k.signature))
    }

    /// Takes in what Connect shows of the archive: the account's devices (archive keys and
    /// current certificates), recovery keys, revocations, manifests (ascending, from the
    /// generation after this device's newest checkpoint, or from 1) and transfers. Manifests are
    /// checked as a reader must (signature, certificate, body, chain, recipients' archive keys,
    /// the creator against the revocations it knows and those the manifest carries); a different
    /// manifest for a generation it saw is refused. It then opens every accepted generation it
    /// can: as a recipient, by a transfer, or with `recovery_secret` (the recovery archive key
    /// derived from the recovery code, [`archive::recovery_key_pair`], for account recovery) the
    /// generations sealed or transferred to the current recovery version.
    #[allow(clippy::too_many_arguments)]
    pub fn archive_sync(
        &mut self,
        devices: &[ArchiveDevice],
        recovery: &[(String, String)],
        revocations: &[ArchiveRevocation],
        manifests: &[Vec<u8>],
        transfers: &[Vec<u8>],
        recovery_secret: Option<[u8; 32]>,
        now: i64,
    ) -> Result<ArchiveState> {
        self.operation(|a| {
            super::current_time(now)?;
            let account_key = a.account_certificate()?.account_key.clone();
            let me = a.credential.device_key().to_owned();
            let mut record = a.archive.clone().unwrap_or_else(|| ArchiveRecord::empty(account_key.clone()));
            if record.account_key != account_key {
                return Err(Invalid("another account's archive"));
            }
            for r in revocations {
                let r = Revocation {
                    device_key: r.device_key.clone(),
                    revoked_at: r.revoked_at,
                    signature: r.signature.clone(),
                };
                if r.verify(&account_key) {
                    record.learn(&r);
                }
            }
            // Recovery keys: versions rise by one; one key per version.
            let mut newest_recovery: Option<RecoveryArchiveKey> = record.recovery_key();
            for (statement, signature) in recovery {
                let Some(k) = RecoveryArchiveKey::parse(statement, signature).filter(|k| k.account_key == account_key)
                else {
                    return Err(Invalid("recovery archive key"));
                };
                match &newest_recovery {
                    Some(old) if old.version == k.version && old.x25519 != k.x25519 => {
                        return Err(Invalid("two recovery keys for one version"));
                    }
                    Some(old) if old.version >= k.version => {}
                    _ => newest_recovery = Some(k),
                }
            }
            record.recovery = newest_recovery.as_ref().map(|k| (k.statement(), k.signature.clone()));
            // Devices: the first archive key statement per device key stays; certificates are
            // what Connect shows now (none for a device it no longer lists).
            for d in record.devices.values_mut() {
                d.certificate = None;
            }
            for d in devices {
                let Some(k) =
                    DeviceArchiveKey::parse(&d.statement, &d.signature).filter(|k| k.account_key == account_key)
                else {
                    continue;
                };
                let known = record.devices.entry(k.device_key.clone()).or_insert_with(|| KnownDevice {
                    statement: d.statement.clone(),
                    signature: d.signature.clone(),
                    x25519: k.x25519,
                    certificate: None,
                });
                if known.x25519 == k.x25519 {
                    known.certificate = d.certificate.clone();
                }
            }
            // Manifests, in order.
            for bytes in manifests {
                let m = Manifest::parse(bytes, &account_key).map_err(|_| Invalid("manifest"))?;
                let hash = m.hash();
                match record.checkpoints.get(&m.generation) {
                    Some(seen) if *seen != hash => return Err(Invalid("a different manifest for a generation it saw")),
                    Some(_) => continue,
                    None => {}
                }
                let chained = match (m.generation, &m.previous_hash) {
                    (1, None) => true,
                    (g, Some(previous)) => record.checkpoints.get(&(g - 1)) == Some(previous),
                    _ => false,
                };
                if !chained {
                    return Err(Invalid("manifest chain"));
                }
                let carried: BTreeMap<String, i64> =
                    m.revocations.iter().map(|r| (r.device_key.clone(), r.revoked_at)).collect();
                if m.creator_revoked(&record.revoked()) || m.creator_revoked(&carried) {
                    return Err(Invalid("a manifest by a revoked device"));
                }
                if m.recipients.iter().any(|r| record.devices.get(&r.device_key).map(|d| d.x25519) != Some(r.x25519)) {
                    return Err(Invalid("a manifest recipient's archive key"));
                }
                for r in &m.revocations {
                    record.learn(r);
                }
                record.checkpoints.insert(m.generation, hash);
                record.manifests.insert(m.generation, bytes.clone());
            }
            // Opening: every accepted generation it has no key for (a recovery code given later
            // opens generations accepted before).
            let unopened: Vec<u64> =
                record.manifests.keys().filter(|g| !record.keys.contains_key(g)).copied().collect();
            for generation in unopened {
                let m = record.manifest(generation)?;
                let opened = m
                    .open(Seat::Device(&me), &a.device.hpke_private)
                    .ok()
                    .or_else(|| recovery_secret.as_ref().and_then(|s| m.open(Seat::Recovery, s).ok()));
                if let Some(key) = opened {
                    record.keys.insert(generation, key);
                }
            }
            // Transfers: those to this device (or, recovering, to the recovery key's version)
            // open generations; every valid one counts as sent, whoever sent it.
            for bytes in transfers {
                let Ok(t) = Transfer::parse(bytes, &account_key) else { continue };
                if record.checkpoints.get(&t.generation) != Some(&t.manifest_hash)
                    || record.revoked().get(&t.sender).is_some_and(|&at| at <= t.created_at)
                {
                    continue;
                }
                let to = match &t.recipient {
                    TransferRecipient::Device { device_key, x25519 } => {
                        if record.devices.get(device_key).map(|d| d.x25519) != Some(*x25519) {
                            continue;
                        }
                        device_key.clone()
                    }
                    TransferRecipient::Recovery { version } => format!("recovery:{version}"),
                };
                record.sent.insert(format!("{}/{to}", t.generation));
                if record.keys.contains_key(&t.generation) {
                    continue;
                }
                let secret = match &t.recipient {
                    TransferRecipient::Device { device_key, x25519 }
                        if *device_key == me && *x25519 == a.device.hpke_public =>
                    {
                        Some(a.device.hpke_private)
                    }
                    TransferRecipient::Recovery { version }
                        if newest_recovery.as_ref().is_some_and(|k| k.version == *version) =>
                    {
                        recovery_secret
                    }
                    _ => None,
                };
                if let Some(key) = secret.and_then(|s| t.open(&s).ok()) {
                    record.keys.insert(t.generation, key);
                }
            }
            let state = ArchiveState {
                newest: record.newest(),
                safe: record.safe(&me, now)?,
                opened: record.keys.keys().copied().collect(),
                waiting: record.owed(&me, now)?.into_keys().collect(),
                recovery_version: record.recovery_key().map(|k| k.version),
            };
            a.archive = Some(record);
            Ok(state)
        })
    }

    /// The next generation's manifest, sealed to every device of the account current at `now`
    /// with an archive key and to the current recovery key, carrying every revocation this
    /// device knows. Posted to Connect, then taken in by the next [`Agent::archive_sync`].
    pub fn archive_create_manifest(&self, now: i64) -> Result<Vec<u8>> {
        let record = self.archive.as_ref().ok_or(Invalid("archive not synced"))?;
        let certificate = self.account_certificate()?;
        let current = record.current(now);
        if !current.contains_key(self.credential.device_key()) {
            return Err(Invalid("this device is not a current archive device"));
        }
        let generation = record.newest().map_or(1, |g| g + 1);
        let previous = record.newest().and_then(|g| record.checkpoints.get(&g)).cloned();
        let recipients: Vec<Recipient> =
            current.iter().map(|(d, x)| Recipient { device_key: d.clone(), x25519: *x }).collect();
        let revocations: Vec<Revocation> = record
            .revocations
            .iter()
            .map(|(d, (at, sig))| Revocation { device_key: d.clone(), revoked_at: *at, signature: sig.clone() })
            .collect();
        let recovery = record.recovery_key();
        let (bytes, _) = Manifest::create(
            generation,
            previous.as_deref(),
            &self.signer.0,
            certificate,
            &recipients,
            recovery.as_ref(),
            &revocations,
            now,
        )
        .map_err(|_| Invalid("manifest"))?;
        Ok(bytes)
    }

    /// Up to `limit` epoch keys not yet in the archive (derived ones and those imported from
    /// history bundles; never `Conflicted` ones or those from the archive), sealed under the
    /// newest generation safe at `now`; `None` without one (create one first).
    pub fn archive_seal(&self, limit: usize, now: i64) -> Result<Option<ArchiveUpload>> {
        let record = self.archive.as_ref().ok_or(Invalid("archive not synced"))?;
        let Some(generation) = record.safe(self.credential.device_key(), now)? else { return Ok(None) };
        let key = record.keys.get(&generation).ok_or(Invalid("generation key"))?;
        let mut upload = ArchiveUpload { generation, records: Vec::new(), epochs: Vec::new() };
        for (epoch_key_text, value) in &self.epochs {
            if upload.records.len() >= limit {
                break;
            }
            let (epoch_value, archived) = match value {
                EpochRecord::Derived { key, archived } | EpochRecord::Imported { key, archived, .. } => {
                    (key, *archived)
                }
                EpochRecord::Conflicted => continue,
            };
            if archived {
                continue;
            }
            let Some((group_text, epoch)) = epoch_key_text.rsplit_once('/') else { continue };
            let Some((server, group)) = group_text.rsplit_once('/') else { continue };
            let (Some(group), Ok(epoch)) = (fixed::<32>(group), epoch.parse::<u64>()) else { continue };
            let entry = RecordKey { server: server.into(), group, epoch, key: *epoch_value };
            let (id, data) =
                archive::seal_record(key, &record.account_key, generation, &entry).map_err(|_| Invalid("record"))?;
            upload.records.push((b64(&id), b64(&data)));
            upload.epochs.push(epoch_key_text.clone());
        }
        Ok(Some(upload))
    }

    /// Marks epochs as in the archive once Connect stored their records.
    pub fn archive_uploaded(&mut self, epochs: &[String]) -> Result<()> {
        self.operation(|a| {
            for e in epochs {
                if let Some(EpochRecord::Derived { archived, .. } | EpochRecord::Imported { archived, .. }) =
                    a.epochs.get_mut(e)
                {
                    *archived = true;
                }
            }
            Ok(())
        })
    }

    /// Imports records of generations this device can open: an `Imported` epoch key attributed to
    /// this account, never overwriting (a different key makes the epoch `Conflicted`); a key of a
    /// group it holds no record of is kept until it does (with the conflict, if the archive holds
    /// different keys for the epoch). Returns how many records it opened.
    pub fn archive_import(&mut self, records: &[(u64, String, String)]) -> Result<usize> {
        self.operation(|a| {
            let record = a.archive.as_mut().ok_or(Invalid("archive not synced"))?;
            let mut imported = 0;
            for (generation, id, data) in records {
                use base64::Engine as _;
                let data = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(data).ok();
                let (Some(key), Some(id), Some(data)) = (record.keys.get(generation), fixed::<16>(id), data) else {
                    continue;
                };
                let Ok(entry) = archive::open_record(key, &record.account_key, *generation, &id, &data) else {
                    continue;
                };
                let k = epoch_key(&group_key(&entry.server, &entry.group), entry.epoch);
                match record.held.get(&k) {
                    None => {
                        record.held.insert(k, Held::Key(entry.key));
                    }
                    Some(Held::Key(existing)) if *existing != entry.key => {
                        record.held.insert(k, Held::Conflicted);
                    }
                    Some(_) => {}
                }
                imported += 1;
            }
            Ok(imported)
        })
    }

    /// Moves held archive keys into the epochs of groups this device now holds (settled: active,
    /// removed, waiting to rejoin or superseded); run at the end of every operation, so a group it
    /// adopts (a join, a rejoin, a restart chain) gets them in the same change.
    pub(super) fn settle_held(&mut self) {
        let Some(record) = self.archive.as_mut() else { return };
        let uid = self.credential.uid();
        let ready: Vec<String> = record
            .held
            .keys()
            .filter(|k| {
                // Groups holding epoch keys: not an unsettled creation, a join in flight or a tombstone.
                k.rsplit_once('/').and_then(|(group, _)| self.groups.get(group)).is_some_and(|r| {
                    matches!(r.status, Status::Active | Status::Removed | Status::Rejoin | Status::Superseded)
                })
            })
            .cloned()
            .collect();
        for k in ready {
            let held = record.held.remove(&k).expect("listed");
            let Held::Key(value) = held else {
                self.epochs.insert(k, EpochRecord::Conflicted);
                continue;
            };
            let entry = self.epochs.entry(k).or_insert_with(|| EpochRecord::Imported {
                key: value,
                sender_uids: [uid.clone()].into(),
                archived: true,
            });
            if entry.key().is_some_and(|existing| *existing != value) {
                *entry = EpochRecord::Conflicted;
            }
        }
    }

    /// Transfers of the generation keys this device holds that `recipient` (a device of the
    /// account current at `now`, or `recovery:{version}` for the current recovery key) lacks and
    /// was not sent yet, by generation. This device must be current itself. Posted to Connect;
    /// then [`Agent::archive_transfers_sent`].
    pub fn archive_transfers_for(&self, recipient: &str, now: i64) -> Result<Vec<(String, Vec<u8>)>> {
        let record = self.archive.as_ref().ok_or(Invalid("archive not synced"))?;
        let certificate = self.account_certificate()?;
        let current = record.current(now);
        if !current.contains_key(self.credential.device_key()) {
            return Err(Invalid("this device is not a current archive device"));
        }
        let (to, x25519) = if let Some(version) = recipient.strip_prefix("recovery:") {
            let k = record
                .recovery_key()
                .filter(|k| k.version.to_string() == version)
                .ok_or(Invalid("recovery version"))?;
            (TransferRecipient::Recovery { version: k.version }, k.x25519)
        } else {
            let x = *current.get(recipient).ok_or(Invalid("not a current device"))?;
            (TransferRecipient::Device { device_key: recipient.into(), x25519: x }, x)
        };
        let owed = record.owed(self.credential.device_key(), now)?;
        let mut out = Vec::new();
        for generation in owed.get(recipient).into_iter().flatten() {
            let m = record.manifest(*generation)?;
            let key = record.keys.get(generation).ok_or(Invalid("generation key"))?;
            let t = Transfer::create(&m, key, &self.signer.0, certificate, to.clone(), &x25519, now)
                .map_err(|_| Invalid("transfer"))?;
            out.push((format!("{generation}/{recipient}"), t));
        }
        Ok(out)
    }

    pub fn archive_transfers_sent(&mut self, tags: &[String]) -> Result<()> {
        self.operation(|a| {
            let record = a.archive.as_mut().ok_or(Invalid("archive not synced"))?;
            for tag in tags {
                let generation = tag.split_once('/').and_then(|(g, _)| g.parse::<u64>().ok());
                if !generation.is_some_and(|g| record.manifests.contains_key(&g)) {
                    return Err(Invalid("transfer tag"));
                }
                record.sent.insert(tag.clone());
            }
            Ok(())
        })
    }
}

/// A stored archive record of `account_key`'s device, checked.
pub(super) fn parse_record(bytes: &[u8], account_key: Option<&str>) -> Result<ArchiveRecord> {
    let record: ArchiveRecord =
        serde_json::from_slice(bytes).map_err(|_| AgentError::Invalid("stored archive record"))?;
    record.validate(account_key.ok_or(Invalid("local identities have no archive"))?)?;
    Ok(record)
}

#[cfg(test)]
mod tests;
