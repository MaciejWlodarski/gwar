//! The MLS delivery service, serialized by the core actor.

use std::collections::BTreeSet;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use gwar_mls::{
    Kind,
    descriptor::dm_participants,
    ds::{self, CommitSender, DsError, Observer},
    history::{Chunk, DeviceHpkeKey},
    signer::Ed25519Signer,
};
use serde::Serialize;
use vc_proto::{
    ErrorBody, ErrorCode, Event, MlsDevice, MlsEvent, MlsGroup, MlsLogEntry, MlsPackage, MlsPage, MlsRequest,
    MlsResponse, Response, SessionId,
};

use super::{Core, Reply, err, now_ms, storage_error};
use crate::store::mls::{GroupRow, ProposalRow};

const BLOB_BYTES: usize = 128 * 1024;
const LIST_BYTES: usize = 1024 * 1024 - 4096;

fn bad(message: &str) -> ErrorBody {
    err(ErrorCode::BadRequest, message)
}
fn ds_error(error: DsError) -> ErrorBody {
    let code = match error {
        DsError::Stale => ErrorCode::Stale,
        DsError::Expired => ErrorCode::Expired,
        DsError::Early => ErrorCode::Early,
        DsError::Invalid(_) => ErrorCode::BadRequest,
    };
    err(code, &error.to_string())
}

fn blob(value: &str, limit: usize) -> Result<Vec<u8>, ErrorBody> {
    if value.len() > limit.div_ceil(3) * 4 {
        return Err(err(ErrorCode::TooLarge, "MLS blob too large"));
    }
    let bytes = B64.decode(value).map_err(|_| bad("invalid base64url"))?;
    if bytes.len() > limit {
        return Err(err(ErrorCode::TooLarge, "MLS blob too large"));
    }
    if B64.encode(&bytes) != value {
        return Err(bad("noncanonical base64url"));
    }
    Ok(bytes)
}

fn limit(value: Option<u32>) -> Result<usize, ErrorBody> {
    let value = value.unwrap_or(100);
    if !(1..=100).contains(&value) {
        return Err(bad("limit must be 1–100"));
    }
    Ok(value as usize)
}

fn uid_filter(uids: Vec<String>) -> Result<BTreeSet<String>, ErrorBody> {
    if uids.len() > 20 || uids.iter().any(|u| vc_proto::decode_fixed::<20>(u).is_none()) {
        return Err(bad("at most 20 valid uids"));
    }
    Ok(uids.into_iter().collect())
}

fn page<T: Serialize>(items: Vec<(String, T)>, paging: MlsPage) -> Result<(Vec<T>, Option<String>), ErrorBody> {
    let limit = limit(paging.limit)?;
    let after = paging
        .after
        .map(|s| {
            let bytes = blob(&s, 512)?;
            String::from_utf8(bytes).map_err(|_| bad("invalid cursor"))
        })
        .transpose()?;
    let mut output = Vec::new();
    let mut last = None;
    let mut bytes = 0;
    for (key, value) in items {
        if after.as_ref().is_some_and(|a| &key <= a) {
            continue;
        }
        let size = serde_json::to_vec(&value).map_err(|e| storage_error(e.into()))?.len() + 1;
        if output.len() == limit || bytes + size > LIST_BYTES {
            return Ok((output, last.map(|key: String| B64.encode(key))));
        }
        bytes += size;
        last = Some(key);
        output.push(value);
    }
    Ok((output, None))
}

fn reply(value: MlsResponse) -> Reply {
    Ok(Response::Mls(value))
}

impl Core {
    fn mls_member(&self, uid: &str) -> Result<bool, ErrorBody> {
        Ok(self.store.user_by_uid(uid).map_err(storage_error)?.is_some()
            && !self.store.bans(now_ms()).map_err(storage_error)?.iter().any(|b| b.uid.as_deref() == Some(uid)))
    }

    fn mls_can_read(&self, uid: &str, conversation: &str) -> Result<bool, ErrorBody> {
        Ok(dm_participants(conversation).is_some_and(|p| p.contains(&uid)) && self.mls_member(uid)?)
    }

    fn mls_readers(&self, conversation: &str) -> BTreeSet<SessionId> {
        self.sessions
            .values()
            .filter(|s| s.out.is_some() && self.mls_can_read(&s.uid, conversation).unwrap_or(false))
            .map(|s| s.id)
            .collect()
    }

    fn mls_group_for(&self, session: SessionId, group: &str) -> Result<GroupRow, ErrorBody> {
        if vc_proto::decode_fixed::<32>(group).is_none() {
            return Err(bad("invalid group id"));
        }
        let row = self
            .store
            .mls_group(group)
            .map_err(storage_error)?
            .ok_or_else(|| err(ErrorCode::NotFound, "no such MLS group"))?;
        if !self.mls_can_read(&self.sessions[&session].uid, &row.conversation)? {
            return Err(err(ErrorCode::Forbidden, "no access to this conversation"));
        }
        Ok(row)
    }

    fn mls_observer(&self, row: &GroupRow) -> Result<Observer, ErrorBody> {
        Observer::load(&row.observer, &self.server_key).map_err(|e| storage_error(e.into()))
    }

    fn mls_missing_proposals(
        &self,
        observer: &mut Observer,
        existing: &[ProposalRow],
    ) -> anyhow::Result<Vec<ProposalRow>> {
        let conversation = observer.descriptor().descriptor.conversation.clone();
        let participants = dm_participants(&conversation);
        let mut proposals = Vec::new();
        for leaf in observer.leaves() {
            let allowed = self.mls_member(&leaf.uid).map_err(|e| anyhow::anyhow!(e.message))?
                && participants.is_some_and(|p| p.contains(&leaf.uid.as_str()))
                && !self.store.mls_revoked(&leaf.device_key, &leaf.uid)?;
            if !allowed && !existing.iter().any(|p| p.leaf == leaf.index) {
                proposals.push(ProposalRow {
                    leaf: leaf.index,
                    proposal: observer.propose_remove(&Ed25519Signer((*self.server_signer).clone()), leaf.index)?,
                });
            }
        }
        Ok(proposals)
    }

    fn mls_announce_proposals(&mut self, row: &GroupRow, proposals: &[ProposalRow]) {
        let readers = self.mls_readers(&row.conversation);
        for p in proposals {
            self.broadcast(
                Event::Mls(MlsEvent::Proposal {
                    group: row.group.clone(),
                    epoch: row.epoch,
                    proposal: B64.encode(&p.proposal),
                }),
                |s| readers.contains(&s.id),
            );
        }
    }

    pub(super) fn mls_reconcile(&mut self) -> anyhow::Result<()> {
        for mut row in self.store.mls_all_groups()? {
            let mut observer = Observer::load(&row.observer, &self.server_key)?;
            let mut existing = self.store.mls_proposals(&row.group)?;
            let added = self.mls_missing_proposals(&mut observer, &existing)?;
            if added.is_empty() {
                continue;
            }
            row.observer = observer.save();
            existing.extend(added.iter().map(|p| ProposalRow { leaf: p.leaf, proposal: p.proposal.clone() }));
            self.store.mls_write_group(&row, &observer.leaves(), &existing, None, None)?;
            self.mls_announce_proposals(&row, &added);
        }
        Ok(())
    }

    pub(super) fn mls_request(&mut self, session: SessionId, request: MlsRequest) -> Reply {
        let s = &self.sessions[&session];
        if s.out.is_none() || vc_proto::decode_fixed::<32>(&s.hello_key).is_none() || !self.mls_member(&s.uid)? {
            return Err(err(ErrorCode::Forbidden, "MLS requires a server member"));
        }
        let device = s.hello_key.clone();
        let uid = s.uid.clone();
        match request {
            MlsRequest::DevicePublish { hpke_key, created_at, signature } => {
                let published = DeviceHpkeKey {
                    device_key: device.clone(),
                    hpke_key: hpke_key.clone(),
                    created_at,
                    signature: signature.clone(),
                };
                if !published.verify() {
                    return Err(bad("invalid device HPKE key"));
                }
                let d = MlsDevice { uid, device_key: device, hpke_key, created_at, signature };
                if !self.store.mls_publish_device(&d).map_err(storage_error)?
                    && self.store.mls_device(&d.device_key).map_err(storage_error)?.as_ref() != Some(&d)
                {
                    return Err(err(ErrorCode::Conflict, "device key publication is older"));
                }
                Ok(Response::Empty {})
            }
            MlsRequest::KeyPackagesPublish { packages, last_resort } => {
                if packages.len() > 20 {
                    return Err(bad("at most 20 packages"));
                }
                let mut parsed = Vec::new();
                for (package, resort) in packages.into_iter().map(|p| (p, false)).chain(last_resort.map(|p| (p, true)))
                {
                    let bytes = blob(&package, 4 * 1024)?;
                    let info = ds::key_package(&bytes).map_err(ds_error)?;
                    if info.device_key != device || info.uid != uid || info.last_resort != resort {
                        return Err(bad("key package identity or type"));
                    }
                    parsed.push((info, bytes));
                }
                // Exceeding the device's quota rolls back the entire publication.
                let count = self.store.mls_publish_packages(&parsed, &device, now_ms()).map_err(|e| {
                    if e.to_string() == "too many key packages" {
                        bad("at most 20 stored packages")
                    } else {
                        storage_error(e)
                    }
                })?;
                reply(MlsResponse::Published { count })
            }
            MlsRequest::KeyPackagesClaim { uids, page: paging } => {
                let uids = uid_filter(uids)?;
                let mut seen = BTreeSet::new();
                let mut items = Vec::new();
                for p in self.store.mls_packages(now_ms()).map_err(storage_error)? {
                    if p.device_key == device
                        || !uids.contains(&p.uid)
                        || self.store.mls_revoked(&p.device_key, &p.uid).map_err(storage_error)?
                        || !self.mls_member(&p.uid)?
                        || !seen.insert(p.device_key.clone())
                    {
                        continue;
                    }
                    items.push((
                        format!("{}:{}", p.uid, p.device_key),
                        (
                            MlsPackage { device_key: p.device_key, package: B64.encode(p.package) },
                            p.reference,
                            p.last_resort,
                        ),
                    ));
                }
                let (selected, next) = page(items, paging)?;
                let references: Vec<_> = selected.iter().filter(|p| !p.2).map(|p| p.1.clone()).collect();
                self.store.mls_consume_packages(&references).map_err(storage_error)?;
                reply(MlsResponse::Packages { packages: selected.into_iter().map(|p| p.0).collect(), next })
            }
            MlsRequest::Devices { uids, page: paging } => {
                let uids = uid_filter(uids)?;
                let mut items = Vec::new();
                for d in self.store.mls_devices().map_err(storage_error)? {
                    if uids.contains(&d.uid)
                        && self.mls_member(&d.uid)?
                        && !self.store.mls_revoked(&d.device_key, &d.uid).map_err(storage_error)?
                    {
                        items.push((format!("{}:{}", d.uid, d.device_key), d));
                    }
                }
                let (devices, next) = page(items, paging)?;
                reply(MlsResponse::Devices { devices, next })
            }
            MlsRequest::GroupCreate { conversation, group_info } => {
                self.mls_create(session, &conversation, &group_info)
            }
            MlsRequest::GroupInfo { group } => {
                let row = self.mls_group_for(session, &group)?;
                reply(MlsResponse::GroupInfo { group_info: B64.encode(row.group_info), epoch: row.epoch })
            }
            MlsRequest::Commit { group, epoch, commit, group_info, welcome } => {
                self.mls_commit(session, &group, epoch, &commit, &group_info, welcome.as_deref())
            }
            MlsRequest::Log { group, since, limit: requested } => {
                let row = self.mls_group_for(session, &group)?;
                if since > vc_proto::MAX_SAFE_INTEGER as u64 {
                    return Err(bad("invalid epoch"));
                }
                let limit = limit(requested)?;
                let pending: Vec<_> = self
                    .store
                    .mls_proposals(&group)
                    .map_err(storage_error)?
                    .into_iter()
                    .map(|p| B64.encode(p.proposal))
                    .collect();
                let (oldest, rows) = self.store.mls_log(&group, since, now_ms()).map_err(storage_error)?;
                let mut bytes = serde_json::to_vec(&pending).map_err(|e| storage_error(e.into()))?.len();
                let mut entries = Vec::new();
                let mut next = None;
                for entry in rows {
                    let size = serde_json::to_vec(&entry).map_err(|e| storage_error(e.into()))?.len() + 1;
                    if entries.len() == limit || bytes + size > LIST_BYTES {
                        next = Some(entry.epoch);
                        break;
                    }
                    bytes += size;
                    entries.push(entry);
                }
                reply(MlsResponse::Log { head: row.epoch, oldest: oldest.unwrap_or(row.epoch), entries, pending, next })
            }
            MlsRequest::Groups { page: paging } => {
                let mut items = Vec::new();
                for row in self.store.mls_groups().map_err(storage_error)? {
                    if self.mls_can_read(&uid, &row.conversation)? {
                        items.push((
                            row.conversation.clone(),
                            MlsGroup {
                                conversation: row.conversation,
                                group: row.group.clone(),
                                epoch: row.epoch,
                                pending: !self.store.mls_proposals(&row.group).map_err(storage_error)?.is_empty(),
                            },
                        ));
                    }
                }
                let (groups, next) = page(items, paging)?;
                reply(MlsResponse::Groups { groups, next })
            }
            MlsRequest::Welcomes { page: paging } => {
                let mut items = Vec::new();
                for w in self.store.mls_welcomes(&device, now_ms()).map_err(storage_error)? {
                    if self.mls_group_for(session, &w.group).is_ok() {
                        items.push((w.id.clone(), w));
                    }
                }
                let (welcomes, next) = page(items, paging)?;
                reply(MlsResponse::Welcomes { welcomes, next })
            }
            MlsRequest::WelcomesAck { ids } => self.mls_ack(&device, &ids, false),
            MlsRequest::HistoryAck { ids } => self.mls_ack(&device, &ids, true),
            MlsRequest::HistoryShare { group, recipient, chunk } => {
                let row = self.mls_group_for(session, &group)?;
                let observer = self.mls_observer(&row)?;
                let leaves = observer.leaves();
                if !leaves.iter().any(|l| l.device_key == device && l.uid == uid)
                    || !leaves.iter().any(|l| l.device_key == recipient)
                {
                    return Err(err(ErrorCode::Forbidden, "history devices must be group members"));
                }
                let recipient_key = self
                    .store
                    .mls_device(&recipient)
                    .map_err(storage_error)?
                    .ok_or_else(|| bad("recipient has no HPKE key"))?;
                let bytes = blob(&chunk, gwar_mls::history::MAX_CHUNK_BYTES)?;
                let chunk = Chunk::parse(&bytes).ok_or_else(|| bad("invalid history chunk"))?;
                if !chunk.is_for(&self.server_key, &observer.group_id(), &recipient, &recipient_key.hpke_key)
                    || chunk.header.sender_device_key != device
                    || !chunk.verify()
                {
                    return Err(bad("history chunk identity or signature"));
                }
                if self
                    .store
                    .mls_share_history(
                        &group,
                        &recipient,
                        &uid,
                        &bytes,
                        (chunk.header.first_epoch, chunk.header.last_epoch),
                        now_ms(),
                    )
                    .map_err(storage_error)?
                {
                    self.broadcast(Event::Mls(MlsEvent::History { group }), |s| s.hello_key == recipient);
                }
                Ok(Response::Empty {})
            }
            MlsRequest::HistoryFetch { group, page: paging } => {
                if let Some(group) = &group {
                    self.mls_group_for(session, group)?;
                }
                let mut items = Vec::new();
                for c in self.store.mls_history(&device, now_ms()).map_err(storage_error)? {
                    if group.as_ref().is_none_or(|g| g == &c.group) && self.mls_group_for(session, &c.group).is_ok() {
                        items.push((c.id.clone(), c));
                    }
                }
                let (chunks, next) = page(items, paging)?;
                reply(MlsResponse::Chunks { chunks, next })
            }
            MlsRequest::HistoryWanted { page: paging } => {
                let mut items = Vec::new();
                for w in self.store.mls_wanted(now_ms()).map_err(storage_error)? {
                    if let Ok(row) = self.mls_group_for(session, &w.group)
                        && self.mls_observer(&row)?.leaves().iter().any(|l| l.device_key == device && l.uid == uid)
                    {
                        items.push((format!("{}:{}", w.group, w.device), w));
                    }
                }
                let (wanted, next) = page(items, paging)?;
                reply(MlsResponse::Wanted { wanted, next })
            }
            MlsRequest::HistoryRequest { group } => {
                let row = self.mls_group_for(session, &group)?;
                if !self.store.mls_request_history(&group, &device, now_ms()).map_err(storage_error)? {
                    return Err(err(ErrorCode::RateLimited, "request history at most once a minute"));
                }
                let leaves = self.mls_observer(&row)?.leaves();
                let readers = self.mls_readers(&row.conversation);
                self.broadcast(Event::Mls(MlsEvent::HistoryWanted { group, device }), |s| {
                    readers.contains(&s.id) && leaves.iter().any(|l| l.device_key == s.hello_key && l.uid == s.uid)
                });
                Ok(Response::Empty {})
            }
        }
    }

    fn mls_ack(&self, device: &str, ids: &[String], history: bool) -> Reply {
        if ids.len() > 100 || ids.iter().any(|id| vc_proto::decode_fixed::<16>(id).is_none()) {
            return Err(bad("at most 100 valid ids"));
        }
        self.store.mls_ack(device, ids, history).map_err(storage_error)?;
        Ok(Response::Empty {})
    }

    fn mls_create(&mut self, session: SessionId, conversation: &str, info: &str) -> Reply {
        let info = blob(info, BLOB_BYTES)?;
        let mut observer = Observer::create(&info, &self.server_key).map_err(ds_error)?;
        let signed = observer.descriptor();
        let d = &signed.descriptor;
        let s = &self.sessions[&session];
        let participants =
            dm_participants(conversation).ok_or_else(|| bad("only direct message groups are supported"))?;
        if d.kind != Kind::Dm
            || d.conversation != conversation
            || d.creator_uid != s.uid
            || signed.creator.device_key() != s.hello_key
            || observer.epoch() != 0
            || observer.leaves().len() != 1
            || observer.leaves()[0].device_key != s.hello_key
            || observer.leaves()[0].uid != s.uid
        {
            return Err(bad("group must contain only its creator at epoch zero"));
        }
        if !participants.contains(&s.uid.as_str())
            || !self.mls_member(participants[0])?
            || !self.mls_member(participants[1])?
        {
            return Err(err(ErrorCode::Forbidden, "both participants must be server members"));
        }
        if let Some(existing) = self.store.mls_live_group(conversation).map_err(storage_error)? {
            if existing.incarnation >= d.incarnation {
                return Err(err(ErrorCode::Conflict, &existing.group));
            }
            if d.incarnation != existing.incarnation + 1 || d.previous_group_id.as_deref() != Some(&existing.group) {
                return Err(bad("invalid group predecessor"));
            }
        } else if d.incarnation != 0 {
            return Err(bad("first incarnation must be zero"));
        }
        let mut row = GroupRow {
            group: B64.encode(observer.group_id()),
            conversation: conversation.into(),
            incarnation: d.incarnation,
            epoch: 0,
            committed_at: d.created_at,
            group_info: info,
            observer: Vec::new(),
            created_at: d.created_at,
        };
        let proposals = self.mls_missing_proposals(&mut observer, &[]).map_err(storage_error)?;
        row.observer = observer.save();
        self.store.mls_write_group(&row, &observer.leaves(), &proposals, None, None).map_err(storage_error)?;
        self.mls_announce_proposals(&row, &proposals);
        reply(MlsResponse::Created { group: row.group })
    }

    fn mls_commit(
        &mut self,
        session: SessionId,
        group: &str,
        epoch: u64,
        commit: &str,
        info: &str,
        welcome: Option<&str>,
    ) -> Reply {
        let mut row = self.mls_group_for(session, group)?;
        if row.epoch != epoch
            || self.store.mls_live_group(&row.conversation).map_err(storage_error)?.is_none_or(|g| g.group != group)
        {
            return Err(err(ErrorCode::Stale, "stale epoch or incarnation"));
        }
        let commit = blob(commit, BLOB_BYTES)?;
        let info = blob(info, BLOB_BYTES)?;
        let welcome = welcome.map(|w| blob(w, BLOB_BYTES)).transpose()?;
        let observer = self.mls_observer(&row)?;
        let before = observer.leaves();
        let stored = self.store.mls_proposals(group).map_err(storage_error)?;
        let clock = now_ms().max(self.mls_commit_clock);
        self.store.set_meta("mls_commit_clock", &clock.to_string()).map_err(storage_error)?;
        self.mls_commit_clock = clock;
        // Read referenced proposals only after the time and full commit validation,
        // so even a now-invalid Add receives the terminal expired reply.
        let original = observer.save();
        let (mut observer, accepted) =
            observer.commit(&commit, &info, welcome.as_deref(), clock, row.committed_at).map_err(ds_error)?;
        let s = &self.sessions[&session];
        let sender_matches = match accepted.sender {
            CommitSender::Member(index) => {
                before.iter().any(|l| l.index == index && l.device_key == s.hello_key && l.uid == s.uid)
            }
            CommitSender::NewMember => {
                accepted.added.iter().any(|l| l.device_key == s.hello_key && l.uid == s.uid)
                    && accepted.added.len() == 1
            }
        };
        if !sender_matches {
            return Err(err(ErrorCode::Forbidden, "commit sender must be this session's device"));
        }
        let referenced = Observer::load(&original, &self.server_key)
            .map_err(ds_error)?
            .referenced_removals(&commit)
            .map_err(ds_error)?;
        let mut proposals = Vec::new();
        for leaf in referenced {
            let p = stored.iter().find(|p| p.leaf == leaf).ok_or_else(|| bad("unknown server proposal"))?;
            proposals.push(B64.encode(&p.proposal));
        }
        let entry = MlsLogEntry { epoch, proposals: proposals.clone(), commit: B64.encode(&commit) };
        row.epoch = accepted.epoch;
        row.committed_at = accepted.committed_at;
        row.group_info = info;
        let pending = self.mls_missing_proposals(&mut observer, &[]).map_err(storage_error)?;
        row.observer = observer.save();
        self.store
            .mls_write_group(
                &row,
                &observer.leaves(),
                &pending,
                Some(&entry),
                welcome.as_deref().map(|w| (w, accepted.welcome_recipients.as_slice())),
            )
            .map_err(storage_error)?;
        let readers = self.mls_readers(&row.conversation);
        self.broadcast(
            Event::Mls(MlsEvent::Commit { group: group.into(), epoch, proposals, commit: entry.commit }),
            |s| readers.contains(&s.id),
        );
        for device in &accepted.welcome_recipients {
            self.broadcast(Event::Mls(MlsEvent::Welcome { group: group.into() }), |s| {
                readers.contains(&s.id) && &s.hello_key == device
            });
        }
        self.mls_announce_proposals(&row, &pending);
        reply(MlsResponse::Committed { epoch: row.epoch })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_pages_limit_wire_bytes_and_resume_at_the_last_returned_entry() {
        let items = || {
            (0..10)
                .map(|i| {
                    let key = format!("{i:02}");
                    (
                        key.clone(),
                        vc_proto::MlsWelcome {
                            id: key,
                            group: B64.encode([7; 32]),
                            welcome: B64.encode(vec![0; BLOB_BYTES]),
                        },
                    )
                })
                .collect()
        };
        let (welcomes, next) = page(items(), MlsPage::default()).unwrap();
        assert_eq!(welcomes.len(), 5);
        let reply = MlsResponse::Welcomes { welcomes, next: next.clone() };
        assert!(serde_json::to_vec(&reply).unwrap().len() <= 1024 * 1024);
        let (rest, end) = page(items(), MlsPage { after: next, limit: None }).unwrap();
        assert_eq!(rest.len(), 5);
        assert_eq!(rest[0].id, "05");
        assert_eq!(end, None);
    }
}
