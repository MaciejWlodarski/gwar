//! Gwar Connect accounts on a Gwar server: one identity for every device, and
//! revoked devices are signed out.

mod common;

use std::time::Duration;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use common::*;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Value, json};

fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

fn now() -> i64 {
    vc_server::core::now_ms()
}

/// A device certificate signed by `account` for `device`.
fn certificate(account: &SigningKey, device: &SigningKey, issued_at: i64, expires_at: i64) -> Value {
    let (account_key, device_key) = (b64(account.verifying_key().as_bytes()), b64(device.verifying_key().as_bytes()));
    let statement = format!("gwar device v1\n{account_key}\n{device_key}\n{issued_at}\n{expires_at}");
    json!({"account_key": account_key, "device_key": device_key, "issued_at": issued_at, "expires_at": expires_at,
           "signature": b64(&account.sign(statement.as_bytes()).to_bytes())})
}

/// Connects as `device`, presenting `cert`.
async fn as_device(server: &TestServer, device: &SigningKey, cert: Value) -> Result<Client, vc_proto::ErrorBody> {
    Client::connect_with(server, device.clone(), move |raw, key| {
        let mut hello = raw.hello(key, "acct", None);
        hello["device"] = cert;
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
    })
    .await
    .unwrap();
    let url = format!("http://{connect}");
    let account = SigningKey::from_bytes(&rand::random());
    let laptop = SigningKey::from_bytes(&rand::random());
    let phone = SigningKey::from_bytes(&rand::random());
    let year = 365 * 24 * 3600 * 1000;
    let mut first = certificate(&account, &laptop, now(), now() + year);
    first["name"] = json!("laptop");
    let blob = b64(&[0u8; 60]);
    let register = json!({
        "handle": "ann", "account_key": first["account_key"],
        "kdf": {"salt": b64(&[1u8; 16]), "m": 19456, "t": 2, "p": 1},
        "auth_key": b64(&[2u8; 32]), "key_blob": blob, "recovery_auth": b64(&[3u8; 32]), "recovery_blob": blob,
        "device": first,
    });
    let http = reqwest::Client::new();
    let reply: Value =
        http.post(format!("{url}/v1/register")).json(&register).send().await.unwrap().json().await.unwrap();
    let token = reply["token"].as_str().unwrap().to_owned();

    let server = TestServer::start().await;
    tokio::spawn(vc_server::connect::follow_every(
        url.clone(),
        server.running.core.clone(),
        0,
        Duration::from_millis(200),
    ));

    // Both devices are the same member: the account's uid.
    let laptop_client = as_device(&server, &laptop, certificate(&account, &laptop, now(), now() + year)).await.unwrap();
    let mut phone_client =
        as_device(&server, &phone, certificate(&account, &phone, now(), now() + year)).await.unwrap();
    let account_uid = vc_server::identity::uid_for_key(account.verifying_key().as_bytes());
    assert_eq!(laptop_client.welcome.uid, account_uid);
    assert_eq!(phone_client.welcome.uid, account_uid);

    // Forged, expired or mismatched certificates are refused.
    let thief = SigningKey::from_bytes(&rand::random());
    assert!(as_device(&server, &thief, certificate(&thief, &phone, now(), now() + year)).await.is_err());
    assert!(as_device(&server, &phone, certificate(&account, &phone, now() - year - 2, now() - 1)).await.is_err());
    assert!(as_device(&server, &thief, certificate(&account, &phone, now(), now() + year)).await.is_err());

    // The phone signed in on Connect too.
    let mut phone_cert = certificate(&account, &phone, now(), now() + year);
    phone_cert["name"] = json!("phone");
    let added = http.post(format!("{url}/v1/devices")).bearer_auth(&token).json(&phone_cert).send().await.unwrap();
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
    let again = as_device(&server, &phone, certificate(&account, &phone, now(), now() + year)).await;
    assert_eq!(again.err().expect("revoked").code, vc_proto::ErrorCode::NotAuthenticated);
    // The laptop is unaffected.
    as_device(&server, &laptop, certificate(&account, &laptop, now(), now() + year)).await.expect("laptop still in");
}

#[tokio::test]
async fn an_expired_certificate_is_reported_as_such() {
    let server = TestServer::start().await;
    let account = SigningKey::from_bytes(&rand::random());
    let device = SigningKey::from_bytes(&rand::random());
    let day = 24 * 3600 * 1000;
    let expired = certificate(&account, &device, now() - 30 * day, now() - day);
    let err = as_device(&server, &device, expired).await.err().expect("refused");
    assert_eq!(err.code, vc_proto::ErrorCode::CertificateExpired);
    // A forged certificate is not reported as merely expired.
    let mut forged = certificate(&account, &device, now() - 30 * day, now() - day);
    forged["account_key"] = json!(b64(SigningKey::from_bytes(&rand::random()).verifying_key().as_bytes()));
    let err = as_device(&server, &device, forged).await.err().expect("refused");
    assert_eq!(err.code, vc_proto::ErrorCode::NotAuthenticated);
}

// --------------------------------------------------------- confirmed handles

#[derive(Default)]
struct FakeAccounts {
    handles: std::sync::Mutex<std::collections::HashMap<String, String>>,
    unavailable: std::sync::Mutex<std::collections::HashSet<String>>,
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
    use axum::http::StatusCode;
    state.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    state.started.notify_one();
    if let Some(gate) = &state.gate {
        gate.acquire().await.unwrap().forget();
    }
    if state.unavailable.lock().unwrap().contains(&key) {
        return (StatusCode::SERVICE_UNAVAILABLE, axum::Json(json!({"error": "unavailable"})));
    }
    match state.handles.lock().unwrap().get(&key) {
        Some(handle) => (StatusCode::OK, axum::Json(json!({"handle": handle, "account_key": key}))),
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
    let database = dir.path().join("vc.sqlite3");
    let server = TestServer::start_with(|c| {
        c.database = Some(database.clone());
        c.connect_url = Some(fake.url.clone());
    })
    .await;
    let mut first = as_device(&server, &device, certificate(&account, &device, now(), now() + 86400000)).await.unwrap();
    assert!(first.welcome.members.iter().find(|m| m.uid == first.welcome.uid).unwrap().connect.is_none());
    within("account lookup started", fake.state.started.notified()).await;
    // Both hellos complete while Connect is still deliberately blocked.
    let second_device = new_key();
    let second = as_device(&server, &second_device, certificate(&account, &second_device, now(), now() + 86400000))
        .await
        .unwrap();
    assert_eq!(fake.calls(), 1, "coalesce simultaneous devices");
    fake.state.gate.as_ref().unwrap().add_permits(1);
    let uid = first.welcome.uid.clone();
    let changed = first.next_event_where("member.updated", |m| m["uid"] == uid).await;
    assert_eq!(changed["connect"], "registered");
    first.close().await;
    second.close().await;
    drop(server);
    // Opening the same DB and reconnecting still uses the persisted daily cache.
    let config = common::base_config();
    let running =
        vc_server::start(vc_server::Config { database: Some(database), connect_url: Some(fake.url.clone()), ..config })
            .await
            .unwrap();
    let restarted = TestServer { running, admin_token: String::new() };
    let again = as_device(&restarted, &device, certificate(&account, &device, now(), now() + 86400000)).await.unwrap();
    assert_eq!(again.welcome.members.iter().find(|m| m.uid == uid).unwrap().connect.as_deref(), Some("registered"));
    assert_eq!(fake.calls(), 1);
}

#[tokio::test]
async fn unknown_accounts_clear_the_cache_and_an_outage_keeps_it() {
    let fake = FakeConnect::start(false).await;
    let known = new_key();
    let unknown = new_key();
    let outage = new_key();
    fake.known(&known, "current");
    fake.state.unavailable.lock().unwrap().insert(public_key(&outage));
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("vc.sqlite3");
    let store = vc_server::store::Store::open(&database).unwrap();
    for key in [&known, &unknown, &outage] {
        let user = store.touch_user(&uid_of(key), &public_key(key), "member", 1).unwrap();
        store.set_connect_handle(user.id, Some("old")).unwrap();
        store.check_connect(user.id, now() - 24 * 3600 * 1000 - 1).unwrap();
    }
    let server = TestServer::start_with(|c| {
        c.database = Some(database);
        c.connect_url = Some(fake.url.clone());
    })
    .await;
    let device = new_key();
    let mut changed = as_device(&server, &device, certificate(&known, &device, now(), now() + 86400000)).await.unwrap();
    assert_eq!(changed.next_event_where("member.updated", |m| m["uid"] == uid_of(&known)).await["connect"], "current");
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
    assert_eq!(store.user_by_uid(&uid_of(&outage)).unwrap().unwrap().1.connect.as_deref(), Some("old"));
    for key in [&unknown, &outage] {
        let again = as_device(&server, &device, certificate(key, &device, now(), now() + 86400000)).await.unwrap();
        let member = again.welcome.members.iter().find(|m| m.uid == uid_of(key)).unwrap();
        assert_eq!(member.connect.as_deref(), if uid_of(key) == uid_of(&unknown) { None } else { Some("old") });
    }
    assert_eq!(fake.calls(), 3, "negative and failed lookups are also daily-cached");
    drop(down);
}

#[tokio::test]
async fn local_identities_and_no_connect_never_show_a_handle() {
    let fake = FakeConnect::start(false).await;
    let local = new_key();
    fake.known(&local, "registered");
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("vc.sqlite3");
    let store = vc_server::store::Store::open(&database).unwrap();
    let id = store.touch_user(&uid_of(&local), &public_key(&local), "Local", 1).unwrap().id;
    store.set_connect_handle(id, Some("registered")).unwrap();
    let server = TestServer::start_with(|c| {
        c.database = Some(database.clone());
        c.connect_url = Some(fake.url.clone());
    })
    .await;
    let client = Client::connect_as(&server, local.clone(), "ignored", None).await.unwrap();
    assert!(client.welcome.members.iter().find(|m| m.uid == uid_of(&local)).unwrap().connect.is_none());
    assert_eq!(fake.calls(), 0);
    client.close().await;
    drop(server);
    store.set_connect_handle(id, Some("registered")).unwrap();
    store.check_connect(id, now()).unwrap();
    let running =
        vc_server::start(vc_server::Config { database: Some(database), ..common::base_config() }).await.unwrap();
    let disabled = TestServer { running, admin_token: String::new() };
    let device = new_key();
    let client = as_device(&disabled, &device, certificate(&local, &device, now(), now() + 86400000)).await.unwrap();
    assert!(client.welcome.members.iter().find(|m| m.uid == uid_of(&local)).unwrap().connect.is_none());
    assert_eq!(store.connect_checked_at(id).unwrap(), None);
    assert_eq!(fake.calls(), 0);
}
