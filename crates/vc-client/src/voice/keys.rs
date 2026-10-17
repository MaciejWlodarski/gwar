//! The key channel of an encrypted native run (docs/e2e-build.md
//! "Installation and activation"). The app delivers controls; the RTC task
//! owns the [`SframeState`], applies them in arrival order between media
//! packets and replies only after application. Statuses go out as events;
//! none of them acknowledges an installation.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use super::sframe::{Binding, ControlReply, Counters, CryptoState, CryptoStatus, SframeState};

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

pub(crate) struct Command {
    body: Value,
    overflow: bool,
    reply: oneshot::Sender<ControlReply>,
}

/// The app's end: cheap to clone, usable without holding app locks.
#[derive(Clone)]
pub struct KeyChannel {
    tx: mpsc::UnboundedSender<Command>,
    pending: Arc<AtomicUsize>,
    counters: Arc<Mutex<Counters>>,
}

impl KeyChannel {
    /// Applies one control body; resolves after the RTC task applied it.
    /// An ended run answers `ignored`.
    pub async fn control(&self, body: Value) -> ControlReply {
        let ignored = ControlReply::ignored(&body);
        let overflow = self.pending.fetch_add(1, Ordering::SeqCst) >= QUEUE;
        let (reply, rx) = oneshot::channel();
        if self.tx.send(Command { body, overflow, reply }).is_err() {
            self.pending.fetch_sub(1, Ordering::SeqCst);
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
    (Crypto { run, commands }, KeyChannel { tx, pending, counters }, statuses)
}

fn wall_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

impl RunCrypto {
    /// Applies one command. `false`: the run failed and must close.
    pub fn command(&mut self, command: Command) -> bool {
        self.pending.fetch_sub(1, Ordering::SeqCst);
        let reply = if command.overflow {
            self.state.overflow(&command.body)
        } else {
            let bytes = serde_json::to_vec(&command.body).map_or(usize::MAX, |b| b.len());
            self.state.control(&command.body, bytes, Instant::now(), wall_ms())
        };
        // Publish statuses before the reply: a failure is visible when the caller resumes.
        let alive = self.publish();
        let _ = command.reply.send(reply);
        alive
    }

    /// Encrypts one Opus packet right before the RTP write, or drops it.
    pub fn encrypt(&mut self, opus: &[u8]) -> (Option<Vec<u8>>, bool) {
        let out = self.state.encrypt(opus, Instant::now());
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

    #[tokio::test]
    async fn replies_only_after_the_rtc_task_applies_the_control() {
        let (mut crypto, keys, _events) = channel(start(), 480);
        let pending = tokio::spawn({
            let keys = keys.clone();
            async move { keys.control(supersede(1, 1)).await }
        });
        tokio::task::yield_now().await;
        assert!(!pending.is_finished());
        let command = crypto.commands.recv().await.unwrap();
        assert!(crypto.run.command(command));
        assert_eq!(pending.await.unwrap().result, Some(Outcome::Applied));
    }

    #[tokio::test]
    async fn the_ninth_waiting_control_fails_the_run_without_losing_earlier_ones() {
        let (mut crypto, keys, mut events) = channel(start(), 480);
        let waits: Vec<_> = (1..=9)
            .map(|i| {
                let keys = keys.clone();
                tokio::spawn(async move { keys.control(supersede(i, i)).await })
            })
            .collect();
        while keys.pending.load(Ordering::SeqCst) < 9 {
            tokio::task::yield_now().await;
        }
        let mut alive = vec![];
        for _ in 0..9 {
            let command = crypto.commands.recv().await.unwrap();
            alive.push(crypto.run.command(command));
        }
        let replies: Vec<ControlReply> =
            futures::future::join_all(waits).await.into_iter().map(Result::unwrap).collect();
        assert_eq!(replies.iter().filter(|r| r.error == Some(ControlError::KeyQueueFull)).count(), 1);
        assert!(alive.contains(&false));
        let status = events.recv().await.unwrap();
        assert_eq!((status.state, status.reason), (CryptoState::Failed, StatusReason::KeyQueueFull));
    }

    #[tokio::test]
    async fn an_ended_run_answers_ignored() {
        let (crypto, keys, _events) = channel(start(), 480);
        drop(crypto);
        let reply = keys.control(supersede(1, 1)).await;
        assert_eq!((reply.serial.as_str(), reply.result), ("1", Some(Outcome::Ignored)));
    }
}
