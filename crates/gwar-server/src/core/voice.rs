//! Volatile call state. No voice observer, log, token or acknowledgement enters SQLite.
use super::mls::{blob, ds_error};
use super::*;
use gwar_mls::{
    Kind,
    ds::{CommitSender, Observer},
    signer::Ed25519Signer,
};
use gwar_proto::{
    MlsEvent, MlsLogEntry, MlsResponse, VoiceBinding, VoiceClosedReason, VoiceEpoch, VoiceOffer, VoiceParticipation,
    VoiceState,
};
use std::collections::VecDeque;

const MEMBERS: usize = 1000;
const TREE_BYTES: usize = 8 * 1024 * 1024;
const BLOB_BYTES: usize = 128 * 1024;
const PROPOSAL_BYTES: usize = 256 * 1024;
const LOG_BYTES: usize = 16 * 1024 * 1024;
const LOG_ENTRIES: usize = 4096;
const RETENTION_MS: i64 = 90 * 86_400_000;

/// A single admission decision, consumed without another clock-sensitive check.
pub(super) struct Admission {
    pub channel: ChannelId,
    parent: Option<String>,
}
#[derive(Clone, Copy)]
pub(super) struct Limits {
    members: usize,
    observer_bytes: usize,
    tree_bytes: usize,
    proposal_count: usize,
    proposal_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            members: MEMBERS,
            observer_bytes: TREE_BYTES,
            tree_bytes: TREE_BYTES,
            proposal_count: MEMBERS,
            proposal_bytes: PROPOSAL_BYTES,
        }
    }
}

pub(super) struct Call {
    token: String,
    parent: Option<String>,
    group: Option<VoiceGroup>,
    repeat_round: u64,
    repeated_at: Option<Instant>,
}
#[derive(Clone)]
struct LeafBinding {
    uid: String,
    device: String,
    session: SessionId,
    participation: String,
}
#[derive(Clone)]
struct VoiceGroup {
    id: String,
    observer: Vec<u8>,
    info: Vec<u8>,
    epoch: u64,
    committed_at: i64,
    leaves: BTreeMap<u32, LeafBinding>,
    proposals: BTreeMap<u32, Vec<u8>>,
    log: VecDeque<(i64, MlsLogEntry, usize)>,
    acks: BTreeMap<String, u64>,
}
pub(super) struct Participation {
    pub binding: VoiceBinding,
    offer: Option<VoiceOffer>,
    answer: Option<String>,
    negotiation: Option<tokio::task::AbortHandle>,
    ack_bucket: Bucket,
    repeat_bucket: Bucket,
}
pub(super) struct Bucket {
    tokens: f64,
    at: Instant,
    rate: f64,
    burst: f64,
}
impl Bucket {
    pub(super) fn new(rate: f64, burst: f64) -> Self {
        Self { tokens: burst, at: Instant::now(), rate, burst }
    }
    pub(super) fn take(&mut self) -> Result<(), ErrorBody> {
        let now = Instant::now();
        self.tokens = (self.tokens + now.duration_since(self.at).as_secs_f64() * self.rate).min(self.burst);
        self.at = now;
        if self.tokens < 1.0 {
            return Err(err(ErrorCode::RateLimited, "voice request rate exceeded"));
        }
        self.tokens -= 1.0;
        Ok(())
    }
}
fn token() -> String {
    let mut b = [0; 16];
    rand::rng().fill_bytes(&mut b);
    URL_SAFE_NO_PAD.encode(b)
}
pub(super) fn bad(message: &str) -> ErrorBody {
    err(ErrorCode::BadRequest, message)
}
pub(super) fn stale() -> ErrorBody {
    err(ErrorCode::Stale, "stale voice binding")
}
fn state_limits(
    limits: Limits,
    observer_bytes: usize,
    tree_bytes: usize,
    leaves: usize,
    proposal_count: usize,
    proposal_bytes: usize,
) -> Result<(), ErrorBody> {
    if observer_bytes > limits.observer_bytes
        || tree_bytes > limits.tree_bytes
        || proposal_count > limits.proposal_count
        || proposal_bytes > limits.proposal_bytes
    {
        return Err(err(ErrorCode::TooLarge, "voice state limit"));
    }
    if leaves > limits.members {
        return Err(err(ErrorCode::ChannelFull, "voice leaf limit"));
    }
    Ok(())
}
fn admission_fits(limits: Limits, sessions: usize, occupied: usize, pending: usize, replaces_own: bool) -> bool {
    sessions < limits.members && occupied + pending + usize::from(!replaces_own) <= limits.members
}
fn bounded(limits: Limits, observer: &Observer, proposals: &BTreeMap<u32, Vec<u8>>) -> Result<Vec<u8>, ErrorBody> {
    let state = observer.save();
    state_limits(
        limits,
        state.len(),
        observer.tree().map_err(ds_error)?.1.len(),
        observer.leaves().len(),
        proposals.len(),
        proposals.values().map(Vec::len).sum(),
    )?;
    Ok(state)
}
fn include_validity(valid_from: &mut Option<i64>, deadline: &mut Option<i64>, c: &gwar_mls::Credential) {
    if let gwar_mls::Credential::Account(c) = c {
        *valid_from = Some(valid_from.map_or(c.issued_at, |old| old.max(c.issued_at)));
        *deadline = Some(deadline.map_or(c.expires_at, |old| old.min(c.expires_at)));
    }
}
impl Core {
    fn parent_observer(&self, channel: ChannelId) -> Result<Observer, ErrorBody> {
        #[cfg(test)]
        self.voice_parent_loads.set(self.voice_parent_loads.get() + 1);
        let row = self
            .store
            .mls_live_group(&format!("channel:{channel}"))
            .map_err(storage_error)?
            .ok_or_else(|| err(ErrorCode::Forbidden, "join the channel group first"))?;
        Observer::load(&row.observer, &self.server_key).map_err(ds_error)
    }
    fn parent_admits(&self, channel: ChannelId, session: SessionId) -> Result<Option<String>, ErrorBody> {
        if !self.is_encrypted(channel) {
            return Ok(None);
        }
        let s = &self.sessions[&session];
        if s.out.is_none()
            || !self.mls_member(&s.uid)?
            || self.store.mls_revoked(&s.hello_key, &s.uid).map_err(storage_error)?
        {
            return Err(err(ErrorCode::Forbidden, "voice requires a current Gwar device"));
        }
        let parent = self.parent_observer(channel)?;
        if !parent
            .credentials()
            .iter()
            .any(|(_, c)| c.uid() == s.uid && c.device_key() == s.hello_key && c.is_current((self.voice_clock)(), &()))
        {
            return Err(err(ErrorCode::Forbidden, "device is not in the current channel tree"));
        }
        Ok(Some(URL_SAFE_NO_PAD.encode(parent.group_id())))
    }
    pub(super) fn voice_admission(&self, session: SessionId, channel: ChannelId) -> Result<Admission, ErrorBody> {
        let parent = self.parent_admits(channel, session)?;
        if self.is_encrypted(channel) && self.sessions[&session].channel != Some(channel) {
            let sessions = self.sessions.values().filter(|s| s.channel == Some(channel)).count();
            if self.channels[&channel].max_clients.is_some_and(|max| sessions >= max as usize) {
                return Err(err(ErrorCode::ChannelFull, "channel is full"));
            }
            let group = self.calls.get(&channel).and_then(|c| c.group.as_ref());
            // Detached leaves still occupy space. A rejoin may replace only its own leaf.
            let occupied =
                group.map(|g| Observer::load(&g.observer, &self.server_key)).transpose().map_err(ds_error)?;
            let occupied_members: BTreeSet<_> = occupied
                .as_ref()
                .map(|o| o.leaves())
                .unwrap_or_default()
                .into_iter()
                .map(|l| (l.uid, l.device_key))
                .collect();
            let pending = self
                .sessions
                .values()
                .filter(|s| {
                    s.channel == Some(channel) && !occupied_members.contains(&(s.uid.clone(), s.hello_key.clone()))
                })
                .count();
            let s = &self.sessions[&session];
            let own = occupied_members.contains(&(s.uid.clone(), s.hello_key.clone()));
            if !admission_fits(self.voice_limits, sessions, occupied_members.len(), pending, own) {
                return Err(err(ErrorCode::ChannelFull, "voice participation limit"));
            }
        }
        Ok(Admission { channel, parent })
    }
    pub(super) fn voice_allocate(&mut self, session: SessionId, admission: Admission) {
        let Admission { channel, parent } = admission;
        self.voice_dirty = true;
        let call = self.calls.entry(channel).or_insert_with(|| Call {
            token: token(),
            parent,
            group: None,
            repeat_round: 0,
            repeated_at: None,
        });
        self.sessions.get_mut(&session).expect("session").participation = Some(Participation {
            binding: VoiceBinding { call: call.token.clone(), participation: token(), run: None },
            offer: None,
            answer: None,
            negotiation: None,
            ack_bucket: Bucket::new(10.0, 20.0),
            repeat_bucket: Bucket::new(1.0 / 6.0, 10.0),
        });
    }
    pub(super) fn voice_state(&self, session: SessionId) -> Option<VoiceState> {
        let s = self.sessions.get(&session)?;
        let p = s.participation.as_ref()?;
        let channel = s.channel?;
        let c = self.calls.get(&channel)?;
        Some(VoiceState {
            channel,
            call: c.token.clone(),
            participation: p.binding.participation.clone(),
            encrypted: c.parent.is_some(),
            channel_group: c.parent.clone(),
            group: c.group.as_ref().map(|g| g.id.clone()),
            epoch: c.group.as_ref().map(|g| g.epoch),
        })
    }
    pub(super) fn voice_snapshot(&mut self, session: SessionId) {
        let voice = self.voice_state(session);
        self.broadcast(Event::VoiceState { voice }, |s| s.id == session);
    }
    pub(super) fn voice_detach(&mut self, session: SessionId) {
        let s = self.sessions.get_mut(&session).expect("session");
        let Some(p) = s.participation.take() else { return };
        if let Some(task) = p.negotiation {
            task.abort();
        }
        s.talking = false;
        // TeamSpeak's transport belongs to the bridge, not the local WebRTC run.
        if s.out.is_some() {
            s.voice = false;
        }
        self.voice_dirty = true;
        let channel = s.channel.expect("participation channel");
        if let Some(group) = self.calls.get_mut(&channel).and_then(|c| c.group.as_mut()) {
            group.leaves.retain(|_, l| l.session != session || l.participation != p.binding.participation);
            group.acks.remove(&s.hello_key);
        }
        let _ = self.media.try_send(MediaCmd::Remove { session });
        if !self.sessions.values().any(|s| s.id != session && s.channel == Some(channel) && s.participation.is_some())
            && let Some(c) = self.calls.remove(&channel)
            && let Some(g) = c.group
        {
            self.mls_trees.remove(&g.id);
        }
    }
    pub(super) fn voice_close_call(&mut self, channel: ChannelId, reason: VoiceClosedReason) {
        let ids: Vec<_> = self.sessions.values().filter(|s| s.channel == Some(channel)).map(|s| s.id).collect();
        for &id in &ids {
            if let Some(p) = self.sessions[&id].participation.as_ref() {
                let binding = p.binding.clone();
                self.broadcast(Event::VoiceClosed { binding, reason }, |s| s.id == id);
            }
        }
        for id in ids {
            let _ = self.move_client(id, None);
        }
    }
    pub(super) fn voice_group_channel(&self, group: &str) -> Option<ChannelId> {
        self.calls.iter().find(|(_, c)| c.group.as_ref().is_some_and(|g| g.id == group)).map(|(id, _)| *id)
    }
    /// The call of `group` as `session` may know it: a hidden channel's voice group is unknown to
    /// it, like a missing one (6a, 6c).
    pub(super) fn voice_group_visible(&self, session: SessionId, group: &str) -> Option<ChannelId> {
        self.voice_group_channel(group).filter(|&c| self.admitted(self.sessions.get(&session), c))
    }
    fn voice_access(&self, session: SessionId, channel: ChannelId) -> Result<(), ErrorBody> {
        let s = self.sessions.get(&session).ok_or_else(stale)?;
        if s.channel != Some(channel) || s.participation.is_none() {
            return Err(err(ErrorCode::Forbidden, "not a call participant"));
        }
        let parent = self.parent_admits(channel, session)?;
        if self.calls.get(&channel).is_none_or(|c| c.parent != parent) {
            return Err(stale());
        }
        Ok(())
    }
    pub(super) fn voice_check(&self, session: SessionId, binding: &VoiceParticipation) -> Result<ChannelId, ErrorBody> {
        if gwar_proto::decode_fixed::<16>(&binding.call).is_none()
            || gwar_proto::decode_fixed::<16>(&binding.participation).is_none()
        {
            return Err(bad("invalid voice token"));
        }
        let state = self.voice_state(session).ok_or_else(stale)?;
        if state.call != binding.call || state.participation != binding.participation {
            return Err(stale());
        }
        self.voice_access(session, state.channel)?;
        Ok(state.channel)
    }
    pub(super) fn voice_tree(&self, session: SessionId, channel: ChannelId) -> Result<(u64, Vec<u8>), ErrorBody> {
        self.voice_access(session, channel)?;
        let group = self.calls[&channel].group.as_ref().expect("group lookup");
        Observer::load(&group.observer, &self.server_key).map_err(ds_error)?.tree().map_err(ds_error)
    }
    pub(super) fn voice_info(&self, session: SessionId, channel: ChannelId) -> Reply {
        self.voice_access(session, channel)?;
        let g = self.calls[&channel].group.as_ref().expect("group lookup");
        Ok(Response::Mls(MlsResponse::GroupInfo { group_info: URL_SAFE_NO_PAD.encode(&g.info), epoch: g.epoch }))
    }
    pub(super) fn voice_log(
        &mut self,
        session: SessionId,
        channel: ChannelId,
        since: u64,
        limit: Option<u32>,
    ) -> Reply {
        self.voice_access(session, channel)?;
        let limit = limit.unwrap_or(100);
        if !(1..=100).contains(&limit) || since > gwar_proto::MAX_SAFE_INTEGER as u64 {
            return Err(bad("invalid log page"));
        }
        let g = self.calls.get_mut(&channel).unwrap().group.as_mut().unwrap();
        prune(g);
        let pending: Vec<_> = g.proposals.values().map(|p| URL_SAFE_NO_PAD.encode(p)).collect();
        let mut budget = 640 * 1024 - 4096 - serde_json::to_vec(&pending).expect("JSON").len();
        let mut entries = Vec::new();
        let mut next = None;
        for (_, e, bytes) in g.log.iter().filter(|(_, e, _)| e.epoch >= since) {
            if *bytes > budget && entries.is_empty() {
                return Err(err(ErrorCode::TooLarge, "log entry and pending proposals exceed response budget"));
            }
            if entries.len() == limit as usize || *bytes > budget {
                next = Some(e.epoch);
                break;
            }
            budget -= bytes;
            entries.push(e.clone());
        }
        Ok(Response::Mls(MlsResponse::Log {
            head: g.epoch,
            oldest: g.log.front().map_or(g.epoch, |(_, e, _)| e.epoch),
            entries,
            pending,
            next,
        }))
    }
    pub(super) fn voice_create(
        &mut self,
        session: SessionId,
        conversation: &str,
        info: &str,
        binding: VoiceParticipation,
    ) -> Reply {
        let channel = self.voice_check(session, &binding)?;
        let c = &self.calls[&channel];
        if c.parent.is_none() {
            return Err(bad("plaintext call has no MLS group"));
        }
        let info = blob(info, BLOB_BYTES)?;
        let observer = Observer::create(&info, &self.server_key).map_err(ds_error)?;
        let d = &observer.descriptor().descriptor;
        let s = &self.sessions[&session];
        if d.kind != Kind::Voice
            || d.conversation != conversation
            || conversation != format!("voice:{channel}")
            || d.channel_group_id != c.parent
            || d.creator_uid != s.uid
            || observer.descriptor().creator.device_key() != s.hello_key
            || !observer.descriptor().creator.is_current(now_ms(), &())
            || !observer.descriptor().creator.is_current(d.created_at, &())
        {
            return Err(bad("voice descriptor or creator binding"));
        }
        let id = URL_SAFE_NO_PAD.encode(observer.group_id());
        if let Some(g) = &c.group {
            return if g.id == id {
                Ok(Response::Mls(MlsResponse::Created { group: id }))
            } else {
                Err(err(ErrorCode::Conflict, &g.id))
            };
        }
        let state = bounded(self.voice_limits, &observer, &BTreeMap::new())?;
        let leaf = observer.leaves()[0].index;
        let g = VoiceGroup {
            id: id.clone(),
            observer: state,
            info,
            epoch: 0,
            committed_at: d.created_at,
            leaves: BTreeMap::from([(
                leaf,
                LeafBinding {
                    uid: s.uid.clone(),
                    device: s.hello_key.clone(),
                    session,
                    participation: binding.participation,
                },
            )]),
            proposals: BTreeMap::new(),
            log: VecDeque::new(),
            acks: BTreeMap::new(),
        };
        self.calls.get_mut(&channel).unwrap().group = Some(g);
        self.voice_dirty = true;
        let ids: Vec<_> = self.sessions.values().filter(|s| s.channel == Some(channel)).map(|s| s.id).collect();
        for id in ids {
            self.voice_snapshot(id);
        }
        Ok(Response::Mls(MlsResponse::Created { group: id }))
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) fn voice_commit(
        &mut self,
        session: SessionId,
        group: &str,
        epoch: u64,
        commit: &str,
        info: &str,
        binding: VoiceParticipation,
    ) -> Reply {
        if gwar_proto::decode_fixed::<32>(group).is_none() {
            return Err(bad("invalid voice group"));
        }
        let channel = self.voice_check(session, &binding)?;
        let old = self.calls[&channel].group.as_ref().ok_or_else(|| err(ErrorCode::NotFound, "no voice group"))?;
        if old.id != group || old.epoch != epoch {
            return Err(stale());
        }
        if epoch >= gwar_proto::MAX_SAFE_INTEGER as u64 {
            self.voice_close_call(channel, VoiceClosedReason::EpochExhausted);
            return Err(stale());
        }
        let mut next = old.clone();
        let commit = blob(commit, BLOB_BYTES)?;
        let info = blob(info, BLOB_BYTES)?;
        let clock = now_ms().max(self.mls_commit_clock);
        self.store.set_meta("mls_commit_clock", &clock.to_string()).map_err(storage_error)?;
        self.mls_commit_clock = clock;
        let original = Observer::load(&next.observer, &self.server_key).map_err(ds_error)?;
        let (mut observer, accepted) = Observer::load(&next.observer, &self.server_key)
            .map_err(ds_error)?
            .commit(&commit, &info, None, clock, next.committed_at)
            .map_err(ds_error)?;
        let s = &self.sessions[&session];
        let matches = match accepted.sender {
            CommitSender::Member(i) => next.leaves.get(&i).is_some_and(|l| {
                l.session == session
                    && l.participation == binding.participation
                    && l.device == s.hello_key
                    && l.uid == s.uid
            }),
            CommitSender::NewMember => {
                accepted.added.len() == 1
                    && accepted.added[0].device_key == s.hello_key
                    && accepted.added[0].uid == s.uid
            }
        };
        if !matches {
            return Err(err(ErrorCode::Forbidden, "voice commit sender binding"));
        }
        let proposals: Vec<_> = original
            .referenced_removals(&commit)
            .map_err(ds_error)?
            .iter()
            .map(|l| next.proposals.get(l).map(|p| URL_SAFE_NO_PAD.encode(p)).ok_or_else(|| bad("unknown proposal")))
            .collect::<Result<_, _>>()?;
        let mut evicted = Vec::new();
        for leaf in &accepted.removed {
            if let Some(l) = next.leaves.remove(&leaf.index) {
                evicted.push(l.session);
            }
        }
        for leaf in &accepted.added {
            next.leaves.insert(
                leaf.index,
                LeafBinding {
                    uid: s.uid.clone(),
                    device: s.hello_key.clone(),
                    session,
                    participation: binding.participation.clone(),
                },
            );
            evicted.retain(|id| *id != session);
        }
        next.proposals.clear();
        self.voice_missing(&mut observer, &next.leaves, &mut next.proposals)?;
        next.observer = bounded(self.voice_limits, &observer, &next.proposals)?;
        next.epoch = accepted.epoch;
        next.committed_at = accepted.committed_at;
        next.info = info;
        next.acks.clear();
        let entry = MlsLogEntry { epoch, proposals: proposals.clone(), commit: URL_SAFE_NO_PAD.encode(commit) };
        let size = serde_json::to_vec(&entry).expect("JSON").len() + 1;
        next.log.push_back((now_ms(), entry.clone(), size));
        prune(&mut next);
        self.mls_trees.remove(group);
        self.calls.get_mut(&channel).unwrap().group = Some(next);
        self.voice_dirty = true;
        self.broadcast(
            Event::Mls(MlsEvent::Commit { group: group.into(), epoch, proposals, commit: entry.commit }),
            |s| s.channel == Some(channel),
        );
        self.voice_announce(channel);
        for id in evicted {
            let _ = self.move_client(id, None);
        }
        Ok(Response::Mls(MlsResponse::Committed { epoch: accepted.epoch }))
    }
    fn voice_missing(
        &self,
        observer: &mut Observer,
        leaves: &BTreeMap<u32, LeafBinding>,
        proposals: &mut BTreeMap<u32, Vec<u8>>,
    ) -> Result<(), ErrorBody> {
        for leaf in observer.leaves() {
            if !leaves.contains_key(&leaf.index) && !proposals.contains_key(&leaf.index) {
                proposals.insert(
                    leaf.index,
                    observer
                        .propose_remove(&Ed25519Signer((*self.server_signer).clone()), leaf.index)
                        .map_err(ds_error)?,
                );
            }
        }
        Ok(())
    }
    fn voice_announce(&mut self, channel: ChannelId) {
        let Some(g) = self.calls.get(&channel).and_then(|c| c.group.as_ref()) else { return };
        let events: Vec<_> = g
            .proposals
            .values()
            .map(|p| {
                Event::Mls(MlsEvent::Proposal {
                    group: g.id.clone(),
                    epoch: g.epoch,
                    proposal: URL_SAFE_NO_PAD.encode(p),
                })
            })
            .collect();
        for event in events {
            self.broadcast(event, |s| s.channel == Some(channel));
        }
    }
    pub(super) fn voice_reconcile(&mut self) {
        let now = (self.voice_clock)();
        // Cache the intersection of certificate validity intervals. A rollback
        // below any issued_at must detach its participant just like expiry does.
        if !self.voice_dirty
            && self.voice_valid_from.is_none_or(|at| at <= now)
            && self.voice_expiry.is_none_or(|at| at > now)
        {
            return;
        }
        // No callbacks reconcile recursively: all detachments complete before
        // proposals are rebuilt, and logs are never copied here.
        #[cfg(test)]
        {
            self.voice_passes += 1;
        }
        let mut valid_from = None;
        let mut deadline = None;
        let channels: Vec<_> = self.calls.keys().copied().collect();
        for channel in channels {
            if self.calls.get(&channel).is_none_or(|c| c.parent.is_none()) {
                continue;
            }
            let parent = self.parent_observer(channel);
            if parent.as_ref().is_ok_and(|o| Some(URL_SAFE_NO_PAD.encode(o.group_id())) != self.calls[&channel].parent)
            {
                self.voice_close_call(channel, VoiceClosedReason::ChannelRestarted);
                continue;
            }
            // Parse/verify each parent credential once, then do indexed lookups.
            let members: BTreeMap<_, _> = parent
                .as_ref()
                .ok()
                .map(|o| o.credentials())
                .unwrap_or_default()
                .into_iter()
                .map(|(_, c)| ((c.uid(), c.device_key().to_owned()), c))
                .collect();
            let mut invalid = BTreeSet::new();
            for s in self.sessions.values().filter(|s| s.channel == Some(channel)) {
                let credential = members.get(&(s.uid.clone(), s.hello_key.clone()));
                if s.out.is_none()
                    || !self.mls_member(&s.uid).unwrap_or(false)
                    || self.store.mls_revoked(&s.hello_key, &s.uid).unwrap_or(true)
                    || credential.is_none_or(|c| !c.is_current(now, &()))
                {
                    invalid.insert(s.id);
                }
            }
            let mut observer = match self.calls.get(&channel).and_then(|c| c.group.as_ref()) {
                Some(g) => match Observer::load(&g.observer, &self.server_key) {
                    Ok(o) => Some(o),
                    Err(_) => {
                        self.voice_close_call(channel, VoiceClosedReason::StateLimit);
                        continue;
                    }
                },
                None => None,
            };
            let credentials = observer.as_ref().map(|o| o.credentials()).unwrap_or_default();
            if let Some(g) = self.calls.get(&channel).and_then(|c| c.group.as_ref()) {
                for (i, c) in &credentials {
                    if (!c.is_current(now, &()) || self.store.mls_revoked(c.device_key(), &c.uid()).unwrap_or(true))
                        && let Some(l) = g.leaves.get(i)
                    {
                        invalid.insert(l.session);
                    }
                }
            }
            for id in invalid {
                let _ = self.move_client(id, None);
            }
            // Detachment may end the call; never put an earlier candidate back.
            let Some(call) = self.calls.get(&channel) else { continue };
            let update = if let (Some(o), Some(g)) = (observer.as_mut(), call.group.as_ref()) {
                let mut proposals = g.proposals.clone();
                let result = self
                    .voice_missing(o, &g.leaves, &mut proposals)
                    .and_then(|()| bounded(self.voice_limits, o, &proposals));
                match result {
                    Ok(state) => Some((state, proposals, g.proposals.len())),
                    Err(_) => {
                        self.voice_close_call(channel, VoiceClosedReason::StateLimit);
                        continue;
                    }
                }
            } else {
                None
            };
            if let Some((state, proposals, before)) = update {
                let g = self.calls.get_mut(&channel).unwrap().group.as_mut().unwrap();
                g.observer = state;
                let changed = proposals.len() != before;
                g.proposals = proposals;
                if changed {
                    self.voice_announce(channel);
                }
            }
            for s in self.sessions.values().filter(|s| s.channel == Some(channel)) {
                if let Some(c) = members.get(&(s.uid.clone(), s.hello_key.clone())) {
                    include_validity(&mut valid_from, &mut deadline, c);
                }
            }
            if let Some(g) = self.calls.get(&channel).and_then(|c| c.group.as_ref()) {
                for (i, c) in &credentials {
                    if g.leaves.contains_key(i) {
                        include_validity(&mut valid_from, &mut deadline, c);
                    }
                }
            }
        }
        self.voice_valid_from = valid_from;
        self.voice_expiry = deadline;
        self.voice_dirty = false;
    }
    pub(super) fn voice_deadline(&self) -> Option<i64> {
        self.voice_expiry
    }
    pub(super) fn voice_clear_ack(&mut self, session: SessionId) {
        let s = &self.sessions[&session];
        if let Some(g) = s.channel.and_then(|c| self.calls.get_mut(&c)).and_then(|c| c.group.as_mut()) {
            g.acks.remove(&s.hello_key);
        }
        if let Some(p) = self.sessions.get_mut(&session).and_then(|s| s.participation.as_mut()) {
            p.answer = None;
            if let Some(task) = p.negotiation.take() {
                task.abort();
            }
        }
    }
    pub(super) fn voice_epoch(&mut self, session: SessionId, request: VoiceEpoch, repeat: bool) -> Reply {
        let channel = self.voice_check(
            session,
            &VoiceParticipation { call: request.call.clone(), participation: request.participation.clone() },
        )?;
        let s = &self.sessions[&session];
        let p = s.participation.as_ref().unwrap();
        let g = self.calls[&channel].group.as_ref().ok_or_else(|| bad("plaintext has no acknowledgements"))?;
        if gwar_proto::decode_fixed::<32>(&request.group).is_none()
            || request.epoch > gwar_proto::MAX_SAFE_INTEGER as u64
            || request.run == 0
            || request.run > gwar_proto::MAX_SAFE_INTEGER as u64
        {
            return Err(bad("invalid voice epoch binding"));
        }
        if p.binding.run != Some(request.run) || g.id != request.group || g.epoch != request.epoch {
            return Err(stale());
        }
        if !g
            .leaves
            .values()
            .any(|l| l.session == session && l.participation == request.participation && l.device == s.hello_key)
        {
            return Err(err(ErrorCode::Forbidden, "unmapped voice device"));
        }
        let device = s.hello_key.clone();
        let p = self.sessions.get_mut(&session).unwrap().participation.as_mut().unwrap();
        if repeat {
            p.repeat_bucket.take()?;
        } else {
            p.ack_bucket.take()?;
        }
        let call = self.calls.get_mut(&channel).unwrap();
        let event = if repeat {
            if call.repeated_at.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
                return Ok(Response::Empty {});
            }
            call.repeated_at = Some(Instant::now());
            call.repeat_round = call.repeat_round.wrapping_add(1);
            Event::VoiceEpochRequest { call: call.token.clone(), group: request.group, epoch: request.epoch }
        } else {
            if call.group.as_mut().unwrap().acks.insert(device.clone(), call.repeat_round) == Some(call.repeat_round) {
                return Ok(Response::Empty {});
            }
            Event::VoiceEpochAck { call: call.token.clone(), group: request.group, epoch: request.epoch, device }
        };
        self.broadcast(event, |s| s.channel == Some(channel));
        Ok(Response::Empty {})
    }
}
fn prune(g: &mut VoiceGroup) {
    let mut bytes: usize = g.log.iter().map(|(_, _, n)| n).sum();
    while g.log.len() > LOG_ENTRIES
        || bytes > LOG_BYTES
        || g.log.front().is_some_and(|(t, _, _)| *t < now_ms() - RETENTION_MS)
    {
        if let Some((_, _, n)) = g.log.pop_front() {
            bytes -= n;
        } else {
            break;
        }
    }
}

/// Exactly one microphone media section and one nonzero SSRC. Repeated cname
/// and msid attributes for that same SSRC are normal SDP, not extra streams.
fn microphone_ssrc(sdp: &str) -> Option<u32> {
    let mut sections: Vec<Vec<&str>> = Vec::new();
    for line in sdp.lines().map(str::trim) {
        if line.starts_with("m=") {
            sections.push(vec![line]);
        } else if let Some(section) = sections.last_mut() {
            section.push(line);
        }
    }
    let microphones: Vec<_> = sections
        .iter()
        .filter(|s| s[0].starts_with("m=audio ") && s.iter().any(|l| matches!(*l, "a=sendonly" | "a=sendrecv")))
        .collect();
    if microphones.len() != 1 {
        return None;
    }
    let sources: Option<BTreeSet<u32>> = microphones[0]
        .iter()
        .filter_map(|l| l.strip_prefix("a=ssrc:"))
        .map(|s| s.split_whitespace().next()?.parse().ok())
        .collect();
    let sources = sources?;
    (sources.len() == 1).then(|| *sources.first().unwrap()).filter(|s| *s != 0)
}
impl Core {
    pub(super) fn voice_offer(&mut self, session: SessionId, id: u32, offer: VoiceOffer) -> Option<Reply> {
        let checked = (|| {
            let channel = self.voice_check(
                session,
                &VoiceParticipation { call: offer.call.clone(), participation: offer.participation.clone() },
            )?;
            if channel != offer.channel || offer.run == 0 || offer.run > gwar_proto::MAX_SAFE_INTEGER as u64 {
                return Err(bad("voice offer binding"));
            }
            if offer.sdp.len() > 64 * 1024 {
                return Err(err(ErrorCode::TooLarge, "SDP too large"));
            }
            if offer.ssrc == 0 || microphone_ssrc(&offer.sdp) != Some(offer.ssrc) {
                return Err(bad("offer must name exactly one microphone SSRC"));
            }
            let c = &self.calls[&channel];
            let s = &self.sessions[&session];
            if s.out.is_none() {
                return Err(bad("remote session has no WebRTC transport"));
            }
            if c.parent.is_some() {
                if c.group.as_ref().is_none_or(|g| Some(&g.id) != offer.group.as_ref()) {
                    return Err(bad("voice group mismatch"));
                }
                if c.group.as_ref().is_none_or(|g| {
                    !g.leaves.values().any(|l| l.session == session && l.participation == offer.participation)
                }) {
                    return Err(err(ErrorCode::Forbidden, "unmapped voice device"));
                }
            } else if offer.group.is_some() {
                return Err(bad("plaintext offer has a group"));
            }
            let p = s.participation.as_ref().unwrap();
            if let Some(previous) = &p.offer {
                if offer.run < previous.run {
                    return Err(stale());
                }
                if offer.run == previous.run {
                    if p.binding.run != Some(offer.run) {
                        return Err(stale());
                    }
                    if previous != &offer {
                        return Err(err(ErrorCode::Conflict, "run reused with different offer"));
                    }
                    return p
                        .answer
                        .clone()
                        .map(|sdp| Some(Response::Answer { binding: offer.binding(), sdp }))
                        .ok_or_else(|| err(ErrorCode::Unavailable, "negotiation pending or failed; start a new run"));
                }
            }
            if s.used_ssrcs.contains(&offer.ssrc) {
                return Err(err(ErrorCode::Conflict, "ssrc_reused"));
            }
            // Replacement retires the active run too. Reserve room for the new
            // SSRC to retire on failure, stop or move; never retain a 33rd.
            if s.used_ssrcs.len() >= 32 {
                return Err(err(ErrorCode::Unavailable, "stream_limit"));
            }
            Ok(None)
        })();
        match checked {
            Err(e) => {
                if e.message == "stream_limit" {
                    let binding = self.sessions[&session].participation.as_ref().unwrap().binding.clone();
                    self.broadcast(Event::VoiceClosed { binding, reason: VoiceClosedReason::StreamLimit }, |s| {
                        s.id == session
                    });
                    let _ = self.move_client(session, None);
                }
                return Some(Err(e));
            }
            Ok(Some(r)) => return Some(Ok(r)),
            Ok(None) => {}
        }
        // Reserve command capacity before replacing any state. A greater run
        // always closes the old peer, even if the new SDP cannot negotiate.
        let media = self.media.clone();
        let Ok(permit) = media.try_reserve() else {
            return Some(Err(err(ErrorCode::Unavailable, "media engine busy")));
        };
        let binding = offer.binding();
        let s = self.sessions.get_mut(&session).unwrap();
        s.used_ssrcs.insert(offer.ssrc);
        s.talking = false;
        s.voice = false;
        let p = s.participation.as_mut().unwrap();
        p.binding.run = Some(offer.run);
        p.offer = Some(offer.clone());
        p.answer = None;
        if let Some(task) = p.negotiation.take() {
            task.abort();
        }
        let out = s.out.clone().unwrap();
        let channel = s.channel.unwrap();
        if let Some(g) = self.calls.get_mut(&channel).and_then(|c| c.group.as_mut()) {
            g.acks.remove(&s.hello_key);
        }
        self.publish_routing();
        self.client_updated(session);
        let (reply, rx) = oneshot::channel();
        let origin = self.routing.load().origin(session).expect("admitted participation");
        let retired = self.sessions[&session].used_ssrcs.iter().copied().filter(|ssrc| *ssrc != offer.ssrc).collect();
        permit.send(MediaCmd::Offer {
            session,
            binding: binding.clone(),
            origin,
            ssrc: offer.ssrc,
            retired,
            sdp: offer.sdp,
            out,
            reply,
        });
        let tx = self.me.upgrade()?;
        let task = tokio::spawn(async move {
            let answer = tokio::time::timeout(Duration::from_secs(30), rx)
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(Err("media negotiation stopped"));
            let resume = move |core: &mut Core| {
                let Some(p) = core.sessions.get_mut(&session).and_then(|s| s.participation.as_mut()) else {
                    return;
                };
                if p.binding != VoiceBinding::from(&binding) {
                    return;
                }
                p.negotiation = None;
                let answer =
                    answer.and_then(|sdp| if sdp.len() <= 64 * 1024 { Ok(sdp) } else { Err("answer too large") });
                let result = match answer {
                    Ok(sdp) => {
                        p.answer = Some(sdp.clone());
                        Ok(Response::Answer { binding, sdp })
                    }
                    Err(message) => {
                        core.handle(CoreMsg::Voice { session, connected: false, binding, failed: true });
                        Err(bad(message))
                    }
                };
                core.reply(session, id, result);
            };
            let _ = tx.send(CoreMsg::Resume(Box::new(resume))).await;
        });
        self.sessions.get_mut(&session).unwrap().participation.as_mut().unwrap().negotiation =
            Some(task.abort_handle());
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use gwar_mls::{Agent, Credential, Descriptor, SignedDescriptor};
    fn fixture() -> (Core, SessionId, mpsc::Receiver<Outbound>, mpsc::Receiver<MediaCmd>) {
        let store = Store::in_memory().unwrap();
        let signer = SigningKey::from_bytes(&[99; 32]);
        let (media, commands) = mpsc::channel(16);
        let (_tx, rx) = mpsc::channel(16);
        let mut core = Core::new(
            store,
            CoreConfig {
                max_clients: 10,
                ice_servers: vec![],
                version: "test".into(),
                password: Arc::new(ArcSwap::from_pointee(None)),
                password_plaintext: None,
                upload_limit: 0,
                upload_quota: 0,
                storage_limit: 0,
                files_dir: PathBuf::new(),
                connect_url: None,
                server_key: URL_SAFE_NO_PAD.encode(signer.verifying_key().as_bytes()),
                server_signer: Arc::new(signer),
                teamspeak: false,
                watch_base: None,
            },
            Arc::new(ArcSwap::from_pointee(Routing::default())),
            media,
            mpsc::channel(8).0,
            _tx.downgrade(),
        )
        .unwrap();
        drop(rx);
        let key = SigningKey::from_bytes(&[11; 32]);
        let public = URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes());
        let uid = gwar_proto::uid_for_key(key.verifying_key().as_bytes());
        let (out, frames) = mpsc::channel(OUTBOUND_QUEUE);
        let session = core
            .connect(ConnectRequest {
                request_id: 1,
                hello_key: public.clone(),
                certificate: None,
                uid,
                public_key: public,
                nickname: "voice".into(),
                platform: Platform::Web,
                ip: None,
                device: None,
                device_expires_at: None,
                password_ok: true,
                invite: None,
                out,
            })
            .unwrap();
        (core, session, frames, commands)
    }
    fn encrypted(core: &mut Core, session: SessionId) -> (ChannelId, String) {
        let (channel, group, _, _) = encrypted_agents(core, session, None);
        (channel, group)
    }
    fn encrypted_agents(
        core: &mut Core,
        session: SessionId,
        expires: Option<i64>,
    ) -> (ChannelId, String, Agent, Agent) {
        let channel = core
            .store
            .create_channel(&ChannelRow {
                id: 0,
                parent: None,
                name: "encrypted".into(),
                topic: String::new(),
                position: 1,
                password_hash: None,
                max_clients: None,
                encrypted: true,
                history: true,
                private: false,
                access: None,
                overrides: Vec::new(),
                synced: false,
            })
            .unwrap();
        core.channels = core.store.channels().unwrap().into_iter().map(|c| (c.id, c)).collect();
        let key = SigningKey::from_bytes(&[11; 32]);
        let credential = test_credential(11, expires);
        let descriptor = |kind, conversation, parent, nonce| {
            SignedDescriptor::sign(
                Descriptor {
                    server_key: core.server_key.clone(),
                    conversation,
                    kind,
                    incarnation: 0,
                    previous_group_id: None,
                    private: false,
                    history: kind == Kind::Channel,
                    founding_owner: None,
                    channel_group_id: parent,
                    creator_uid: core.sessions[&session].uid.clone(),
                    created_at: now_ms(),
                    creation_nonce: URL_SAFE_NO_PAD.encode([nonce; 16]),
                },
                credential.clone(),
                &key,
            )
        };
        let parent = descriptor(Kind::Channel, format!("channel:{}", channel), None, 1);
        let mut agent = Agent::new([11; 32], credential.clone(), now_ms()).unwrap();
        agent.take_changes();
        let info = agent.create(parent.clone()).unwrap();
        core.mls_request(
            session,
            gwar_proto::MlsRequest::GroupCreate {
                conversation: parent.descriptor.conversation.clone(),
                group_info: URL_SAFE_NO_PAD.encode(info),
                voice: None,
            },
        )
        .unwrap();
        agent.take_changes();
        agent.settle(&core.server_key, &parent.group_id(), Some(&parent.group_id())).unwrap();
        agent.take_changes();
        let parent_agent = agent;
        core.enter_channel(session, channel).unwrap();
        let voice = SignedDescriptor::sign(
            Descriptor {
                server_key: core.server_key.clone(),
                conversation: format!("voice:{}", channel),
                kind: Kind::Voice,
                incarnation: 0,
                previous_group_id: None,
                private: false,
                history: false,
                founding_owner: None,
                channel_group_id: Some(URL_SAFE_NO_PAD.encode(parent.group_id())),
                creator_uid: core.sessions[&session].uid.clone(),
                created_at: now_ms(),
                creation_nonce: URL_SAFE_NO_PAD.encode([2; 16]),
            },
            credential.clone(),
            &key,
        );
        let mut agent = Agent::new_voice([11; 32], credential, now_ms()).unwrap();
        agent.take_changes();
        let info = agent.create(voice).unwrap();
        let state = core.voice_state(session).unwrap();
        let reply = core
            .voice_create(
                session,
                &format!("voice:{}", channel),
                &URL_SAFE_NO_PAD.encode(info),
                VoiceParticipation { call: state.call, participation: state.participation },
            )
            .unwrap();
        let Response::Mls(MlsResponse::Created { group }) = reply else { panic!() };
        agent.take_changes();
        let id = gwar_proto::decode_fixed::<32>(&group).unwrap();
        agent.settle(&core.server_key, &id, Some(&id)).unwrap();
        agent.take_changes();
        (channel, group, parent_agent, agent)
    }
    fn test_credential(seed: u8, expires: Option<i64>) -> Credential {
        use ed25519_dalek::Signer;
        let signer = SigningKey::from_bytes(&[seed; 32]);
        let public = URL_SAFE_NO_PAD.encode(signer.verifying_key().as_bytes());
        match expires {
            None => Credential::Local { device_key: public },
            Some(expires_at) => {
                let mut c = gwar_proto::DeviceCertificate {
                    account_key: public.clone(),
                    device_key: public,
                    issued_at: expires_at - 86_400_000,
                    expires_at,
                    signature: String::new(),
                };
                c.signature =
                    URL_SAFE_NO_PAD.encode(signer.sign(gwar_proto::device_statement(&c).as_bytes()).to_bytes());
                Credential::Account(c)
            }
        }
    }
    fn settle_commit(agent: &mut Agent, server: &str, group: &str, out: &gwar_mls::agent::Outbox) {
        let id = gwar_proto::decode_fixed::<32>(group).unwrap();
        agent.take_changes();
        agent
            .reconcile(
                server,
                &id,
                gwar_mls::agent::Reconcile::Entry { epoch: out.epoch, commit: out.commit.clone(), proposals: vec![] },
            )
            .unwrap();
        agent.take_changes();
    }
    struct TestCall {
        core: Core,
        channel: ChannelId,
        group: String,
        sessions: Vec<SessionId>,
        frames: Vec<mpsc::Receiver<Outbound>>,
        voices: Vec<Agent>,
        parent: Agent,
        _commands: mpsc::Receiver<MediaCmd>,
    }
    impl TestCall {
        fn new() -> Self {
            let (mut core, session, frames, commands) = fixture();
            core.info.max_clients = 1000;
            let (channel, group, parent, voice) = encrypted_agents(&mut core, session, None);
            Self {
                core,
                channel,
                group,
                sessions: vec![session],
                frames: vec![frames],
                voices: vec![voice],
                parent,
                _commands: commands,
            }
        }
        fn add(&mut self, parent_expiry: Option<i64>, voice_expiry: Option<i64>, enter: bool, join: bool) {
            let seed = 11 + self.sessions.len() as u8;
            let credential = test_credential(seed, parent_expiry);
            let (out, frames) = mpsc::channel(16384);
            let session = self
                .core
                .connect(ConnectRequest {
                    request_id: 1,
                    hello_key: credential.device_key().into(),
                    certificate: None,
                    uid: credential.uid(),
                    public_key: credential.device_key().into(),
                    nickname: format!("device-{seed}"),
                    platform: Platform::Web,
                    ip: None,
                    device: None,
                    device_expires_at: None,
                    password_ok: true,
                    invite: None,
                    out,
                })
                .unwrap();
            let parent = self.core.store.mls_live_group(&format!("channel:{}", self.channel)).unwrap().unwrap();
            let tree = Observer::load(&parent.observer, &self.core.server_key).unwrap().tree().unwrap().1;
            let mut agent = Agent::new([seed; 32], credential, now_ms()).unwrap();
            agent.take_changes();
            let out =
                agent.join_external(&self.core.server_key, &parent.group_info, Some(&tree), now_ms(), &()).unwrap();
            self.parent
                .receive(
                    &self.core.server_key,
                    &gwar_proto::decode_fixed::<32>(&parent.group).unwrap(),
                    &out.commit,
                    &[],
                )
                .unwrap();
            self.parent.take_changes();
            self.core
                .mls_request(
                    session,
                    gwar_proto::MlsRequest::Commit {
                        group: parent.group,
                        epoch: out.epoch,
                        commit: URL_SAFE_NO_PAD.encode(out.commit),
                        group_info: URL_SAFE_NO_PAD.encode(out.group_info),
                        welcome: None,
                        voice: None,
                    },
                )
                .unwrap();
            self.sessions.push(session);
            self.frames.push(frames);
            let mut voice = Agent::new_voice([seed; 32], test_credential(seed, voice_expiry), now_ms()).unwrap();
            voice.take_changes();
            self.voices.push(voice);
            if enter {
                self.core.enter_channel(session, self.channel).unwrap();
            }
            if join {
                self.join(self.sessions.len() - 1);
            }
            self.drain();
        }
        fn binding(&self, index: usize) -> VoiceParticipation {
            let state = self.core.voice_state(self.sessions[index]).unwrap();
            VoiceParticipation { call: state.call, participation: state.participation }
        }
        fn join(&mut self, index: usize) {
            let g = self.core.calls[&self.channel].group.as_ref().unwrap();
            let tree = Observer::load(&g.observer, &self.core.server_key).unwrap().tree().unwrap().1;
            let out =
                self.voices[index].join_external(&self.core.server_key, &g.info, Some(&tree), now_ms(), &()).unwrap();
            self.submit(index, &out).unwrap();
            settle_commit(&mut self.voices[index], &self.core.server_key, &self.group, &out);
            let id = gwar_proto::decode_fixed::<32>(&self.group).unwrap();
            self.voices[0].receive(&self.core.server_key, &id, &out.commit, &[]).unwrap();
            self.voices[0].take_changes();
        }
        fn submit(&mut self, index: usize, out: &gwar_mls::agent::Outbox) -> Reply {
            self.core.voice_commit(
                self.sessions[index],
                &self.group,
                out.epoch,
                &URL_SAFE_NO_PAD.encode(&out.commit),
                &URL_SAFE_NO_PAD.encode(&out.group_info),
                self.binding(index),
            )
        }
        fn drain(&mut self) {
            for frames in &mut self.frames {
                while frames.try_recv().is_ok() {}
            }
        }
        fn no_events(&mut self) {
            for frames in &mut self.frames {
                assert!(frames.try_recv().is_err());
            }
        }
        fn snapshot(&self) -> serde_json::Value {
            let g = self.core.calls[&self.channel].group.as_ref().unwrap();
            serde_json::json!({ "observer": g.observer, "info": g.info, "epoch": g.epoch,
                "committed_at": g.committed_at, "proposals": g.proposals, "log": g.log, "acks": g.acks,
                "leaves": g.leaves.iter().map(|(i,l)| (i, &l.uid, &l.device, l.session, &l.participation)).collect::<Vec<_>>(),
                "states": self.sessions.iter().map(|s| self.core.voice_state(*s)).collect::<Vec<_>>() })
        }
    }
    #[test]
    fn recovery_log_reissues_detached_removals_and_releases_rejoined_leaves() {
        use gwar_mls::agent::{AgentError, Reconcile, Resolution};
        for rejoined in [false, true] {
            let mut f = TestCall::new();
            f.add(None, None, true, true);
            f.add(None, None, true, true);
            let server = f.core.server_key.clone();
            let id = gwar_proto::decode_fixed::<32>(&f.group).unwrap();
            let target = f.core.calls[&f.channel]
                .group
                .as_ref()
                .unwrap()
                .leaves
                .iter()
                .find(|(_, l)| l.session == f.sessions[1])
                .map(|(index, _)| *index)
                .unwrap();
            f.core.move_client(f.sessions[1], None).unwrap();
            f.core.voice_reconcile();
            let old_proposal = f.core.calls[&f.channel].group.as_ref().unwrap().proposals[&target].clone();
            f.voices[0].proposal(&server, &id, &old_proposal).unwrap();
            f.voices[0].take_changes();
            let pending = f.voices[0].commit(&server, &id, &[], &[], now_ms(), &()).unwrap();
            f.voices[0].take_changes();
            // C wins before A can commit the removal. A misses that commit.
            let update = f.voices[2].commit(&server, &id, &[], &[], now_ms(), &()).unwrap();
            f.submit(2, &update).unwrap();
            settle_commit(&mut f.voices[2], &server, &f.group, &update);
            if rejoined {
                f.core.enter_channel(f.sessions[1], f.channel).unwrap();
                let g = f.core.calls[&f.channel].group.as_ref().unwrap();
                let tree = Observer::load(&g.observer, &server).unwrap().tree().unwrap().1;
                let join = f.voices[1].join_external(&server, &g.info, Some(&tree), now_ms(), &()).unwrap();
                f.submit(1, &join).unwrap();
                settle_commit(&mut f.voices[1], &server, &f.group, &join);
                let g = f.core.calls[&f.channel].group.as_ref().unwrap();
                assert_eq!(g.leaves[&target].session, f.sessions[1]);
                assert!(g.proposals.is_empty());
            }
            // Force a genuine log gap, retaining the current pending set.
            f.core.calls.get_mut(&f.channel).unwrap().group.as_mut().unwrap().log.clear();
            let Response::Mls(MlsResponse::Log { oldest, .. }) =
                f.core.voice_log(f.sessions[0], f.channel, pending.epoch, None).unwrap()
            else {
                panic!()
            };
            assert_eq!(f.voices[0].reconcile(&server, &id, Reconcile::Pruned { oldest }).unwrap(), Resolution::Rejoin);
            f.voices[0].take_changes();
            let g = f.core.calls[&f.channel].group.as_ref().unwrap();
            let tree = Observer::load(&g.observer, &server).unwrap().tree().unwrap().1;
            let recovery = f.voices[0].join_external(&server, &g.info, Some(&tree), now_ms(), &()).unwrap();
            f.voices[0].take_changes();
            f.submit(0, &recovery).unwrap();
            settle_commit(&mut f.voices[0], &server, &f.group, &recovery);
            assert_eq!(f.voices[0].voice_keys(&server, &id, &f.parent, now_ms(), &()), Err(AgentError::Blocked));
            let Response::Mls(MlsResponse::Log { head, pending, .. }) =
                f.core.voice_log(f.sessions[0], f.channel, recovery.epoch, None).unwrap()
            else {
                panic!()
            };
            assert_eq!(pending.len(), usize::from(!rejoined));
            let pending: Vec<_> = pending.iter().map(|p| URL_SAFE_NO_PAD.decode(p).unwrap()).collect();
            if !rejoined {
                assert_ne!(pending[0], old_proposal, "the server reissues Remove at the recovery epoch");
            }
            f.voices[0].voice_pending(&server, &id, head, &pending).unwrap();
            f.voices[0].take_changes();
            if rejoined {
                assert_eq!(f.voices[0].voice_keys(&server, &id, &f.parent, now_ms(), &()).unwrap().members.len(), 3);
            } else {
                assert_eq!(f.voices[0].voice_keys(&server, &id, &f.parent, now_ms(), &()), Err(AgentError::Blocked));
                let remove = f.voices[0].commit(&server, &id, &[], &[], now_ms(), &()).unwrap();
                f.submit(0, &remove).unwrap();
                settle_commit(&mut f.voices[0], &server, &f.group, &remove);
                assert_eq!(f.voices[0].voice_keys(&server, &id, &f.parent, now_ms(), &()).unwrap().members.len(), 2);
            }
        }
    }

    thread_local! { static VOICE_TIME: std::cell::Cell<i64> = const { std::cell::Cell::new(0) }; }
    fn test_clock() -> i64 {
        VOICE_TIME.get()
    }

    #[test]
    fn backdated_fresh_certificates_admit_voice_across_clock_skew() {
        for skew in [-300_000, -180_000, -2_600, 2_600, 180_000, 300_000] {
            let mut f = TestCall::new();
            let server_now = now_ms();
            // Same issued_at as the shared web/desktop certificate issuer.
            let issued_at = server_now + skew - 600_000;
            let expiry = issued_at + 86_400_000;
            f.core.voice_clock = test_clock;
            VOICE_TIME.set(server_now - skew);
            f.add(Some(expiry), Some(expiry), true, true);
            let session = f.sessions[1];
            assert!(f.core.voice_state(session).is_some());
            f.core.voice_reconcile();
            assert!(f.core.voice_state(session).is_some());
            // Admission and the cached voice gate still enforce the signed
            // interval, including rollback just before issuance and exact expiry.
            VOICE_TIME.set(issued_at - 1);
            assert_eq!(f.core.voice_admission(session, f.channel).err().unwrap().code, ErrorCode::Forbidden);
            f.core.voice_reconcile();
            assert!(f.core.voice_state(session).is_none());
            VOICE_TIME.set(expiry);
            assert_eq!(f.core.voice_admission(session, f.channel).err().unwrap().code, ErrorCode::Forbidden);
        }
    }

    #[test]
    fn simultaneous_voice_expiry_batches_detachments_and_reuses_parent_view() {
        let mut f = TestCall::new();
        let expiry = now_ms() + 3_600_000;
        for _ in 0..32 {
            f.add(None, Some(expiry), true, true);
        }
        f.core.voice_clock = test_clock;
        VOICE_TIME.set(expiry - 1);
        f.core.voice_reconcile();
        assert_eq!(f.core.voice_deadline(), Some(expiry));
        let passes = f.core.voice_passes;
        let loads = f.core.voice_parent_loads.get();
        let g = f.core.calls[&f.channel].group.as_ref().unwrap();
        let log_pointer = g.log.front().unwrap().1.commit.as_ptr();
        // Unrelated actor traffic and deadline reads never reload the trees.
        for _ in 0..8 {
            f.core.handle(CoreMsg::Talking { session: f.sessions[0], talking: false, binding: None });
            assert_eq!(f.core.voice_deadline(), Some(expiry));
        }
        assert_eq!(f.core.voice_passes, passes);
        assert_eq!(f.core.voice_parent_loads.get(), loads);
        assert!(
            f.core
                .mls_request(
                    f.sessions[0],
                    gwar_proto::MlsRequest::GroupCreate {
                        conversation: format!("channel:{}", f.channel),
                        group_info: String::new(),
                        voice: None,
                    }
                )
                .is_err()
        );
        f.core.voice_reconcile();
        assert_eq!(f.core.voice_passes, passes, "rejected requests do not invalidate call membership");
        assert_eq!(f.core.voice_parent_loads.get(), loads);
        VOICE_TIME.set(expiry);
        f.core.voice_reconcile();
        assert_eq!(f.core.voice_passes, passes + 1, "no recursive reconciliation per expired leaf");
        assert_eq!(f.core.voice_parent_loads.get(), loads + 1, "one parent load for the entire call");
        let g = f.core.calls[&f.channel].group.as_ref().unwrap();
        assert_eq!(g.log.front().unwrap().1.commit.as_ptr(), log_pointer, "no log clone during reconciliation");
        assert_eq!(g.leaves.len(), 1);
        assert_eq!(g.proposals.len(), 32);
        assert!(f.sessions.iter().skip(1).all(|s| f.core.voice_state(*s).is_none()));
        assert_eq!(f.core.voice_deadline(), None);
        f.core.voice_reconcile();
        assert_eq!(f.core.voice_passes, passes + 1);
    }

    #[test]
    fn clock_rollback_before_parent_or_voice_certificate_issuance_detaches_participant() {
        for parent_certificate in [false, true] {
            let mut f = TestCall::new();
            let expiry = now_ms() + 3_600_000;
            let issued_at = expiry - 86_400_000;
            f.add(parent_certificate.then_some(expiry), (!parent_certificate).then_some(expiry), true, true);
            let binding = f.binding(1);
            f.core.voice_clock = test_clock;
            VOICE_TIME.set(issued_at);
            f.core.voice_reconcile();
            assert_eq!(f.core.voice_deadline(), Some(expiry));
            assert_eq!(f.core.voice_check(f.sessions[1], &binding).unwrap(), f.channel);
            assert!(!f.core.voice_dirty);
            let passes = f.core.voice_passes;
            VOICE_TIME.set(issued_at - 1);
            // Ordinary actor traffic must invalidate a previously clean cache.
            f.core.handle(CoreMsg::Talking { session: f.sessions[0], talking: false, binding: None });
            assert_eq!(f.core.voice_passes, passes + 1);
            assert!(f.core.voice_state(f.sessions[1]).is_none());
            assert_eq!(f.core.voice_check(f.sessions[1], &binding).unwrap_err().code, ErrorCode::Stale);
            let group = f.core.calls[&f.channel].group.as_ref().unwrap();
            assert_eq!(group.leaves.len(), 1);
            assert_eq!(group.proposals.len(), 1);
            assert_eq!(f.core.voice_deadline(), None);
        }
    }

    fn advancing_clock() -> i64 {
        let now = VOICE_TIME.get();
        VOICE_TIME.set(now + 1);
        now
    }

    #[test]
    fn admission_consumes_validation_before_clock_expiry_and_rejects_before_detach() {
        let mut f = TestCall::new();
        let expiry = now_ms() + 3_600_000;
        f.add(Some(expiry), None, false, false);
        let session = f.sessions[1];
        f.core.enter_channel(session, 1).unwrap();
        f.core.voice_clock = test_clock;
        VOICE_TIME.set(expiry - 1);
        // Time crosses expiry immediately after the admission check, before allocation.
        f.core.voice_clock = advancing_clock;
        f.core.enter_channel(session, f.channel).unwrap();
        assert_eq!(VOICE_TIME.get(), expiry, "exactly one admission clock read");
        f.core.voice_clock = test_clock;
        assert_eq!(f.core.voice_state(session).unwrap().channel, f.channel);
        f.core.voice_reconcile();
        assert!(f.core.voice_state(session).is_none());
        f.core.enter_channel(session, 1).unwrap();
        let old = f.core.voice_state(session).unwrap();
        f.drain();
        assert_eq!(f.core.enter_channel(session, f.channel).unwrap_err().code, ErrorCode::Forbidden);
        assert_eq!(f.core.voice_state(session).unwrap(), old);
        f.no_events();
    }

    #[test]
    fn admission_handlers_count_detached_leaves_and_pending_reservations_atomically() {
        let mut f = TestCall::new();
        f.add(None, None, true, true);
        f.add(None, None, false, false);
        f.core.voice_limits.members = 2;
        f.core.move_client(f.sessions[1], None).unwrap();
        f.core.voice_reconcile();
        f.core.enter_channel(f.sessions[2], 1).unwrap();
        let old = f.core.voice_state(f.sessions[2]).unwrap();
        let before = f.snapshot();
        f.drain();
        assert_eq!(f.core.enter_channel(f.sessions[2], f.channel).unwrap_err().code, ErrorCode::ChannelFull);
        assert_eq!(f.snapshot(), before);
        assert_eq!(f.core.voice_state(f.sessions[2]).unwrap(), old);
        f.no_events();
        // Replacing one's own detached leaf consumes no extra leaf slot.
        f.core.enter_channel(f.sessions[1], f.channel).unwrap();
        assert!(f.core.voice_state(f.sessions[1]).is_some());
        f.core.move_client(f.sessions[1], None).unwrap();
        f.add(None, None, false, false);
        f.core.voice_limits.members = 3;
        // Two occupied leaves (one detached) plus a pending admission fill the
        // leaf budget, while only two sessions occupy this three-session call.
        f.core.enter_channel(f.sessions[2], f.channel).unwrap();
        let group = f.core.calls[&f.channel].group.as_ref().unwrap();
        assert_eq!(Observer::load(&group.observer, &f.core.server_key).unwrap().leaves().len(), 2);
        assert_eq!(group.leaves.len(), 1);
        assert_eq!(f.core.sessions.values().filter(|s| s.channel == Some(f.channel)).count(), 2);
        assert!(f.core.voice_state(f.sessions[3]).is_none());
        let before = f.snapshot();
        f.drain();
        assert_eq!(f.core.enter_channel(f.sessions[3], f.channel).unwrap_err().code, ErrorCode::ChannelFull);
        assert_eq!(f.snapshot(), before);
        f.no_events();
    }

    #[test]
    fn commit_handlers_enforce_each_state_limit_without_partial_acceptance() {
        let mut f = TestCall::new();
        f.add(None, None, true, true);
        f.core.move_client(f.sessions[1], None).unwrap();
        f.core.voice_reconcile();
        let id = gwar_proto::decode_fixed::<32>(&f.group).unwrap();
        // A valid competing update must reissue the detached leaf's removal.
        let out = f.voices[0].commit(&f.core.server_key, &id, &[], &[], now_ms(), &()).unwrap();
        f.voices[0].take_changes();
        let g = f.core.calls[&f.channel].group.as_ref().unwrap();
        let (mut candidate, _) = Observer::load(&g.observer, &f.core.server_key)
            .unwrap()
            .commit(&out.commit, &out.group_info, None, now_ms(), g.committed_at)
            .unwrap();
        let mut proposals = BTreeMap::new();
        f.core.voice_missing(&mut candidate, &g.leaves, &mut proposals).unwrap();
        let exact = Limits {
            members: candidate.leaves().len(),
            observer_bytes: candidate.save().len(),
            tree_bytes: candidate.tree().unwrap().1.len(),
            proposal_count: proposals.len(),
            proposal_bytes: proposals.values().map(Vec::len).sum(),
        };
        let device = f.core.sessions[&f.sessions[0]].hello_key.clone();
        f.core.calls.get_mut(&f.channel).unwrap().group.as_mut().unwrap().acks.insert(device, 1);
        let before = f.snapshot();
        f.drain();
        let mut limits = [exact; 5];
        limits[0].observer_bytes -= 1;
        limits[1].tree_bytes -= 1;
        limits[2].proposal_count -= 1;
        limits[3].proposal_bytes -= 1;
        limits[4].members -= 1;
        for (i, limit) in limits.into_iter().enumerate() {
            f.core.voice_limits = limit;
            assert_eq!(
                f.submit(0, &out).unwrap_err().code,
                if i == 4 { ErrorCode::ChannelFull } else { ErrorCode::TooLarge }
            );
            assert_eq!(f.snapshot(), before, "limit {i} must preserve all call state");
            f.no_events();
        }
        f.core.voice_limits = exact;
        f.submit(0, &out).unwrap();
        let g = f.core.calls[&f.channel].group.as_ref().unwrap();
        assert_eq!(g.epoch, out.epoch + 1);
        assert!(g.acks.is_empty());
        assert_eq!(g.proposals.len(), 1);
    }

    #[test]
    fn acknowledgements_are_bound_rate_limited_and_coalesced() {
        let (mut core, session, mut frames, _commands) = fixture();
        let (channel, group) = encrypted(&mut core, session);
        let p = core.sessions.get_mut(&session).unwrap().participation.as_mut().unwrap();
        p.binding.run = Some(1);
        // Exercise burst limits without CPU contention refilling tokens mid-test.
        p.ack_bucket.rate = 0.0;
        p.repeat_bucket.rate = 0.0;
        while frames.try_recv().is_ok() {}
        let state = core.voice_state(session).unwrap();
        let request = VoiceEpoch { call: state.call, participation: state.participation, run: 1, group, epoch: 0 };
        core.voice_epoch(session, request.clone(), false).unwrap();
        let first = frames.try_recv().unwrap();
        assert!(matches!(first,Outbound::Frame(f) if f.contains("voice.epoch.ack")));
        core.voice_epoch(session, request.clone(), false).unwrap();
        assert!(frames.try_recv().is_err());
        core.voice_epoch(session, request.clone(), true).unwrap();
        assert!(frames.try_recv().is_ok());
        core.voice_epoch(session, request.clone(), true).unwrap();
        assert!(frames.try_recv().is_err());
        core.voice_epoch(session, request.clone(), false).unwrap();
        assert!(frames.try_recv().is_ok());
        for mut wrong in [request.clone(), request.clone(), request.clone()] {
            wrong.epoch += 1;
            assert_eq!(core.voice_epoch(session, wrong, false).unwrap_err().code, ErrorCode::Stale);
        }
        let mut wrong = request.clone();
        wrong.run = 2;
        assert_eq!(core.voice_epoch(session, wrong, false).unwrap_err().code, ErrorCode::Stale);
        let mut wrong = request.clone();
        wrong.participation = token();
        assert_eq!(core.voice_epoch(session, wrong, false).unwrap_err().code, ErrorCode::Stale);
        for _ in 0..17 {
            core.voice_epoch(session, request.clone(), false).unwrap();
        }
        assert_eq!(core.voice_epoch(session, request.clone(), false).unwrap_err().code, ErrorCode::RateLimited);
        for _ in 0..8 {
            core.voice_epoch(session, request.clone(), true).unwrap();
        }
        assert_eq!(core.voice_epoch(session, request, true).unwrap_err().code, ErrorCode::RateLimited);
        assert_eq!(core.calls[&channel].group.as_ref().unwrap().acks.len(), 1);
        core.move_client(session, None).unwrap();
        assert!(core.calls.is_empty());
        assert!(core.routing.load().members.is_empty());
    }
    #[test]
    fn logs_prune_whole_entries_by_count_bytes_and_age() {
        let (mut core, session, _frames, _commands) = fixture();
        let (channel, _) = encrypted(&mut core, session);
        let g = core.calls.get_mut(&channel).unwrap().group.as_mut().unwrap();
        for epoch in 0..4097 {
            g.log.push_back((now_ms(), MlsLogEntry { epoch, proposals: vec![], commit: "commit".into() }, 10));
        }
        prune(g);
        assert_eq!(g.log.len(), 4096);
        assert_eq!(g.log.front().unwrap().1.epoch, 1);
        g.log.clear();
        g.log.push_back((
            now_ms() - RETENTION_MS - 1,
            MlsLogEntry { epoch: 1, proposals: vec![], commit: "old".into() },
            3,
        ));
        g.log.push_back((now_ms(), MlsLogEntry { epoch: 2, proposals: vec![], commit: "large".into() }, LOG_BYTES));
        g.log.push_back((now_ms(), MlsLogEntry { epoch: 3, proposals: vec![], commit: "new".into() }, 3));
        prune(g);
        assert_eq!(g.log.len(), 1);
        assert_eq!(g.log.front().unwrap().1.epoch, 3);
    }
    #[test]
    fn logs_prune_by_age_below_count_and_byte_limits() {
        let (mut core, session, _frames, _commands) = fixture();
        let (channel, _) = encrypted(&mut core, session);
        let g = core.calls.get_mut(&channel).unwrap().group.as_mut().unwrap();
        g.log.push_back((
            now_ms() - RETENTION_MS - 1,
            MlsLogEntry { epoch: 1, proposals: vec![], commit: "old".into() },
            3,
        ));
        g.log.push_back((now_ms(), MlsLogEntry { epoch: 2, proposals: vec![], commit: "recent".into() }, 6));
        prune(g);
        assert_eq!(g.log.len(), 1);
        assert_eq!(g.log.front().unwrap().1.epoch, 2);
    }
    #[test]
    fn microphone_ssrc_is_bound_to_the_single_sending_audio_section() {
        let valid = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=sendonly\r\na=ssrc:123 cname:x\r\na=ssrc:123 msid:x\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=recvonly\r\n";
        assert_eq!(microphone_ssrc(valid), Some(123));
        assert_eq!(microphone_ssrc(&valid.replace("123", "0")), None);
        assert_eq!(microphone_ssrc(&valid.replace("a=ssrc:123 msid:x", "a=ssrc:456 msid:x")), None);
        assert_eq!(microphone_ssrc(&valid.replace("a=recvonly", "a=sendonly")), None);
    }
    #[test]
    fn bridge_only_mixed_forced_and_repeated_moves_share_one_allocator() {
        use super::super::bridge::RemoteUpdate;
        let (mut core, local, mut frames, _commands) = fixture();
        let (tx, mut rx) = oneshot::channel();
        core.bridge_msg(BridgeMsg::Join(
            RemoteClient {
                uid: "ts:bridge".into(),
                nickname: "bridge".into(),
                channel: 1,
                platform: Platform::Ts3,
                muted: false,
                deafened: false,
                away: None,
            },
            tx,
        ));
        let remote = rx.try_recv().unwrap().unwrap();
        let original = core.voice_state(remote).unwrap();
        assert_eq!(core.sessions[&remote].participation.as_ref().unwrap().binding.run, None);
        core.enter_channel(local, 1).unwrap();
        assert_eq!(core.voice_state(local).unwrap().call, original.call);
        let previous = core.sessions[&local].participation.as_ref().unwrap().binding.clone();
        core.bridge_msg(BridgeMsg::Update(remote, RemoteUpdate { channel: Some(1), ..Default::default() }));
        assert_eq!(core.voice_state(remote).unwrap(), original);
        let Response::Channel(other) = core
            .insert_channel(
                local,
                ChannelRow {
                    id: 0,
                    parent: None,
                    name: "other".into(),
                    topic: String::new(),
                    position: 2,
                    password_hash: None,
                    max_clients: None,
                    encrypted: false,
                    history: false,
                    private: false,
                    access: None,
                    overrides: Vec::new(),
                    synced: false,
                },
                None,
            )
            .unwrap()
        else {
            panic!()
        };
        core.sessions.get_mut(&local).unwrap().groups = vec![ADMIN_GROUP];
        core.client_move(local, local, None, other.id).unwrap();
        assert_eq!(core.calls.len(), 2);
        core.bridge_msg(BridgeMsg::Update(remote, RemoteUpdate { channel: Some(other.id), ..Default::default() }));
        assert_eq!(core.calls.len(), 1);
        assert_eq!(core.voice_state(local).unwrap().call, core.voice_state(remote).unwrap().call);
        assert_ne!(core.voice_state(remote).unwrap().participation, original.participation);
        assert!(core.sessions[&remote].voice, "bridge move retains remote transport");
        core.client_move(local, remote, None, 1).unwrap();
        assert!(core.sessions[&remote].voice, "forced move retains remote transport");
        core.client_move(local, remote, None, other.id).unwrap();
        assert!(core.sessions[&remote].voice);
        while frames.try_recv().is_ok() {}
        core.handle(CoreMsg::Talking { session: local, talking: true, binding: Some(previous) });
        assert!(!core.sessions[&local].talking);
        assert!(frames.try_recv().is_err());
        let binding = core.sessions[&local].participation.as_ref().unwrap().binding.clone();
        core.client_updated(local);
        core.handle(CoreMsg::Talking { session: local, talking: true, binding: Some(binding) });
        let Outbound::Frame(roster) = frames.try_recv().unwrap() else { panic!() };
        let Outbound::Frame(talking) = frames.try_recv().unwrap() else { panic!() };
        assert!(roster.contains("client.updated"));
        assert!(talking.contains("voice.talking"));
        core.move_client(local, None).unwrap();
        assert_eq!(core.calls.len(), 1);
        core.remove(remote, LeaveReason::Quit);
        assert!(core.calls.is_empty());
    }
    #[test]
    fn proposal_overflow_ends_the_call_and_wire_blob_limits_reject_before_mutation() {
        let (mut core, session, mut frames, _commands) = fixture();
        let (channel, _) = encrypted(&mut core, session);
        assert_eq!(
            blob(&URL_SAFE_NO_PAD.encode(vec![0; BLOB_BYTES + 1]), BLOB_BYTES).unwrap_err().code,
            ErrorCode::TooLarge
        );
        let g = core.calls.get_mut(&channel).unwrap().group.as_mut().unwrap();
        g.leaves.clear();
        g.proposals.insert(999, vec![0; PROPOSAL_BYTES]);
        core.voice_reconcile();
        assert!(core.calls.is_empty());
        assert!(core.sessions[&session].participation.is_none());
        assert!(core.routing.load().members.is_empty());
        let mut closed = false;
        while let Ok(frame) = frames.try_recv() {
            if let Outbound::Frame(frame) = frame {
                closed |= frame.contains("state_limit");
            }
        }
        assert!(closed);
    }
    fn offer(core: &Core, session: SessionId, run: u64) -> VoiceOffer {
        let state = core.voice_state(session).unwrap();
        VoiceOffer {
            channel: state.channel,
            call: state.call,
            participation: state.participation,
            group: state.group,
            run,
            ssrc: run as u32,
            sdp: format!("v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=sendonly\r\na=ssrc:{run} cname:mic\r\n"),
        }
    }
    async fn complete(
        core: &mut Core,
        rx: &mut mpsc::Receiver<CoreMsg>,
        reply: oneshot::Sender<Result<String, &'static str>>,
        answer: Result<String, &'static str>,
    ) {
        reply.send(answer).unwrap();
        let message = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await.unwrap().unwrap();
        core.handle(message);
    }
    #[tokio::test]
    async fn offers_are_idempotent_and_late_completions_cannot_revive_a_retired_run() {
        let (mut core, session, _frames, mut commands) = fixture();
        let (tx, mut rx) = mpsc::channel(16);
        core.me = tx.downgrade();
        core.enter_channel(session, 1).unwrap();
        let first = offer(&core, session, 1);
        assert!(core.voice_offer(session, 10, first.clone()).is_none());
        let MediaCmd::Offer { reply, .. } = commands.recv().await.unwrap() else { panic!() };
        complete(&mut core, &mut rx, reply, Ok("answer".into())).await;
        assert!(
            matches!(core.voice_offer(session, 11, first.clone()), Some(Ok(Response::Answer { sdp, .. })) if sdp == "answer")
        );
        assert!(commands.try_recv().is_err());
        let mut changed = first.clone();
        changed.sdp.push_str("a=extra:x\r\n");
        assert_eq!(core.voice_offer(session, 12, changed).unwrap().unwrap_err().code, ErrorCode::Conflict);
        let mut reused = first.clone();
        reused.run = 2;
        assert_eq!(core.voice_offer(session, 13, reused).unwrap().unwrap_err().message, "ssrc_reused");
        let second = offer(&core, session, 2);
        assert!(core.voice_offer(session, 14, second.clone()).is_none());
        let MediaCmd::Offer { reply: old_reply, .. } = commands.recv().await.unwrap() else { panic!() };
        old_reply.send(Ok("late answer".into())).unwrap();
        let late = rx.recv().await.unwrap();
        let third = offer(&core, session, 3);
        assert!(core.voice_offer(session, 15, third.clone()).is_none());
        let MediaCmd::Offer { reply: new_reply, .. } = commands.recv().await.unwrap() else { panic!() };
        core.handle(late);
        assert!(core.sessions[&session].participation.as_ref().unwrap().answer.is_none());
        complete(&mut core, &mut rx, new_reply, Ok("current answer".into())).await;
        core.handle(CoreMsg::Voice { session, connected: false, binding: second.binding(), failed: true });
        assert_eq!(core.sessions[&session].participation.as_ref().unwrap().binding.run, Some(3));
        assert_eq!(core.voice_offer(session, 16, first).unwrap().unwrap_err().code, ErrorCode::Stale);
        let fourth = offer(&core, session, 4);
        assert!(core.voice_offer(session, 17, fourth).is_none());
        let MediaCmd::Offer { reply, .. } = commands.recv().await.unwrap() else { panic!() };
        reply.send(Ok("moved answer".into())).unwrap();
        let late = rx.recv().await.unwrap();
        core.move_client(session, None).unwrap();
        core.enter_channel(session, 1).unwrap();
        let current = core.voice_state(session).unwrap();
        core.handle(late);
        assert_eq!(core.voice_state(session).unwrap(), current);
        assert_eq!(core.sessions[&session].participation.as_ref().unwrap().binding.run, None);
    }
    #[tokio::test]
    async fn failed_offer_retires_the_run_and_caps_refuse_without_partial_state() {
        let (mut core, session, mut frames, mut commands) = fixture();
        let (tx, mut rx) = mpsc::channel(16);
        core.me = tx.downgrade();
        core.enter_channel(session, 1).unwrap();
        let request = offer(&core, session, 1);
        let mut oversized = request.clone();
        oversized.sdp = "x".repeat(64 * 1024 + 1);
        assert_eq!(core.voice_offer(session, 2, oversized).unwrap().unwrap_err().code, ErrorCode::TooLarge);
        assert!(core.sessions[&session].used_ssrcs.is_empty());
        assert!(core.voice_offer(session, 3, request.clone()).is_none());
        let MediaCmd::Offer { reply, .. } = commands.recv().await.unwrap() else { panic!() };
        complete(&mut core, &mut rx, reply, Err("invalid SDP")).await;
        assert_eq!(core.sessions[&session].participation.as_ref().unwrap().binding.run, None);
        assert_eq!(core.voice_offer(session, 4, request).unwrap().unwrap_err().code, ErrorCode::Stale);
        assert!(
            std::iter::from_fn(|| frames.try_recv().ok())
                .any(|f| matches!(f, Outbound::Frame(s) if s.contains("transport_failed")))
        );
        // The queue is reserved before the participation, SSRC history or roster changes.
        for _ in 0..16 {
            core.media.try_send(MediaCmd::Remove { session }).unwrap();
        }
        let second = offer(&core, session, 2);
        assert_eq!(core.voice_offer(session, 5, second).unwrap().unwrap_err().code, ErrorCode::Unavailable);
        assert_eq!(core.sessions[&session].used_ssrcs.len(), 1);
        while commands.try_recv().is_ok() {}
        core.sessions.get_mut(&session).unwrap().used_ssrcs = (1..=32).collect();
        let exhausted = offer(&core, session, 34);
        assert_eq!(core.voice_offer(session, 6, exhausted).unwrap().unwrap_err().message, "stream_limit");
        assert!(core.voice_state(session).is_none());
        assert!(core.calls.is_empty());
    }
    #[tokio::test]
    async fn active_replacements_reserve_their_eventual_retirement() {
        for failure in [false, true] {
            let (mut core, session, mut frames, mut commands) = fixture();
            let (tx, mut rx) = mpsc::channel(16);
            core.me = tx.downgrade();
            core.enter_channel(session, 1).unwrap();
            for run in 1..=32 {
                let request = offer(&core, session, run);
                assert!(core.voice_offer(session, run as u32, request).is_none());
                let MediaCmd::Offer { reply, retired, .. } = commands.recv().await.unwrap() else { panic!() };
                assert_eq!(retired.len(), run as usize - 1);
                let answer = if failure && run == 32 { Err("negotiation failed") } else { Ok("answer".into()) };
                complete(&mut core, &mut rx, reply, answer).await;
                assert!(core.sessions[&session].used_ssrcs.len() <= 32);
                while frames.try_recv().is_ok() {}
            }
            let request = offer(&core, session, 33);
            assert_eq!(core.voice_offer(session, 33, request).unwrap().unwrap_err().message, "stream_limit");
            assert!(core.voice_state(session).is_none());
            assert_eq!(core.sessions[&session].used_ssrcs.len(), 32);
            assert!(!core.sessions[&session].used_ssrcs.contains(&33));
            assert!(
                std::iter::from_fn(|| frames.try_recv().ok())
                    .any(|f| matches!(f, Outbound::Frame(s) if s.contains("stream_limit")))
            );
            core.enter_channel(session, 1).unwrap();
            let request = offer(&core, session, 34);
            assert_eq!(core.voice_offer(session, 34, request).unwrap().unwrap_err().message, "stream_limit");
            assert_eq!(core.sessions[&session].used_ssrcs.len(), 32);
        }
    }
    #[test]
    fn wire_epoch_exhaustion_closes_the_call_without_wrapping() {
        let (mut core, session, mut frames, _commands) = fixture();
        let (channel, group) = encrypted(&mut core, session);
        let state = core.voice_state(session).unwrap();
        let epoch = gwar_proto::MAX_SAFE_INTEGER as u64;
        core.calls.get_mut(&channel).unwrap().group.as_mut().unwrap().epoch = epoch;
        assert_eq!(
            core.voice_commit(
                session,
                &group,
                epoch,
                "",
                "",
                VoiceParticipation { call: state.call, participation: state.participation }
            )
            .unwrap_err()
            .code,
            ErrorCode::Stale
        );
        assert!(core.calls.is_empty());
        assert!(
            std::iter::from_fn(|| frames.try_recv().ok())
                .any(|f| matches!(f, Outbound::Frame(s) if s.contains("epoch_exhausted")))
        );
    }
    #[test]
    fn log_pages_include_pending_bytes_and_tree_cache_keeps_only_the_latest_bounded_set() {
        let (mut core, session, _frames, _commands) = fixture();
        let (channel, group) = encrypted(&mut core, session);
        let g = core.calls.get_mut(&channel).unwrap().group.as_mut().unwrap();
        g.epoch = 8;
        g.proposals.insert(1, vec![1; 128 * 1024]);
        for epoch in 0..8 {
            let entry = MlsLogEntry { epoch, proposals: vec![], commit: "A".repeat(175_000) };
            let size = serde_json::to_vec(&entry).unwrap().len() + 1;
            g.log.push_back((now_ms(), entry, size));
        }
        let response = core.voice_log(session, channel, 0, Some(100)).unwrap();
        assert!(serde_json::to_vec(&response).unwrap().len() < 640 * 1024);
        let Response::Mls(MlsResponse::Log { entries, pending, next, .. }) = response else { panic!() };
        assert_eq!(entries.len(), 2);
        assert_eq!(pending.len(), 1);
        assert_eq!(next, Some(2));
        assert_eq!(core.voice_log(session, channel, 0, Some(101)).unwrap_err().code, ErrorCode::BadRequest);
        for i in 0..64 {
            core.mls_trees.insert(format!("old-{i}"), (0, axum::body::Bytes::new()));
        }
        let access = core.sessions[&session].file_access.clone();
        core.mls_tree(&group, &access).unwrap();
        assert_eq!(core.mls_trees.len(), 1);
        core.move_client(session, None).unwrap();
        assert!(core.mls_trees.is_empty());
    }
    #[test]
    fn forced_encrypted_entry_cannot_bypass_channel_capacity() {
        let (mut core, session, _frames, _commands) = fixture();
        let (channel, _) = encrypted(&mut core, session);
        core.move_client(session, None).unwrap();
        core.channels.get_mut(&channel).unwrap().max_clients = Some(0);
        core.sessions.get_mut(&session).unwrap().groups = vec![ADMIN_GROUP];
        assert_eq!(core.client_move(session, session, None, channel).unwrap_err().code, ErrorCode::ChannelFull);
        assert!(core.calls.is_empty());
        assert!(core.sessions[&session].participation.is_none());
    }
}
