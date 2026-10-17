//! The key channel of an encrypted native run (docs/e2e-build.md
//! "Installation and activation"). The app delivers controls; the RTC task
//! owns the [`SframeState`], applies them in arrival order between media
//! packets and replies only after application. Statuses go out as events;
//! none of them acknowledges an installation.
//!
//! [`RunGate`] is the run's retirement fence: dropping the `VoiceHandle` closes
//! it under the same lock the RTC task holds while applying a control or
//! writing/reading a packet, so nothing is applied, sent or played afterwards.

use std::{
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::Value;
use tokio::sync::{Notify, mpsc, oneshot};

use super::sframe::{
    Binding, Control, ControlError, ControlReply, Counters, CryptoState, CryptoStatus, MAX_CONTROL_BYTES, SframeState,
    parse_control, scrub,
};

/// Controls waiting for the RTC task; one more fails the run (`key_queue_full`).
const QUEUE: usize = 8;
const SWEEP: Duration = Duration::from_secs(1);

/// What an encrypted run is bound to before any frame can pass.
#[derive(Clone, Debug)]
pub struct EncryptedStart {
    pub binding: Binding,
    /// The controller's highest processed epoch before a restart's own update.
    pub resume_after: Option<u64>,
}

/// Open while the run is current. Closing waits for an in-progress control or
/// packet and wakes the RTC task, which then ends.
#[derive(Clone, Default)]
pub(crate) struct RunGate {
    open: Arc<Mutex<bool>>,
    closed: Arc<Notify>,
}

impl RunGate {
    pub fn new() -> Self {
        Self { open: Arc::new(Mutex::new(true)), closed: Arc::new(Notify::new()) }
    }
    /// Held across one control application or packet write/read.
    pub fn enter(&self) -> Option<MutexGuard<'_, bool>> {
        let guard = self.open.lock().unwrap_or_else(|e| e.into_inner());
        (*guard).then_some(guard)
    }
    pub fn close(&self) {
        *self.open.lock().unwrap_or_else(|e| e.into_inner()) = false;
        self.closed.notify_one();
    }
    pub async fn closed(&self) {
        loop {
            if !*self.open.lock().unwrap_or_else(|e| e.into_inner()) {
                return;
            }
            self.closed.notified().await;
        }
    }
}

pub(crate) enum Command {
    Control {
        control: Control,
        reply: oneshot::Sender<ControlReply>,
    },
    /// The queue overflowed: fail the run. Carries no key material.
    Overflow {
        reply: oneshot::Sender<ControlReply>,
        rejected: ControlReply,
    },
}

/// The app's end: cheap to clone, usable without holding app locks.
#[derive(Clone)]
pub struct KeyChannel {
    tx: mpsc::UnboundedSender<Command>,
    pending: Arc<AtomicUsize>,
    overflowed: Arc<AtomicBool>,
    gate: RunGate,
    counters: Arc<Mutex<Counters>>,
}

impl KeyChannel {
    /// Applies one control body; resolves after the RTC task applied it. The
    /// body is validated before admission and its exporter keys are wiped. A
    /// retired or failed run answers `ignored`.
    pub async fn control(&self, mut body: Value) -> ControlReply {
        let size = serde_json::to_vec(&body).map(zeroize::Zeroizing::new).map_or(usize::MAX, |b| b.len());
        let parsed = (size <= MAX_CONTROL_BYTES).then(|| parse_control(&body)).flatten();
        let ignored = ControlReply::ignored(&body);
        let bad = ControlReply::bad(&body, ControlError::BadKeys);
        scrub(&mut body);
        drop(body);
        let Some(control) = parsed else { return bad };
        if self.gate.enter().is_none() || self.overflowed.load(Ordering::SeqCst) {
            return ignored;
        }
        let (reply, rx) = oneshot::channel();
        let command = if self.pending.fetch_add(1, Ordering::SeqCst) >= QUEUE {
            self.pending.fetch_sub(1, Ordering::SeqCst);
            if self.overflowed.swap(true, Ordering::SeqCst) {
                return ignored;
            }
            let rejected = ControlReply::error_for(&control, ControlError::KeyQueueFull);
            Command::Overflow { reply, rejected }
        } else {
            Command::Control { control, reply }
        };
        let counted = matches!(command, Command::Control { .. });
        if self.tx.send(command).is_err() {
            if counted {
                self.pending.fetch_sub(1, Ordering::SeqCst);
            }
            return ignored;
        }
        rx.await.unwrap_or(ignored)
    }

    /// Aggregate counters of this run (never keys).
    pub fn counters(&self) -> Counters {
        *self.counters.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The RTC task's end.
pub(crate) struct RunCrypto {
    state: SframeState,
    pending: Arc<AtomicUsize>,
    counters: Arc<Mutex<Counters>>,
    events: mpsc::UnboundedSender<CryptoStatus>,
    next_sweep: Instant,
}

pub(crate) struct Crypto {
    pub run: RunCrypto,
    pub commands: mpsc::UnboundedReceiver<Command>,
}

/// Creates both ends; `frame_samples` is the native packet duration.
pub(crate) fn channel(
    start: EncryptedStart,
    frame_samples: u32,
    gate: RunGate,
) -> (Crypto, KeyChannel, mpsc::UnboundedReceiver<CryptoStatus>) {
    let (tx, commands) = mpsc::unbounded_channel();
    let (events, statuses) = mpsc::unbounded_channel();
    let pending = Arc::new(AtomicUsize::new(0));
    let counters = Arc::new(Mutex::new(Counters::default()));
    let run = RunCrypto {
        state: SframeState::new(start.binding, start.resume_after, frame_samples),
        pending: pending.clone(),
        counters: counters.clone(),
        events,
        next_sweep: Instant::now() + SWEEP,
    };
    let keys = KeyChannel { tx, pending, overflowed: Arc::new(AtomicBool::new(false)), gate, counters };
    (Crypto { run, commands }, keys, statuses)
}

pub(crate) fn wall_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

impl RunCrypto {
    /// Applies one command; the caller holds the run gate. `false`: the run
    /// failed and must close.
    pub fn command(&mut self, command: Command) -> bool {
        let (reply, sender) = match command {
            Command::Control { control, reply } => {
                self.pending.fetch_sub(1, Ordering::SeqCst);
                (self.state.control_parsed(control, Instant::now(), wall_ms()), reply)
            }
            Command::Overflow { reply, rejected } => {
                self.state.fail_queue_full();
                (rejected, reply)
            }
        };
        // Publish statuses before the reply: a failure is visible when the caller resumes.
        let alive = self.publish();
        let _ = sender.send(reply);
        alive
    }

    /// Answers a command that arrives after the gate closed, without applying it.
    pub fn refuse(&mut self, command: Command) {
        let (reply, ignored) = match command {
            Command::Control { control, reply } => {
                self.pending.fetch_sub(1, Ordering::SeqCst);
                (reply, ControlReply::ignored_for(&control))
            }
            Command::Overflow { reply, mut rejected } => {
                rejected.error = None;
                rejected.result = Some(super::sframe::Outcome::Ignored);
                (reply, rejected)
            }
        };
        let _ = reply.send(ignored);
    }

    /// Encrypts one Opus packet right before the RTP write, or drops it.
    pub fn encrypt(&mut self, opus: &[u8]) -> (Option<Vec<u8>>, bool) {
        let out = self.state.encrypt(opus, Instant::now(), wall_ms());
        (out, self.publish())
    }

    /// Opens one received payload; only an authenticated packet reaches playout.
    pub fn decrypt(&mut self, payload: &[u8]) -> (Option<Vec<u8>>, bool) {
        let out = self.state.decrypt(payload, Instant::now());
        (out, self.publish())
    }

    /// Retention sweep once a second; returns the next deadline.
    pub fn tick(&mut self, now: Instant) -> Instant {
        if now >= self.next_sweep {
            self.state.sweep(now);
            self.next_sweep = now + SWEEP;
        }
        self.next_sweep
    }

    /// Stops the run's keys first when the transport ends.
    pub fn stop(&mut self) {
        self.state.stop();
        self.publish();
    }

    fn publish(&mut self) -> bool {
        *self.counters.lock().unwrap_or_else(|e| e.into_inner()) = self.state.counters;
        let mut alive = true;
        for status in self.state.take_statuses() {
            alive &= status.state != CryptoState::Failed;
            let _ = self.events.send(status);
        }
        alive && !self.state.stopped()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::sframe::{ControlError, Outcome, StatusReason};
    use serde_json::json;

    fn binding_json() -> Value {
        let b = |n: usize, v: u8| base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, vec![v; n]);
        json!({ "server_key": b(32, 1), "session": 7, "channel": 3, "call": b(16, 2), "participation": b(16, 3), "run": 1, "group": b(32, 4) })
    }

    fn start() -> EncryptedStart {
        EncryptedStart { binding: Binding::parse(&binding_json()).unwrap(), resume_after: None }
    }

    fn supersede(serial: u64, epoch: u64) -> Value {
        json!({ "binding": binding_json(), "serial": serial.to_string(), "action": "supersede", "epoch": epoch.to_string() })
    }

    /// What the RTC task does with one command.
    fn apply(crypto: &mut Crypto, gate: &RunGate, command: Command) -> bool {
        match gate.enter() {
            Some(_open) => crypto.run.command(command),
            None => {
                crypto.run.refuse(command);
                false
            }
        }
    }

    #[tokio::test]
    async fn replies_only_after_the_rtc_task_applies_the_control() {
        let gate = RunGate::new();
        let (mut crypto, keys, _events) = channel(start(), 480, gate.clone());
        let pending = tokio::spawn({
            let keys = keys.clone();
            async move { keys.control(supersede(1, 1)).await }
        });
        tokio::task::yield_now().await;
        assert!(!pending.is_finished());
        let command = crypto.commands.recv().await.unwrap();
        assert!(apply(&mut crypto, &gate, command));
        assert_eq!(pending.await.unwrap().result, Some(Outcome::Applied));
    }

    #[tokio::test]
    async fn malformed_controls_are_refused_before_admission() {
        let gate = RunGate::new();
        let (mut crypto, keys, _events) = channel(start(), 480, gate);
        for i in 1..=20 {
            let mut body = supersede(i, i);
            body["extra"] = json!(1);
            assert_eq!(keys.control(body).await.error, Some(ControlError::BadKeys));
        }
        assert_eq!(keys.pending.load(Ordering::SeqCst), 0);
        assert!(crypto.commands.try_recv().is_err(), "nothing was queued");
    }

    #[tokio::test]
    async fn the_ninth_waiting_control_fails_the_run_and_the_queue_stays_bounded() {
        let gate = RunGate::new();
        let (mut crypto, keys, mut events) = channel(start(), 480, gate.clone());
        let waits: Vec<_> = (1..=20)
            .map(|i| {
                let keys = keys.clone();
                tokio::spawn(async move { keys.control(supersede(i, i)).await })
            })
            .collect();
        let replies: Vec<ControlReply> = {
            let mut received = 0;
            while received < 9 {
                let command = crypto.commands.recv().await.unwrap();
                apply(&mut crypto, &gate, command);
                received += 1;
            }
            assert!(crypto.commands.try_recv().is_err(), "at most eight controls and one overflow were admitted");
            futures::future::join_all(waits).await.into_iter().map(Result::unwrap).collect()
        };
        assert_eq!(replies.iter().filter(|r| r.error == Some(ControlError::KeyQueueFull)).count(), 1);
        let status = events.recv().await.unwrap();
        assert_eq!((status.state, status.reason), (CryptoState::Failed, StatusReason::KeyQueueFull));
    }

    #[tokio::test]
    async fn a_retained_channel_cannot_apply_after_retirement() {
        let gate = RunGate::new();
        let (mut crypto, keys, _events) = channel(start(), 480, gate.clone());
        let retained = keys.clone();
        // A control queued before retirement is refused by the RTC side, not applied.
        let queued = tokio::spawn({
            let keys = keys.clone();
            async move { keys.control(supersede(1, 1)).await }
        });
        while keys.pending.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        gate.close();
        let command = crypto.commands.recv().await.unwrap();
        assert!(!apply(&mut crypto, &gate, command));
        assert_eq!(queued.await.unwrap().result, Some(Outcome::Ignored));
        // Afterwards the retained clone does not even enqueue.
        assert_eq!(retained.control(supersede(2, 2)).await.result, Some(Outcome::Ignored));
        assert!(crypto.commands.try_recv().is_err());
        tokio::time::timeout(Duration::from_secs(1), gate.closed()).await.expect("closed wakes the RTC task");
    }

    #[tokio::test]
    async fn an_ended_run_answers_ignored() {
        let (crypto, keys, _events) = channel(start(), 480, RunGate::new());
        drop(crypto);
        let reply = keys.control(supersede(1, 1)).await;
        assert_eq!((reply.serial.as_str(), reply.result), ("1", Some(Outcome::Ignored)));
    }
}
