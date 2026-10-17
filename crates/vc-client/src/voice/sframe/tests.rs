//! The browser state tests (`apps/web/src/voice/sframe/state.test.ts`) against
//! the native implementation, plus the shared RFC 9605 vectors.

use super::*;
use serde_json::json;

const VECTORS: &str = include_str!("../../../tests/sframe-vectors.json");

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}
fn filled(n: usize, v: u8) -> String {
    b64(&vec![v; n])
}
fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}
fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn binding_json() -> Value {
    json!({ "server_key": filled(32, 1), "session": 7, "channel": 3, "call": filled(16, 2), "participation": filled(16, 3), "run": 1, "group": filled(32, 4) })
}
fn binding() -> Binding {
    Binding::parse(&binding_json()).unwrap()
}
fn device(i: u8) -> String {
    filled(32, 10 + i)
}
const OPUS: [u8; 4] = [0xf8, 1, 2, 3]; // 20 ms CELT, one frame
const NATIVE: [u8; 4] = [0xf0, 1, 2, 3]; // 10 ms CELT, one frame

/// The exporter output every member computes for (epoch, leaf).
fn exporter(epoch: u64, leaf: u32, salt: u8) -> Vec<u8> {
    let mut key = vec![0; 32];
    key[..8].copy_from_slice(&epoch.to_be_bytes());
    key[8..12].copy_from_slice(&leaf.to_be_bytes());
    key[12] = salt;
    key
}
fn members_salted(epoch: u64, leaves: &[(u32, u8)], salt: u8) -> Value {
    leaves
        .iter()
        .map(|&(leaf, d)| json!({ "leaf": leaf, "device": device(d), "key": b64(&exporter(epoch, leaf, salt)) }))
        .collect()
}
fn members(epoch: u64, leaves: &[(u32, u8)]) -> Value {
    members_salted(epoch, leaves, 0)
}

struct Engine {
    state: SframeState,
    serial: u64,
    now: Instant,
    wall: u64,
}

impl Engine {
    fn new() -> Self {
        Self::resuming(None)
    }
    fn resuming(resume_after: Option<u64>) -> Self {
        Self {
            state: SframeState::new(binding(), resume_after, 960),
            serial: 0,
            now: Instant::now(),
            wall: 1_800_000_000_000,
        }
    }
    fn raw(&mut self, body: Value) -> ControlReply {
        let bytes = body.to_string().len();
        self.state.control(&body, bytes, self.now, self.wall)
    }
    fn send(&mut self, action: &str, epoch: u64, extra: Value) -> ControlReply {
        self.serial += 1;
        let mut body = json!({ "binding": binding_json(), "serial": self.serial.to_string(), "action": action, "epoch": epoch.to_string() });
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        self.raw(body)
    }
    fn ok(&mut self, action: &str, epoch: u64, extra: Value) {
        let reply = self.send(action, epoch, extra);
        assert_eq!(reply.result, Some(Outcome::Applied), "{action} {epoch}: {reply:?}");
    }
    fn install(&mut self, epoch: u64, own: u32, all: &Value, removed: &[String]) -> ControlReply {
        self.send("install", epoch, json!({ "own_leaf": own, "members": all, "removed": removed }))
    }
    /// Barrier, install and activation of a processed epoch.
    fn enter(&mut self, epoch: u64, own: u32, all: &Value, removed: &[String]) {
        self.ok("supersede", epoch, json!({}));
        assert_eq!(self.install(epoch, own, all, removed).result, Some(Outcome::Applied));
        self.ok("activate", epoch, json!({ "send_until": null }));
    }
    fn encrypt(&mut self, pt: &[u8]) -> Option<Vec<u8>> {
        self.state.encrypt(pt, self.now)
    }
    fn decrypt(&mut self, frame: &[u8]) -> Option<Vec<u8>> {
        self.state.decrypt(frame, self.now)
    }
    fn sweep(&mut self) {
        self.state.sweep(self.now)
    }
}

fn sealed(epoch: u64, leaf: u32, base: &[u8], ctr: u64, pt: &[u8]) -> Vec<u8> {
    let key = EncryptionKey::derive_from(SUITE, voice_kid(epoch, leaf), base).unwrap();
    let frame = MediaFrameView::try_new(&mut SingleUseCounter::new(ctr), pt).unwrap();
    frame.encrypt(&key).unwrap().sframe_cipher_text().to_vec()
}

#[test]
fn header_vectors_and_rejections() {
    let v: Value = serde_json::from_str(VECTORS).unwrap();
    let headers = v["headers"].as_array().unwrap();
    assert_eq!(headers.len(), 289);
    for h in headers {
        let (kid, ctr) = (h["kid"].as_str().unwrap().parse().unwrap(), h["ctr"].as_str().unwrap().parse().unwrap());
        let bytes = hex(h["header"].as_str().unwrap());
        assert_eq!(to_hex(&encode_header(kid, ctr)), h["header"].as_str().unwrap(), "{kid}/{ctr}");
        let mut extended = bytes.clone();
        extended.extend([0xaa, 0xbb]);
        assert_eq!(decode_header(&extended), Some((kid, ctr, bytes.len())));
    }
    assert_eq!(encode_header(0, 0), vec![0]);
    assert_eq!(encode_header(u64::MAX, u64::MAX).len(), 17);
    for bytes in [
        &[][..],
        &[0x80],
        &[0x08],
        &[0x81, 0x01],
        &[0x88, 0x07],
        &[0x08, 0x07],
        &[0x09, 0x00, 0xff],
        &[0x90, 0x00, 0x08],
        &[0x8f, 0x08],
    ] {
        assert_eq!(decode_header(bytes), None, "{bytes:?}");
    }
}

#[test]
fn encryption_vector_with_metadata_proves_the_aad_order() {
    let v: Value = serde_json::from_str(VECTORS).unwrap();
    let e = &v["encryption"][0];
    assert_eq!(e["cipher_suite"], json!(0x0004));
    let field = |k: &str| hex(e[k].as_str().unwrap());
    let kid: u64 = e["kid"].as_str().unwrap().parse().unwrap();
    let ctr: u64 = e["ctr"].as_str().unwrap().parse().unwrap();
    let enc = EncryptionKey::derive_from(SUITE, kid, field("base_key")).unwrap();
    let metadata = field("metadata");
    let pt = field("pt");
    let frame = MediaFrameView::try_with_meta_data(&mut SingleUseCounter::new(ctr), &pt, &metadata).unwrap();
    let ct = frame.encrypt(&enc).unwrap();
    // `as_ref` is metadata || header || ciphertext; the SFrame bytes exclude metadata.
    assert_eq!(to_hex(ct.sframe_cipher_text()), e["ct"].as_str().unwrap());
    let dec = DecryptionKey::derive_from(SUITE, kid, field("base_key")).unwrap();
    let expected = field("ct");
    let view = EncryptedFrameView::try_with_meta_data(&expected, &metadata).unwrap();
    assert_eq!(view.decrypt(&dec).unwrap().payload(), &pt[..]);
    // Without the metadata, or with any byte flipped, authentication fails.
    assert!(EncryptedFrameView::try_new(&expected).unwrap().decrypt(&dec).is_err());
    for i in 0..expected.len() {
        let mut bad = expected.clone();
        bad[i] ^= 1;
        let opened = EncryptedFrameView::try_with_meta_data(&bad, &metadata).ok().and_then(|f| f.decrypt(&dec).ok());
        assert!(opened.is_none(), "byte {i}");
    }
}

#[test]
fn round_trips_between_two_devices_with_one_directional_keys() {
    let (mut a, mut b) = (Engine::new(), Engine::new());
    let all = members(5, &[(0, 0), (1, 1)]);
    a.enter(5, 0, &all, &[]);
    b.enter(5, 1, &all, &[]);
    let frame = a.encrypt(&OPUS).unwrap();
    assert_eq!(decode_header(&frame).map(|h| (h.0, h.1)), Some((voice_kid(5, 0), 0)));
    assert_eq!(b.decrypt(&frame).as_deref(), Some(&OPUS[..]));
    // A device never decrypts its own kid, before trying any key.
    assert_eq!(a.decrypt(&frame), None);
    assert_eq!(a.state.counters.own_kid_dropped, 1);
    assert_eq!(a.state.counters.auth_failed + a.state.counters.no_key, 0);
    assert_eq!(b.state.counters.decrypted, 1);
}

#[test]
fn starts_closed_and_drops_without_keys() {
    let mut a = Engine::new();
    assert_eq!(a.encrypt(&OPUS), None);
    assert_eq!(a.decrypt(&sealed(1, 1, &[0; 32], 0, &OPUS)), None);
    assert_eq!(a.state.counters.no_key, 1);
}

#[test]
fn admits_each_counter_once_across_slots() {
    let (mut a, mut b) = (Engine::new(), Engine::new());
    let all = members(1, &[(0, 0), (1, 1)]);
    a.enter(1, 0, &all, &[]);
    b.enter(1, 1, &all, &[]);
    let frame = a.encrypt(&OPUS).unwrap();
    let opened = (0..3).filter(|_| b.decrypt(&frame).is_some()).count();
    assert_eq!(opened, 1);
    assert_eq!(b.state.counters.replay_dropped, 2);
}

#[test]
fn never_mutates_receive_state_on_failed_authentication() {
    let (mut a, mut b) = (Engine::new(), Engine::new());
    let all = members(1, &[(0, 0), (1, 1)]);
    a.enter(1, 0, &all, &[]);
    b.enter(1, 1, &all, &[]);
    let frame = a.encrypt(&OPUS).unwrap();
    for i in 0..frame.len() {
        let mut bad = frame.clone();
        bad[i] ^= 0x40;
        assert_eq!(b.decrypt(&bad), None, "byte {i}");
    }
    // Plaintext injection and truncation are rejected before cryptography.
    assert_eq!(b.decrypt(&OPUS), None);
    assert_eq!(b.decrypt(&frame[..16]), None);
    assert_eq!(b.state.counters.decrypted, 0);
    assert_eq!(b.decrypt(&frame).as_deref(), Some(&OPUS[..]));
}

#[test]
fn drops_frames_outside_17_to_1033_bytes_and_invalid_opened_opus() {
    let (mut a, mut b) = (Engine::new(), Engine::new());
    let all = members(1, &[(0, 0), (1, 1)]);
    a.enter(1, 0, &all, &[]);
    b.enter(1, 1, &all, &[]);
    assert_eq!(a.encrypt(&[0; 1001]), None);
    assert_eq!(a.state.counters.oversize_dropped, 1);
    assert_eq!(b.decrypt(&[0; 1034]), None);
    assert_eq!(b.state.counters.oversize_dropped, 1);
    // The sender drops malformed Opus before encryption and counts other durations.
    assert_eq!(a.encrypt(&[0xf1, 1]), None); // CBR two frames, odd body
    assert_eq!(a.state.counters.unexpected_duration, 1);
    assert!(a.encrypt(&[0xf0, 1]).is_some()); // valid 10 ms: counted, sent
    assert_eq!(a.state.counters.unexpected_duration, 2);
    let mut widest = vec![7; 1000];
    widest[0] = 0xf8;
    let widest = a.encrypt(&widest).unwrap();
    assert!(widest.len() <= 1033);
    assert_eq!(b.decrypt(&widest).map(|p| p.len()), Some(1000));
    // A malformed packet under another sender's key is dropped after opening.
    let invalid = sealed(1, 0, &exporter(1, 0, 0), 1000, &[0xf1, 1]);
    assert_eq!(b.decrypt(&invalid), None);
    assert_eq!(b.state.counters.unexpected_duration, 1);
}

#[test]
fn native_ten_ms_packets_are_the_expected_duration() {
    let mut a = Engine { state: SframeState::new(binding(), None, 480), ..Engine::new() };
    a.enter(1, 0, &members(1, &[(0, 0), (1, 1)]), &[]);
    assert!(a.encrypt(&NATIVE).is_some());
    assert_eq!(a.state.counters.unexpected_duration, 0);
}

#[test]
fn follows_the_128_counter_window_and_the_widest_header_encodings() {
    let mut b = Engine::new();
    let epoch = 0xffff_ffff;
    let all = members(epoch, &[(0, 0), (0xffff_ffff, 1)]);
    b.enter(epoch, 0, &all, &[]);
    let base = exporter(epoch, 0xffff_ffff, 0);
    let frame = |ctr| sealed(epoch, 0xffff_ffff, &base, ctr, &OPUS);
    assert_eq!(frame(u64::MAX).len(), 17 + OPUS.len() + 16);
    assert_eq!(b.decrypt(&frame(u64::MAX)).as_deref(), Some(&OPUS[..]));
    assert_eq!(b.decrypt(&frame(u64::MAX - 127)).as_deref(), Some(&OPUS[..]));
    assert_eq!(b.decrypt(&frame(u64::MAX - 128)), None);
    assert_eq!(b.state.counters.replay_dropped, 1);
}

#[test]
fn replay_window_marks_and_shifts() {
    let mut r = Replay::default();
    assert!(r.admit(5));
    assert!(!r.admit(5));
    assert!(r.admit(3));
    assert!(r.admit(200));
    assert!(!r.fresh(72)); // 128 behind
    assert!(r.fresh(73));
    assert!(r.admit(73));
    assert!(!r.admit(73));
    assert!(r.admit(200 + 500));
    assert!(r.fresh(699) && !r.fresh(700));
}

#[test]
fn separates_epochs_whose_low_32_bits_collide() {
    let (mut a, mut b) = (Engine::new(), Engine::new());
    let (low, high) = (7, 7 + (1 << 32));
    a.enter(low, 0, &members(low, &[(0, 0), (1, 1)]), &[]);
    b.enter(low, 1, &members(low, &[(0, 0), (1, 1)]), &[]);
    let old = a.encrypt(&OPUS).unwrap();
    a.enter(high, 0, &members(high, &[(0, 0), (1, 1)]), &[]);
    b.enter(high, 1, &members(high, &[(0, 0), (1, 1)]), &[]);
    let fresh = a.encrypt(&OPUS).unwrap();
    assert_eq!(decode_header(&old).unwrap().0, decode_header(&fresh).unwrap().0);
    assert_eq!(b.decrypt(&fresh).as_deref(), Some(&OPUS[..]));
    assert_eq!(b.decrypt(&old).as_deref(), Some(&OPUS[..])); // the older epoch's key, newest first
    assert_eq!(b.state.retained(), vec![low, high]);
}

#[test]
fn purges_a_removed_device_everywhere_before_its_leaf_index_is_reused() {
    let (mut a, mut b, mut c) = (Engine::new(), Engine::new(), Engine::new());
    a.enter(1, 0, &members(1, &[(0, 0), (1, 1)]), &[]);
    b.enter(1, 1, &members(1, &[(0, 0), (1, 1)]), &[]);
    let before = a.encrypt(&OPUS).unwrap();
    // Device 0 is removed; device 2 takes leaf 0 in epoch 2.
    b.ok("hold", 1, json!({ "removed": [device(0)] }));
    assert_eq!(b.decrypt(&before), None);
    b.enter(2, 1, &members(2, &[(0, 2), (1, 1)]), &[device(0)]);
    c.enter(2, 0, &members(2, &[(0, 2), (1, 1)]), &[]);
    assert_eq!(b.decrypt(&before), None);
    let frame = c.encrypt(&OPUS).unwrap();
    assert_eq!(b.decrypt(&frame).as_deref(), Some(&OPUS[..]));
}

#[test]
fn an_install_purges_its_removed_devices_from_older_epochs_without_a_hold() {
    let (mut a, mut b) = (Engine::new(), Engine::new());
    a.enter(1, 0, &members(1, &[(0, 0), (1, 1)]), &[]);
    b.enter(1, 1, &members(1, &[(0, 0), (1, 1)]), &[]);
    let old = a.encrypt(&OPUS).unwrap();
    // Epoch 2 removes device 0 (leaf 0 reused by device 2); no separate hold arrived.
    b.enter(2, 1, &members(2, &[(0, 2), (1, 1)]), &[device(0)]);
    assert_eq!(b.state.retained(), vec![1, 2]);
    assert_eq!(b.decrypt(&old), None);
    assert_eq!(b.state.counters.no_key, 1);
}

#[test]
fn a_device_gone_from_the_latest_tree_no_longer_retains_its_epoch() {
    let (mut a, mut b) = (Engine::new(), Engine::new());
    let one = members(1, &[(0, 0), (1, 1)]);
    a.enter(1, 0, &one, &[]);
    b.enter(1, 1, &one, &[]);
    let f = a.encrypt(&OPUS).unwrap();
    assert!(b.decrypt(&f).is_some());
    // Epoch 2 no longer contains device 0, even without an explicit removal entry.
    b.enter(2, 1, &members(2, &[(1, 1), (2, 2)]), &[]);
    b.now += RETAIN;
    b.sweep();
    assert_eq!(b.state.retained(), vec![2]);
}

#[test]
fn retains_a_silent_senders_epoch_and_expires_others_10_s_after_the_first_barrier() {
    let (mut a, mut b, mut c) = (Engine::new(), Engine::new(), Engine::new());
    let one = members(1, &[(0, 0), (1, 1), (2, 2)]);
    a.enter(1, 0, &one, &[]);
    b.enter(1, 1, &one, &[]);
    c.enter(1, 2, &one, &[]);
    let f = a.encrypt(&OPUS).unwrap();
    assert!(b.decrypt(&f).is_some());
    // Device 2 never spoke at epoch 1; device 0 did and then stays silent.
    let two = members(2, &[(0, 0), (1, 1), (2, 2)]);
    b.enter(2, 1, &two, &[]);
    c.enter(2, 2, &two, &[]);
    b.now += Duration::from_secs(10);
    b.sweep();
    assert_eq!(b.state.retained(), vec![1, 2]);
    let f = a.encrypt(&OPUS).unwrap();
    assert!(b.decrypt(&f).is_some());
    a.enter(2, 0, &two, &[]);
    let f = a.encrypt(&OPUS).unwrap();
    assert!(b.decrypt(&f).is_some());
    assert_eq!(b.state.retained(), vec![2]);
    // A late older frame cannot move the pointer back.
    let mut late = Engine::new();
    late.enter(1, 0, &one, &[]);
    let f = late.encrypt(&OPUS).unwrap();
    assert_eq!(b.decrypt(&f), None);
}

#[test]
fn retention_starts_at_the_barrier_not_the_install() {
    // Barrier for epoch 2 applied at engine time 1 s, prompt install: epoch 1 kept until 11 s.
    let mut b = Engine::new();
    let t0 = b.now;
    b.enter(1, 1, &members(1, &[(0, 0), (1, 1)]), &[]);
    b.now = t0 + Duration::from_secs(1);
    b.ok("supersede", 2, json!({}));
    assert_eq!(b.install(2, 1, &members(2, &[(0, 0), (1, 1)]), &[]).result, Some(Outcome::Applied));
    b.now = t0 + Duration::from_millis(10_999);
    b.sweep();
    assert_eq!(b.state.retained(), vec![1, 2]);
    b.now = t0 + Duration::from_secs(11);
    b.sweep();
    assert_eq!(b.state.retained(), vec![2]);
    // Barrier at 0, install retried at 3 s after a later chain epoch: deadline still 10 s.
    let mut c = Engine::new();
    let t0 = c.now;
    c.enter(1, 1, &members(1, &[(0, 0), (1, 1)]), &[]);
    c.ok("supersede", 2, json!({}));
    c.now = t0 + Duration::from_secs(3);
    c.ok("supersede", 3, json!({}));
    assert_eq!(c.install(2, 1, &members(2, &[(0, 0), (1, 1)]), &[]).result, Some(Outcome::Ignored));
    assert_eq!(c.install(3, 1, &members(3, &[(0, 0), (1, 1)]), &[]).result, Some(Outcome::Applied));
    c.now = t0 + Duration::from_millis(9_999);
    c.sweep();
    assert_eq!(c.state.retained(), vec![1, 3]);
    c.now = t0 + Duration::from_secs(10);
    c.sweep();
    assert_eq!(c.state.retained(), vec![3]);
    assert_eq!(c.state.counters.stale_install, 1);
}

#[test]
fn evicts_the_ninth_epoch_regardless_of_retention() {
    let mut b = Engine::new();
    for e in 1..=9 {
        b.enter(e, 1, &members(e, &[(0, 0), (1, 1)]), &[]);
    }
    assert_eq!(b.state.retained(), (2..=9).collect::<Vec<_>>());
}

#[test]
fn orders_controls_by_serial_and_treats_duplicates_and_conflicts_exactly() {
    let mut b = Engine::new();
    let all = members(1, &[(0, 0), (1, 1)]);
    b.ok("supersede", 1, json!({}));
    let install = json!({ "binding": binding_json(), "serial": "2", "action": "install", "epoch": "1", "own_leaf": 1, "members": all, "removed": [] });
    b.serial = 2;
    assert_eq!(b.raw(install.clone()).result, Some(Outcome::Applied));
    assert_eq!(b.raw(install.clone()).result, Some(Outcome::Unchanged));
    let mut lower = install.clone();
    lower["serial"] = json!("1");
    assert_eq!(b.raw(lower).result, Some(Outcome::Ignored));
    // A newer serial with identical keys preserves the counter and replay windows.
    b.ok("activate", 1, json!({ "send_until": null }));
    let first = decode_header(&b.encrypt(&OPUS).unwrap()).unwrap().1;
    assert_eq!(b.install(1, 1, &all, &[]).result, Some(Outcome::Unchanged));
    let second = decode_header(&b.encrypt(&OPUS).unwrap()).unwrap().1;
    assert_eq!(second, first + 1);
    // Different keys at the installed epoch stop the engine.
    let mut other = Engine::new();
    other.ok("supersede", 1, json!({}));
    other.install(1, 1, &all, &[]);
    assert_eq!(
        other.install(1, 1, &members_salted(1, &[(0, 0), (1, 1)], 9), &[]).error,
        Some(ControlError::KeyConflict)
    );
    assert_eq!(
        other.state.take_statuses(),
        vec![CryptoStatus { binding: binding(), state: CryptoState::Failed, reason: StatusReason::KeyConflict }]
    );
    assert!(other.state.stopped());
    // A different body at the same serial stops it as well, for a barrier too.
    let mut third = Engine::new();
    let barrier = json!({ "binding": binding_json(), "serial": "1", "action": "supersede", "epoch": "1" });
    third.raw(barrier.clone());
    let mut changed = barrier;
    changed["epoch"] = json!("2");
    assert_eq!(third.raw(changed).error, Some(ControlError::KeyConflict));
}

#[test]
fn refuses_future_malformed_and_unbound_controls_without_mutation() {
    let mut b = Engine::new();
    assert_eq!(b.install(1, 1, &members(1, &[(0, 0), (1, 1)]), &[]).error, Some(ControlError::BadKeys));
    assert_eq!(b.send("activate", 1, json!({ "send_until": null })).error, Some(ControlError::NotInstalled));
    let all = members(1, &[(0, 0), (1, 1)]);
    let valid = json!({ "binding": binding_json(), "serial": "50", "action": "install", "epoch": "1", "own_leaf": 1, "members": all, "removed": [] });
    assert!(parse_control(&valid).is_some());
    let with = |k: &str, v: Value| {
        let mut x = valid.clone();
        x[k] = v;
        x
    };
    let mut reversed = all.as_array().unwrap().clone();
    reversed.reverse();
    let mut dup = all.clone();
    dup[1]["device"] = all[0]["device"].clone();
    let mut short_key = all.clone();
    short_key[1]["key"] = json!(filled(31, 1));
    let mut binding_run0 = binding_json();
    binding_run0["run"] = json!(0);
    let mut binding_call15 = binding_json();
    binding_call15["call"] = json!(filled(15, 1));
    let mut padded = all.clone();
    padded[0]["key"] = json!(format!("{}=", all[0]["key"].as_str().unwrap()));
    for bad in [
        with("serial", json!("0")),
        with("serial", json!("01")),
        with("epoch", json!("18446744073709551616")),
        with("epoch", json!(1)),
        with("own_leaf", json!(2)),
        with("members", json!([])),
        with("members", Value::Array(reversed)),
        with("members", dup),
        with("members", short_key),
        with("members", padded),
        with("removed", json!([device(0), device(0)])),
        with("extra", json!(1)),
        with("binding", binding_run0),
        with("binding", binding_call15),
        json!({ "binding": binding_json(), "serial": "1", "action": "activate", "epoch": "1", "send_until": 1.5 }),
        json!({ "binding": binding_json(), "serial": "1", "action": "reset", "epoch": "1" }),
    ] {
        assert!(parse_control(&bad).is_none(), "{bad}");
    }
    assert_eq!(
        b.state.control(&with("serial", json!("2")), MAX_CONTROL_BYTES + 1, b.now, b.wall).error,
        Some(ControlError::BadKeys)
    );
    // Bad structure did not consume serial 50; a foreign binding is ignored.
    let mut foreign = binding_json();
    foreign["run"] = json!(2);
    let reply = b.raw(json!({ "binding": foreign, "serial": "60", "action": "supersede", "epoch": "1" }));
    assert_eq!(reply.result, Some(Outcome::Ignored));
    assert_eq!(
        b.raw(json!({ "binding": binding_json(), "serial": "4", "action": "supersede", "epoch": "1" })).result,
        Some(Outcome::Applied)
    );
}

#[test]
fn refuses_installs_at_or_below_a_restarts_resume_floor() {
    let mut b = Engine::resuming(Some(4));
    assert_eq!(b.send("supersede", 4, json!({})).result, Some(Outcome::Ignored));
    b.ok("supersede", 5, json!({}));
    assert_eq!(b.install(5, 1, &members(5, &[(0, 0), (1, 1)]), &[]).result, Some(Outcome::Applied));
}

#[test]
fn closes_sending_at_certificate_expiry_without_extending_it_on_clock_rollback() {
    let mut a = Engine::new();
    let all = members(1, &[(0, 0), (1, 1)]);
    a.ok("supersede", 1, json!({}));
    a.install(1, 0, &all, &[]);
    a.ok("activate", 1, json!({ "send_until": a.wall + 1_000 }));
    assert!(a.encrypt(&OPUS).is_some());
    a.wall -= 60_000; // rollback, then a newer activation of the same epoch
    assert_eq!(a.send("activate", 1, json!({ "send_until": a.wall + 61_000 })).result, Some(Outcome::Unchanged));
    a.now += Duration::from_secs(1);
    assert_eq!(a.encrypt(&OPUS), None);
    assert_eq!(
        a.state.take_statuses(),
        vec![CryptoStatus { binding: binding(), state: CryptoState::Securing, reason: StatusReason::KeyExpired }]
    );
    assert!(!a.state.sending());
}

#[test]
fn hold_stops_sending_and_counters_are_never_reused() {
    let mut a = Engine::new();
    a.enter(1, 0, &members(1, &[(0, 0), (1, 1)]), &[]);
    let first = decode_header(&a.encrypt(&OPUS).unwrap()).unwrap().1;
    a.ok("hold", 1, json!({ "removed": [] }));
    assert_eq!(a.encrypt(&OPUS), None);
    a.ok("activate", 1, json!({ "send_until": null }));
    let counters: Vec<u64> = (0..5).map(|_| decode_header(&a.encrypt(&OPUS).unwrap()).unwrap().1).collect();
    assert_eq!(first, 0);
    assert_eq!(counters, vec![1, 2, 3, 4, 5]);
}

#[test]
fn stops_at_2_pow_32_attempts_under_one_base_key() {
    let mut a = Engine::new();
    a.enter(1, 0, &members(1, &[(0, 0), (1, 1)]), &[]);
    a.state.epochs.get_mut(&1).unwrap().counter = COUNTER_LIMIT - 1;
    assert_eq!(decode_header(&a.encrypt(&OPUS).unwrap()).unwrap().1, COUNTER_LIMIT - 1);
    assert_eq!(a.encrypt(&OPUS), None);
    assert_eq!(
        a.state.take_statuses(),
        vec![CryptoStatus { binding: binding(), state: CryptoState::Securing, reason: StatusReason::CounterExhausted }]
    );
    assert_eq!(a.state.counters.counter_exhausted, 1);
}

#[test]
fn compares_installs_by_normalized_members() {
    let mut b = Engine::new();
    let all = members(1, &[(0, 0), (1, 1)]);
    b.enter(1, 1, &all, &[]);
    let reordered: Value = all
        .as_array()
        .unwrap()
        .iter()
        .map(|m| json!({ "key": m["key"], "device": m["device"], "leaf": m["leaf"] }))
        .collect();
    assert_eq!(b.install(1, 1, &reordered, &[]).result, Some(Outcome::Unchanged));
    assert!(!b.state.stopped());
}

#[test]
fn a_removed_device_that_keeps_its_keys_opens_nothing_encrypted_after_the_cutoff() {
    let (mut a, mut c) = (Engine::new(), Engine::new());
    let one = members(1, &[(0, 0), (1, 1), (2, 2)]);
    a.enter(1, 0, &one, &[]);
    c.enter(1, 2, &one, &[]);
    let before = a.encrypt(&OPUS).unwrap();
    assert!(c.decrypt(&before).is_some()); // the design's stated removal limit
    // Carol is removed: the sender holds, then moves to epoch 2 without her.
    a.ok("hold", 2, json!({ "removed": [device(2)] }));
    assert_eq!(a.encrypt(&OPUS), None);
    a.enter(2, 0, &members(2, &[(0, 0), (1, 1)]), &[device(2)]);
    for _ in 0..5 {
        let f = a.encrypt(&OPUS).unwrap();
        assert_eq!(c.decrypt(&f), None);
    }
    assert_eq!(c.state.counters.no_key, 5);
    assert_eq!(c.state.counters.decrypted, 1);
}

#[test]
fn stop_drops_everything_and_ignores_later_controls() {
    let mut a = Engine::new();
    a.enter(1, 0, &members(1, &[(0, 0), (1, 1)]), &[]);
    a.state.stop();
    assert!(a.state.stopped() && a.state.retained().is_empty());
    assert_eq!(a.encrypt(&OPUS), None);
    assert_eq!(a.send("supersede", 2, json!({})).result, Some(Outcome::Ignored));
    assert!(a.state.take_statuses().is_empty());
}

#[test]
fn replies_and_statuses_serialize_like_the_worker() {
    let reply = ControlReply { serial: "3".into(), epoch: "1".into(), result: Some(Outcome::Applied), error: None };
    assert_eq!(serde_json::to_value(&reply).unwrap(), json!({ "serial": "3", "epoch": "1", "result": "applied" }));
    let reply = ControlReply::bad(&json!({ "serial": 5 }), ControlError::BadKeys);
    assert_eq!(serde_json::to_value(&reply).unwrap(), json!({ "serial": "0", "epoch": "0", "error": "bad_keys" }));
    let status = CryptoStatus { binding: binding(), state: CryptoState::Failed, reason: StatusReason::KeyQueueFull };
    assert_eq!(
        serde_json::to_value(&status).unwrap(),
        json!({ "binding": binding_json(), "state": "failed", "reason": "key_queue_full" })
    );
}

/// Frames sealed by this implementation; `apps/web/src/voice/sframe/state.test.ts`
/// seals and opens the same cases with WebCrypto. Regenerate with
/// `GWAR_WRITE_SFRAME_INTEROP=1 cargo test -p vc-client --lib interop`.
#[test]
fn interop_frames_match_the_shared_fixture() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/sframe-interop.json");
    let mut long = vec![0x55; 1000];
    long[0] = 0xf8;
    let cases: Vec<(u64, u32, u64, Vec<u8>)> = vec![
        (1, 0, 0, OPUS.to_vec()),
        (1 << 32 | 7, 3, 300, NATIVE.to_vec()),
        (0xffff_ffff, 0xffff_ffff, u64::MAX, OPUS.to_vec()),
        (u64::MAX, 999, 1 << 40, long),
    ];
    let frames: Vec<Value> = cases
        .iter()
        .map(|(epoch, leaf, ctr, pt)| {
            let base = exporter(*epoch, *leaf, 0x5a);
            json!({
                "epoch": epoch.to_string(), "leaf": leaf, "ctr": ctr.to_string(),
                "base_key": to_hex(&base), "pt": to_hex(pt), "frame": to_hex(&sealed(*epoch, *leaf, &base, *ctr, pt)),
            })
        })
        .collect();
    let fixture = json!({ "source": "vc-client voice::sframe (sframe 2.1.0), kid = (epoch mod 2^32) << 32 | leaf", "frames": frames });
    if std::env::var_os("GWAR_WRITE_SFRAME_INTEROP").is_some() {
        std::fs::write(path, serde_json::to_string_pretty(&fixture).unwrap() + "\n").unwrap();
    }
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(saved, fixture);
    // A receiver state opens each frame from its own leaf's exporter.
    for f in saved["frames"].as_array().unwrap() {
        let epoch: u64 = f["epoch"].as_str().unwrap().parse().unwrap();
        let leaf = f["leaf"].as_u64().unwrap() as u32;
        let own = if leaf == 0 { 1 } else { 0 };
        let mut b = Engine::new();
        let all = json!([
            { "leaf": own.min(leaf), "device": device(0), "key": b64(&if own < leaf { exporter(epoch, own, 0) } else { exporter(epoch, leaf, 0x5a) }) },
            { "leaf": own.max(leaf), "device": device(1), "key": b64(&if own < leaf { exporter(epoch, leaf, 0x5a) } else { exporter(epoch, own, 0) }) },
        ]);
        b.enter(epoch, own, &all, &[]);
        assert_eq!(b.decrypt(&hex(f["frame"].as_str().unwrap())), Some(hex(f["pt"].as_str().unwrap())), "{f}");
    }
}

#[test]
fn every_valid_opus_duration_passes_through_sframe() {
    let (mut a, mut b) = (Engine { state: SframeState::new(binding(), None, 480), ..Engine::new() }, Engine::new());
    let all = members(1, &[(0, 0), (1, 1)]);
    a.enter(1, 0, &all, &[]);
    b.enter(1, 1, &all, &[]);
    // CELT 2.5/5/10/20 ms, SILK 10-60 ms, and a code-3 packet of six 20 ms frames (120 ms).
    let packets: [&[u8]; 6] =
        [&[0x80, 1], &[0x88, 1], &[0xf0, 1], &[0xf8, 1], &[0x18, 1], &[0xfb, 0x06, 1, 1, 1, 1, 1, 1]];
    for packet in packets {
        let frame = a.encrypt(packet).expect("valid Opus is sent");
        assert_eq!(b.decrypt(&frame).as_deref(), Some(packet), "{packet:02x?}");
    }
    // Only the 10 ms packet matches the native duration; the others are counted, not dropped.
    assert_eq!(a.state.counters.unexpected_duration, 5);
    assert_eq!(b.state.counters.unexpected_duration, 0);
}
