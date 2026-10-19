//! Gwar Connect accounts on a Gwar server: one identity for every device, and
//! revoked devices are signed out.

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use common::*;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Value, json};

fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

const CERTIFICATE_LIFETIME: i64 = 90 * 24 * 3600 * 1000;

fn now() -> i64 {
    gwar_server::core::now_ms()
}

fn fresh_certificate(account: &SigningKey, device: &SigningKey, mut clock: impl FnMut() -> i64) -> Value {
    let issued_at = clock();
    certificate(account, device, issued_at, issued_at + CERTIFICATE_LIFETIME)
}

/// Connects as `device`, presenting `cert`.
async fn as_device(server: &TestServer, device: &SigningKey, cert: Value) -> Result<Client, gwar_proto::ErrorBody> {
    Client::connect_with(server, device.clone(), move |raw, key| {
        let mut hello = raw.hello(key, "acct", None);
        set_certificate(&mut hello, cert, key);
        hello
    })
    .await
}

#[tokio::test]
async fn devices_of_one_account_are_one_person_until_revoked() {
    // Gwar Connect with one account and its first device.
    let connect = gwar_connect::start(gwar_connect::Config {
        database: None,
        bind: "127.0.0.1:0".parse().unwrap(),
        origins: vec![],
        client_ip_header: None,
    })
    .await
    .unwrap();
    let url = format!("http://{connect}");
    let account = SigningKey::from_bytes(&rand::random());
    let laptop = SigningKey::from_bytes(&rand::random());
    let phone = SigningKey::from_bytes(&rand::random());
    let mut first = fresh_certificate(&account, &laptop, now);
    first["name"] = json!("laptop");
    let blob = b64(&[0u8; 60]);
    let register = json!({
        "handle": "ann", "account_key": first["account_key"],
        "kdf": {"salt": b64(&[1u8; 16]), "m": 65536, "t": 3, "p": 1},
        "auth_key": b64(&[2u8; 32]), "key_blob": blob, "recovery_auth": b64(&[3u8; 32]), "recovery_blob": blob,
        "device": first,
    });
    let http = reqwest::Client::new();
    let reply: Value =
        http.post(format!("{url}/v1/register")).json(&register).send().await.unwrap().json().await.unwrap();
    let enrollment = reply["token"].as_str().unwrap().to_owned();
    let token = connect_session(&http, &url, &enrollment, &account, &laptop, first.clone()).await;

    let server = TestServer::start().await;
    tokio::spawn(gwar_server::connect::follow_every(
        url.clone(),
        server.running.core.clone(),
        0,
        Duration::from_millis(200),
    ));

    // Both devices are the same member: the account's uid.
    let laptop_client = as_device(&server, &laptop, fresh_certificate(&account, &laptop, now)).await.unwrap();
    let mut phone_client = as_device(&server, &phone, fresh_certificate(&account, &phone, now)).await.unwrap();
    let account_uid = gwar_server::identity::uid_for_key(account.verifying_key().as_bytes());
    assert_eq!(laptop_client.welcome.uid, account_uid);
    assert_eq!(phone_client.welcome.uid, account_uid);

    // Forged, expired or mismatched certificates are refused.
    let thief = SigningKey::from_bytes(&rand::random());
    assert!(as_device(&server, &thief, fresh_certificate(&thief, &phone, now)).await.is_err());
    let expired_at = now() - 1;
    let expired = certificate(&account, &phone, expired_at - CERTIFICATE_LIFETIME, expired_at);
    let err = as_device(&server, &phone, expired).await.err().expect("expired certificate refused");
    assert_eq!(err.code, gwar_proto::ErrorCode::CertificateExpired);
    assert!(as_device(&server, &thief, fresh_certificate(&account, &phone, now)).await.is_err());

    // The phone signed in on Connect too.
    let mut phone_cert = fresh_certificate(&account, &phone, now);
    phone_cert["name"] = json!("phone");
    let login: Value = http
        .post(format!("{url}/v1/login"))
        .json(&json!({"handle": "ann", "auth_key": b64(&[2; 32])}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let added = http
        .post(format!("{url}/v1/devices"))
        .bearer_auth(login["token"].as_str().unwrap())
        .json(&phone_cert)
        .send()
        .await
        .unwrap();
    assert_eq!(added.status(), 200);

    // Revoking the phone on Connect signs it out here, and keeps it out.
    let (account_key, phone_key) = (b64(account.verifying_key().as_bytes()), b64(phone.verifying_key().as_bytes()));
    let revoked_at = now();
    let statement = format!("gwar revoke v1\n{account_key}\n{phone_key}\n{revoked_at}");
    let revoke = json!({"device_key": phone_key, "revoked_at": revoked_at,
                        "signature": b64(&account.sign(statement.as_bytes()).to_bytes())});
    let status = http.post(format!("{url}/v1/devices/revoke")).bearer_auth(&token).json(&revoke).send().await.unwrap();
    assert_eq!(status.status(), 200);
    let gone = phone_client.expect_disconnect().await;
    assert_eq!(gone["kind"], "kicked");
    let again = as_device(&server, &phone, fresh_certificate(&account, &phone, now)).await;
    assert_eq!(again.err().expect("revoked").code, gwar_proto::ErrorCode::NotAuthenticated);
    // The laptop is unaffected.
    as_device(&server, &laptop, fresh_certificate(&account, &laptop, now)).await.expect("laptop still in");
}

#[tokio::test]
async fn maximum_lifetime_certificate_uses_one_clock_sample() {
    let server = TestServer::start().await;
    let (account, device) = (new_key(), new_key());
    let issued_at = now();
    let mut time = issued_at;
    let cert = fresh_certificate(&account, &device, || {
        let sample = time;
        time += 1;
        sample
    });
    let client = as_device(&server, &device, cert).await.expect("90-day certificate accepted across a clock tick");
    assert_eq!(client.welcome.uid, uid_of(&account));
    client.close().await;

    // A second clock sample would exceed the lifetime limit by one millisecond.
    let overlong = certificate(&account, &device, issued_at, issued_at + CERTIFICATE_LIFETIME + 1);
    let err = as_device(&server, &device, overlong).await.err().expect("overlong certificate refused");
    assert_eq!(err.code, gwar_proto::ErrorCode::NotAuthenticated);
    assert_eq!(err.message, "invalid identity signature");
}

#[tokio::test]
async fn an_expired_certificate_is_reported_as_such() {
    let server = TestServer::start().await;
    let account = SigningKey::from_bytes(&rand::random());
    let device = SigningKey::from_bytes(&rand::random());
    let day = 24 * 3600 * 1000;
    let expired = certificate(&account, &device, now() - 30 * day, now() - day);
    let err = as_device(&server, &device, expired).await.err().expect("refused");
    assert_eq!(err.code, gwar_proto::ErrorCode::CertificateExpired);
    // A forged certificate is not reported as merely expired.
    let mut forged = certificate(&account, &device, now() - 30 * day, now() - day);
    forged["account_key"] = json!(b64(SigningKey::from_bytes(&rand::random()).verifying_key().as_bytes()));
    let err = as_device(&server, &device, forged).await.err().expect("refused");
    assert_eq!(err.code, gwar_proto::ErrorCode::NotAuthenticated);
}

fn service_key() -> SigningKey {
    SigningKey::from_bytes(&[42; 32])
}

fn attestation(account_key: &str, handle: &str) -> gwar_proto::HandleAttestation {
    gwar_connect::service_key::attest(&service_key(), account_key, handle, now())
}

// --------------------------------------------------------- confirmed handles

#[derive(Default)]
struct FakeAccounts {
    handles: std::sync::Mutex<std::collections::HashMap<String, String>>,
    responses: std::sync::Mutex<std::collections::HashMap<String, (StatusCode, Value)>>,
    calls: std::sync::atomic::AtomicUsize,
    started: tokio::sync::Notify,
    gate: Option<tokio::sync::Semaphore>,
}

struct FakeConnect {
    url: String,
    state: std::sync::Arc<FakeAccounts>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeConnect {
    async fn start(gated: bool) -> Self {
        use axum::{Router, routing::get};
        let state = std::sync::Arc::new(FakeAccounts {
            gate: gated.then(|| tokio::sync::Semaphore::new(0)),
            ..Default::default()
        });
        let app = Router::new()
            .route("/v1/accounts/by-key/{key}", get(fake_account))
            .route("/v1/service-key", get(|| async { axum::Json(json!({"key": public_key(&service_key())})) }))
            .route("/v1/revocations", get(|| async { axum::Json(json!({"revocations": []})) }))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { url, state, task }
    }

    fn known(&self, account: &SigningKey, handle: &str) {
        self.state.handles.lock().unwrap().insert(public_key(account), handle.into());
    }

    fn reply(&self, account: &SigningKey, status: StatusCode, body: Value) {
        self.state.responses.lock().unwrap().insert(public_key(account), (status, body));
    }

    fn calls(&self) -> usize {
        self.state.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Drop for FakeConnect {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn fake_account(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<FakeAccounts>>,
    axum::extract::Path(key): axum::extract::Path<String>,
) -> (axum::http::StatusCode, axum::Json<Value>) {
    state.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    state.started.notify_one();
    if let Some(gate) = &state.gate {
        gate.acquire().await.unwrap().forget();
    }
    if let Some((status, body)) = state.responses.lock().unwrap().get(&key).cloned() {
        return (status, axum::Json(body));
    }
    match state.handles.lock().unwrap().get(&key) {
        Some(handle) => (
            StatusCode::OK,
            axum::Json(json!({"handle": handle, "account_key": key, "attestation": attestation(&key, handle)})),
        ),
        None => (StatusCode::NOT_FOUND, axum::Json(json!({"error": "not_found"}))),
    }
}

#[tokio::test]
async fn handles_are_confirmed_asynchronously_and_cached_across_devices_and_restarts() {
    let fake = FakeConnect::start(true).await;
    let account = new_key();
    let device = new_key();
    fake.known(&account, "registered");
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("gwar.sqlite3");
    let server = TestServer::start_with(|c| {
        c.database = Some(database.clone());
        c.connect_url = Some(fake.url.clone());
    })
    .await;
    let mut first = as_device(&server, &device, certificate(&account, &device, now(), now() + 86400000)).await.unwrap();
    assert!(first.welcome.members.iter().find(|m| m.uid == first.welcome.uid).unwrap().connect.is_none());
    within("account lookup started", fake.state.started.notified()).await;
    let store = gwar_server::store::Store::open(&database).unwrap();
    let id = store.user_by_uid(&first.welcome.uid).unwrap().unwrap().0;
    assert_next_check_in(&store, id, 5 * 60 * 1000);
    // Both hellos complete while Connect is still deliberately blocked.
    let second_device = new_key();
    let second = as_device(&server, &second_device, certificate(&account, &second_device, now(), now() + 86400000))
        .await
        .unwrap();
    assert_eq!(fake.calls(), 1, "coalesce simultaneous devices");
    fake.state.gate.as_ref().unwrap().add_permits(1);
    let uid = first.welcome.uid.clone();
    let changed = first.next_event_where("member.updated", |m| m["uid"] == uid).await;
    assert_eq!(changed["connect"]["handle"], "registered");
    assert_next_check_in(&store, id, 24 * 3600 * 1000);
    first.close().await;
    second.close().await;
    drop(server);
    // Opening the same DB and reconnecting still uses the persisted daily cache.
    let config = common::base_config();
    let running = gwar_server::start(gwar_server::Config {
        database: Some(database),
        connect_url: Some(fake.url.clone()),
        ..config
    })
    .await
    .unwrap();
    let restarted = TestServer { running, admin_token: String::new() };
    let again = as_device(&restarted, &device, certificate(&account, &device, now(), now() + 86400000)).await.unwrap();
    assert_eq!(
        again.welcome.members.iter().find(|m| m.uid == uid).unwrap().connect.as_ref().map(|a| a.handle.as_str()),
        Some("registered")
    );
    assert_eq!(fake.calls(), 1);
}

#[tokio::test]
async fn unknown_accounts_clear_the_cache_and_an_outage_keeps_it() {
    let fake = FakeConnect::start(false).await;
    let known = new_key();
    let unknown = new_key();
    let outage = new_key();
    fake.known(&known, "current");
    fake.reply(&outage, StatusCode::SERVICE_UNAVAILABLE, json!({"error": "unavailable"}));
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("gwar.sqlite3");
    let store = gwar_server::store::Store::open(&database).unwrap();
    for key in [&known, &unknown, &outage] {
        let user = store.touch_user(&uid_of(key), &public_key(key), "member", 1).unwrap();
        store.set_connect_attestation(user.id, Some(&attestation(&public_key(key), "old"))).unwrap();
        store.schedule_connect(user.id, now() - 1).unwrap();
    }
    let server = TestServer::start_with(|c| {
        c.database = Some(database);
        c.connect_url = Some(fake.url.clone());
    })
    .await;
    let device = new_key();
    let mut changed = as_device(&server, &device, certificate(&known, &device, now(), now() + 86400000)).await.unwrap();
    assert_eq!(
        changed.next_event_where("member.updated", |m| m["uid"] == uid_of(&known)).await["connect"]["handle"],
        "current"
    );
    let mut missing =
        as_device(&server, &device, certificate(&unknown, &device, now(), now() + 86400000)).await.unwrap();
    let event = missing.next_event_where("member.updated", |m| m["uid"] == uid_of(&unknown)).await;
    assert!(event.get("connect").is_none(), "404 removes a formerly confirmed handle");
    let down = as_device(&server, &device, certificate(&outage, &device, now(), now() + 86400000)).await.unwrap();
    within("all lookups", async {
        while fake.calls() < 3 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert_eq!(
        store.user_by_uid(&uid_of(&outage)).unwrap().unwrap().1.connect.as_ref().map(|a| a.handle.as_str()),
        Some("old")
    );
    for key in [&unknown, &outage] {
        let again = as_device(&server, &device, certificate(key, &device, now(), now() + 86400000)).await.unwrap();
        let member = again.welcome.members.iter().find(|m| m.uid == uid_of(key)).unwrap();
        assert_eq!(
            member.connect.as_ref().map(|a| a.handle.as_str()),
            if uid_of(key) == uid_of(&unknown) { None } else { Some("old") }
        );
    }
    assert_eq!(fake.calls(), 3, "negative and failed lookups wait until their next check");
    drop(down);
}

fn assert_next_check_in(store: &gwar_server::store::Store, id: i64, delay: i64) {
    let next = store.connect_next_check(id).unwrap().expect("lookup scheduled");
    assert!((next - (now() + delay)).abs() < 5000, "unexpected next check: {next}");
}

#[tokio::test]
async fn failed_lookups_retry_after_five_minutes_across_restarts() {
    let account = new_key();
    let key = public_key(&account);
    let wrong_service = gwar_connect::service_key::attest(&new_key(), &key, "registered", now());
    let wrong_account = attestation(&public_key(&new_key()), "registered");
    let expired = gwar_connect::service_key::attest(&service_key(), &key, "registered", now() - 8 * 86400000);
    let future = gwar_connect::service_key::attest(&service_key(), &key, "registered", now() + 6 * 60 * 1000);
    let responses = [
        (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "unavailable"})),
        (StatusCode::CREATED, json!({"handle": "registered", "account_key": public_key(&account)})),
        (StatusCode::OK, json!({"invalid": "body"})),
        (StatusCode::OK, json!({"handle": "registered", "account_key": public_key(&new_key())})),
        (StatusCode::OK, json!({"handle": "Invalid!", "account_key": public_key(&account)})),
        (StatusCode::OK, json!({"handle": "registered", "account_key": key, "attestation": wrong_service})),
        (StatusCode::OK, json!({"handle": "registered", "account_key": key, "attestation": wrong_account})),
        (StatusCode::OK, json!({"handle": "registered", "account_key": key, "attestation": expired})),
        (StatusCode::OK, json!({"handle": "registered", "account_key": key, "attestation": future})),
    ];
    for (status, body) in responses {
        let fake = FakeConnect::start(false).await;
        fake.reply(&account, status, body);
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("gwar.sqlite3");
        let store = gwar_server::store::Store::open(&database).unwrap();
        let id = store.touch_user(&uid_of(&account), &public_key(&account), "Account", 1).unwrap().id;
        store.set_connect_attestation(id, Some(&attestation(&public_key(&account), "cached"))).unwrap();
        let server = TestServer::start_with(|c| {
            c.database = Some(database.clone());
            c.connect_url = Some(fake.url.clone());
        })
        .await;
        let device = new_key();
        let first = as_device(&server, &device, certificate(&account, &device, now(), now() + 86400000)).await.unwrap();
        within("failed account lookup", fake.state.started.notified()).await;
        assert_next_check_in(&store, id, 5 * 60 * 1000);
        first.close().await;
        drop(server);
        // A restart leaves only the persisted retry time to suppress another lookup.
        let running = gwar_server::start(gwar_server::Config {
            database: Some(database),
            connect_url: Some(fake.url.clone()),
            ..common::base_config()
        })
        .await
        .unwrap();
        let restarted = TestServer { running, admin_token: String::new() };
        let again =
            as_device(&restarted, &device, certificate(&account, &device, now(), now() + 86400000)).await.unwrap();
        assert_eq!(
            again
                .welcome
                .members
                .iter()
                .find(|m| m.uid == uid_of(&account))
                .unwrap()
                .connect
                .as_ref()
                .map(|a| a.handle.as_str()),
            Some("cached")
        );
        assert_next_check_in(&store, id, 5 * 60 * 1000);
        assert_eq!(fake.calls(), 1);
        again.close().await;
        fake.state.responses.lock().unwrap().remove(&public_key(&account));
        fake.known(&account, "registered");
        store.schedule_connect(id, now() - 1).unwrap();
        let mut retry =
            as_device(&restarted, &device, certificate(&account, &device, now(), now() + 86400000)).await.unwrap();
        assert_eq!(
            retry.next_event_where("member.updated", |m| m["uid"] == uid_of(&account)).await["connect"]["handle"],
            "registered"
        );
        assert_eq!(fake.calls(), 2);
        assert_next_check_in(&store, id, 24 * 3600 * 1000);
    }
}

#[tokio::test]
async fn unknown_accounts_retry_after_an_hour() {
    let fake = FakeConnect::start(false).await;
    let account = new_key();
    let device = new_key();
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("gwar.sqlite3");
    let server = TestServer::start_with(|c| {
        c.database = Some(database.clone());
        c.connect_url = Some(fake.url.clone());
    })
    .await;
    let first = as_device(&server, &device, certificate(&account, &device, now(), now() + 86400000)).await.unwrap();
    let store = gwar_server::store::Store::open(&database).unwrap();
    let id = store.user_by_uid(&uid_of(&account)).unwrap().unwrap().0;
    within("404 retry scheduled", async {
        while store.connect_next_check(id).unwrap().unwrap() < now() + 59 * 60 * 1000 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert_next_check_in(&store, id, 3600 * 1000);
    first.close().await;
    fake.known(&account, "registered");
    let again = as_device(&server, &device, certificate(&account, &device, now(), now() + 86400000)).await.unwrap();
    assert!(again.welcome.members.iter().find(|m| m.uid == uid_of(&account)).unwrap().connect.is_none());
    assert_eq!(fake.calls(), 1);
    again.close().await;
    store.schedule_connect(id, now() - 1).unwrap();
    let mut retry = as_device(&server, &device, certificate(&account, &device, now(), now() + 86400000)).await.unwrap();
    assert_eq!(
        retry.next_event_where("member.updated", |m| m["uid"] == uid_of(&account)).await["connect"]["handle"],
        "registered"
    );
    assert_eq!(fake.calls(), 2);
}

#[tokio::test]
async fn successful_lookups_schedule_a_day_even_when_the_handle_is_unchanged() {
    let fake = FakeConnect::start(false).await;
    let account = new_key();
    let device = new_key();
    fake.known(&account, "registered");
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("gwar.sqlite3");
    let store = gwar_server::store::Store::open(&database).unwrap();
    let id = store.touch_user(&uid_of(&account), &public_key(&account), "Account", 1).unwrap().id;
    let previous =
        gwar_connect::service_key::attest(&service_key(), &public_key(&account), "registered", now() - 86400000);
    store.set_connect_attestation(id, Some(&previous)).unwrap();
    store.schedule_connect(id, now() - 1).unwrap();
    let server = TestServer::start_with(|c| {
        c.database = Some(database);
        c.connect_url = Some(fake.url.clone());
    })
    .await;
    let mut first = as_device(&server, &device, certificate(&account, &device, now(), now() + 86400000)).await.unwrap();
    within("successful lookup scheduled", async {
        while store.connect_next_check(id).unwrap().unwrap() < now() + 23 * 3600 * 1000 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert_next_check_in(&store, id, 24 * 3600 * 1000);
    let updated = first.next_event_where("member.updated", |m| m["uid"] == uid_of(&account)).await;
    assert_eq!(updated["connect"]["handle"], "registered");
    let stored = store.user_by_uid(&uid_of(&account)).unwrap().unwrap().1.connect.unwrap();
    assert_ne!(stored, previous);
    assert_eq!(serde_json::to_value(stored).unwrap(), updated["connect"]);
    first.close().await;
    let again = as_device(&server, &device, certificate(&account, &device, now(), now() + 86400000)).await.unwrap();
    assert_eq!(
        again
            .welcome
            .members
            .iter()
            .find(|m| m.uid == uid_of(&account))
            .unwrap()
            .connect
            .as_ref()
            .map(|a| a.handle.as_str()),
        Some("registered")
    );
    assert_eq!(fake.calls(), 1);
}

#[tokio::test]
async fn an_account_key_without_a_device_keeps_its_handle() {
    // A browser that signed out but kept the account's key is still that account.
    let fake = FakeConnect::start(false).await;
    let account = new_key();
    fake.known(&account, "current");
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("gwar.sqlite3");
    let store = gwar_server::store::Store::open(&database).unwrap();
    let id = store.touch_user(&uid_of(&account), &public_key(&account), "Account", 1).unwrap().id;
    store.set_connect_attestation(id, Some(&attestation(&public_key(&account), "old"))).unwrap();
    store.schedule_connect(id, now() - 1).unwrap();
    let server = TestServer::start_with(|c| {
        c.database = Some(database.clone());
        c.connect_url = Some(fake.url.clone());
    })
    .await;
    let mut client = Client::connect_as(&server, account.clone(), "ignored", None).await.unwrap();
    assert_eq!(
        client
            .welcome
            .members
            .iter()
            .find(|m| m.uid == uid_of(&account))
            .unwrap()
            .connect
            .as_ref()
            .map(|a| a.handle.as_str()),
        Some("old")
    );
    let changed = client.next_event_where("member.updated", |m| m["uid"] == uid_of(&account)).await;
    assert_eq!(changed["connect"]["handle"], "current");
    assert_eq!(fake.calls(), 1);
    client.close().await;
    // Within a day the next device-less sign-in neither asks Connect again nor forgets the handle.
    let again = Client::connect_as(&server, account.clone(), "ignored", None).await.unwrap();
    assert_eq!(
        again
            .welcome
            .members
            .iter()
            .find(|m| m.uid == uid_of(&account))
            .unwrap()
            .connect
            .as_ref()
            .map(|a| a.handle.as_str()),
        Some("current")
    );
    assert_eq!(fake.calls(), 1);
}

#[tokio::test]
async fn local_identities_and_no_connect_never_show_a_handle() {
    let fake = FakeConnect::start(false).await;
    let local = new_key();
    fake.known(&local, "registered");
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("gwar.sqlite3");
    let store = gwar_server::store::Store::open(&database).unwrap();
    let id = store.touch_user(&uid_of(&local), &public_key(&local), "Local", 1).unwrap().id;
    let server = TestServer::start_with(|c| {
        c.database = Some(database.clone());
        c.connect_url = Some(fake.url.clone());
    })
    .await;
    // A key never seen with a device certificate is not sent to Connect.
    let client = Client::connect_as(&server, local.clone(), "ignored", None).await.unwrap();
    assert!(client.welcome.members.iter().find(|m| m.uid == uid_of(&local)).unwrap().connect.is_none());
    assert_eq!(fake.calls(), 0);
    client.close().await;
    drop(server);
    store.set_connect_attestation(id, Some(&attestation(&public_key(&local), "registered"))).unwrap();
    store.schedule_connect(id, now() + 24 * 3600 * 1000).unwrap();
    let running =
        gwar_server::start(gwar_server::Config { database: Some(database), ..common::base_config() }).await.unwrap();
    let disabled = TestServer { running, admin_token: String::new() };
    let device = new_key();
    let client = as_device(&disabled, &device, certificate(&local, &device, now(), now() + 86400000)).await.unwrap();
    assert!(client.welcome.members.iter().find(|m| m.uid == uid_of(&local)).unwrap().connect.is_none());
    assert_eq!(store.connect_next_check(id).unwrap(), None);
    assert_eq!(fake.calls(), 0);
}

#[tokio::test]
async fn expired_cached_attestations_are_hidden_and_refreshed_without_a_device() {
    let fake = FakeConnect::start(false).await;
    let account = new_key();
    fake.known(&account, "current");
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("gwar.sqlite3");
    let store = gwar_server::store::Store::open(&database).unwrap();
    let id = store.touch_user(&uid_of(&account), &public_key(&account), "Account", 1).unwrap().id;
    let expired = gwar_connect::service_key::attest(&service_key(), &public_key(&account), "old", now() - 8 * 86400000);
    store.set_connect_attestation(id, Some(&expired)).unwrap();
    store.schedule_connect(id, now() + 86400000).unwrap();
    assert!(store.user_by_uid(&uid_of(&account)).unwrap().unwrap().1.connect.is_none());
    assert!(store.members(0, 10).unwrap()[0].connect.is_none());
    assert!(store.member_rows(None).unwrap()[0].member.connect.is_none());
    let server = TestServer::start_with(|c| {
        c.database = Some(database);
        c.connect_url = Some(fake.url.clone());
    })
    .await;
    let client = Client::connect_as(&server, account.clone(), "ignored", None).await.unwrap();
    assert!(client.welcome.members.iter().find(|m| m.uid == uid_of(&account)).unwrap().connect.is_none());
    assert_eq!(fake.calls(), 0);
    client.close().await;
    store.schedule_connect(id, now() - 1).unwrap();
    let mut again = Client::connect_as(&server, account.clone(), "ignored", None).await.unwrap();
    let updated = again.next_event_where("member.updated", |m| m["uid"] == uid_of(&account)).await;
    let a: gwar_proto::HandleAttestation = serde_json::from_value(updated["connect"].clone()).unwrap();
    assert_eq!(a.handle, "current");
    assert_eq!(a.account_key, public_key(&account));
    assert!(gwar_server::identity::signed_by(
        &public_key(&service_key()),
        &gwar_proto::handle_statement(&a),
        &a.signature
    ));
    assert_eq!(store.user_by_uid(&uid_of(&account)).unwrap().unwrap().1.connect, Some(a));
    assert_next_check_in(&store, id, 24 * 3600 * 1000);
}

#[tokio::test]
async fn a_404_clears_even_an_expired_cached_attestation() {
    let fake = FakeConnect::start(false).await;
    let account = new_key();
    let device = new_key();
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("gwar.sqlite3");
    let store = gwar_server::store::Store::open(&database).unwrap();
    let id = store.touch_user(&uid_of(&account), &public_key(&account), "Account", 1).unwrap().id;
    let expired = gwar_connect::service_key::attest(&service_key(), &public_key(&account), "old", now() - 8 * 86400000);
    store.set_connect_attestation(id, Some(&expired)).unwrap();
    let server = TestServer::start_with(|c| {
        c.database = Some(database);
        c.connect_url = Some(fake.url.clone());
    })
    .await;
    let client = as_device(&server, &device, certificate(&account, &device, now(), now() + 86400000)).await.unwrap();
    assert!(client.welcome.members.iter().find(|m| m.uid == uid_of(&account)).unwrap().connect.is_none());
    within("expired cache cleared", async {
        while store.has_connect_attestation(id).unwrap() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert_next_check_in(&store, id, 3600 * 1000);
}

async fn connect_session(
    http: &reqwest::Client,
    url: &str,
    token: &str,
    account: &SigningKey,
    device: &SigningKey,
    cert: Value,
) -> String {
    let reply: Value = http
        .post(format!("{url}/v1/challenges"))
        .bearer_auth(token)
        .json(&json!({"purpose": "session", "device": cert}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let c = gwar_connect::store::Challenge {
        nonce: reply["challenge"].as_str().unwrap().into(),
        token_hash: gwar_connect::crypto::hash(token.as_bytes()),
        account_id: 0,
        purpose: "session".into(),
        certificate_hash: gwar_connect::crypto::certificate_hash(
            &public_key(account),
            &public_key(device),
            &gwar_connect::store::Certificate {
                issued_at: cert["issued_at"].as_i64().unwrap(),
                expires_at: cert["expires_at"].as_i64().unwrap(),
                signature: cert["signature"].as_str().unwrap().into(),
            },
        ),
        device_key: None,
        certificate_expires_at: 0,
        credential_version: 0,
        expires_at: reply["expires_at"].as_i64().unwrap(),
    };
    let statement =
        gwar_connect::crypto::session_statement(reply["service_key"].as_str().unwrap(), &public_key(account), &c);
    let reply: Value = http.post(format!("{url}/v1/session")).bearer_auth(token).json(&json!({"challenge": c.nonce, "device": cert, "signature": b64(&device.sign(statement.as_bytes()).to_bytes())})).send().await.unwrap().json().await.unwrap();
    reply["token"].as_str().unwrap().into()
}

fn signed_revocation(account: &SigningKey, device: &SigningKey, seq: i64) -> gwar_server::core::RevokedDevice {
    let at = now();
    let statement = gwar_server::identity::revoke_statement(&public_key(account), &public_key(device), at);
    gwar_server::core::RevokedDevice {
        seq,
        account_key: public_key(account),
        device_key: public_key(device),
        revoked_at: at,
        signature: b64(&account.sign(statement.as_bytes()).to_bytes()),
    }
}

#[tokio::test]
async fn cross_account_revocations_do_not_poison_login_or_live_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("gwar.sqlite3");
    let server = TestServer::start_with(|c| c.database = Some(database.clone())).await;
    let victim = new_key();
    let attacker = new_key();
    let device = new_key();
    let mut live = as_device(&server, &device, certificate(&victim, &device, now(), now() + 86400000)).await.unwrap();
    let mut other =
        as_device(&server, &device, certificate(&attacker, &device, now(), now() + 86400000)).await.unwrap();
    server.running.core.revoked(vec![signed_revocation(&attacker, &device, 1)], 1).await;
    other.expect_disconnect().await;
    live.ok("ping", json!({})).await;
    let replacement =
        as_device(&server, &device, certificate(&victim, &device, now(), now() + 86400000)).await.unwrap();
    assert_eq!(live.expect_disconnect().await["kind"], "replaced");
    live = replacement;
    let store = gwar_server::store::Store::open(&database).unwrap();
    assert!(store.device_revoked(&public_key(&attacker), &public_key(&device)).unwrap());
    assert!(!store.device_revoked(&public_key(&victim), &public_key(&device)).unwrap());
    // An old feed entry cannot revoke the victim or move the cursor backwards.
    server.running.core.revoked(vec![signed_revocation(&victim, &device, 0)], 0).await;
    live.ok("ping", json!({})).await;
    assert_eq!(store.meta("connect_seq").unwrap().as_deref(), Some("1"));
    server.running.core.revoked(vec![signed_revocation(&victim, &device, 2)], 2).await;
    live.expect_disconnect().await;
    server.running.core.revoked(vec![], 3).await;
    server.running.core.info().await.unwrap();
    assert_eq!(store.meta("connect_seq").unwrap().as_deref(), Some("3"));
    drop(server);
    let running = gwar_server::start(gwar_server::Config { database: Some(database), ..base_config() }).await.unwrap();
    let restarted = TestServer { running, admin_token: String::new() };
    assert!(as_device(&restarted, &device, certificate(&victim, &device, now(), now() + 86400000)).await.is_err());
    assert!(store.device_revoked(&public_key(&victim), &public_key(&device)).unwrap());
}

#[tokio::test]
async fn certificate_expiry_disconnects_an_idle_session_and_ends_voice_routing() {
    let server = TestServer::start().await;
    let mut observer = Client::connect(&server, "observer").await;
    let account = new_key();
    let device = new_key();
    let mut client = as_device(&server, &device, certificate(&account, &device, now(), now() + 600)).await.unwrap();
    let session = client.welcome.session;
    client.ok("channel.join", json!({"channel": client.default_channel()})).await;
    assert!(server.running.plane.routing.load().members.contains_key(&session));
    let reason = client.expect_disconnect().await;
    assert_eq!(reason["kind"], "kicked");
    assert!(reason["reason"].as_str().unwrap().contains("certificate expired"));
    assert!(!server.running.plane.routing.load().members.contains_key(&session));
    let left = observer.next_event_where("client.left", |e| e["client"] == session).await;
    assert!(left["reason"]["reason"].as_str().unwrap().contains("certificate expired"));
    let late = Client::connect(&server, "late").await;
    assert!(!late.welcome.clients.iter().any(|c| c.id == session));
}
