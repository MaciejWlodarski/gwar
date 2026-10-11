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
    store::{Account, Certificate, Challenge, Credentials, Kdf, NewAccount, Store},
};

const SESSION_MS: i64 = 90 * 24 * 3600 * 1000;
const CHALLENGE_MS: i64 = 5 * 60 * 1000;
const MAX_DEVICE_NAME: usize = 64;

pub struct Connect {
    pub store: Mutex<Store>,
    service_key: ed25519_dalek::SigningKey,
    limits: Mutex<HashMap<String, Vec<Instant>>>,
    /// Keys the fake salts of unknown handles, so prelogin reveals nothing.
    secret: [u8; 32],
}

impl Connect {
    pub fn new(store: Store) -> anyhow::Result<Self> {
        Self::with_service_key(store, crate::service_key::generate())
    }

    pub fn with_service_key(store: Store, service_key: ed25519_dalek::SigningKey) -> anyhow::Result<Self> {
        let secret = match store.meta("secret")?.and_then(|s| crypto::unb64_len(&s, 32)) {
            Some(secret) => secret,
            None => {
                let secret: [u8; 32] = rand::random();
                store.set_meta("secret", &b64(&secret))?;
                secret.to_vec()
            }
        };
        Ok(Self {
            store: Mutex::new(store),
            service_key,
            limits: Mutex::default(),
            secret: secret.try_into().expect("32 bytes"),
        })
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
    store.touch_device(&device, now)?;
    store.account(id)?.ok_or_else(ApiError::unauthorized)
}

fn certificate(d: &DeviceCert) -> Certificate {
    Certificate { issued_at: d.issued_at, expires_at: d.expires_at, signature: d.signature.clone() }
}

fn new_enrollment(store: &Store, account: i64, device: Option<&str>) -> anyhow::Result<String> {
    let token = b64(&rand::random::<[u8; 32]>());
    let now = now_ms();
    store.insert_enrollment(&token_hash(&token), account, device, now, now + CHALLENGE_MS)?;
    Ok(token)
}

fn enrollment(state: &Connect, headers: &HeaderMap) -> Result<Account, ApiError> {
    let token = bearer(headers).ok_or_else(ApiError::unauthorized)?;
    let store = state.store.lock().expect("store lock");
    let (id, _) = store.enrollment(&token_hash(token), now_ms())?.ok_or_else(ApiError::unauthorized)?;
    store.account(id)?.ok_or_else(ApiError::unauthorized)
}

fn service(state: &Connect) -> String {
    b64(state.service_key.verifying_key().as_bytes())
}

fn check_kdf(kdf: &Kdf) -> Result<(), ApiError> {
    let salt = crypto::unb64(&kdf.salt).ok_or_else(|| ApiError::bad("invalid salt"))?;
    let ok = (16..=64).contains(&salt.len()) && kdf.m == 65_536 && kdf.t == 3 && kdf.p == 1;
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
    let token = new_enrollment(&store, id, Some(&d.device_key))?;
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
    let store = state.store.lock().expect("store lock");
    if store.account(account.id)?.is_none_or(|a| a.credential_version != account.credential_version) {
        return Err(ApiError::unauthorized());
    }
    let token = new_enrollment(&store, account.id, None)?;
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
    let token = new_enrollment(&store, account.id, None)?;
    Ok(Json(json!({"token": token, "account_key": account.account_key, "recovery_blob": account.recovery_blob})))
}

pub async fn account(State(state): State<AppState>, headers: HeaderMap) -> Reply {
    let a = authenticated(&state, &headers)?;
    Ok(Json(json!({"handle": a.handle, "account_key": a.account_key, "created_at": a.created_at})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChallengeRequest {
    purpose: String,
    device: Option<DeviceCert>,
}

pub async fn challenge(State(state): State<AppState>, headers: HeaderMap, Json(r): Json<ChallengeRequest>) -> Reply {
    let account = match r.purpose.as_str() {
        "session" => enrollment(&state, &headers)?,
        "password" | "recovery" => authenticated(&state, &headers)?,
        _ => return Err(ApiError::bad("unsupported challenge purpose")),
    };
    let hash = token_hash(bearer(&headers).ok_or_else(ApiError::unauthorized)?);
    if !state.allow(format!("challenge:{hash}"), 30, Duration::from_secs(60)) {
        return Err(ApiError::limited());
    }
    let now = now_ms();
    let store = state.store.lock().expect("store lock");
    let (certificate_hash, device_key, certificate_expires_at) = if r.purpose == "session" {
        let d = r.device.ok_or_else(|| ApiError::bad("device certificate required"))?;
        crypto::check_certificate(&account.account_key, &d.device_key, d.issued_at, d.expires_at, &d.signature, now)
            .map_err(ApiError::bad)?;
        let (_, bound) = store.enrollment(&hash, now)?.ok_or_else(ApiError::unauthorized)?;
        if bound.as_deref() != Some(&d.device_key) || store.device_owner(&d.device_key)? != Some((account.id, None)) {
            return Err(ApiError::unauthorized());
        }
        (
            crypto::certificate_hash(&account.account_key, &d.device_key, &certificate(&d)),
            Some(d.device_key),
            d.expires_at,
        )
    } else {
        if r.device.is_some() {
            return Err(ApiError::bad("unexpected device certificate"));
        }
        ("-".into(), None, 0)
    };
    let c = Challenge {
        nonce: b64(&rand::random::<[u8; 32]>()),
        token_hash: hash,
        account_id: account.id,
        purpose: r.purpose,
        certificate_hash,
        device_key,
        certificate_expires_at,
        credential_version: account.credential_version,
        expires_at: now + CHALLENGE_MS,
    };
    store.insert_challenge(&c)?;
    Ok(Json(
        json!({"challenge": c.nonce, "expires_at": c.expires_at, "credential_version": c.credential_version, "service_key": service(&state)}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionProof {
    challenge: String,
    device: DeviceCert,
    signature: String,
}

pub async fn session(State(state): State<AppState>, headers: HeaderMap, Json(r): Json<SessionProof>) -> Reply {
    let account = enrollment(&state, &headers)?;
    let hash = token_hash(bearer(&headers).ok_or_else(ApiError::unauthorized)?);
    let now = now_ms();
    let store = state.store.lock().expect("store lock");
    let c = store.challenge(&hash, &r.challenge, now)?.ok_or_else(ApiError::unauthorized)?;
    let d = &r.device;
    crypto::check_certificate(&account.account_key, &d.device_key, d.issued_at, d.expires_at, &d.signature, now)
        .map_err(ApiError::bad)?;
    if c.purpose != "session"
        || c.account_id != account.id
        || c.device_key.as_deref() != Some(&d.device_key)
        || c.certificate_hash != crypto::certificate_hash(&account.account_key, &d.device_key, &certificate(d))
        || !crypto::verify(
            &d.device_key,
            &crypto::session_statement(&service(&state), &account.account_key, &c),
            &r.signature,
        )
    {
        return Err(ApiError::bad("device proof does not match the challenge"));
    }
    let token = b64(&rand::random::<[u8; 32]>());
    if !store.exchange_session(&c, &token_hash(&token), now, now + SESSION_MS)? {
        return Err(ApiError::unauthorized());
    }
    Ok(Json(json!({"token": token})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Password {
    kdf: Kdf,
    auth_key: String,
    key_blob: String,
    challenge: String,
    credential_version: i64,
    signature: String,
}

fn credential_challenge(
    state: &Connect,
    headers: &HeaderMap,
    account: &Account,
    nonce: &str,
    version: i64,
    purpose: &str,
) -> Result<Challenge, ApiError> {
    let hash = token_hash(bearer(headers).ok_or_else(ApiError::unauthorized)?);
    let store = state.store.lock().expect("store lock");
    let c = store.challenge(&hash, nonce, now_ms())?.ok_or_else(ApiError::unauthorized)?;
    if c.account_id != account.id || c.purpose != purpose {
        return Err(ApiError::bad("wrong challenge purpose"));
    }
    if c.credential_version != version || account.credential_version != version {
        return Err(ApiError(StatusCode::CONFLICT, "conflict", "the credentials changed; unlock again".into()));
    }
    Ok(c)
}

pub async fn password(State(state): State<AppState>, headers: HeaderMap, Json(r): Json<Password>) -> Reply {
    let account = authenticated(&state, &headers)?;
    check_kdf(&r.kdf)?;
    check_blob(&r.key_blob)?;
    let auth = unb64_len(&r.auth_key, 32).ok_or_else(|| ApiError::bad("invalid authentication key"))?;
    let c = credential_challenge(&state, &headers, &account, &r.challenge, r.credential_version, "password")?;
    let statement = crypto::credentials_statement(
        &service(&state),
        &account.account_key,
        &c,
        &crypto::kdf_hash(&r.kdf),
        &crypto::hash(&auth),
        &crypto::hash(&crypto::unb64(&r.key_blob).expect("checked blob")),
    );
    if !crypto::verify(&account.account_key, &statement, &r.signature) {
        return Err(ApiError::bad("credential signature does not match the account key"));
    }
    let auth_hash = hash_auth(&r.auth_key).await?;
    let store = state.store.lock().expect("store lock");
    if !store.update_credentials(
        &c,
        &Credentials::Password { kdf: &r.kdf, auth_hash: &auth_hash, key_blob: &r.key_blob },
        now_ms(),
    )? {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "conflict",
            "challenge expired, consumed or credentials changed".into(),
        ));
    }
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryCredentials {
    recovery_auth: String,
    recovery_blob: String,
    challenge: String,
    credential_version: i64,
    signature: String,
}

pub async fn recovery_credentials(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(r): Json<RecoveryCredentials>,
) -> Reply {
    let account = authenticated(&state, &headers)?;
    check_blob(&r.recovery_blob)?;
    let auth_hash = recovery_hash(&r.recovery_auth)?;
    let c = credential_challenge(&state, &headers, &account, &r.challenge, r.credential_version, "recovery")?;
    let statement = crypto::credentials_statement(
        &service(&state),
        &account.account_key,
        &c,
        "-",
        &auth_hash,
        &crypto::hash(&crypto::unb64(&r.recovery_blob).expect("checked blob")),
    );
    if !crypto::verify(&account.account_key, &statement, &r.signature) {
        return Err(ApiError::bad("credential signature does not match the account key"));
    }
    if !state.store.lock().expect("store lock").update_credentials(
        &c,
        &Credentials::Recovery { auth_hash: &auth_hash, key_blob: &r.recovery_blob },
        now_ms(),
    )? {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "conflict",
            "challenge expired, consumed or credentials changed".into(),
        ));
    }
    Ok(Json(json!({})))
}

pub async fn devices(State(state): State<AppState>, headers: HeaderMap) -> Reply {
    let account = authenticated(&state, &headers)?;
    let devices = state.store.lock().expect("store lock").devices(account.id)?;
    Ok(Json(json!({"devices": devices})))
}

pub async fn add_device(State(state): State<AppState>, headers: HeaderMap, Json(d): Json<DeviceCert>) -> Reply {
    let account = enrollment(&state, &headers)?;
    let now = now_ms();
    crypto::check_certificate(&account.account_key, &d.device_key, d.issued_at, d.expires_at, &d.signature, now)
        .map_err(ApiError::bad)?;
    let store = state.store.lock().expect("store lock");
    let hash = token_hash(bearer(&headers).ok_or_else(ApiError::unauthorized)?);
    let (_, bound) = store.enrollment(&hash, now)?.ok_or_else(ApiError::unauthorized)?;
    if bound.as_ref().is_some_and(|key| key != &d.device_key) {
        return Err(ApiError::bad("enrollment belongs to another device"));
    }
    match store.device_owner(&d.device_key)? {
        Some((owner, _)) if owner != account.id => {
            return Err(ApiError(StatusCode::CONFLICT, "taken", "device key belongs to another account".into()));
        }
        Some((_, Some(_))) => return Err(ApiError(StatusCode::GONE, "revoked", "this device was revoked".into())),
        _ => {}
    }
    store.upsert_device(account.id, &d.device_key, &device_name(&d.name), now)?;
    store.set_certificate(&d.device_key, &certificate(&d))?;
    store.bind_enrollment(&hash, &d.device_key)?;
    Ok(Json(json!({})))
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
    if r.revoked_at < 0 || r.revoked_at.abs_diff(now) > SKEW_MS as u64 {
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

pub async fn service_key(State(state): State<AppState>) -> Json<Value> {
    Json(json!({"key": b64(state.service_key.verifying_key().as_bytes())}))
}

fn public_account(state: &Connect, a: Account) -> Json<Value> {
    let attestation = crate::service_key::attest(&state.service_key, &a.account_key, &a.handle, now_ms());
    Json(json!({"handle": a.handle, "account_key": a.account_key, "attestation": attestation}))
}

pub async fn lookup(State(state): State<AppState>, Path(handle): Path<String>) -> Reply {
    let handle = crypto::handle(&handle)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "not_found", "no such account".into()))?;
    let account = state.store.lock().expect("store lock").account_by_handle(&handle)?;
    match account {
        Some(a) => Ok(public_account(&state, a)),
        None => Err(ApiError(StatusCode::NOT_FOUND, "not_found", "no such account".into())),
    }
}

pub async fn lookup_by_key(State(state): State<AppState>, Path(key): Path<String>) -> Reply {
    let account = state.store.lock().expect("store lock").account_by_key(&key)?;
    match account {
        Some(a) => Ok(public_account(&state, a)),
        None => Err(ApiError(StatusCode::NOT_FOUND, "not_found", "no such account".into())),
    }
}
