//! HTTP API (see docs/connect.md).

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use axum::{
    Json,
    extract::{ConnectInfo, Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::{
    crypto::{self, KEY_BLOB_LEN, MAX_VAULT_LEN, SKEW_MS, b64, unb64_len},
    store::{Account, Certificate, Kdf, NewAccount, Store},
};

const SESSION_MS: i64 = 90 * 24 * 3600 * 1000;
const MAX_DEVICE_NAME: usize = 64;

pub struct Connect {
    pub store: Mutex<Store>,
    limits: Mutex<HashMap<String, Vec<Instant>>>,
    /// Keys the fake salts of unknown handles, so prelogin reveals nothing.
    secret: [u8; 32],
}

impl Connect {
    pub fn new(store: Store) -> anyhow::Result<Self> {
        let secret = match store.meta("secret")?.and_then(|s| crypto::unb64_len(&s, 32)) {
            Some(secret) => secret,
            None => {
                let secret: [u8; 32] = rand::random();
                store.set_meta("secret", &b64(&secret))?;
                secret.to_vec()
            }
        };
        Ok(Self { store: Mutex::new(store), limits: Mutex::default(), secret: secret.try_into().expect("32 bytes") })
    }

    /// Allows at most `max` calls per `window` for `key`.
    fn allow(&self, key: String, max: usize, window: Duration) -> bool {
        let now = Instant::now();
        let mut limits = self.limits.lock().expect("limits lock");
        if limits.len() > 100_000 {
            limits.retain(|_, hits| hits.iter().any(|t| now.duration_since(*t) < Duration::from_secs(3600)));
        }
        let hits = limits.entry(key).or_default();
        hits.retain(|t| now.duration_since(*t) < window);
        if hits.len() >= max {
            return false;
        }
        hits.push(now);
        true
    }
}

pub type AppState = Arc<Connect>;

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

// ------------------------------------------------------------------ errors

pub struct ApiError(StatusCode, &'static str, String);

impl ApiError {
    fn bad(message: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, "bad_request", message.into())
    }
    fn unauthorized() -> Self {
        Self(StatusCode::UNAUTHORIZED, "unauthorized", "wrong handle or password".into())
    }
    fn limited() -> Self {
        Self(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "too many attempts, try again later".into())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        warn!("connect: {e:#}");
        Self(StatusCode::INTERNAL_SERVER_ERROR, "internal", "internal error".into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1, "message": self.2}))).into_response()
    }
}

type Reply = Result<Json<Value>, ApiError>;

/// The caller's address; behind a local reverse proxy, its forwarded one.
fn client_ip(peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
    if !peer.ip().is_loopback() {
        return peer.ip();
    }
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    header("x-real-ip")
        .or_else(|| header("x-forwarded-for").and_then(|v| v.rsplit(',').next()))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(peer.ip())
}

fn token_hash(token: &str) -> String {
    b64(&Sha256::digest(token.as_bytes()))
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ")
}

/// The account behind the request's session. Using a session keeps it (and
/// its device) alive: sessions end after [`SESSION_MS`] without use.
fn authenticated(state: &Connect, headers: &HeaderMap) -> Result<Account, ApiError> {
    let token = bearer(headers).ok_or_else(ApiError::unauthorized)?;
    let hash = token_hash(token);
    let now = now_ms();
    let store = state.store.lock().expect("store lock");
    let (id, device) = store.session(&hash, now)?.ok_or_else(ApiError::unauthorized)?;
    store.extend_session(&hash, now + SESSION_MS)?;
    if let Some(device) = device {
        store.touch_device(&device, now)?;
    }
    store.account(id)?.ok_or_else(ApiError::unauthorized)
}

fn certificate(d: &DeviceCert) -> Certificate {
    Certificate { issued_at: d.issued_at, expires_at: d.expires_at, signature: d.signature.clone() }
}

fn new_session(store: &Store, account: i64) -> anyhow::Result<String> {
    let token = b64(&rand::random::<[u8; 32]>());
    let now = now_ms();
    store.insert_session(&token_hash(&token), account, now, now + SESSION_MS)?;
    Ok(token)
}

fn check_kdf(kdf: &Kdf) -> Result<(), ApiError> {
    let salt = crypto::unb64(&kdf.salt).ok_or_else(|| ApiError::bad("invalid salt"))?;
    let ok = (16..=64).contains(&salt.len())
        && (19_456..=1_048_576).contains(&kdf.m)
        && (1..=10).contains(&kdf.t)
        && (1..=4).contains(&kdf.p);
    if ok { Ok(()) } else { Err(ApiError::bad("unsupported key derivation parameters")) }
}

fn check_blob(blob: &str) -> Result<(), ApiError> {
    unb64_len(blob, KEY_BLOB_LEN).map(|_| ()).ok_or_else(|| ApiError::bad("invalid key blob"))
}

/// Argon2 of the client's authentication key (32 bytes, base64url).
async fn hash_auth(auth_key: &str) -> Result<String, ApiError> {
    let key = unb64_len(auth_key, 32).ok_or_else(|| ApiError::bad("invalid authentication key"))?;
    let hashed = tokio::task::spawn_blocking(move || {
        let salt = SaltString::encode_b64(&rand::random::<[u8; 16]>()).expect("salt");
        Argon2::default().hash_password(&key, &salt).map(|h| h.to_string())
    })
    .await
    .map_err(anyhow::Error::from)?
    .map_err(|e| anyhow::anyhow!("argon2: {e}"))?;
    Ok(hashed)
}

async fn verify_auth(auth_key: &str, hash: &str) -> bool {
    let (Some(key), hash) = (unb64_len(auth_key, 32), hash.to_owned()) else { return false };
    tokio::task::spawn_blocking(move || {
        PasswordHash::new(&hash).is_ok_and(|parsed| Argon2::default().verify_password(&key, &parsed).is_ok())
    })
    .await
    .unwrap_or(false)
}

fn recovery_hash(recovery_auth: &str) -> Result<String, ApiError> {
    let key = unb64_len(recovery_auth, 32).ok_or_else(|| ApiError::bad("invalid recovery key"))?;
    Ok(b64(&Sha256::digest(key)))
}

fn device_name(name: &str) -> String {
    let name: String = name.chars().filter(|c| !c.is_control()).take(MAX_DEVICE_NAME).collect();
    if name.trim().is_empty() { "Device".into() } else { name.trim().to_owned() }
}

// ---------------------------------------------------------------- handlers

#[derive(Deserialize)]
pub struct DeviceCert {
    device_key: String,
    #[serde(default)]
    name: String,
    issued_at: i64,
    expires_at: i64,
    signature: String,
}

#[derive(Deserialize)]
pub struct Register {
    handle: String,
    account_key: String,
    kdf: Kdf,
    auth_key: String,
    key_blob: String,
    recovery_auth: String,
    recovery_blob: String,
    device: DeviceCert,
}

pub async fn register(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(r): Json<Register>,
) -> Reply {
    let ip = client_ip(peer, &headers);
    if !state.allow(format!("register:{ip}"), 5, Duration::from_secs(3600)) {
        return Err(ApiError::limited());
    }
    let handle = crypto::handle(&r.handle).ok_or_else(|| ApiError::bad("handle must be 3–32 of a-z 0-9 _ ."))?;
    check_kdf(&r.kdf)?;
    check_blob(&r.key_blob)?;
    check_blob(&r.recovery_blob)?;
    let now = now_ms();
    let d = &r.device;
    // The certificate proves the caller holds the account key: nobody can
    // claim someone else's existing identity.
    crypto::check_certificate(&r.account_key, &d.device_key, d.issued_at, d.expires_at, &d.signature, now)
        .map_err(ApiError::bad)?;
    let auth_hash = hash_auth(&r.auth_key).await?;
    let recovery_hash = recovery_hash(&r.recovery_auth)?;
    let store = state.store.lock().expect("store lock");
    let account = NewAccount {
        handle: &handle,
        account_key: &r.account_key,
        kdf: &r.kdf,
        auth_hash: &auth_hash,
        key_blob: &r.key_blob,
        recovery_hash: &recovery_hash,
        recovery_blob: &r.recovery_blob,
        now,
    };
    if store.device_owner(&d.device_key)?.is_some() {
        return Err(ApiError(StatusCode::CONFLICT, "taken", "this device key is already registered".into()));
    }
    let Some(id) = store.insert_account(&account)? else {
        return Err(ApiError(StatusCode::CONFLICT, "taken", "this handle or identity already has an account".into()));
    };
    store.upsert_device(id, &d.device_key, &device_name(&d.name), now)?;
    store.set_certificate(&d.device_key, &certificate(d))?;
    let token = new_session(&store, id)?;
    store.bind_session(&token_hash(&token), &d.device_key)?;
    Ok(Json(json!({"token": token})))
}

#[derive(Deserialize)]
pub struct Prelogin {
    handle: String,
}

pub async fn prelogin(State(state): State<AppState>, Json(r): Json<Prelogin>) -> Reply {
    let handle = crypto::handle(&r.handle).unwrap_or_default();
    let known = state.store.lock().expect("store lock").account_by_handle(&handle)?;
    let kdf = match known {
        Some(account) => account.kdf,
        // Unknown handles get stable, plausible parameters.
        None => {
            let mut mac = Hmac::<Sha256>::new_from_slice(&state.secret).expect("hmac key");
            mac.update(handle.as_bytes());
            Kdf { salt: b64(&mac.finalize().into_bytes()[..16]), m: 65_536, t: 3, p: 1 }
        }
    };
    Ok(Json(json!({"kdf": kdf})))
}

#[derive(Deserialize)]
pub struct Login {
    handle: String,
    auth_key: String,
}

fn limit_login(state: &Connect, ip: IpAddr, handle: &str) -> Result<(), ApiError> {
    if state.allow(format!("login-ip:{ip}"), 30, Duration::from_secs(600))
        && state.allow(format!("login-handle:{handle}"), 10, Duration::from_secs(600))
    {
        Ok(())
    } else {
        Err(ApiError::limited())
    }
}

pub async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(r): Json<Login>,
) -> Reply {
    let handle = crypto::handle(&r.handle).ok_or_else(ApiError::unauthorized)?;
    limit_login(&state, client_ip(peer, &headers), &handle)?;
    let account = state.store.lock().expect("store lock").account_by_handle(&handle)?;
    let Some(account) = account else { return Err(ApiError::unauthorized()) };
    if !verify_auth(&r.auth_key, &account.auth_hash).await {
        return Err(ApiError::unauthorized());
    }
    let token = new_session(&state.store.lock().expect("store lock"), account.id)?;
    Ok(Json(json!({"token": token, "account_key": account.account_key, "key_blob": account.key_blob})))
}

#[derive(Deserialize)]
pub struct Recover {
    handle: String,
    recovery_auth: String,
}

pub async fn recover(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(r): Json<Recover>,
) -> Reply {
    let handle = crypto::handle(&r.handle).ok_or_else(ApiError::unauthorized)?;
    limit_login(&state, client_ip(peer, &headers), &handle)?;
    let store = state.store.lock().expect("store lock");
    let Some(account) = store.account_by_handle(&handle)? else { return Err(ApiError::unauthorized()) };
    let given = recovery_hash(&r.recovery_auth).map_err(|_| ApiError::unauthorized())?;
    // Constant-time enough: both sides are hashes of 32 random bytes.
    if given != account.recovery_hash {
        return Err(ApiError::unauthorized());
    }
    let token = new_session(&store, account.id)?;
    Ok(Json(json!({"token": token, "account_key": account.account_key, "recovery_blob": account.recovery_blob})))
}

pub async fn account(State(state): State<AppState>, headers: HeaderMap) -> Reply {
    let a = authenticated(&state, &headers)?;
    Ok(Json(json!({"handle": a.handle, "account_key": a.account_key, "created_at": a.created_at})))
}

#[derive(Deserialize)]
pub struct Password {
    kdf: Kdf,
    auth_key: String,
    key_blob: String,
}

pub async fn password(State(state): State<AppState>, headers: HeaderMap, Json(r): Json<Password>) -> Reply {
    let account = authenticated(&state, &headers)?;
    check_kdf(&r.kdf)?;
    check_blob(&r.key_blob)?;
    let auth_hash = hash_auth(&r.auth_key).await?;
    let store = state.store.lock().expect("store lock");
    store.set_password(account.id, &r.kdf, &auth_hash, &r.key_blob)?;
    if let Some(token) = bearer(&headers) {
        store.end_other_sessions(account.id, &token_hash(token))?;
    }
    Ok(Json(json!({})))
}

pub async fn devices(State(state): State<AppState>, headers: HeaderMap) -> Reply {
    let account = authenticated(&state, &headers)?;
    let devices = state.store.lock().expect("store lock").devices(account.id)?;
    Ok(Json(json!({"devices": devices})))
}

pub async fn add_device(State(state): State<AppState>, headers: HeaderMap, Json(d): Json<DeviceCert>) -> Reply {
    let account = authenticated(&state, &headers)?;
    let now = now_ms();
    crypto::check_certificate(&account.account_key, &d.device_key, d.issued_at, d.expires_at, &d.signature, now)
        .map_err(ApiError::bad)?;
    let store = state.store.lock().expect("store lock");
    match store.device_owner(&d.device_key)? {
        Some((owner, _)) if owner != account.id => {
            return Err(ApiError(StatusCode::CONFLICT, "taken", "device key belongs to another account".into()));
        }
        Some((_, Some(_))) => return Err(ApiError(StatusCode::GONE, "revoked", "this device was revoked".into())),
        _ => {}
    }
    store.upsert_device(account.id, &d.device_key, &device_name(&d.name), now)?;
    store.set_certificate(&d.device_key, &certificate(&d))?;
    if let Some(token) = bearer(&headers) {
        store.bind_session(&token_hash(token), &d.device_key)?;
    }
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
pub struct Renew {
    certificates: Vec<DeviceCert>,
}

/// New certificates for the account's devices, made by a client that holds
/// the account key. Each device picks its own up from `GET /v1/devices`.
pub async fn renew_devices(State(state): State<AppState>, headers: HeaderMap, Json(r): Json<Renew>) -> Reply {
    let account = authenticated(&state, &headers)?;
    if r.certificates.len() > 100 {
        return Err(ApiError::bad("too many certificates"));
    }
    let now = now_ms();
    let store = state.store.lock().expect("store lock");
    let mut renewed = 0;
    for d in &r.certificates {
        crypto::check_certificate(&account.account_key, &d.device_key, d.issued_at, d.expires_at, &d.signature, now)
            .map_err(ApiError::bad)?;
        match store.device_owner(&d.device_key)? {
            Some((owner, None)) if owner == account.id => {
                renewed += usize::from(store.set_certificate(&d.device_key, &certificate(d))?);
            }
            // Revoked or someone else's: never renewed.
            _ => {}
        }
    }
    Ok(Json(json!({"renewed": renewed})))
}

#[derive(Deserialize)]
pub struct Revoke {
    device_key: String,
    revoked_at: i64,
    signature: String,
}

pub async fn revoke_device(State(state): State<AppState>, headers: HeaderMap, Json(r): Json<Revoke>) -> Reply {
    let account = authenticated(&state, &headers)?;
    let now = now_ms();
    if (r.revoked_at - now).abs() > SKEW_MS {
        return Err(ApiError::bad("revocation time is off"));
    }
    let statement = crypto::revoke_statement(&account.account_key, &r.device_key, r.revoked_at);
    if !crypto::verify(&account.account_key, &statement, &r.signature) {
        return Err(ApiError::bad("revocation signature does not match the account key"));
    }
    let store = state.store.lock().expect("store lock");
    match store.device_owner(&r.device_key)? {
        Some((owner, None)) if owner == account.id => {}
        Some((owner, Some(_))) if owner == account.id => return Ok(Json(json!({}))),
        _ => return Err(ApiError(StatusCode::NOT_FOUND, "not_found", "no such device".into())),
    }
    store.revoke(&account.account_key, &r.device_key, r.revoked_at, &r.signature)?;
    Ok(Json(json!({})))
}

pub async fn vault(State(state): State<AppState>, headers: HeaderMap) -> Reply {
    let account = authenticated(&state, &headers)?;
    let vault = state.store.lock().expect("store lock").vault(account.id)?;
    Ok(Json(json!(vault)))
}

#[derive(Deserialize)]
pub struct PutVault {
    vault: String,
    /// The version the change was made on (0 when there was no vault).
    version: i64,
}

pub async fn put_vault(State(state): State<AppState>, headers: HeaderMap, Json(r): Json<PutVault>) -> Reply {
    let account = authenticated(&state, &headers)?;
    let len = crypto::unb64(&r.vault).map(|b| b.len()).unwrap_or(0);
    if !(12 + 16..=MAX_VAULT_LEN).contains(&len) {
        return Err(ApiError::bad("invalid vault"));
    }
    let store = state.store.lock().expect("store lock");
    match store.put_vault(account.id, &r.vault, r.version, now_ms())? {
        Some(version) => Ok(Json(json!({"version": version}))),
        None => Err(ApiError(StatusCode::CONFLICT, "conflict", "the vault changed; fetch it and try again".into())),
    }
}

pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Reply {
    if let Some(token) = bearer(&headers) {
        state.store.lock().expect("store lock").delete_session(&token_hash(token))?;
    }
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
pub struct Since {
    #[serde(default)]
    since: i64,
}

pub async fn revocations(State(state): State<AppState>, Query(q): Query<Since>) -> Reply {
    let list = state.store.lock().expect("store lock").revocations(q.since, 1000)?;
    Ok(Json(json!({"revocations": list})))
}

pub async fn lookup(State(state): State<AppState>, Path(handle): Path<String>) -> Reply {
    let handle = crypto::handle(&handle)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "not_found", "no such account".into()))?;
    let account = state.store.lock().expect("store lock").account_by_handle(&handle)?;
    match account {
        Some(a) => Ok(Json(json!({"handle": a.handle, "account_key": a.account_key}))),
        None => Err(ApiError(StatusCode::NOT_FOUND, "not_found", "no such account".into())),
    }
}

pub async fn lookup_by_key(State(state): State<AppState>, Path(key): Path<String>) -> Reply {
    let account = state.store.lock().expect("store lock").account_by_key(&key)?;
    match account {
        Some(a) => Ok(Json(json!({"handle": a.handle, "account_key": a.account_key}))),
        None => Err(ApiError(StatusCode::NOT_FOUND, "not_found", "no such account".into())),
    }
}
