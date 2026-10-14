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
    ErrorBody, ErrorCode, Event, MlsDevice, MlsEvent, MlsIncarnation, MlsLogEntry, MlsPackage, MlsPage, MlsRequest,
    MlsResponse, Response, SessionId,
};

use super::{Core, Reply, err, now_ms, storage_error};
use crate::store::mls::{GroupRow, ProposalRow, ShareHistory};
use vc_proto::ChannelId;

const BLOB_BYTES: usize = 128 * 1024;
const LIST_BYTES: usize = 640 * 1024 - 4096;

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

struct ListPage<T> {
    items: Vec<T>,
    count: usize,
    bytes: usize,
    last: Option<String>,
    next: Option<String>,
}

fn cursor(paging: &MlsPage) -> Result<String, ErrorBody> {
    paging
        .after
        .as_ref()
        .map(|s| String::from_utf8(blob(s, 512)?).map_err(|_| bad("invalid cursor")))
        .transpose()
        .map(|s| s.unwrap_or_default())
}

impl<T: Serialize> ListPage<T> {
    fn new(count: usize, bytes: usize) -> Self {
        Self { items: Vec::new(), count, bytes, last: None, next: None }
    }

    // The SQL cursor reads one row at a time, including at most one lookahead.
    fn push(&mut self, key: String, value: T) -> anyhow::Result<bool> {
        let size = serde_json::to_vec(&value)?.len() + 1;
        if self.items.len() == self.count || size > self.bytes {
            anyhow::ensure!(!self.items.is_empty(), "MLS entry exceeds response budget");
            self.next = self.last.as_ref().map(|key| B64.encode(key));
            return Ok(false);
        }
        self.bytes -= size;
        self.last = Some(key);
        self.items.push(value);
        Ok(true)
    }
}

#[cfg(test)]
fn page<T: Serialize>(items: Vec<(String, T)>, paging: MlsPage) -> Result<(Vec<T>, Option<String>), ErrorBody> {
    let after = cursor(&paging)?;
    let mut page = ListPage::new(limit(paging.limit)?, LIST_BYTES);
    for (key, value) in items {
        if key > after && !page.push(key, value).map_err(storage_error)? {
            break;
        }
    }
    Ok((page.items, page.next))
}

/// The channel of a canonical `channel:{id}` conversation.
pub(super) fn channel_id(conversation: &str) -> Option<ChannelId> {
    let id: ChannelId = conversation.strip_prefix("channel:")?.parse().ok()?;
    (format!("channel:{id}") == conversation).then_some(id)
}

fn reply(value: MlsResponse) -> Reply {
    Ok(Response::Mls(value))
}

impl Core {
    pub(super) fn mls_member(&self, uid: &str) -> Result<bool, ErrorBody> {
        Ok(self.store.user_by_uid(uid).map_err(storage_error)?.is_some()
            && !self.store.bans(now_ms()).map_err(storage_error)?.iter().any(|b| b.uid.as_deref() == Some(uid)))
    }

    /// Direct messages: their two participants. Encrypted channels: every member.
    pub(super) fn mls_can_read(&self, uid: &str, conversation: &str) -> Result<bool, ErrorBody> {
        let allowed = match dm_participants(conversation) {
            Some(participants) => participants.contains(&uid),
            None => channel_id(conversation).is_some_and(|c| self.is_encrypted(c)),
        };
        Ok(allowed && self.mls_member(uid)?)
    }

    pub(super) fn mls_readers(&self, conversation: &str) -> BTreeSet<SessionId> {
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
            // Any member's well-formed device may be in a channel group.
            let allowed = self.mls_member(&leaf.uid).map_err(|e| anyhow::anyhow!(e.message))?
                && participants.is_none_or(|p| p.contains(&leaf.uid.as_str()))
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

    pub(super) fn mls_reconcile_group(&mut self, mut row: GroupRow) -> anyhow::Result<()> {
        let mut observer = Observer::load(&row.observer, &self.server_key)?;
        let mut existing = self.store.mls_proposals(&row.group)?;
        let added = self.mls_missing_proposals(&mut observer, &existing)?;
        if !added.is_empty() {
            row.observer = observer.save();
            existing.extend(added.iter().map(|p| ProposalRow { leaf: p.leaf, proposal: p.proposal.clone() }));
            self.store.mls_write_group(&row, &observer.leaves(), &existing, None, None, &[])?;
            self.mls_announce_proposals(&row, &added);
        }
        Ok(())
    }

    pub(super) fn mls_reconcile(&mut self) -> anyhow::Result<()> {
        for row in self.store.mls_all_groups()? {
            self.mls_reconcile_group(row)?;
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
                let uids: Vec<_> = uids
                    .into_iter()
                    .filter_map(|uid| match self.mls_member(&uid) {
                        Ok(true) => Some(Ok(uid)),
                        Ok(false) => None,
                        Err(e) => Some(Err(e)),
                    })
                    .collect::<Result<_, _>>()?;
                let after = cursor(&paging)?;
                let mut selected = ListPage::new(limit(paging.limit)?, LIST_BYTES);
                let mut references = Vec::new();
                self.store
                    .mls_packages(now_ms(), &uids, &device, &after, |p| {
                        if self.store.mls_revoked(&p.device_key, &p.uid)? {
                            return Ok(true);
                        }
                        let key = format!("{}:{}", p.uid, p.device_key);
                        let accepted = selected
                            .push(key, MlsPackage { device_key: p.device_key, package: B64.encode(p.package) })?;
                        if accepted && !p.last_resort {
                            references.push(p.reference);
                        }
                        Ok(accepted)
                    })
                    .map_err(storage_error)?;
                self.store.mls_consume_packages(&references).map_err(storage_error)?;
                reply(MlsResponse::Packages { packages: selected.items, next: selected.next })
            }
            MlsRequest::Devices { uids, page: paging } => {
                let uids = uid_filter(uids)?;
                let uids: Vec<_> = uids
                    .into_iter()
                    .filter_map(|uid| match self.mls_member(&uid) {
                        Ok(true) => Some(Ok(uid)),
                        Ok(false) => None,
                        Err(e) => Some(Err(e)),
                    })
                    .collect::<Result<_, _>>()?;
                let after = cursor(&paging)?;
                let mut page = ListPage::new(limit(paging.limit)?, LIST_BYTES);
                self.store
                    .mls_devices(&uids, &after, |d| {
                        if self.store.mls_revoked(&d.device_key, &d.uid)? {
                            return Ok(true);
                        }
                        page.push(format!("{}:{}", d.uid, d.device_key), d)
                    })
                    .map_err(storage_error)?;
                reply(MlsResponse::Devices { devices: page.items, next: page.next })
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
                let oldest = self.store.mls_oldest(&group, now_ms()).map_err(storage_error)?;
                let pending_bytes = serde_json::to_vec(&pending).map_err(|e| storage_error(e.into()))?.len();
                let budget = LIST_BYTES
                    .checked_sub(pending_bytes)
                    .ok_or_else(|| err(ErrorCode::TooLarge, "pending proposals exceed response budget"))?;
                let mut page = ListPage::new(limit, budget);
                let mut next = None;
                self.store
                    .mls_log(&group, since, now_ms(), limit + 1, |entry| {
                        let epoch = entry.epoch;
                        let accepted = page.push(epoch.to_string(), entry)?;
                        if !accepted {
                            next = Some(epoch);
                        }
                        Ok(accepted)
                    })
                    .map_err(storage_error)?;
                reply(MlsResponse::Log {
                    head: row.epoch,
                    oldest: oldest.unwrap_or(row.epoch),
                    entries: page.items,
                    pending,
                    next,
                })
            }
            MlsRequest::Groups { page: paging } => {
                let after = cursor(&paging)?;
                let count = limit(paging.limit)?;
                let mut page = ListPage::new(count, LIST_BYTES);
                self.store
                    .mls_groups(&uid, &after, count + 1, |g| page.push(g.conversation.clone(), g))
                    .map_err(storage_error)?;
                reply(MlsResponse::Groups { groups: page.items, next: page.next })
            }
            MlsRequest::Welcomes { page: paging } => {
                let after = cursor(&paging)?;
                let count = limit(paging.limit)?;
                let mut page = ListPage::new(count, LIST_BYTES);
                self.store
                    .mls_welcomes(&device, &uid, now_ms(), &after, count + 1, |w| page.push(w.id.clone(), w))
                    .map_err(storage_error)?;
                reply(MlsResponse::Welcomes { welcomes: page.items, next: page.next })
            }
            MlsRequest::WelcomesAck { ids } => self.mls_ack(&device, &ids, false),
            MlsRequest::HistoryAck { ids } => self.mls_ack(&device, &ids, true),
            MlsRequest::HistoryShare { group, recipient, chunk, assignment } => {
                let row = self.mls_group_for(session, &group)?;
                // In channel groups only a chunk answering an active assignment ends the request.
                let routed = channel_id(&row.conversation).is_some();
                let answers = assignment.filter(|a| routed && self.history.is_active(a, &group, &recipient, &device));
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
                let outcome = self
                    .store
                    .mls_share_history(
                        &group,
                        &recipient,
                        (&uid, &device),
                        &bytes,
                        (chunk.header.first_epoch, chunk.header.last_epoch),
                        now_ms(),
                        !routed || answers.is_some(),
                    )
                    .map_err(storage_error)?;
                match outcome {
                    ShareHistory::Stored => {
                        if let Some(assignment) = answers {
                            self.history_answered(&assignment);
                        }
                        self.broadcast(Event::Mls(MlsEvent::History { group }), |s| s.hello_key == recipient)
                    }
                    ShareHistory::Duplicate => {}
                    ShareHistory::OwnDevice => return Err(bad("cannot share history with the sender's own device")),
                    ShareHistory::FutureEpoch => return Err(bad("history extends beyond the group's epoch")),
                    ShareHistory::Limit => return Err(err(ErrorCode::QuotaExceeded, "history chunk or sender limit")),
                }
                Ok(Response::Empty {})
            }
            MlsRequest::HistoryFetch { group, page: paging } => {
                if let Some(group) = &group {
                    self.mls_group_for(session, group)?;
                }
                let after = cursor(&paging)?;
                let count = limit(paging.limit)?;
                let mut page = ListPage::new(count, LIST_BYTES);
                self.store
                    .mls_history(&device, &uid, group.as_deref(), now_ms(), (&after, count + 1), |c| {
                        page.push(c.id.clone(), c)
                    })
                    .map_err(storage_error)?;
                reply(MlsResponse::Chunks { chunks: page.items, next: page.next })
            }
            MlsRequest::HistoryWanted { page: paging } => {
                let after = cursor(&paging)?;
                let count = limit(paging.limit)?;
                // Direct messages' stored requests merged with this device's channel assignments.
                let key = |w: &vc_proto::MlsHistoryWanted| match &w.assignment {
                    Some(assignment) => format!("{}:{}:{assignment}", w.group, w.device),
                    None => format!("{}:{}", w.group, w.device),
                };
                let mut wanted: Vec<_> = self
                    .history
                    .wanted(&device)
                    .into_iter()
                    .map(|w| (key(&w), w))
                    .filter(|(k, _)| *k > after)
                    .collect();
                self.store
                    .mls_wanted(&uid, &device, now_ms(), &after, count + 1, |w| {
                        wanted.push((key(&w), w));
                        Ok(true)
                    })
                    .map_err(storage_error)?;
                wanted.sort_by(|a, b| a.0.cmp(&b.0));
                let mut page = ListPage::new(count, LIST_BYTES);
                for (key, w) in wanted.into_iter().filter(|(k, _)| *k > after) {
                    if !page.push(key, w).map_err(storage_error)? {
                        break;
                    }
                }
                reply(MlsResponse::Wanted { wanted: page.items, next: page.next })
            }
            MlsRequest::HistoryDecline { group, device: recipient, assignment } => {
                self.mls_group_for(session, &group)?;
                self.history_declined(&assignment, &group, &recipient, &device);
                Ok(Response::Empty {})
            }
            MlsRequest::GroupHistory { conversation, page: paging } => {
                if !self.mls_can_read(&uid, &conversation)? {
                    return Err(err(ErrorCode::Forbidden, "no access to this conversation"));
                }
                let after = paging
                    .after
                    .as_ref()
                    .map(|_| cursor(&paging)?.parse::<u64>().map_err(|_| bad("invalid cursor")))
                    .transpose()?;
                let count = limit(paging.limit)?;
                let mut page = ListPage::new(count, LIST_BYTES);
                for row in self.store.mls_incarnations(&conversation, after, count + 1).map_err(storage_error)? {
                    let descriptor = B64.encode(self.mls_observer(&row)?.descriptor().encode());
                    let entry = MlsIncarnation { group: row.group, descriptor };
                    if !page.push(row.incarnation.to_string(), entry).map_err(storage_error)? {
                        break;
                    }
                }
                reply(MlsResponse::Incarnations { incarnations: page.items, next: page.next })
            }
            MlsRequest::HistoryRequest { group } => {
                let row = self.mls_group_for(session, &group)?;
                if !self.store.mls_request_history(&group, &device, now_ms()).map_err(storage_error)? {
                    return Err(err(ErrorCode::RateLimited, "request history at most once a minute"));
                }
                if channel_id(&row.conversation).is_some() {
                    self.history_requested(&group, &device);
                    return Ok(Response::Empty {});
                }
                let leaves = self.mls_observer(&row)?.leaves();
                let readers = self.mls_readers(&row.conversation);
                self.broadcast(Event::Mls(MlsEvent::HistoryWanted { group, device, assignment: None }), |s| {
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
        let kind = match (dm_participants(conversation), channel_id(conversation)) {
            (Some(participants), _) => {
                if !participants.contains(&s.uid.as_str())
                    || !self.mls_member(participants[0])?
                    || !self.mls_member(participants[1])?
                {
                    return Err(err(ErrorCode::Forbidden, "both participants must be server members"));
                }
                Kind::Dm
            }
            (None, Some(channel)) => {
                let Some(row) = self.channels.get(&channel).filter(|c| c.encrypted) else {
                    return Err(err(ErrorCode::NotFound, "no such encrypted channel"));
                };
                if d.private || d.history != row.history {
                    return Err(bad("the descriptor must match the channel"));
                }
                Kind::Channel
            }
            _ => return Err(bad("only direct message and encrypted channel groups are supported")),
        };
        if d.kind != kind
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
        self.store.mls_write_group(&row, &observer.leaves(), &proposals, None, None, &[]).map_err(storage_error)?;
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
        // A removed leaf ends its device's history routing, even when the same device
        // rejoins in this commit: its request was for the old leaf.
        let routed = channel_id(&row.conversation).is_some();
        let removed: Vec<String> =
            if routed { accepted.removed.iter().map(|l| l.device_key.clone()).collect() } else { Vec::new() };
        self.store
            .mls_write_group(
                &row,
                &observer.leaves(),
                &pending,
                Some(&entry),
                welcome.as_deref().map(|w| (w, accepted.welcome_recipients.as_slice())),
                &removed,
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
        if routed {
            self.history_members_changed(group, &removed.into_iter().collect());
        }
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
        assert_eq!(welcomes.len(), 3);
        let reply = MlsResponse::Welcomes { welcomes, next: next.clone() };
        assert!(serde_json::to_vec(&reply).unwrap().len() <= 640 * 1024);
        let (rest, end) = page(items(), MlsPage { after: next, limit: None }).unwrap();
        assert_eq!(rest.len(), 3);
        assert_eq!(rest[0].id, "03");
        assert!(end.is_some());
    }
}
