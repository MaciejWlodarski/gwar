//! History requests in channel groups, routed by assignment (docs/e2e-build.md,
//! section 4, "History in channels"). Assignments and rotations live in memory;
//! outstanding requests are stored, so a restarted server routes them afresh.

use std::collections::{BTreeMap, BTreeSet};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use tracing::warn;
use vc_proto::{Event, MlsEvent, MlsHistoryWanted, SessionId};

use super::{Core, now_ms};
use crate::store::mls::REQUEST_RETENTION_MS;

/// Active assignments per request.
const ACTIVE: usize = 3;
const ASSIGNMENT_MS: i64 = 60_000;
const FIRST_BACKOFF_MS: i64 = 60_000;
const MAX_BACKOFF_MS: i64 = 3_600_000;

/// A request: `(group, recipient device)`.
type Key = (String, String);

struct Assignment {
    key: Key,
    /// The asked device and its session.
    device: String,
    uid: String,
    session: SessionId,
    expires_at: i64,
}

/// The devices asked for one recipient's history in a group, across its requests.
struct Rotation {
    attempted: BTreeSet<String>,
    started_at: i64,
    backoff: i64,
    requested_at: i64,
    /// Set while every eligible device has been attempted and the backoff runs.
    waiting_until: Option<i64>,
}

#[derive(Default)]
pub(super) struct HistoryRouting {
    assignments: BTreeMap<String, Assignment>,
    rotations: BTreeMap<Key, Rotation>,
}

/// How an assignment ended. Only a cancellation leaves its device's attempt unused.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ended {
    Used,
    Cancelled,
}

impl HistoryRouting {
    /// The earliest assignment timeout or rotation restart.
    pub(super) fn deadline(&self) -> Option<i64> {
        let assignments = self.assignments.values().map(|a| a.expires_at);
        let rotations = self.rotations.values().filter_map(|r| r.waiting_until);
        assignments.chain(rotations).min()
    }

    fn active(&self, key: &Key) -> impl Iterator<Item = (&String, &Assignment)> {
        self.assignments.iter().filter(move |(_, a)| &a.key == key)
    }

    fn end(&mut self, id: &str, how: Ended) -> Option<Assignment> {
        let a = self.assignments.remove(id)?;
        if how == Ended::Cancelled
            && let Some(rotation) = self.rotations.get_mut(&a.key)
        {
            rotation.attempted.remove(&a.device);
        }
        Some(a)
    }

    /// The device's active assignments, for `mls.history.wanted {}`.
    pub(super) fn wanted(&self, device: &str) -> Vec<MlsHistoryWanted> {
        self.assignments
            .iter()
            .filter(|(_, a)| a.device == device)
            .map(|(id, a)| MlsHistoryWanted {
                group: a.key.0.clone(),
                device: a.key.1.clone(),
                assignment: Some(id.clone()),
            })
            .collect()
    }

    /// Whether `assignment` is active for this request and device.
    pub(super) fn is_active(&self, assignment: &str, group: &str, recipient: &str, device: &str) -> bool {
        self.assignments.get(assignment).is_some_and(|a| a.key.0 == group && a.key.1 == recipient && a.device == device)
    }

    /// The groups with assignments or rotations.
    pub(super) fn groups(&self) -> BTreeSet<String> {
        self.assignments.values().map(|a| a.key.0.clone()).chain(self.rotations.keys().map(|k| k.0.clone())).collect()
    }

    /// Forgets everything about a group (deleted with its channel).
    pub(super) fn forget_group(&mut self, group: &str) {
        self.assignments.retain(|_, a| a.key.0 != group);
        self.rotations.retain(|key, _| key.0 != group);
    }
}

impl Core {
    /// A device asked for history in a channel group (the request is stored).
    pub(super) fn history_requested(&mut self, group: &str, recipient: &str) {
        let now = now_ms();
        let key = (group.to_owned(), recipient.to_owned());
        let rotation = self.history.rotations.entry(key.clone()).or_insert_with(|| Rotation {
            attempted: BTreeSet::new(),
            started_at: now,
            backoff: FIRST_BACKOFF_MS,
            requested_at: now,
            waiting_until: None,
        });
        rotation.requested_at = now;
        self.history_route(&key);
    }

    /// A chunk naming an active assignment was stored, which ended the request:
    /// the sender's turn is used, the other assignments are cancelled.
    pub(super) fn history_answered(&mut self, assignment: &str) {
        let Some(answered) = self.history.end(assignment, Ended::Used) else { return };
        let others: Vec<_> = self.history.active(&answered.key).map(|(id, _)| id.clone()).collect();
        for id in others {
            self.history.end(&id, Ended::Cancelled);
        }
    }

    /// The asked device has nothing to share.
    pub(super) fn history_declined(&mut self, assignment: &str, group: &str, recipient: &str, device: &str) {
        if self.history.is_active(assignment, group, recipient, device)
            && let Some(a) = self.history.end(assignment, Ended::Used)
        {
            self.history_route(&a.key);
        }
    }

    /// A session left: its assignments are used up and passed on.
    pub(super) fn history_session_left(&mut self, session: SessionId) {
        let ended: Vec<_> =
            self.history.assignments.iter().filter(|(_, a)| a.session == session).map(|(id, _)| id.clone()).collect();
        let mut keys = BTreeSet::new();
        for id in ended {
            keys.extend(self.history.end(&id, Ended::Used).map(|a| a.key));
        }
        for key in keys {
            self.history_route(&key);
        }
    }

    /// A session connected: the requests it could answer or made may move on.
    pub(super) fn history_session_joined(&mut self, device: &str) {
        let requests = self.store.mls_channel_requests(Some(device), None, now_ms());
        match requests {
            Ok(requests) => {
                for key in requests {
                    self.history_route_stored(key);
                }
            }
            Err(e) => warn!("store: {e:#}"),
        }
    }

    /// A commit changed a channel group's members: requests of removed devices end
    /// with their rotations, removed senders' assignments are used up, and every
    /// outstanding request may reach new members.
    pub(super) fn history_members_changed(&mut self, group: &str, removed: &BTreeSet<String>) {
        let mut keys = BTreeSet::new();
        for recipient in removed {
            let key = (group.to_owned(), recipient.clone());
            let ids: Vec<_> = self.history.active(&key).map(|(id, _)| id.clone()).collect();
            for id in ids {
                self.history.assignments.remove(&id);
            }
            self.history.rotations.remove(&key);
            if let Err(e) = self.store.mls_cancel_request(group, recipient) {
                warn!("store: {e:#}");
            }
        }
        let senders: Vec<_> = self
            .history
            .assignments
            .iter()
            .filter(|(_, a)| a.key.0 == group && removed.contains(&a.device))
            .map(|(id, _)| id.clone())
            .collect();
        for id in senders {
            keys.extend(self.history.end(&id, Ended::Used).map(|a| a.key));
        }
        match self.store.mls_channel_requests(None, Some(group), now_ms()) {
            Ok(requests) => keys.extend(requests),
            Err(e) => warn!("store: {e:#}"),
        }
        for key in keys {
            self.history_route_stored(key);
        }
    }

    /// Timeouts and finished backoffs.
    pub(super) fn history_wake(&mut self) {
        let now = now_ms();
        let expired: Vec<_> =
            self.history.assignments.iter().filter(|(_, a)| a.expires_at <= now).map(|(id, _)| id.clone()).collect();
        let mut keys = BTreeSet::new();
        for id in expired {
            keys.extend(self.history.end(&id, Ended::Used).map(|a| a.key));
        }
        for (key, rotation) in &mut self.history.rotations {
            if rotation.waiting_until.is_some_and(|at| at <= now) {
                rotation.waiting_until = None;
                keys.insert(key.clone());
            }
        }
        for key in keys {
            self.history_route(&key);
        }
    }

    /// Drops rotations of recipients that made no request for 30 days.
    pub(super) fn history_prune(&mut self) {
        let horizon = now_ms() - REQUEST_RETENTION_MS;
        self.history.rotations.retain(|_, r| r.requested_at > horizon);
    }

    /// Routes a stored request, starting its rotation if this server has none (after a restart).
    fn history_route_stored(&mut self, key: Key) {
        let now = now_ms();
        self.history.rotations.entry(key.clone()).or_insert_with(|| Rotation {
            attempted: BTreeSet::new(),
            started_at: now,
            backoff: FIRST_BACKOFF_MS,
            requested_at: now,
            waiting_until: None,
        });
        self.history_route(&key);
    }

    /// Announces the request to devices not yet attempted until three are active,
    /// preferring distinct uids and the most recently connected sessions.
    fn history_route(&mut self, key: &Key) {
        if let Err(e) = self.try_history_route(key) {
            warn!("history routing: {e:#}");
        }
    }

    fn try_history_route(&mut self, key: &Key) -> anyhow::Result<()> {
        let (group, recipient) = key;
        let now = now_ms();
        if !self.store.mls_history_requested(group, recipient, now)? {
            let ids: Vec<_> = self.history.active(key).map(|(id, _)| id.clone()).collect();
            for id in ids {
                self.history.end(&id, Ended::Cancelled);
            }
            return Ok(());
        }
        let Some(row) = self.store.mls_group(group)? else { return Ok(()) };
        let leaves: BTreeMap<String, String> = self.store.mls_leaf_devices(group)?.into_iter().collect();
        // Only while the recipient has a leaf.
        if !leaves.contains_key(recipient) {
            return Ok(());
        }
        let readers = self.mls_readers(&row.conversation);
        // The most recent session of every eligible device.
        let mut eligible: BTreeMap<&str, (&str, SessionId, u64)> = BTreeMap::new();
        for s in self.sessions.values().filter(|s| readers.contains(&s.id)) {
            if s.hello_key == *recipient || leaves.get(&s.hello_key) != Some(&s.uid) {
                continue;
            }
            let entry = eligible.entry(&s.hello_key).or_insert((&s.uid, s.id, s.since));
            if s.since > entry.2 {
                *entry = (&s.uid, s.id, s.since);
            }
        }
        let eligible: Vec<(String, String, SessionId, u64)> =
            eligible.into_iter().map(|(d, (u, s, since))| (d.to_owned(), u.to_owned(), s, since)).collect();
        let mut announce = Vec::new();
        loop {
            let active: Vec<_> = self.history.active(key).map(|(_, a)| (a.device.clone(), a.uid.clone())).collect();
            if active.len() >= ACTIVE {
                break;
            }
            let Some(rotation) = self.history.rotations.get_mut(key) else { break };
            let candidates: Vec<_> = eligible
                .iter()
                .filter(|(device, ..)| !rotation.attempted.contains(device) && !active.iter().any(|(d, _)| d == device))
                .collect();
            let Some((device, uid, session, _)) = candidates
                .iter()
                .max_by_key(|(_, uid, _, since)| (!active.iter().any(|(_, u)| u == uid), *since))
                .map(|c| (*c).clone())
            else {
                // Everyone eligible was asked: start over after the backoff, once nothing is active.
                if active.is_empty() && !eligible.is_empty() {
                    let at = rotation.started_at + rotation.backoff;
                    if now >= at {
                        rotation.attempted.clear();
                        rotation.started_at = now;
                        rotation.backoff = (rotation.backoff * 2).min(MAX_BACKOFF_MS);
                        rotation.waiting_until = None;
                        continue;
                    }
                    rotation.waiting_until = Some(at);
                }
                break;
            };
            rotation.attempted.insert(device.clone());
            let id = B64.encode(rand::random::<[u8; 16]>());
            self.history.assignments.insert(
                id.clone(),
                Assignment {
                    key: key.clone(),
                    device: device.clone(),
                    uid: uid.clone(),
                    session,
                    expires_at: now + ASSIGNMENT_MS,
                },
            );
            announce.push((session, id));
        }
        for (session, assignment) in announce {
            self.broadcast(
                Event::Mls(MlsEvent::HistoryWanted {
                    group: group.clone(),
                    device: recipient.clone(),
                    assignment: Some(assignment),
                }),
                |s| s.id == session,
            );
        }
        Ok(())
    }
}
