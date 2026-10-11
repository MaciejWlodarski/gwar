//! Gwar Connect end to end, with a client written from docs/connect.md.

use aes_gcm::{Aes256Gcm, KeyInit, aead::Aead};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ed25519_dalek::{Signer, SigningKey};
use gwar_connect::{Config, crypto};
use hkdf::Hkdf;
use serde_json::{Value, json};
use sha2::Sha256;
use std::sync::Arc;

const M: u32 = 65_536;
const T: u32 = 3;

fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

/// `enc_key ‖ auth_key` from a password (docs/connect.md).
fn password_secrets(password: &str, salt: &[u8], m: u32, t: u32) -> ([u8; 32], [u8; 32]) {
    let mut out = [0u8; 64];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, Params::new(m, t, 1, Some(64)).unwrap())
        .hash_password_into(password.as_bytes(), salt, &mut out)
        .unwrap();
    (out[..32].try_into().unwrap(), out[32..].try_into().unwrap())
}

fn recovery_secrets(code: &[u8], account_key: &[u8]) -> ([u8; 32], [u8; 32]) {
    let mut out = [0u8; 64];
    Hkdf::<Sha256>::new(Some(account_key), code).expand(b"gwar recovery v1", &mut out).unwrap();
    (out[..32].try_into().unwrap(), out[32..].try_into().unwrap())
}

fn seal(enc_key: &[u8; 32], nonce: [u8; 12], seed: &[u8; 32], account_key: &str) -> String {
    let aad = format!("gwar key v1\n{account_key}");
    let sealed = Aes256Gcm::new(enc_key.into())
        .encrypt(&nonce.into(), aes_gcm::aead::Payload { msg: seed, aad: aad.as_bytes() })
        .unwrap();
    b64(&[nonce.as_slice(), &sealed].concat())
}

fn open(enc_key: &[u8; 32], blob: &str, account_key: &str) -> [u8; 32] {
    let bytes = B64.decode(blob).unwrap();
    let aad = format!("gwar key v1\n{account_key}");
    let seed = Aes256Gcm::new(enc_key.into())
        .decrypt(bytes[..12].into(), aes_gcm::aead::Payload { msg: &bytes[12..], aad: aad.as_bytes() })
        .unwrap();
    seed.try_into().unwrap()
}

/// The vault key: only holders of the account key (and the devices they give it to) have it.
fn vault_key(account: &SigningKey) -> [u8; 32] {
    let mut out = [0u8; 32];
    Hkdf::<Sha256>::new(Some(account.verifying_key().as_bytes()), &account.to_bytes())
        .expand(b"gwar vault v1", &mut out)
        .unwrap();
    out
}

fn seal_vault(key: &[u8; 32], nonce: [u8; 12], contents: &Value, account_key: &str) -> String {
    let aad = format!("gwar vault v1\n{account_key}");
    let plain = serde_json::to_vec(contents).unwrap();
    let sealed = Aes256Gcm::new(key.into())
        .encrypt(&nonce.into(), aes_gcm::aead::Payload { msg: &plain, aad: aad.as_bytes() })
        .unwrap();
    b64(&[nonce.as_slice(), &sealed].concat())
}

fn open_vault(key: &[u8; 32], blob: &str, account_key: &str) -> Value {
    let bytes = B64.decode(blob).unwrap();
    let aad = format!("gwar vault v1\n{account_key}");
    let plain = Aes256Gcm::new(key.into())
        .decrypt(bytes[..12].into(), aes_gcm::aead::Payload { msg: &bytes[12..], aad: aad.as_bytes() })
        .unwrap();
    serde_json::from_slice(&plain).unwrap()
}

fn revocation(account: &SigningKey, device: &SigningKey) -> Value {
    let (account_key, device_key) = (b64(account.verifying_key().as_bytes()), b64(device.verifying_key().as_bytes()));
    let at = gwar_connect::api::now_ms();
    let statement = crypto::revoke_statement(&account_key, &device_key, at);
    json!({"device_key": device_key, "revoked_at": at, "signature": b64(&account.sign(statement.as_bytes()).to_bytes())})
}

fn certificate(account: &SigningKey, device: &SigningKey, name: &str) -> Value {
    let (account_key, device_key) = (b64(account.verifying_key().as_bytes()), b64(device.verifying_key().as_bytes()));
    let issued_at = gwar_connect::api::now_ms();
    let expires_at = issued_at + 90 * 24 * 3600 * 1000;
    let statement = crypto::device_statement(&account_key, &device_key, issued_at, expires_at);
    json!({"device_key": device_key, "name": name, "issued_at": issued_at, "expires_at": expires_at,
           "signature": b64(&account.sign(statement.as_bytes()).to_bytes())})
}

struct Api {
    base: String,
    http: reqwest::Client,
    state: Arc<gwar_connect::Connect>,
}

impl Api {
    async fn start() -> Self {
        let state = Arc::new(gwar_connect::Connect::new(gwar_connect::store::Store::in_memory().unwrap()).unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = gwar_connect::router(state.clone(), &["https://app.example".into()]);
        tokio::spawn(async move {
            axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>()).await.unwrap();
        });
        Self { base: format!("http://{addr}/v1"), http: reqwest::Client::new(), state }
    }

    async fn challenge(&self, token: &str, purpose: &str, device: Option<Value>) -> gwar_connect::store::Challenge {
        let (status, reply) =
            self.post("/challenges", Some(token), json!({"purpose": purpose, "device": device})).await;
        assert_eq!(status, 200, "{reply}");
        gwar_connect::store::Challenge {
            nonce: reply["challenge"].as_str().unwrap().into(),
            token_hash: crypto::hash(token.as_bytes()),
            account_id: self
                .state
                .store
                .lock()
                .unwrap()
                .session(&crypto::hash(token.as_bytes()), gwar_connect::api::now_ms())
                .unwrap()
                .map(|s| s.0)
                .unwrap_or(0),
            purpose: purpose.into(),
            certificate_hash: String::new(),
            device_key: None,
            certificate_expires_at: 0,
            credential_version: reply["credential_version"].as_i64().unwrap(),
            expires_at: reply["expires_at"].as_i64().unwrap(),
        }
    }

    async fn device_session(&self, token: &str, account: &SigningKey, device: &SigningKey, cert: Value) -> String {
        let mut c = self.challenge(token, "session", Some(cert.clone())).await;
        let (_, service) = self.get("/service-key", None).await;
        let account_key = b64(account.verifying_key().as_bytes());
        let device_key = b64(device.verifying_key().as_bytes());
        let certificate = gwar_connect::store::Certificate {
            issued_at: cert["issued_at"].as_i64().unwrap(),
            expires_at: cert["expires_at"].as_i64().unwrap(),
            signature: cert["signature"].as_str().unwrap().into(),
        };
        c.certificate_hash = crypto::certificate_hash(&account_key, &device_key, &certificate);
        let statement = crypto::session_statement(service["key"].as_str().unwrap(), &account_key, &c);
        let body = json!({"challenge": c.nonce, "device": cert, "signature": b64(&device.sign(statement.as_bytes()).to_bytes())});
        let (status, reply) = self.post("/session", Some(token), body).await;
        assert_eq!(status, 200, "{reply}");
        reply["token"].as_str().unwrap().into()
    }

    async fn register(
        &self,
        handle: &str,
        password: &str,
        account: &SigningKey,
        device: &SigningKey,
        code: &[u8; 20],
    ) -> String {
        let body = sign_up(handle, password, account, device, code);
        let (status, reply) = self.post("/register", None, body.clone()).await;
        assert_eq!(status, 200, "{reply}");
        self.device_session(reply["token"].as_str().unwrap(), account, device, body["device"].clone()).await
    }

    async fn signed_credentials(&self, token: &str, account: &SigningKey, purpose: &str, mut body: Value) -> Value {
        let c = self.challenge(token, purpose, None).await;
        let (_, service) = self.get("/service-key", None).await;
        let recovery = purpose == "recovery";
        let kdf_hash =
            if recovery { "-".into() } else { crypto::kdf_hash(&serde_json::from_value(body["kdf"].clone()).unwrap()) };
        let auth_hash = crypto::hash(
            &crypto::unb64(body[if recovery { "recovery_auth" } else { "auth_key" }].as_str().unwrap()).unwrap(),
        );
        let blob_hash = crypto::hash(
            &crypto::unb64(body[if recovery { "recovery_blob" } else { "key_blob" }].as_str().unwrap()).unwrap(),
        );
        let statement = crypto::credentials_statement(
            service["key"].as_str().unwrap(),
            &b64(account.verifying_key().as_bytes()),
            &c,
            &kdf_hash,
            &auth_hash,
            &blob_hash,
        );
        body["challenge"] = json!(c.nonce);
        body["credential_version"] = json!(c.credential_version);
        body["signature"] = json!(b64(&account.sign(statement.as_bytes()).to_bytes()));
        body
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut request = self.http.request(method, format!("{}{path}", self.base));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn post(&self, path: &str, token: Option<&str>, body: Value) -> (u16, Value) {
        self.call(reqwest::Method::POST, path, token, Some(body)).await
    }

    async fn get(&self, path: &str, token: Option<&str>) -> (u16, Value) {
        self.call(reqwest::Method::GET, path, token, None).await
    }

    async fn put(&self, path: &str, token: Option<&str>, body: Value) -> (u16, Value) {
        self.call(reqwest::Method::PUT, path, token, Some(body)).await
    }
}

/// Everything a client sends to sign up, built from a password and an account key.
fn sign_up(handle: &str, password: &str, account: &SigningKey, device: &SigningKey, code: &[u8; 20]) -> Value {
    let account_key = b64(account.verifying_key().as_bytes());
    let salt: [u8; 16] = rand::random();
    let (enc, auth) = password_secrets(password, &salt, M, T);
    let (renc, rauth) = recovery_secrets(code, account.verifying_key().as_bytes());
    json!({
        "handle": handle,
        "account_key": account_key,
        "kdf": {"salt": b64(&salt), "m": M, "t": T, "p": 1},
        "auth_key": b64(&auth),
        "key_blob": seal(&enc, rand::random(), &account.to_bytes(), &account_key),
        "recovery_auth": b64(&rauth),
        "recovery_blob": seal(&renc, rand::random(), &account.to_bytes(), &account_key),
        "device": certificate(account, device, "laptop"),
    })
}

/// What a second device does: prelogin, derive, log in, decrypt the account key.
async fn log_in(api: &Api, handle: &str, password: &str) -> Option<(String, SigningKey)> {
    let (_, pre) = api.post("/prelogin", None, json!({"handle": handle})).await;
    let kdf = &pre["kdf"];
    let salt = B64.decode(kdf["salt"].as_str().unwrap()).unwrap();
    let (enc, auth) = password_secrets(password, &salt, kdf["m"].as_u64()? as u32, kdf["t"].as_u64()? as u32);
    let (status, reply) = api.post("/login", None, json!({"handle": handle, "auth_key": b64(&auth)})).await;
    if status != 200 {
        return None;
    }
    let account_key = reply["account_key"].as_str().unwrap();
    let seed = open(&enc, reply["key_blob"].as_str().unwrap(), account_key);
    Some((reply["token"].as_str().unwrap().to_owned(), SigningKey::from_bytes(&seed)))
}

#[tokio::test]
async fn one_identity_on_every_device() {
    let api = Api::start().await;
    let account = SigningKey::from_bytes(&rand::random());
    let laptop = SigningKey::from_bytes(&rand::random());
    let code: [u8; 20] = rand::random();
    let token = api.register("Maciej", "correct horse", &account, &laptop, &code).await;
    let (_, me) = api.get("/account", Some(&token)).await;
    assert_eq!(me["handle"], "maciej");

    // A phone logs in: same account key, its own device key.
    let (phone_token, decrypted) = log_in(&api, "@MACIEJ", "correct horse").await.expect("login works");
    assert_eq!(decrypted.to_bytes(), account.to_bytes(), "the phone recovers the same identity");
    let phone = SigningKey::from_bytes(&rand::random());
    let (status, _) = api.post("/devices", Some(&phone_token), certificate(&decrypted, &phone, "phone")).await;
    assert_eq!(status, 200);
    let phone_token = api.device_session(&phone_token, &account, &phone, certificate(&account, &phone, "phone")).await;
    let (_, list) = api.get("/devices", Some(&token)).await;
    assert_eq!(list["devices"].as_array().unwrap().len(), 2);

    // Wrong password, unknown handle: the same answer.
    assert!(log_in(&api, "maciej", "wrong").await.is_none());
    let (_, ghost) = api.post("/prelogin", None, json!({"handle": "nobody"})).await;
    let (_, again) = api.post("/prelogin", None, json!({"handle": "nobody"})).await;
    assert_eq!(ghost, again, "unknown handles get stable fake parameters");
    assert!(log_in(&api, "nobody", "x").await.is_none());

    // Revoking the phone is signed by the account key and published.
    let phone_key = b64(phone.verifying_key().as_bytes());
    let account_key = b64(account.verifying_key().as_bytes());
    let at = gwar_connect::api::now_ms();
    let statement = crypto::revoke_statement(&account_key, &phone_key, at);
    let signature = b64(&account.sign(statement.as_bytes()).to_bytes());
    let forged = b64(&phone.sign(statement.as_bytes()).to_bytes());
    let bad = json!({"device_key": phone_key, "revoked_at": at, "signature": forged});
    assert_eq!(api.post("/devices/revoke", Some(&token), bad).await.0, 400);
    let good = json!({"device_key": phone_key, "revoked_at": at, "signature": signature});
    assert_eq!(api.post("/devices/revoke", Some(&token), good).await.0, 200);
    let (_, published) = api.get("/revocations?since=0", None).await;
    let entry = &published["revocations"][0];
    assert_eq!(entry["device_key"], phone_key);
    assert!(crypto::verify(&account_key, &statement, entry["signature"].as_str().unwrap()));
    assert_eq!(api.get("/devices", Some(&phone_token)).await.0, 401, "its session ended");
    let (enrollment, _) = log_in(&api, "maciej", "correct horse").await.unwrap();
    let (status, _) = api.post("/devices", Some(&enrollment), certificate(&account, &phone, "phone")).await;
    assert_eq!(status, 410, "a revoked device stays revoked");
}

#[tokio::test]
async fn nobody_claims_an_identity_they_do_not_hold() {
    let api = Api::start().await;
    let victim = SigningKey::from_bytes(&rand::random());
    let thief = SigningKey::from_bytes(&rand::random());
    let mut forged = sign_up("thief", "pw", &victim, &thief, &rand::random());
    // The thief knows the victim's public key but can only sign as themselves.
    forged["device"] = certificate(&thief, &thief, "x");
    forged["account_key"] = json!(b64(victim.verifying_key().as_bytes()));
    assert_eq!(api.post("/register", None, forged).await.0, 400);
    // Handles and identities are unique.
    let first = sign_up("alice", "pw", &victim, &SigningKey::from_bytes(&rand::random()), &rand::random());
    assert_eq!(api.post("/register", None, first).await.0, 200);
    let other = SigningKey::from_bytes(&rand::random());
    let taken = sign_up("Alice", "pw", &other, &SigningKey::from_bytes(&rand::random()), &rand::random());
    assert_eq!(api.post("/register", None, taken).await.0, 409);
}

#[tokio::test]
async fn the_recovery_code_restores_access_and_a_new_password() {
    let api = Api::start().await;
    let account = SigningKey::from_bytes(&rand::random());
    let code: [u8; 20] = rand::random();
    let body = sign_up("bob", "old password", &account, &SigningKey::from_bytes(&rand::random()), &code);
    api.post("/register", None, body).await;

    let (_, rauth) = recovery_secrets(&code, account.verifying_key().as_bytes());
    let (status, reply) = api.post("/recover", None, json!({"handle": "bob", "recovery_auth": b64(&rauth)})).await;
    assert_eq!(status, 200);
    let account_key = reply["account_key"].as_str().unwrap();
    let (renc, _) = recovery_secrets(&code, account.verifying_key().as_bytes());
    assert_eq!(open(&renc, reply["recovery_blob"].as_str().unwrap(), account_key), account.to_bytes());

    // Set a new password with the recovered key.
    let salt: [u8; 16] = rand::random();
    let (enc, auth) = password_secrets("new password", &salt, M, T);
    let body = json!({"kdf": {"salt": b64(&salt), "m": M, "t": T, "p": 1}, "auth_key": b64(&auth),
                      "key_blob": seal(&enc, rand::random(), &account.to_bytes(), account_key)});
    let enrollment = reply["token"].as_str().unwrap();
    let device = SigningKey::from_bytes(&rand::random());
    let cert = certificate(&account, &device, "recovered");
    assert_eq!(api.post("/devices", Some(enrollment), cert.clone()).await.0, 200);
    let token = api.device_session(enrollment, &account, &device, cert).await;
    let body = api.signed_credentials(&token, &account, "password", body).await;
    let (status, _) = api.put("/account/password", Some(&token), body).await;
    assert_eq!(status, 200);
    assert!(log_in(&api, "bob", "old password").await.is_none());
    assert!(log_in(&api, "bob", "new password").await.is_some());
}

#[tokio::test]
async fn the_vault_follows_the_account_and_survives_a_password_change() {
    let api = Api::start().await;
    let account = SigningKey::from_bytes(&rand::random());
    let account_key = b64(account.verifying_key().as_bytes());
    let laptop = SigningKey::from_bytes(&rand::random());
    let laptop_token = api.register("dana", "pw", &account, &laptop, &rand::random()).await;
    let (status, empty) = api.get("/vault", Some(&laptop_token)).await;
    assert_eq!((status, &empty["vault"], &empty["version"]), (200, &Value::Null, &json!(0)));

    // The laptop stores its TeamSpeak identity.
    let key = vault_key(&account);
    let contents = json!({"teamspeak": {"identity": "123VAAAA", "uid": "x", "updated_at": 1}});
    let blob = seal_vault(&key, rand::random(), &contents, &account_key);
    let (status, put) = api.put("/vault", Some(&laptop_token), json!({"vault": blob, "version": 0})).await;
    assert_eq!((status, &put["version"]), (200, &json!(1)));
    // A stale write is refused instead of overwriting.
    let (status, _) = api.put("/vault", Some(&laptop_token), json!({"vault": blob, "version": 0})).await;
    assert_eq!(status, 409);
    assert_eq!(api.put("/vault", None, json!({"vault": blob, "version": 1})).await.0, 401);
    assert_eq!(api.put("/vault", Some(&laptop_token), json!({"vault": "AAAA", "version": 1})).await.0, 400);

    // A new device logs in and reads it with the key it derives from the account key.
    let (enrollment, decrypted) = log_in(&api, "dana", "pw").await.unwrap();
    let phone = SigningKey::from_bytes(&rand::random());
    let cert = certificate(&account, &phone, "phone");
    assert_eq!(api.post("/devices", Some(&enrollment), cert.clone()).await.0, 200);
    let phone_token = api.device_session(&enrollment, &account, &phone, cert).await;
    let (_, got) = api.get("/vault", Some(&phone_token)).await;
    assert_eq!(open_vault(&vault_key(&decrypted), got["vault"].as_str().unwrap(), &account_key), contents);

    // The vault key does not depend on the password.
    let salt: [u8; 16] = rand::random();
    let (enc, auth) = password_secrets("pw2", &salt, M, T);
    let body = json!({"kdf": {"salt": b64(&salt), "m": M, "t": T, "p": 1}, "auth_key": b64(&auth),
                      "key_blob": seal(&enc, rand::random(), &account.to_bytes(), &account_key)});
    let body = api.signed_credentials(&phone_token, &account, "password", body).await;
    assert_eq!(api.put("/account/password", Some(&phone_token), body).await.0, 200);
    let (enrollment, decrypted) = log_in(&api, "dana", "pw2").await.unwrap();
    let cert = certificate(&account, &phone, "phone");
    assert_eq!(api.post("/devices", Some(&enrollment), cert.clone()).await.0, 200);
    let token = api.device_session(&enrollment, &account, &phone, cert).await;
    let (_, got) = api.get("/vault", Some(&token)).await;
    assert_eq!(open_vault(&vault_key(&decrypted), got["vault"].as_str().unwrap(), &account_key), contents);
}

#[tokio::test]
async fn revoking_a_device_ends_its_sessions() {
    let api = Api::start().await;
    let account = SigningKey::from_bytes(&rand::random());
    let laptop = SigningKey::from_bytes(&rand::random());
    let laptop_token = api.register("erin", "pw", &account, &laptop, &rand::random()).await;
    let (phone_token, _) = log_in(&api, "erin", "pw").await.unwrap();
    let phone = SigningKey::from_bytes(&rand::random());
    let cert = certificate(&account, &phone, "phone");
    assert_eq!(api.post("/devices", Some(&phone_token), cert.clone()).await.0, 200);
    let phone_token = api.device_session(&phone_token, &account, &phone, cert).await;
    assert_eq!(api.get("/vault", Some(&phone_token)).await.0, 200);

    assert_eq!(api.post("/devices/revoke", Some(&laptop_token), revocation(&account, &phone)).await.0, 200);
    assert_eq!(api.get("/vault", Some(&phone_token)).await.0, 401, "the phone's session is gone");
    assert_eq!(api.get("/vault", Some(&laptop_token)).await.0, 200, "the laptop's is not");
}

#[tokio::test]
async fn no_reply_may_be_cached() {
    let api = Api::start().await;
    for (path, method) in [("/service-key", "GET"), ("/vault", "GET"), ("/login", "POST"), ("/missing", "GET")] {
        let request = api.http.request(method.parse().unwrap(), format!("{}{path}", api.base));
        let reply = request.header("content-type", "application/json").body("{}").send().await.unwrap();
        assert_eq!(reply.headers()["cache-control"], "no-store", "{method} {path}");
    }
}

#[tokio::test]
async fn bulk_renewal_is_removed() {
    let api = Api::start().await;
    assert_eq!(api.post("/devices/renew", None, json!({"certificates": []})).await.0, 404);
}

#[tokio::test]
async fn registration_rejects_unsupported_password_profiles() {
    let api = Api::start().await;
    let account = SigningKey::from_bytes(&rand::random());
    let device = SigningKey::from_bytes(&rand::random());
    let body = sign_up("profile", "password", &account, &device, &rand::random());
    for (field, value) in [("m", json!(19456)), ("t", json!(1)), ("p", json!(2)), ("salt", json!(b64(&[0; 65])))] {
        let mut bad = body.clone();
        bad["kdf"][field] = value;
        assert_eq!(api.post("/register", None, bad).await.0, 400);
    }
}

#[tokio::test]
async fn logins_are_rate_limited() {
    let api = Api::start().await;
    let mut limited = false;
    for _ in 0..12 {
        let (status, _) = api.post("/login", None, json!({"handle": "carol", "auth_key": b64(&[0u8; 32])})).await;
        limited |= status == 429;
    }
    assert!(limited);
}

/// Fixed inputs → outputs, for checking other client implementations (web, mobile).
#[test]
fn client_crypto_vectors() {
    let (enc, auth) = password_secrets("correct horse", &[7u8; 16], 65_536, 3);
    assert_eq!(b64(&enc), ENC_KEY);
    assert_eq!(b64(&auth), AUTH_KEY);
    let account = SigningKey::from_bytes(&[1u8; 32]);
    let account_key = b64(account.verifying_key().as_bytes());
    assert_eq!(account_key, ACCOUNT_KEY);
    let (renc, rauth) = recovery_secrets(&[9u8; 20], account.verifying_key().as_bytes());
    assert_eq!((b64(&renc), b64(&rauth)), (RECOVERY_ENC.to_owned(), RECOVERY_AUTH.to_owned()));
    assert_eq!(seal(&enc, [3u8; 12], &account.to_bytes(), &account_key), KEY_BLOB);
    let device = SigningKey::from_bytes(&[2u8; 32]);
    let statement = crypto::device_statement(&account_key, &b64(device.verifying_key().as_bytes()), 1, 2);
    assert_eq!(b64(&account.sign(statement.as_bytes()).to_bytes()), CERT_SIGNATURE);
    let key = vault_key(&account);
    assert_eq!(b64(&key), VAULT_KEY);
    let contents = json!({"teamspeak": {"identity": "1V", "uid": "u", "updated_at": 1}});
    assert_eq!(serde_json::to_string(&contents).unwrap(), VAULT_JSON);
    assert_eq!(seal_vault(&key, [3u8; 12], &contents, &account_key), VAULT_BLOB);
}

const ENC_KEY: &str = "qXcjpHbTOr46vWBB7wO-b3l-lYMp7lsl8sa_SekxVr0";
const AUTH_KEY: &str = "wWSVGXCU8xhR5oEnsTNapLPwB1BEFO42MQySTQK-LK0";
const ACCOUNT_KEY: &str = "iojj3XQJ8ZX9UtstPLpdcspnCb8dlBIb83SIAbQPb1w";
const RECOVERY_ENC: &str = "WA19s_g5IcLVVJ_xQ3KHzTpUn7mkdbLdTKOA3GCok10";
const RECOVERY_AUTH: &str = "42kqZaRu_Jtg8r8S7P-39L9-QR5IQM8eSwza09oTy3M";
const KEY_BLOB: &str = "AwMDAwMDAwMDAwMDEz57zhVdpeE9DuPc-Z8DMvCEty7oHW-BIq7g3-5P-1LKErFt1chgOHB2VhOzkcXy";
const VAULT_KEY: &str = "_LvSNss8p0PDJFYkx77Gv9dldHZztz1NPse-h8ZV6bQ";
const VAULT_JSON: &str = r#"{"teamspeak":{"identity":"1V","uid":"u","updated_at":1}}"#;
const VAULT_BLOB: &str =
    "AwMDAwMDAwMDAwMDZgbHGQNI33i1QJYlr7AP1RbMIJv2WqEwRVph4x0WE_aS19RAscHc07sG1nH6et8NwocuVgdgFXyTeXCsQHpTYOstEa3cT7_Q";
const CERT_SIGNATURE: &str = "hrl_m1SlEOKgUYV7RMz4dnlZ5wa5OY_m5-ZS9qWcZU82oGdpwkQdm5wVyUgxplRzxEiMoJnAs-BN6ktG0ufaDA";

#[tokio::test]
async fn public_accounts_can_be_looked_up_by_handle_or_key() {
    let api = Api::start().await;
    let account = SigningKey::from_bytes(&rand::random());
    let device = SigningKey::from_bytes(&rand::random());
    let (status, _) = api.post("/register", None, sign_up("lookup", "pw", &account, &device, &rand::random())).await;
    assert_eq!(status, 200);
    let key = b64(account.verifying_key().as_bytes());
    let (status, service) = api.get("/service-key", None).await;
    assert_eq!(status, 200);
    let service = service["key"].as_str().unwrap();
    assert_eq!(crypto::unb64_len(service, 32).unwrap().len(), 32);
    for path in ["/accounts/lookup".into(), format!("/accounts/by-key/{key}")] {
        let before = gwar_connect::api::now_ms();
        let (status, reply) = api.get(&path, None).await;
        assert_eq!(status, 200);
        assert_eq!(reply["handle"], "lookup");
        assert_eq!(reply["account_key"], key);
        let a: vc_proto::HandleAttestation = serde_json::from_value(reply["attestation"].clone()).unwrap();
        assert_eq!(a.account_key, key);
        assert_eq!(a.handle, "lookup");
        assert!((before..=gwar_connect::api::now_ms()).contains(&a.issued_at));
        assert_eq!(a.expires_at - a.issued_at, 7 * 24 * 3600 * 1000);
        assert!(crypto::verify(service, &vc_proto::handle_statement(&a), &a.signature));
    }
    let unknown = b64(SigningKey::from_bytes(&rand::random()).verifying_key().as_bytes());
    for path in [format!("/accounts/by-key/{unknown}"), "/accounts/by-key/malformed".into(), "/accounts/by-key".into()]
    {
        let (status, reply) = api.get(&path, None).await;
        assert_eq!(status, 404);
        assert_eq!(reply["error"], "not_found");
    }
    // A hyphen is not permitted in handles, so the static route cannot hide an account.
    assert_eq!(crypto::handle("by-key"), None);
}

#[tokio::test]
async fn service_key_survives_restart_and_matches_cli() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("connect.sqlite3");
    let http = reqwest::Client::new();
    let mut keys = Vec::new();
    for _ in 0..2 {
        let addr = gwar_connect::start(Config {
            database: Some(database.clone()),
            bind: "127.0.0.1:0".parse().unwrap(),
            origins: vec![],
        })
        .await
        .unwrap();
        let reply: Value =
            http.get(format!("http://{addr}/v1/service-key")).send().await.unwrap().json().await.unwrap();
        keys.push(reply["key"].as_str().unwrap().to_owned());
    }
    assert_eq!(keys[0], keys[1]);
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_gwar-connect"))
        .arg("--data-dir")
        .arg(dir.path())
        .arg("service-key")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), keys[0]);
}

#[tokio::test]
async fn enrollment_tokens_cannot_access_the_vault_or_change_credentials() {
    let api = Api::start().await;
    let account = SigningKey::from_bytes(&rand::random());
    let device = SigningKey::from_bytes(&rand::random());
    let body = sign_up("enroll", "password", &account, &device, &rand::random());
    let (_, reply) = api.post("/register", None, body.clone()).await;
    let token = reply["token"].as_str().unwrap();
    for path in ["/account", "/vault", "/devices"] {
        assert_eq!(api.get(path, Some(token)).await.0, 401);
    }
    assert_eq!(api.put("/vault", Some(token), json!({"vault": body["key_blob"], "version": 0})).await.0, 401);
    assert_eq!(api.post("/challenges", Some(token), json!({"purpose": "password"})).await.0, 401);
    assert_eq!(api.post("/devices/revoke", Some(token), revocation(&account, &device)).await.0, 401);
    // One enrollment credential cannot enroll another device after its first one.
    assert_eq!(
        api.post("/devices", Some(token), certificate(&account, &SigningKey::from_bytes(&rand::random()), "other"))
            .await
            .0,
        400
    );
    let session = api.device_session(token, &account, &device, body["device"].clone()).await;
    assert_eq!(api.get("/vault", Some(&session)).await.0, 200);
    assert_eq!(api.get("/vault", Some(token)).await.0, 401);
    let (login, _) = log_in(&api, "enroll", "password").await.unwrap();
    assert_eq!(api.get("/vault", Some(&login)).await.0, 401);
    api.state.store.lock().unwrap().insert_enrollment(&crypto::hash(b"expired"), 1, None, 0, 1).unwrap();
    assert_eq!(api.post("/devices", Some("expired"), body["device"].clone()).await.0, 401);
}

#[tokio::test]
async fn session_proofs_are_one_use_and_bind_service_account_certificate_and_token() {
    let api = Api::start().await;
    let account = SigningKey::from_bytes(&rand::random());
    let device = SigningKey::from_bytes(&rand::random());
    let body = sign_up("proof", "password", &account, &device, &rand::random());
    let (_, reply) = api.post("/register", None, body.clone()).await;
    let token = reply["token"].as_str().unwrap();
    let cert = body["device"].clone();
    api.challenge(token, "session", Some(cert.clone())).await;
    let hash = crypto::hash(token.as_bytes());
    // Read the challenge through the API, then use the exact stored statement.
    let reply = api.post("/challenges", Some(token), json!({"purpose": "session", "device": cert})).await.1;
    let c = api
        .state
        .store
        .lock()
        .unwrap()
        .challenge(&hash, reply["challenge"].as_str().unwrap(), gwar_connect::api::now_ms())
        .unwrap()
        .unwrap();
    let service = reply["service_key"].as_str().unwrap();
    let statement = crypto::session_statement(service, &b64(account.verifying_key().as_bytes()), &c);
    let proof =
        json!({"challenge": c.nonce, "device": cert, "signature": b64(&device.sign(statement.as_bytes()).to_bytes())});
    for wrong in [
        statement.replace(service, &b64(&[0; 32])),
        statement.replace(&b64(account.verifying_key().as_bytes()), &b64(&[1; 32])),
        statement.replace(&c.certificate_hash, &b64(&[2; 32])),
        statement.replace(&hash, &crypto::hash(b"other-token")),
        statement.replace("gwar session v1", "gwar credentials v1"),
    ] {
        let mut bad = proof.clone();
        bad["signature"] = json!(b64(&device.sign(wrong.as_bytes()).to_bytes()));
        assert_eq!(api.post("/session", Some(token), bad).await.0, 400);
    }
    let mut bad = proof.clone();
    bad["signature"] = json!(b64(&account.sign(statement.as_bytes()).to_bytes()));
    assert_eq!(api.post("/session", Some(token), bad).await.0, 400);
    let mut bad = proof.clone();
    bad["device"]["expires_at"] = json!(cert["expires_at"].as_i64().unwrap() + 1);
    assert_eq!(api.post("/session", Some(token), bad).await.0, 400);
    assert_eq!(api.post("/session", Some(token), proof.clone()).await.0, 200);
    assert_eq!(api.post("/session", Some(token), proof).await.0, 401);
}

#[tokio::test]
async fn expired_challenges_and_device_certificates_end_connect_access() {
    let api = Api::start().await;
    let account = SigningKey::from_bytes(&rand::random());
    let device = SigningKey::from_bytes(&rand::random());
    let mut body = sign_up("expiry", "password", &account, &device, &rand::random());
    let at = gwar_connect::api::now_ms();
    let until = at + 2000;
    let text = crypto::device_statement(
        &b64(account.verifying_key().as_bytes()),
        &b64(device.verifying_key().as_bytes()),
        at,
        until,
    );
    body["device"] = json!({"device_key": b64(device.verifying_key().as_bytes()), "name": "device", "issued_at": at, "expires_at": until, "signature": b64(&account.sign(text.as_bytes()).to_bytes())});
    let (_, reply) = api.post("/register", None, body.clone()).await;
    let enrollment = reply["token"].as_str().unwrap();
    let c = api.challenge(enrollment, "session", Some(body["device"].clone())).await;
    let mut stored = api
        .state
        .store
        .lock()
        .unwrap()
        .challenge(&crypto::hash(enrollment.as_bytes()), &c.nonce, gwar_connect::api::now_ms())
        .unwrap()
        .unwrap();
    stored.expires_at = gwar_connect::api::now_ms() - 1;
    api.state.store.lock().unwrap().insert_challenge(&stored).unwrap();
    assert_eq!(
        api.post(
            "/session",
            Some(enrollment),
            json!({"challenge": c.nonce, "device": body["device"], "signature": b64(&[0;64])})
        )
        .await
        .0,
        401
    );
    let session = api.device_session(enrollment, &account, &device, body["device"].clone()).await;
    assert_eq!(api.get("/vault", Some(&session)).await.0, 200);
    tokio::time::sleep(std::time::Duration::from_millis((until - gwar_connect::api::now_ms()).max(0) as u64 + 10))
        .await;
    assert_eq!(api.get("/vault", Some(&session)).await.0, 401);
}

#[tokio::test]
async fn credential_updates_require_the_account_signature_over_every_new_field() {
    let api = Api::start().await;
    let account = SigningKey::from_bytes(&rand::random());
    let device = SigningKey::from_bytes(&rand::random());
    let token = api.register("credentials", "old password", &account, &device, &rand::random()).await;
    let credentials = sign_up("unused", "new password", &account, &device, &rand::random());
    let body =
        json!({"kdf": credentials["kdf"], "auth_key": credentials["auth_key"], "key_blob": credentials["key_blob"]});
    assert_eq!(api.put("/account/password", Some(&token), body.clone()).await.0, 422, "bearer alone is insufficient");
    let signed = api.signed_credentials(&token, &account, "password", body).await;
    for (field, value) in [
        ("auth_key", json!(b64(&[99; 32]))),
        ("key_blob", json!(b64(&[99; 60]))),
        ("signature", json!(b64(&device.sign(b"not an account signature").to_bytes()))),
    ] {
        let mut bad = signed.clone();
        bad[field] = value;
        assert_eq!(api.put("/account/password", Some(&token), bad).await.0, 400);
    }
    let mut bad = signed.clone();
    bad["kdf"]["salt"] = json!(b64(&[99; 16]));
    assert_eq!(api.put("/account/password", Some(&token), bad).await.0, 400);
    let mut bad = signed.clone();
    bad["kdf"]["m"] = json!(19456);
    assert_eq!(api.put("/account/password", Some(&token), bad).await.0, 400);
    let mut bad = signed.clone();
    bad["credential_version"] = json!(1);
    assert_eq!(api.put("/account/password", Some(&token), bad).await.0, 409);
    let (first, second) = tokio::join!(
        api.put("/account/password", Some(&token), signed.clone()),
        api.put("/account/password", Some(&token), signed)
    );
    assert_eq!([first.0, second.0].iter().filter(|&&s| s == 200).count(), 1, "one challenge commits once");
    assert!(log_in(&api, "credentials", "old password").await.is_none());
    assert!(log_in(&api, "credentials", "new password").await.is_some());
    let current = api.state.store.lock().unwrap().account_by_handle("credentials").unwrap().unwrap();
    assert_eq!(current.credential_version, 1);
}

#[tokio::test]
async fn recovery_credential_updates_are_signed_and_versioned_too() {
    let api = Api::start().await;
    let account = SigningKey::from_bytes(&rand::random());
    let device = SigningKey::from_bytes(&rand::random());
    let token = api.register("rotation", "password", &account, &device, &[1; 20]).await;
    let credentials = sign_up("unused", "password", &account, &device, &[2; 20]);
    let body = json!({"recovery_auth": credentials["recovery_auth"], "recovery_blob": credentials["recovery_blob"]});
    let signed = api.signed_credentials(&token, &account, "recovery", body).await;
    for field in ["recovery_auth", "recovery_blob"] {
        let mut bad = signed.clone();
        bad[field] = json!(b64(&vec![99; if field == "recovery_auth" { 32 } else { 60 }]));
        assert_eq!(api.put("/account/recovery", Some(&token), bad).await.0, 400);
    }
    assert_eq!(api.put("/account/recovery", Some(&token), signed.clone()).await.0, 200);
    assert_eq!(api.put("/account/recovery", Some(&token), signed).await.0, 401);
    let (_, old_auth) = recovery_secrets(&[1; 20], account.verifying_key().as_bytes());
    assert_eq!(api.post("/recover", None, json!({"handle":"rotation", "recovery_auth":b64(&old_auth)})).await.0, 401);
    assert_eq!(
        api.post("/recover", None, json!({"handle":"rotation", "recovery_auth":credentials["recovery_auth"]})).await.0,
        200
    );
}

#[test]
fn client_integrity_vectors() {
    let v: Value = serde_json::from_str(include_str!("integrity-vectors.json")).unwrap();
    let account = SigningKey::from_bytes(&[1; 32]);
    let device = SigningKey::from_bytes(&[2; 32]);
    let account_key = b64(account.verifying_key().as_bytes());
    let cert = gwar_connect::store::Certificate {
        issued_at: 1,
        expires_at: 2,
        signature: v["device"]["signature"].as_str().unwrap().into(),
    };
    let certificate_hash = crypto::certificate_hash(&account_key, &b64(device.verifying_key().as_bytes()), &cert);
    assert_eq!(certificate_hash, v["certificate_hash"]);
    let mut c = gwar_connect::store::Challenge {
        nonce: v["challenge"]["challenge"].as_str().unwrap().into(),
        token_hash: crypto::hash(b"token"),
        account_id: 0,
        purpose: "session".into(),
        certificate_hash,
        device_key: None,
        certificate_expires_at: 2,
        credential_version: 0,
        expires_at: 5,
    };
    let service = v["challenge"]["service_key"].as_str().unwrap();
    let statement = crypto::session_statement(service, &account_key, &c);
    assert_eq!(statement, v["session_statement"]);
    assert_eq!(crypto::hash(statement.as_bytes()), v["session_hash"]);
    assert_eq!(b64(&device.sign(statement.as_bytes()).to_bytes()), v["session_signature"]);
    let kdf_hash = crypto::kdf_hash(&serde_json::from_value(v["kdf"].clone()).unwrap());
    assert_eq!(kdf_hash, v["kdf_hash"]);
    for purpose in ["password", "recovery"] {
        c.purpose = purpose.into();
        let statement = crypto::credentials_statement(
            service,
            &account_key,
            &c,
            if purpose == "password" { &kdf_hash } else { "-" },
            &crypto::hash(&[4; 32]),
            &crypto::hash(&[5; 60]),
        );
        assert_eq!(statement, v[format!("{purpose}_statement")]);
        assert_eq!(crypto::hash(statement.as_bytes()), v[format!("{purpose}_hash")]);
        assert_eq!(b64(&account.sign(statement.as_bytes()).to_bytes()), v[format!("{purpose}_signature")]);
    }
}
