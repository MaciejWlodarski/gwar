//! Gwar Connect end to end, with a client written from docs/connect.md.

use aes_gcm::{Aes256Gcm, KeyInit, aead::Aead};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ed25519_dalek::{Signer, SigningKey};
use gwar_connect::{Config, crypto};
use hkdf::Hkdf;
use serde_json::{Value, json};
use sha2::Sha256;

const M: u32 = 19_456; // fast in tests; the protocol allows 19456..1048576
const T: u32 = 2;

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
    let expires_at = issued_at + 365 * 24 * 3600 * 1000;
    let statement = crypto::device_statement(&account_key, &device_key, issued_at, expires_at);
    json!({"device_key": device_key, "name": name, "issued_at": issued_at, "expires_at": expires_at,
           "signature": b64(&account.sign(statement.as_bytes()).to_bytes())})
}

struct Api {
    base: String,
    http: reqwest::Client,
}

impl Api {
    async fn start() -> Self {
        let addr = gwar_connect::start(Config {
            database: None,
            bind: "127.0.0.1:0".parse().unwrap(),
            origins: vec!["https://app.example".into()],
        })
        .await
        .unwrap();
        Self { base: format!("http://{addr}/v1"), http: reqwest::Client::new() }
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
    let (status, reply) =
        api.post("/register", None, sign_up("Maciej", "correct horse", &account, &laptop, &code)).await;
    assert_eq!(status, 200, "{reply}");
    let token = reply["token"].as_str().unwrap().to_owned();
    let (_, me) = api.get("/account", Some(&token)).await;
    assert_eq!(me["handle"], "maciej");

    // A phone logs in: same account key, its own device key.
    let (phone_token, decrypted) = log_in(&api, "@MACIEJ", "correct horse").await.expect("login works");
    assert_eq!(decrypted.to_bytes(), account.to_bytes(), "the phone recovers the same identity");
    let phone = SigningKey::from_bytes(&rand::random());
    let (status, _) = api.post("/devices", Some(&phone_token), certificate(&decrypted, &phone, "phone")).await;
    assert_eq!(status, 200);
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
    let (status, _) = api.post("/devices", Some(&token), certificate(&account, &phone, "phone")).await;
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
    let token = reply["token"].as_str().unwrap();
    let (status, _) = api.call(reqwest::Method::PUT, "/account/password", Some(token), Some(body)).await;
    assert_eq!(status, 200);
    assert!(log_in(&api, "bob", "old password").await.is_none());
    assert!(log_in(&api, "bob", "new password").await.is_some());
}

#[tokio::test]
async fn the_vault_follows_the_account_and_survives_a_password_change() {
    let api = Api::start().await;
    let account = SigningKey::from_bytes(&rand::random());
    let account_key = b64(account.verifying_key().as_bytes());
    let (_, reply) = api
        .post(
            "/register",
            None,
            sign_up("dana", "pw", &account, &SigningKey::from_bytes(&rand::random()), &rand::random()),
        )
        .await;
    let laptop_token = reply["token"].as_str().unwrap().to_owned();
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
    let (phone_token, decrypted) = log_in(&api, "dana", "pw").await.unwrap();
    let (_, got) = api.get("/vault", Some(&phone_token)).await;
    assert_eq!(open_vault(&vault_key(&decrypted), got["vault"].as_str().unwrap(), &account_key), contents);

    // The vault key does not depend on the password.
    let salt: [u8; 16] = rand::random();
    let (enc, auth) = password_secrets("pw2", &salt, M, T);
    let body = json!({"kdf": {"salt": b64(&salt), "m": M, "t": T, "p": 1}, "auth_key": b64(&auth),
                      "key_blob": seal(&enc, rand::random(), &account.to_bytes(), &account_key)});
    assert_eq!(api.put("/account/password", Some(&phone_token), body).await.0, 200);
    let (token, decrypted) = log_in(&api, "dana", "pw2").await.unwrap();
    let (_, got) = api.get("/vault", Some(&token)).await;
    assert_eq!(open_vault(&vault_key(&decrypted), got["vault"].as_str().unwrap(), &account_key), contents);
}

#[tokio::test]
async fn revoking_a_device_ends_its_sessions() {
    let api = Api::start().await;
    let account = SigningKey::from_bytes(&rand::random());
    let laptop = SigningKey::from_bytes(&rand::random());
    let (_, reply) = api.post("/register", None, sign_up("erin", "pw", &account, &laptop, &rand::random())).await;
    let laptop_token = reply["token"].as_str().unwrap().to_owned();
    let (phone_token, _) = log_in(&api, "erin", "pw").await.unwrap();
    let phone = SigningKey::from_bytes(&rand::random());
    assert_eq!(api.post("/devices", Some(&phone_token), certificate(&account, &phone, "phone")).await.0, 200);
    assert_eq!(api.get("/vault", Some(&phone_token)).await.0, 200);

    assert_eq!(api.post("/devices/revoke", Some(&laptop_token), revocation(&account, &phone)).await.0, 200);
    assert_eq!(api.get("/vault", Some(&phone_token)).await.0, 401, "the phone's session is gone");
    assert_eq!(api.get("/vault", Some(&laptop_token)).await.0, 200, "the laptop's is not");
}

/// A certificate for `device` lasting `days` from now.
fn certificate_for(account: &SigningKey, device: &SigningKey, days: i64) -> Value {
    let mut c = certificate(account, device, "");
    let issued_at = c["issued_at"].as_i64().unwrap();
    let expires_at = issued_at + days * 24 * 3600 * 1000;
    let (account_key, device_key) = (b64(account.verifying_key().as_bytes()), b64(device.verifying_key().as_bytes()));
    let statement = crypto::device_statement(&account_key, &device_key, issued_at, expires_at);
    c["expires_at"] = json!(expires_at);
    c["signature"] = json!(b64(&account.sign(statement.as_bytes()).to_bytes()));
    c
}

fn expiry_of(devices: &Value, device: &SigningKey) -> i64 {
    let key = b64(device.verifying_key().as_bytes());
    let entry = devices["devices"].as_array().unwrap().iter().find(|d| d["device_key"] == key).unwrap();
    entry["certificate"]["expires_at"].as_i64().unwrap()
}

#[tokio::test]
async fn one_device_renews_every_device_of_the_account() {
    let api = Api::start().await;
    let account = SigningKey::from_bytes(&rand::random());
    let laptop = SigningKey::from_bytes(&rand::random());
    let (_, reply) = api.post("/register", None, sign_up("fran", "pw", &account, &laptop, &rand::random())).await;
    let laptop_token = reply["token"].as_str().unwrap().to_owned();
    let (phone_token, _) = log_in(&api, "fran", "pw").await.unwrap();
    let (phone, old) = (SigningKey::from_bytes(&rand::random()), SigningKey::from_bytes(&rand::random()));
    assert_eq!(api.post("/devices", Some(&phone_token), certificate_for(&account, &phone, 30)).await.0, 200);
    assert_eq!(api.post("/devices", Some(&laptop_token), certificate_for(&account, &old, 30)).await.0, 200);
    assert_eq!(api.post("/devices/revoke", Some(&laptop_token), revocation(&account, &old)).await.0, 200);
    let (_, before) = api.get("/devices", Some(&phone_token)).await;

    // The laptop unlocks the account key and renews everything it lists.
    let renewal =
        json!({"certificates": [certificate_for(&account, &phone, 365), certificate_for(&account, &old, 365)]});
    let (status, reply) = api.post("/devices/renew", Some(&laptop_token), renewal).await;
    assert_eq!((status, &reply["renewed"]), (200, &json!(1)), "the revoked device is not renewed");

    // The phone finds its newer certificate without the password.
    let (_, after) = api.get("/devices", Some(&phone_token)).await;
    assert!(expiry_of(&after, &phone) > expiry_of(&before, &phone));
    // An older certificate never replaces a newer one, and only the account key signs them.
    let shorter = json!({"certificates": [certificate_for(&account, &phone, 10)]});
    assert_eq!(api.post("/devices/renew", Some(&laptop_token), shorter).await.1["renewed"], 0);
    let forged = json!({"certificates": [certificate_for(&phone, &phone, 365)]});
    assert_eq!(api.post("/devices/renew", Some(&phone_token), forged).await.0, 400);
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
    let expected = json!({"handle": "lookup", "account_key": key});
    assert_eq!(api.get("/accounts/lookup", None).await, (200, expected.clone()));
    assert_eq!(api.get(&format!("/accounts/by-key/{key}"), None).await, (200, expected));
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
