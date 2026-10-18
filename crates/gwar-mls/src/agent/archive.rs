//! The device side of the key archive (docs/e2e-build.md, section 7, "Client"): checkpoints of
//! the account's manifests, the revocations, devices and recovery key it knows, the generation
//! keys it opened, records to upload and import, and transfers. Connect is reached by the caller.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use vc_proto::DeviceCertificate;

use super::{Agent, AgentError, EpochRecord, Invalid, Result, epoch_key, group_key};
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
    /// `(generation, manifest hash)` of every generation it accepted.
    pub checkpoints: BTreeMap<u64, String>,
    /// The accepted manifests, for the safe-generation rule and transfers.
    manifests: BTreeMap<u64, Vec<u8>>,
    /// Generation keys it opened.
    keys: BTreeMap<u64, [u8; 32]>,
    /// Every revocation of the account it learned (from Connect's feed and manifests).
    revocations: BTreeMap<String, (i64, String)>,
    /// Current devices of the account with their archive keys, as of the last sync.
    current: BTreeMap<String, [u8; 32]>,
    /// The newest recovery archive key it verified (statement, signature).
    recovery: Option<(String, String)>,
    /// Keys of groups this device holds no record of yet: epoch key → key.
    held: BTreeMap<String, [u8; 32]>,
    /// Transfers sent, as `generation/recipient`.
    sent: BTreeSet<String>,
}

/// A device entry of `GET /v1/archive/devices`.
#[derive(Clone, Debug, Deserialize)]
pub struct ArchiveDevice {
    pub statement: String,
    pub signature: String,
    /// Its current certificate (`issued_at`, `expires_at`, `signature`), if Connect has one.
    pub certificate: Option<ArchiveCertificate>,
}
#[derive(Clone, Debug, Deserialize)]
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
    /// Current devices without any generation they can read yet (they need transfers).
    pub waiting_devices: Vec<String>,
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
    c.issued_at <= now
        && now < c.expires_at
        && c.expires_at - c.issued_at <= 90 * 86_400_000
        && vc_proto::signed_by(account_key, &vc_proto::device_statement(&certificate), &c.signature)
}

impl ArchiveRecord {
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
    /// The newest generation safe for uploads by what this device knows now.
    fn safe(&self, me: &str) -> Result<Option<u64>> {
        let revocations = self.revoked();
        let k = Knowledge {
            revocations: &revocations,
            current: &self.current,
            recovery_version: self.recovery_key().map(|r| r.version),
            me,
        };
        for (generation, bytes) in self.manifests.iter().rev() {
            let m = Manifest::parse(bytes, &self.account_key).map_err(|_| Invalid("stored manifest"))?;
            if self.keys.contains_key(generation) && m.safe(&k) {
                return Ok(Some(*generation));
            }
        }
        Ok(None)
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
    /// checked as a reader must (signature, certificate, body, chain, recipients' archive keys);
    /// a different manifest for a generation it saw is refused. With `recovery_code`, the recovery
    /// archive key opens what is sealed or transferred to its version (account recovery).
    #[allow(clippy::too_many_arguments)]
    pub fn archive_sync(
        &mut self,
        devices: &[ArchiveDevice],
        recovery: &[(String, String)],
        revocations: &[ArchiveRevocation],
        manifests: &[Vec<u8>],
        transfers: &[Vec<u8>],
        recovery_code: Option<&[u8]>,
        now: i64,
    ) -> Result<ArchiveState> {
        self.operation(|a| {
            super::current_time(now)?;
            let account_key = a.account_certificate()?.account_key.clone();
            let me = a.credential.device_key().to_owned();
            let mut record = a.archive.clone().unwrap_or(ArchiveRecord {
                account_key: account_key.clone(),
                checkpoints: BTreeMap::new(),
                manifests: BTreeMap::new(),
                keys: BTreeMap::new(),
                revocations: BTreeMap::new(),
                current: BTreeMap::new(),
                recovery: None,
                held: BTreeMap::new(),
                sent: BTreeSet::new(),
            });
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
            // Devices: archive keys signed by themselves; current with a valid certificate and no
            // known revocation.
            let mut archive_keys = BTreeMap::new();
            for d in devices {
                let Some(k) =
                    DeviceArchiveKey::parse(&d.statement, &d.signature).filter(|k| k.account_key == account_key)
                else {
                    continue;
                };
                archive_keys.entry(k.device_key.clone()).or_insert(k.x25519);
            }
            let revoked = record.revoked();
            record.current = devices
                .iter()
                .filter_map(|d| {
                    let k = DeviceArchiveKey::parse(&d.statement, &d.signature)?;
                    let current = d
                        .certificate
                        .as_ref()
                        .is_some_and(|c| certificate_current(&account_key, &k.device_key, c, now))
                        && !revoked.contains_key(&k.device_key);
                    (current && archive_keys.get(&k.device_key) == Some(&k.x25519)).then_some((k.device_key, k.x25519))
                })
                .collect();
            // Manifests, in order.
            let recovery_secret = recovery_code.map(|code| {
                let account = fixed::<32>(&account_key).expect("account key");
                archive::recovery_key_pair(code, &account).0
            });
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
                if m.creator_revoked(&record.revoked()) {
                    return Err(Invalid("a manifest by a revoked device"));
                }
                if m.recipients.iter().any(|r| archive_keys.get(&r.device_key) != Some(&r.x25519)) {
                    return Err(Invalid("a manifest recipient's archive key"));
                }
                for r in &m.revocations {
                    record.learn(r);
                }
                record.checkpoints.insert(m.generation, hash);
                record.manifests.insert(m.generation, bytes.clone());
                let opened = m
                    .open(Seat::Device(&me), &a.device.hpke_private)
                    .ok()
                    .or_else(|| recovery_secret.as_ref().and_then(|s| m.open(Seat::Recovery, s).ok()));
                if let Some(key) = opened {
                    record.keys.insert(m.generation, key);
                }
            }
            // Transfers to this device (or, recovering, to the recovery key's version).
            for bytes in transfers {
                let Ok(t) = Transfer::parse(bytes, &account_key) else { continue };
                if record.checkpoints.get(&t.generation) != Some(&t.manifest_hash)
                    || record.revoked().get(&t.sender).is_some_and(|&at| at <= t.created_at)
                    || record.keys.contains_key(&t.generation)
                {
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
            // Devices that some accepted generation names can read the archive already.
            let readable: BTreeSet<String> = record
                .manifests
                .values()
                .filter_map(|b| Manifest::parse(b, &account_key).ok())
                .flat_map(|m| m.recipients.into_iter().map(|r| r.device_key))
                .collect();
            let waiting_devices: Vec<String> = record
                .current
                .keys()
                .filter(|d| !readable.contains(*d) && !record.sent.iter().any(|s| s.ends_with(&format!("/{d}"))))
                .cloned()
                .collect();
            let state = ArchiveState {
                newest: record.newest(),
                safe: record.safe(&me)?,
                opened: record.keys.keys().copied().collect(),
                waiting_devices,
                recovery_version: record.recovery_key().map(|k| k.version),
            };
            a.archive = Some(record);
            a.settle_held();
            Ok(state)
        })
    }

    /// The next generation's manifest, sealed to every current device of the account with an
    /// archive key and to the current recovery key, carrying every revocation this device knows.
    /// Posted to Connect, then taken in by the next [`Agent::archive_sync`].
    pub fn archive_create_manifest(&self, now: i64) -> Result<Vec<u8>> {
        let record = self.archive.as_ref().ok_or(Invalid("archive not synced"))?;
        let certificate = self.account_certificate()?;
        let me = self.credential.device_key();
        if !record.current.contains_key(me) {
            return Err(Invalid("this device is not a current archive device"));
        }
        let generation = record.newest().map_or(1, |g| g + 1);
        let previous = record.newest().and_then(|g| record.checkpoints.get(&g)).cloned();
        let recipients: Vec<Recipient> =
            record.current.iter().map(|(d, x)| Recipient { device_key: d.clone(), x25519: *x }).collect();
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
    /// newest safe generation; `None` without a safe generation (create one first).
    pub fn archive_seal(&self, limit: usize) -> Result<Option<ArchiveUpload>> {
        let record = self.archive.as_ref().ok_or(Invalid("archive not synced"))?;
        let Some(generation) = record.safe(self.credential.device_key())? else { return Ok(None) };
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
    /// group it holds no record of is kept until it does. Returns how many keys it imported.
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
                record.held.entry(k).or_insert(entry.key);
                imported += 1;
            }
            a.settle_held();
            Ok(imported)
        })
    }

    /// Moves held archive keys into the epochs of groups this device now holds.
    fn settle_held(&mut self) {
        let Some(record) = self.archive.as_mut() else { return };
        let uid = self.credential.uid();
        let ready: Vec<String> = record
            .held
            .keys()
            .filter(|k| k.rsplit_once('/').is_some_and(|(group, _)| self.groups.contains_key(group)))
            .cloned()
            .collect();
        for k in ready {
            let value = record.held.remove(&k).expect("listed");
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

    /// Transfers of every generation key this device holds to a current device of the account
    /// that cannot read them yet (or to the recovery key's `version`), once per generation and
    /// recipient. Posted to Connect; then [`Agent::archive_transfers_sent`].
    pub fn archive_transfers_for(&self, recipient: &str, now: i64) -> Result<Vec<(String, Vec<u8>)>> {
        let record = self.archive.as_ref().ok_or(Invalid("archive not synced"))?;
        let certificate = self.account_certificate()?;
        let (to, x25519) = if let Some(version) = recipient.strip_prefix("recovery:") {
            let k = record
                .recovery_key()
                .filter(|k| k.version.to_string() == version)
                .ok_or(Invalid("recovery version"))?;
            (TransferRecipient::Recovery { version: k.version }, k.x25519)
        } else {
            let x = *record.current.get(recipient).ok_or(Invalid("not a current device"))?;
            (TransferRecipient::Device { device_key: recipient.into(), x25519: x }, x)
        };
        let mut out = Vec::new();
        for (generation, key) in &record.keys {
            let tag = format!("{generation}/{recipient}");
            if record.sent.contains(&tag) {
                continue;
            }
            let m = Manifest::parse(&record.manifests[generation], &record.account_key)
                .map_err(|_| Invalid("stored manifest"))?;
            if m.recipients.iter().any(|r| r.device_key == recipient) {
                continue;
            }
            let t = Transfer::create(&m, key, &self.signer.0, certificate, to.clone(), &x25519, now)
                .map_err(|_| Invalid("transfer"))?;
            out.push((tag, t));
        }
        Ok(out)
    }

    pub fn archive_transfers_sent(&mut self, tags: &[String]) -> Result<()> {
        self.operation(|a| {
            let record = a.archive.as_mut().ok_or(Invalid("archive not synced"))?;
            record.sent.extend(tags.iter().cloned());
            Ok(())
        })
    }
}

/// Whether `bytes` are a stored archive record.
pub(super) fn parse_record(bytes: &[u8]) -> Result<ArchiveRecord> {
    serde_json::from_slice(bytes).map_err(|_| AgentError::Invalid("stored archive record"))
}

#[cfg(test)]
mod tests;
