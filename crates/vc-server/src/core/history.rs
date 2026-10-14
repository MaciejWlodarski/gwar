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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use gwar_mls::ds::Leaf;
    use tokio::sync::mpsc;
    use vc_proto::Platform;

    use super::*;
    use crate::{
        core::{
            ConnectRequest, CoreHandle, OUTBOUND_QUEUE, Outbound,
            expiry_tests::{on_core, test_core},
        },
        store::{ChannelRow, Store, mls::GroupRow},
    };

    const GROUP: &str = "group";

    fn device(seed: u8) -> String {
        URL_SAFE_NO_PAD.encode([seed; 32])
    }

    fn uid(seed: u8) -> String {
        crate::identity::uid_for_key(&[seed; 32])
    }

    /// An encrypted channel whose group holds `leaves` as (account seed, device seed),
    /// written after the core started (its observer is a stand-in that only routing tolerates).
    async fn store(leaves: &[(u8, u8)]) -> CoreHandle {
        let store = Store::in_memory().unwrap();
        let channel = store
            .create_channel(&ChannelRow {
                id: 0,
                parent: None,
                name: "secret".into(),
                topic: String::new(),
                position: 9,
                password_hash: None,
                max_clients: None,
                encrypted: true,
                history: true,
            })
            .unwrap();
        let leaves: Vec<_> = leaves
            .iter()
            .enumerate()
            .map(|(index, (account, d))| Leaf { index: index as u32, device_key: device(*d), uid: uid(*account) })
            .collect();
        let row = GroupRow {
            group: GROUP.into(),
            conversation: format!("channel:{channel}"),
            incarnation: 0,
            epoch: 3,
            committed_at: 1,
            group_info: vec![],
            observer: vec![],
            created_at: 1,
        };
        let core = test_core(store, None);
        on_core(&core, move |core| core.store.mls_write_group(&row, &leaves, &[], None, None).unwrap()).await;
        core
    }

    struct Device {
        session: SessionId,
        frames: mpsc::Receiver<Outbound>,
    }

    impl Device {
        /// Assignments announced to this device since the last call.
        fn wanted(&mut self) -> Vec<String> {
            let mut ids = Vec::new();
            while let Ok(frame) = self.frames.try_recv() {
                let Outbound::Frame(frame) = frame else { continue };
                let frame: serde_json::Value = serde_json::from_str(&frame).unwrap();
                if frame["ev"] == "mls.history.wanted" {
                    assert_eq!(frame["d"]["group"], GROUP);
                    ids.push(frame["d"]["assignment"].as_str().unwrap().to_owned());
                }
            }
            ids
        }
    }

    async fn connect(core: &CoreHandle, account: u8, d: u8) -> Device {
        let (out, frames) = mpsc::channel(OUTBOUND_QUEUE);
        let session = core
            .connect(ConnectRequest {
                request_id: 1,
                hello_key: device(d),
                certificate: None,
                uid: uid(account),
                public_key: URL_SAFE_NO_PAD.encode([account; 32]),
                nickname: format!("user{account}"),
                platform: Platform::Web,
                out,
                ip: None,
                device: None,
                device_expires_at: None,
                invite: None,
                password_ok: true,
            })
            .await
            .unwrap();
        Device { session, frames }
    }

    async fn request(core: &CoreHandle, recipient: u8) {
        on_core(core, move |core| {
            assert!(core.store.mls_request_history(GROUP, &device(recipient), now_ms()).unwrap());
            core.history_requested(GROUP, &device(recipient));
        })
        .await;
    }

    /// The active assignments as (id, device seed).
    async fn active(core: &CoreHandle) -> BTreeMap<String, String> {
        on_core(core, |core| core.history.assignments.iter().map(|(id, a)| (id.clone(), a.device.clone())).collect())
            .await
    }

    async fn net(leaves: &[(u8, u8)], order: &[(u8, u8)]) -> (CoreHandle, HashMap<u8, Device>) {
        let core = store(leaves).await;
        let mut devices = HashMap::new();
        for (account, d) in order {
            devices.insert(*d, connect(&core, *account, *d).await);
        }
        for device in devices.values_mut() {
            device.wanted();
        }
        (core, devices)
    }

    #[tokio::test]
    async fn three_assignments_prefer_distinct_uids_and_recent_sessions() {
        // Recipient 10 (account 1); senders 20 (account 2), 30 (3), 40 and 41 (both account 4).
        let leaves = [(1, 10), (2, 20), (3, 30), (4, 40), (4, 41)];
        let (core, mut d) = net(&leaves, &[(2, 20), (3, 30), (4, 40), (4, 41), (1, 10)]).await;
        request(&core, 10).await;
        let first: BTreeSet<_> = active(&core).await.into_values().collect();
        assert_eq!(first, BTreeSet::from([device(41), device(30), device(20)]));
        for seed in [41, 30, 20] {
            assert_eq!(d.get_mut(&seed).unwrap().wanted().len(), 1, "device {seed}");
        }
        assert!(d.get_mut(&40).unwrap().wanted().is_empty());
        assert!(d.get_mut(&10).unwrap().wanted().is_empty(), "never the recipient");
        // Each device lists only its own assignment.
        let listed = on_core(&core, |core| core.history.wanted(&device(30))).await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].device, device(10));
    }

    #[tokio::test]
    async fn decline_timeout_and_disconnect_pass_the_request_on_and_stale_answers_end_nothing() {
        let leaves = [(1, 10), (2, 20), (3, 30), (4, 40), (5, 50), (6, 60), (7, 70)];
        let (core, mut d) = net(&leaves, &[(2, 20), (3, 30), (4, 40), (5, 50), (6, 60), (1, 10)]).await;
        request(&core, 10).await;
        let assigned = active(&core).await;
        assert_eq!(assigned.len(), 3);
        let by_device: HashMap<_, _> = assigned.iter().map(|(id, dev)| (dev.clone(), id.clone())).collect();

        // A decline from another device, or for another recipient, ends nothing.
        let id60 = by_device[&device(60)].clone();
        on_core(&core, {
            let id = id60.clone();
            move |core| {
                core.history_declined(&id, GROUP, &device(10), &device(50));
                core.history_declined(&id, GROUP, &device(20), &device(60));
            }
        })
        .await;
        assert_eq!(active(&core).await, assigned);

        // A decline uses the device's turn; the next one is asked.
        on_core(&core, {
            let id = id60.clone();
            move |core| core.history_declined(&id, GROUP, &device(10), &device(60))
        })
        .await;
        assert_eq!(d.get_mut(&30).unwrap().wanted().len(), 1);
        // The same decline again is stale.
        let before = active(&core).await;
        on_core(&core, move |core| core.history_declined(&id60, GROUP, &device(10), &device(60))).await;
        assert_eq!(active(&core).await, before);

        // A timeout, too.
        let id50 = by_device[&device(50)].clone();
        on_core(&core, move |core| {
            core.history.assignments.get_mut(&id50).unwrap().expires_at = 0;
            core.history_wake();
        })
        .await;
        assert_eq!(d.get_mut(&20).unwrap().wanted().len(), 1);

        // And a disconnect: no device is left to ask, so two stay active.
        let gone = d.remove(&40).unwrap().session;
        core.disconnect(gone, vc_proto::LeaveReason::Quit).await;
        let left: BTreeSet<_> = active(&core).await.into_values().collect();
        assert_eq!(left, BTreeSet::from([device(30), device(20)]));

        // A newly connected device with a leaf is asked right away.
        let mut late = connect(&core, 7, 70).await;
        assert_eq!(late.wanted().len(), 1);
        assert_eq!(active(&core).await.len(), 3);
    }

    #[tokio::test]
    async fn a_chunk_ends_the_request_and_the_next_request_reaches_devices_not_yet_asked() {
        let leaves = [(1, 10), (2, 20), (3, 30), (4, 40), (5, 50)];
        let (core, mut d) = net(&leaves, &[(2, 20), (3, 30), (4, 40), (5, 50), (1, 10)]).await;
        request(&core, 10).await;
        let assigned = active(&core).await;
        let answered = assigned.iter().find(|(_, dev)| **dev == device(50)).unwrap().0.clone();
        // What a stored chunk naming the assignment does (see `mls.history.share`).
        on_core(&core, move |core| {
            core.store.mls_cancel_request(GROUP, &device(10)).unwrap();
            core.history_answered(&answered);
        })
        .await;
        assert!(active(&core).await.is_empty());
        for dev in d.values_mut() {
            dev.wanted();
        }
        // The cancelled devices may be asked again, and the one with no turn yet; not the sharer.
        // A minute later (requests are limited to one a minute).
        on_core(&core, |core| {
            core.store.set_meta(&format!("mls_history_request:{GROUP}:{}", device(10)), "0").unwrap()
        })
        .await;
        request(&core, 10).await;
        let next: BTreeSet<_> = active(&core).await.into_values().collect();
        assert_eq!(next, BTreeSet::from([device(20), device(30), device(40)]));
        assert!(d.get_mut(&50).unwrap().wanted().is_empty());
    }

    #[tokio::test]
    async fn unresponsive_rounds_back_off_and_a_removed_recipient_drops_everything() {
        let leaves = [(1, 10), (2, 20)];
        let (core, mut d) = net(&leaves, &[(2, 20), (1, 10)]).await;
        request(&core, 10).await;
        assert_eq!(d.get_mut(&20).unwrap().wanted().len(), 1);
        // The only device times out: everyone was asked, the rotation waits for its backoff.
        let waiting = on_core(&core, |core| {
            core.history.assignments.values_mut().for_each(|a| a.expires_at = 0);
            core.history_wake();
            let rotation = core.history.rotations.values().next().unwrap();
            (rotation.waiting_until, rotation.started_at + rotation.backoff, core.history.deadline())
        })
        .await;
        assert_eq!(waiting.0, Some(waiting.1));
        assert_eq!(waiting.2, waiting.0);
        assert!(active(&core).await.is_empty());
        assert!(d.get_mut(&20).unwrap().wanted().is_empty());
        // After the backoff it starts over, and the next backoff doubles.
        let backoff = on_core(&core, |core| {
            let rotation = core.history.rotations.values_mut().next().unwrap();
            rotation.started_at -= FIRST_BACKOFF_MS;
            rotation.waiting_until = Some(0);
            core.history_wake();
            core.history.rotations.values().next().unwrap().backoff
        })
        .await;
        assert_eq!(backoff, 2 * FIRST_BACKOFF_MS);
        let ids = d.get_mut(&20).unwrap().wanted();
        assert_eq!(ids.len(), 1);
        // A decline round backs off the same way, capped at an hour.
        let capped = on_core(&core, move |core| {
            for _ in 0..10 {
                let ids: Vec<_> = core.history.assignments.keys().cloned().collect();
                for id in ids {
                    core.history_declined(&id, GROUP, &device(10), &device(20));
                }
                let rotation = core.history.rotations.values_mut().next().unwrap();
                rotation.started_at = 0;
                rotation.waiting_until = Some(0);
                core.history_wake();
            }
            core.history.rotations.values().next().unwrap().backoff
        })
        .await;
        assert_eq!(capped, MAX_BACKOFF_MS);
        assert_eq!(active(&core).await.len(), 1);
        // Removing the recipient's leaf cancels its request, assignments and rotation at once.
        on_core(&core, |core| core.history_members_changed(GROUP, &BTreeSet::from([device(10)]))).await;
        assert!(active(&core).await.is_empty());
        let state = on_core(&core, |core| {
            (core.history.rotations.len(), core.store.mls_history_requested(GROUP, &device(10), now_ms()).unwrap())
        })
        .await;
        assert_eq!(state, (0, false));
    }

    #[tokio::test]
    async fn a_device_that_keeps_timing_out_is_asked_again_only_in_a_later_rotation() {
        let leaves = [(1, 10), (2, 20), (3, 30)];
        let (core, mut d) = net(&leaves, &[(2, 20), (1, 10)]).await;
        request(&core, 10).await;
        assert_eq!(d.get_mut(&20).unwrap().wanted().len(), 1);
        // 20 times out and reconnects: its turn is used, so it waits for the next rotation,
        // while a device not yet asked gets the request at once.
        on_core(&core, |core| {
            core.history.assignments.values_mut().for_each(|a| a.expires_at = 0);
            core.history_wake();
        })
        .await;
        let mut again = connect(&core, 2, 20).await;
        assert!(again.wanted().is_empty());
        let mut fresh = connect(&core, 3, 30).await;
        assert_eq!(fresh.wanted().len(), 1);
    }
}
