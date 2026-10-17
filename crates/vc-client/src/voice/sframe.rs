//! The SFrame key state of one encrypted native voice run (docs/e2e-build.md
//! "SFrame and receiver state" and "Installation and activation"). It
//! implements the browser worker's control contract
//! (`apps/web/src/voice/sframe/state.ts`); the RTC task owns one instance and
//! applies controls, encryption and decryption in order on one thread, so no
//! result can outlive the control that invalidated it. Key material is never
//! logged, persisted or archived; derived keys zeroize on drop.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Serialize;
use serde_json::{Map, Value};
use sframe::{
    CipherSuite,
    frame::{EncryptedFrameView, MediaFrameView, SingleUseCounter},
    header::SframeHeader,
    key::{DecryptionKey, EncryptionKey},
};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

/// RFC 9605 `AES_128_GCM_SHA256_128` with a 16-byte tag; no other suite.
pub const SUITE: CipherSuite = CipherSuite::AesGcm128Sha256;
pub const TAG: usize = 16;
/// Encryption attempts under one base key before an own-leaf update is required.
pub const COUNTER_LIMIT: u64 = 1 << 32;
pub const MAX_EPOCHS: usize = 8;
pub const RETAIN: Duration = Duration::from_secs(10);
pub const MAX_CONTROL_BYTES: usize = 256 * 1024;
const MAX_LEAVES: usize = 1000;
const MIN_FRAME: usize = 1 + TAG;
const MAX_FRAME: usize = 1033;
const MAX_OPUS: usize = 1000;
const MAX_SAFE: u64 = (1 << 53) - 1;
const REPLAY_WINDOW: u64 = 128;

pub fn voice_kid(epoch: u64, leaf: u32) -> u64 {
    ((epoch & 0xffff_ffff) << 32) | u64::from(leaf)
}

/// The run an encrypted engine was created for, fixed before any frame passes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Binding {
    pub server_key: String,
    pub session: u64,
    pub channel: u32,
    pub call: String,
    pub participation: String,
    pub run: u64,
    pub group: String,
}

impl Binding {
    pub fn parse(value: &Value) -> Option<Self> {
        let b = object(value, &["server_key", "session", "channel", "call", "participation", "run", "group"])?;
        let run = safe(&b["run"]).filter(|run| *run > 0)?;
        Some(Self {
            server_key: fixed(&b["server_key"], 32)?,
            session: safe(&b["session"])?,
            channel: u32_of(&b["channel"])?,
            call: fixed(&b["call"], 16)?,
            participation: fixed(&b["participation"], 16)?,
            run,
            group: fixed(&b["group"], 32)?,
        })
    }
}

fn object<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Map<String, Value>> {
    let map = value.as_object()?;
    (map.len() == keys.len() && keys.iter().all(|k| map.contains_key(*k))).then_some(map)
}

/// Canonical unpadded base64url of exactly `len` bytes.
fn decode_fixed(value: &Value, len: usize) -> Option<Zeroizing<Vec<u8>>> {
    let text = value.as_str()?;
    let bytes = Zeroizing::new(URL_SAFE_NO_PAD.decode(text).ok()?);
    (bytes.len() == len && *Zeroizing::new(URL_SAFE_NO_PAD.encode(&*bytes)) == text).then_some(bytes)
}

fn fixed(value: &Value, len: usize) -> Option<String> {
    decode_fixed(value, len).map(|_| value.as_str().unwrap_or_default().to_owned())
}

/// Nonnegative safe integer (the wire's JS Number bound).
fn safe(value: &Value) -> Option<u64> {
    value.as_u64().filter(|v| *v <= MAX_SAFE)
}

fn u32_of(value: &Value) -> Option<u32> {
    value.as_u64().and_then(|v| u32::try_from(v).ok())
}

/// Canonical decimal u64 string: no sign, no leading zero, at most 20 digits.
fn decimal(value: &Value) -> Option<u64> {
    let text = value.as_str()?;
    let canonical = !text.is_empty()
        && text.len() <= 20
        && text.bytes().all(|b| b.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'));
    canonical.then(|| text.parse().ok()).flatten()
}

fn devices(value: &Value) -> Option<Vec<String>> {
    let list = value.as_array()?;
    let out: Vec<String> = list.iter().map(|d| fixed(d, 32)).collect::<Option<_>>()?;
    let distinct = out.iter().collect::<HashSet<_>>().len() == out.len();
    (out.len() <= MAX_LEAVES && distinct).then_some(out)
}

pub struct Member {
    pub leaf: u32,
    pub device: String,
    key: Zeroizing<Vec<u8>>,
}

pub enum Action {
    Hold { removed: Vec<String> },
    Supersede,
    Install { own_leaf: u32, members: Vec<Member>, removed: Vec<String>, digest: [u8; 32] },
    Activate { send_until: Option<u64> },
}

pub struct Control {
    pub binding: Binding,
    pub serial: u64,
    pub epoch: u64,
    pub action: Action,
    /// Fingerprint of the canonical body; the body itself is not kept.
    body: [u8; 32],
}

fn fingerprint(text: &str) -> [u8; 32] {
    Sha256::digest(text.as_bytes()).into()
}

/// Validates the exact tagged control body; `None` for any structural error.
pub fn parse_control(value: &Value) -> Option<Control> {
    let c = value.as_object()?;
    let binding = Binding::parse(c.get("binding")?)?;
    let serial = decimal(c.get("serial")?).filter(|s| *s > 0)?;
    let epoch = decimal(c.get("epoch")?)?;
    let keys = |extra: &[&str]| {
        let mut all = vec!["action", "binding", "serial", "epoch"];
        all.extend_from_slice(extra);
        object(value, &all).is_some()
    };
    let action = match c.get("action")?.as_str()? {
        "hold" if keys(&["removed"]) => Action::Hold { removed: devices(&c["removed"])? },
        "supersede" if keys(&[]) => Action::Supersede,
        "activate" if keys(&["send_until"]) => {
            let until = &c["send_until"];
            Action::Activate { send_until: if until.is_null() { None } else { Some(safe(until)?) } }
        }
        "install" if keys(&["own_leaf", "members", "removed"]) => {
            let removed = devices(&c["removed"])?;
            let own_leaf = u32_of(&c["own_leaf"])?;
            let list = c["members"].as_array()?;
            if list.is_empty() || list.len() > MAX_LEAVES {
                return None;
            }
            let mut members = Vec::with_capacity(list.len());
            let mut seen = HashSet::new();
            for m in list {
                let m = object(m, &["leaf", "device", "key"])?;
                let leaf = u32_of(&m["leaf"])?;
                let device = fixed(&m["device"], 32)?;
                let key = decode_fixed(&m["key"], 32)?;
                if members.last().is_some_and(|p: &Member| leaf <= p.leaf) || !seen.insert(device.clone()) {
                    return None;
                }
                members.push(Member { leaf, device, key });
            }
            if members.iter().filter(|m| m.leaf == own_leaf).count() != 1 {
                return None;
            }
            // Normalized member fields, fingerprinted: no raw exporter key is retained.
            let normalized = Zeroizing::new(
                serde_json::to_string(&(
                    own_leaf,
                    members.iter().map(|m| (m.leaf, &m.device, URL_SAFE_NO_PAD.encode(&*m.key))).collect::<Vec<_>>(),
                ))
                .ok()?,
            );
            Action::Install { own_leaf, members, removed, digest: fingerprint(&normalized) }
        }
        _ => return None,
    };
    let body = Zeroizing::new(canonical(&binding, value)?);
    Some(Control { binding, serial, epoch, action, body: fingerprint(&body) })
}

/// Fixed field order, the same text as the browser's `canonical`: bytewise
/// comparison of duplicate serials.
fn canonical(binding: &Binding, value: &Value) -> Option<String> {
    let c = value.as_object()?;
    let b = serde_json::json!({
        "server_key": binding.server_key, "session": binding.session, "channel": binding.channel,
        "call": binding.call, "participation": binding.participation, "run": binding.run, "group": binding.group,
    });
    let mut out = Map::new();
    let order: &[&str] = match c["action"].as_str()? {
        "hold" => &["removed"],
        "supersede" => &[],
        "activate" => &["send_until"],
        _ => &["own_leaf", "members", "removed"],
    };
    out.insert("action".into(), c["action"].clone());
    out.insert("binding".into(), b);
    out.insert("serial".into(), c["serial"].clone());
    out.insert("epoch".into(), c["epoch"].clone());
    for key in order {
        let field = if *key == "members" {
            Value::Array(
                c["members"]
                    .as_array()?
                    .iter()
                    .map(|m| serde_json::json!({ "leaf": m["leaf"], "device": m["device"], "key": m["key"] }))
                    .collect(),
            )
        } else {
            c[*key].clone()
        };
        out.insert((*key).into(), field);
    }
    serde_json::to_string(&Value::Object(out)).ok()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Applied,
    Unchanged,
    Ignored,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlError {
    BadKeys,
    NotInstalled,
    KeyConflict,
    KeyQueueFull,
}

/// `{serial, epoch, result}` or `{serial, epoch, error}`, returned only after
/// the RTC task applied the control.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ControlReply {
    pub serial: String,
    pub epoch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Outcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ControlError>,
}

impl ControlReply {
    fn result(c: &Control, result: Outcome) -> Self {
        Self { serial: c.serial.to_string(), epoch: c.epoch.to_string(), result: Some(result), error: None }
    }
    fn error(c: &Control, error: ControlError) -> Self {
        Self { serial: c.serial.to_string(), epoch: c.epoch.to_string(), result: None, error: Some(error) }
    }
    /// Echoes the body's serial/epoch strings, "0" when absent.
    pub fn bad(body: &Value, error: ControlError) -> Self {
        Self { result: None, error: Some(error), ..Self::ignored(body) }
    }
    /// For a run that already ended.
    pub fn ignored(body: &Value) -> Self {
        let tag = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or("0").to_owned();
        Self { serial: tag("serial"), epoch: tag("epoch"), result: Some(Outcome::Ignored), error: None }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusReason {
    CounterExhausted,
    KeyExpired,
    KeyConflict,
    KeyQueueFull,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CryptoState {
    Securing,
    Failed,
}

/// `voice://crypto`; never an installation acknowledgement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CryptoStatus {
    pub binding: Binding,
    pub state: CryptoState,
    pub reason: StatusReason,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Counters {
    pub encrypted: u64,
    pub decrypted: u64,
    pub no_key: u64,
    pub auth_failed: u64,
    pub replay_dropped: u64,
    pub own_kid_dropped: u64,
    pub oversize_dropped: u64,
    pub unexpected_duration: u64,
    pub stale_install: u64,
    pub counter_exhausted: u64,
}

/// A 128-counter window per (group, full epoch, leaf); bit 0 is the highest
/// authenticated counter.
#[derive(Default)]
struct Replay {
    highest: Option<u64>,
    bitmap: u128,
}

impl Replay {
    /// Read-only: whether `ctr` could still be admitted.
    fn fresh(&self, ctr: u64) -> bool {
        match self.highest {
            None => true,
            Some(high) if ctr > high => true,
            Some(high) => high - ctr < REPLAY_WINDOW && self.bitmap >> (high - ctr) & 1 == 0,
        }
    }

    /// Marks an authenticated counter; false (and no change) for a replay.
    fn admit(&mut self, ctr: u64) -> bool {
        if !self.fresh(ctr) {
            return false;
        }
        match self.highest {
            Some(high) if ctr <= high => self.bitmap |= 1 << (high - ctr),
            Some(high) => {
                let shift = ctr - high;
                self.bitmap = if shift >= REPLAY_WINDOW { 1 } else { self.bitmap << shift | 1 };
                self.highest = Some(ctr);
            }
            None => {
                self.highest = Some(ctr);
                self.bitmap = 1;
            }
        }
        true
    }
}

/// The RFC 9605 header at the start of `frame`: `(kid, ctr, length)`, or
/// `None` for a truncated or non-minimal encoding.
pub fn decode_header(frame: &[u8]) -> Option<(u64, u64, usize)> {
    let header = SframeHeader::deserialize(frame).ok()?;
    let len = header.len();
    let mut canonical = vec![0; len];
    SframeHeader::new(header.key_id(), header.counter()).serialize(&mut canonical).ok()?;
    (frame.get(..len) == Some(&canonical[..])).then_some((header.key_id(), header.counter(), len))
}

pub fn encode_header(kid: u64, ctr: u64) -> Vec<u8> {
    let header = SframeHeader::new(kid, ctr);
    let mut out = vec![0; header.len()];
    header.serialize(&mut out).expect("sized buffer");
    out
}

struct Receiver {
    device: String,
    key: DecryptionKey,
    replay: Replay,
}

struct EpochKeys {
    superseded_at: Option<Instant>,
    own_leaf: u32,
    own: EncryptionKey,
    /// Reserved before each attempt; never reused under this base key.
    counter: u64,
    /// Monotonic send deadline; activation can only shorten it.
    deadline: Option<Instant>,
    receivers: HashMap<u32, Receiver>,
}

struct Latest {
    epoch: u64,
    digest: [u8; 32],
    devices: HashSet<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    Stopped,
    Failed,
}

pub struct SframeState {
    binding: Binding,
    /// The controller's highest processed epoch before a restart's own update.
    resume_after: Option<u64>,
    /// This client's configured packet duration in 48 kHz samples (native: 10 ms).
    frame_samples: u32,
    pub counters: Counters,
    ended: Option<End>,
    serial: u64,
    last: Option<([u8; 32], ControlReply)>,
    /// The processed-epoch floor: barriers advance it, installs require it.
    floor: Option<u64>,
    epochs: BTreeMap<u64, EpochKeys>,
    latest: Option<Latest>,
    last_authenticated: HashMap<String, u64>,
    sender: Option<u64>,
    securing: Option<StatusReason>,
    statuses: Vec<CryptoStatus>,
}

impl SframeState {
    pub fn new(binding: Binding, resume_after: Option<u64>, frame_samples: u32) -> Self {
        Self {
            binding,
            resume_after,
            frame_samples,
            counters: Counters::default(),
            ended: None,
            serial: 0,
            last: None,
            floor: None,
            epochs: BTreeMap::new(),
            latest: None,
            last_authenticated: HashMap::new(),
            sender: None,
            securing: None,
            statuses: Vec::new(),
        }
    }

    pub fn binding(&self) -> &Binding {
        &self.binding
    }

    pub fn stopped(&self) -> bool {
        self.ended.is_some()
    }

    pub fn sending(&self) -> bool {
        self.sender.is_some()
    }

    /// Retained full epochs, oldest first (diagnostics and tests).
    pub fn retained(&self) -> Vec<u64> {
        self.epochs.keys().copied().collect()
    }

    /// Status events since the last call, in order.
    pub fn take_statuses(&mut self) -> Vec<CryptoStatus> {
        std::mem::take(&mut self.statuses)
    }

    /// Applies one control body of `bytes` serialized bytes in arrival order.
    /// `now` is the engine's monotonic clock; `wall_ms` Unix milliseconds,
    /// used only to convert `send_until`.
    pub fn control(&mut self, body: &Value, bytes: usize, now: Instant, wall_ms: u64) -> ControlReply {
        let Some(c) = (bytes <= MAX_CONTROL_BYTES).then(|| parse_control(body)).flatten() else {
            return ControlReply::bad(body, ControlError::BadKeys);
        };
        if self.ended.is_some() || c.binding != self.binding || c.serial < self.serial {
            return ControlReply::result(&c, Outcome::Ignored);
        }
        if c.serial == self.serial
            && let Some((body, reply)) = &self.last
        {
            if *body != c.body {
                return self.fail(StatusReason::KeyConflict, &c);
            }
            let mut reply = reply.clone();
            if reply.result == Some(Outcome::Applied) {
                reply.result = Some(Outcome::Unchanged);
            }
            return reply;
        }
        self.serial = c.serial;
        let reply = self.apply(&c, now, wall_ms);
        self.last = (self.ended.is_none()).then(|| (c.body, reply.clone()));
        reply
    }

    /// The engine's control queue overflowed: the run fails without applying `body`.
    pub fn overflow(&mut self, body: &Value) -> ControlReply {
        self.stop_with(StatusReason::KeyQueueFull);
        ControlReply::bad(body, ControlError::KeyQueueFull)
    }

    fn fail(&mut self, reason: StatusReason, c: &Control) -> ControlReply {
        self.stop_with(reason);
        ControlReply::error(
            c,
            if reason == StatusReason::KeyQueueFull { ControlError::KeyQueueFull } else { ControlError::KeyConflict },
        )
    }

    fn apply(&mut self, c: &Control, now: Instant, wall_ms: u64) -> ControlReply {
        let epoch = c.epoch;
        match &c.action {
            Action::Hold { removed } => {
                self.sender = None;
                self.purge(removed);
                ControlReply::result(c, Outcome::Applied)
            }
            Action::Supersede => {
                if self.resume_after.is_some_and(|floor| epoch <= floor) {
                    return ControlReply::result(c, Outcome::Ignored);
                }
                if let Some(floor) = self.floor
                    && epoch <= floor
                {
                    return ControlReply::result(c, if epoch == floor { Outcome::Unchanged } else { Outcome::Ignored });
                }
                // E's retention starts at its first supersession; retries keep it.
                for (_, keys) in self.epochs.range_mut(..epoch) {
                    keys.superseded_at.get_or_insert(now);
                }
                self.floor = Some(epoch);
                ControlReply::result(c, Outcome::Applied)
            }
            Action::Activate { send_until } => {
                if self.floor.is_some_and(|floor| epoch < floor) {
                    return ControlReply::result(c, Outcome::Ignored);
                }
                if self.floor != Some(epoch) || !self.epochs.contains_key(&epoch) {
                    return ControlReply::error(c, ControlError::NotInstalled);
                }
                if self.sender.is_some_and(|sender| sender > epoch) {
                    return ControlReply::result(c, Outcome::Ignored);
                }
                let keys = self.epochs.get_mut(&epoch).expect("installed");
                if let Some(until) = send_until {
                    let deadline = now + Duration::from_millis(until.saturating_sub(wall_ms));
                    keys.deadline = Some(keys.deadline.map_or(deadline, |d| d.min(deadline)));
                }
                let unchanged = self.sender == Some(epoch);
                self.sender = Some(epoch);
                self.securing = None;
                ControlReply::result(c, if unchanged { Outcome::Unchanged } else { Outcome::Applied })
            }
            Action::Install { own_leaf, members, removed, digest } => {
                self.install(c, *own_leaf, members, removed, *digest, now)
            }
        }
    }

    fn install(
        &mut self,
        c: &Control,
        own_leaf: u32,
        members: &[Member],
        removed: &[String],
        digest: [u8; 32],
        now: Instant,
    ) -> ControlReply {
        let epoch = c.epoch;
        let stale = |state: &mut Self| {
            state.counters.stale_install += 1;
            ControlReply::result(c, Outcome::Ignored)
        };
        if self.resume_after.is_some_and(|floor| epoch <= floor) {
            return stale(self);
        }
        match self.floor {
            Some(floor) if epoch == floor => {}
            Some(floor) if epoch < floor => return stale(self),
            _ => return ControlReply::error(c, ControlError::BadKeys),
        }
        if let Some(latest) = &self.latest {
            if latest.epoch == epoch {
                if latest.digest == digest {
                    return ControlReply::result(c, Outcome::Unchanged);
                }
                return self.fail(StatusReason::KeyConflict, c);
            }
            if epoch < latest.epoch {
                return stale(self);
            }
        }
        // Derive every key before mutating anything; keys are one-directional.
        let mut own = None;
        let mut receivers = HashMap::new();
        for m in members {
            let kid = voice_kid(epoch, m.leaf);
            if m.leaf == own_leaf {
                own = EncryptionKey::derive_from(SUITE, kid, &*m.key).ok();
                if own.is_none() {
                    return ControlReply::error(c, ControlError::BadKeys);
                }
            } else {
                let Ok(key) = DecryptionKey::derive_from(SUITE, kid, &*m.key) else {
                    return ControlReply::error(c, ControlError::BadKeys);
                };
                receivers.insert(m.leaf, Receiver { device: m.device.clone(), key, replay: Replay::default() });
            }
        }
        // Removed devices go first: a reused leaf index must not inherit them.
        self.purge(removed);
        let own = own.expect("own leaf validated");
        self.epochs
            .insert(epoch, EpochKeys { superseded_at: None, own_leaf, own, counter: 0, deadline: None, receivers });
        let devices: HashSet<String> =
            members.iter().filter(|m| m.leaf != own_leaf).map(|m| m.device.clone()).collect();
        self.last_authenticated.retain(|device, _| devices.contains(device));
        self.latest = Some(Latest { epoch, digest, devices });
        self.sweep(now);
        ControlReply::result(c, Outcome::Applied)
    }

    /// Every removal purges the device from all kept epochs; only a new
    /// validated epoch can give it keys again.
    fn purge(&mut self, removed: &[String]) {
        for device in removed {
            for keys in self.epochs.values_mut() {
                keys.receivers.retain(|_, r| r.device != *device);
            }
            self.last_authenticated.remove(device);
        }
    }

    /// Retention: the processed head, 10 s from first supersession, or a
    /// current device's latest authenticated epoch; never more than eight.
    pub fn sweep(&mut self, now: Instant) {
        if self.ended.is_some() {
            return;
        }
        let authenticated: HashSet<u64> = self.last_authenticated.values().copied().collect();
        let floor = self.floor;
        let expired: Vec<u64> = self
            .epochs
            .iter()
            .filter(|(epoch, keys)| {
                let keep = Some(**epoch) == floor
                    || keys.superseded_at.is_none_or(|at| now.saturating_duration_since(at) < RETAIN)
                    || authenticated.contains(epoch);
                !keep
            })
            .map(|(epoch, _)| *epoch)
            .collect();
        for epoch in expired {
            self.evict(epoch);
        }
        while self.epochs.len() > MAX_EPOCHS {
            let oldest = *self.epochs.keys().next().expect("nonempty");
            self.evict(oldest);
        }
    }

    fn evict(&mut self, epoch: u64) {
        self.epochs.remove(&epoch);
        if self.sender == Some(epoch) {
            self.sender = None;
        }
    }

    fn close_sending(&mut self, reason: StatusReason) {
        self.sender = None;
        if self.securing == Some(reason) {
            return;
        }
        self.securing = Some(reason);
        self.statuses.push(CryptoStatus { binding: self.binding.clone(), state: CryptoState::Securing, reason });
    }

    /// Stops the run: no later control, frame or result passes.
    pub fn stop(&mut self) {
        self.end(End::Stopped, None);
    }

    /// Stops the run and reports why (`voice://crypto` "failed").
    pub fn stop_with(&mut self, reason: StatusReason) {
        self.end(End::Failed, Some(reason));
    }

    fn end(&mut self, end: End, reason: Option<StatusReason>) {
        if self.ended.is_some() {
            return;
        }
        self.ended = Some(end);
        self.sender = None;
        self.epochs.clear();
        self.last_authenticated.clear();
        self.latest = None;
        self.last = None;
        if let Some(reason) = reason {
            self.statuses.push(CryptoStatus { binding: self.binding.clone(), state: CryptoState::Failed, reason });
        }
    }

    /// One Opus packet in, one SFrame ciphertext out, or `None` to drop it.
    pub fn encrypt(&mut self, plaintext: &[u8], now: Instant) -> Option<Vec<u8>> {
        if self.ended.is_some() {
            return None;
        }
        let epoch = self.sender?;
        if plaintext.is_empty() || plaintext.len() > MAX_OPUS {
            self.counters.oversize_dropped += 1;
            return None;
        }
        // Malformed Opus never leaves; a valid packet of another duration is counted but sent.
        let Some(samples) = vc_proto::opus::packet_samples(plaintext) else {
            self.counters.unexpected_duration += 1;
            return None;
        };
        if samples != self.frame_samples {
            self.counters.unexpected_duration += 1;
        }
        let keys = self.epochs.get_mut(&epoch)?;
        if keys.deadline.is_some_and(|deadline| now >= deadline) {
            self.close_sending(StatusReason::KeyExpired);
            return None;
        }
        if keys.counter >= COUNTER_LIMIT {
            self.counters.counter_exhausted += 1;
            self.close_sending(StatusReason::CounterExhausted);
            return None;
        }
        let ctr = keys.counter;
        keys.counter += 1;
        let frame = MediaFrameView::try_new(&mut SingleUseCounter::new(ctr), plaintext).ok()?;
        let encrypted = frame.encrypt(&keys.own).ok()?;
        self.counters.encrypted += 1;
        Some(encrypted.sframe_cipher_text().to_vec())
    }

    /// One SFrame ciphertext in, a validated Opus packet out, or `None`.
    pub fn decrypt(&mut self, frame: &[u8], now: Instant) -> Option<Vec<u8>> {
        if self.ended.is_some() {
            return None;
        }
        if !(MIN_FRAME..=MAX_FRAME).contains(&frame.len()) {
            self.counters.oversize_dropped += 1;
            return None;
        }
        let Some((kid, ctr, _)) = decode_header(frame) else {
            self.counters.auth_failed += 1;
            return None;
        };
        let (low, leaf) = (kid >> 32, kid as u32);
        let newest: Vec<u64> = self.epochs.keys().rev().copied().filter(|e| e & 0xffff_ffff == low).collect();
        if newest.iter().any(|e| self.epochs[e].own_leaf == leaf) {
            self.counters.own_kid_dropped += 1;
            return None;
        }
        let candidates: Vec<u64> =
            newest.into_iter().filter(|e| self.epochs[e].receivers.contains_key(&leaf)).collect();
        if candidates.is_empty() {
            self.counters.no_key += 1;
            return None;
        }
        let view = EncryptedFrameView::try_new(frame).ok()?;
        let mut replayed = false;
        for epoch in candidates {
            let receiver = self.epochs.get_mut(&epoch)?.receivers.get_mut(&leaf)?;
            if !receiver.replay.fresh(ctr) {
                replayed = true;
                continue;
            }
            // Only successful authentication and the final replay check admit a frame.
            let Ok(opened) = view.decrypt(&receiver.key) else { continue };
            receiver.replay.admit(ctr);
            let device = receiver.device.clone();
            let payload = opened.payload();
            if payload.len() > MAX_OPUS || vc_proto::opus::packet_samples(payload).is_none() {
                self.counters.unexpected_duration += 1;
                return None;
            }
            if self.latest.as_ref().is_some_and(|latest| latest.devices.contains(&device)) {
                let last = self.last_authenticated.entry(device).or_insert(epoch);
                *last = (*last).max(epoch);
            }
            self.counters.decrypted += 1;
            let out = payload.to_vec();
            self.sweep(now);
            return Some(out);
        }
        if replayed {
            self.counters.replay_dropped += 1;
        } else {
            self.counters.auth_failed += 1;
        }
        None
    }
}

#[cfg(test)]
mod tests;
