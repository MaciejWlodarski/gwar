//! The key archive (docs/e2e-build.md, section 7, "Connect API"). Connect stores ciphertext and
//! checks what it can: shapes and sizes, who signed each statement, the first manifest per
//! generation and quotas. It never sees generation keys or what a record belongs to.

use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
};
use serde::Deserialize;
use serde_json::json;

use super::{ApiError, AppState, Reply, bearer, now_ms, token_hash};
use crate::crypto::{self, SKEW_MS, unb64, unb64_len};
use crate::store::{Account, Store};

const MAX_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_TRANSFER_BYTES: usize = 4096;
const MAX_RECORD_BYTES: usize = 12 + 512 + 16;
const MAX_RECORDS: i64 = 200_000;
const MAX_RECORD_TOTAL: i64 = 64 * 1024 * 1024;
const MAX_MANIFESTS: i64 = 1000;
const MAX_TRANSFERS: i64 = 10_000;
/// Recovery key versions an account may publish (each a recovery code change).
const MAX_RECOVERY_VERSIONS: i64 = 100;
/// Archive writes per account: a device's pass uploads at most 50 batches of 100 records.
const WRITES_PER_WINDOW: usize = 600;
const WRITE_WINDOW: std::time::Duration = std::time::Duration::from_secs(600);

/// A device session: the account and the session's own device.
fn device_session(state: &AppState, headers: &HeaderMap) -> Result<(Account, String), ApiError> {
    let token = bearer(headers).ok_or_else(ApiError::unauthorized)?;
    let hash = token_hash(token);
    let now = now_ms();
    let store = state.store.lock().expect("store lock");
    let (id, device) = store.session(&hash, now)?.ok_or_else(ApiError::unauthorized)?;
    let account = store.account(id)?.ok_or_else(ApiError::unauthorized)?;
    Ok((account, device))
}
/// Before an archive write: the account's write budget, then (by the caller, under the store lock
/// that covers the write) the session again, so a device revoked meanwhile writes nothing.
fn limit_writes(state: &AppState, account: &Account) -> Result<(), ApiError> {
    if state.allow(format!("archive-write:{}", account.id), WRITES_PER_WINDOW, WRITE_WINDOW) {
        Ok(())
    } else {
        Err(ApiError::limited())
    }
}
fn recheck(store: &Store, headers: &HeaderMap, account: &Account, device: &str) -> Result<(), ApiError> {
    let token = bearer(headers).ok_or_else(ApiError::unauthorized)?;
    match store.session(&token_hash(token), now_ms())? {
        Some((id, d)) if id == account.id && d == device => Ok(()),
        _ => Err(ApiError::unauthorized()),
    }
}
fn conflict(message: &str) -> ApiError {
    ApiError(StatusCode::CONFLICT, "conflict", message.into())
}
fn quota() -> ApiError {
    ApiError(StatusCode::INSUFFICIENT_STORAGE, "quota_exceeded", "the archive is full".into())
}
fn decimal(value: &str) -> Option<i64> {
    let ok =
        !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) && (value == "0" || !value.starts_with('0'));
    ok.then(|| value.parse::<i64>().ok()).flatten().filter(|v| *v <= 9_007_199_254_740_991)
}
fn key32(value: &str) -> bool {
    unb64_len(value, 32).is_some()
}

#[derive(Deserialize)]
pub struct Signed {
    statement: String,
    signature: String,
}

/// `PUT /v1/archive/device`: the session's own device's archive key, once.
pub async fn put_device(State(state): State<AppState>, headers: HeaderMap, Json(r): Json<Signed>) -> Reply {
    let (account, device) = device_session(&state, &headers)?;
    let [header, account_key, device_key, x25519, created_at] = r.statement.split('\n').collect::<Vec<_>>()[..] else {
        return Err(ApiError::bad("invalid statement"));
    };
    if header != "gwar archive device v1"
        || account_key != account.account_key
        || device_key != device
        || !key32(x25519)
        || decimal(created_at).is_none_or(|t| t.abs_diff(now_ms()) > SKEW_MS as u64)
    {
        return Err(ApiError::bad("invalid statement"));
    }
    if !crypto::verify(&device, &r.statement, &r.signature) {
        return Err(ApiError::bad("the statement is not signed by this device"));
    }
    limit_writes(&state, &account)?;
    let store = state.store.lock().expect("store lock");
    recheck(&store, &headers, &account, &device)?;
    if !store.put_archive_device(account.id, &device, &r.statement, &r.signature)? {
        return Err(conflict("this device already has an archive key"));
    }
    Ok(Json(json!({})))
}

/// `GET /v1/archive/devices`: the account's devices' archive keys with their current certificates.
pub async fn devices(State(state): State<AppState>, headers: HeaderMap) -> Reply {
    let (account, _) = device_session(&state, &headers)?;
    let list = state.store.lock().expect("store lock").archive_devices(account.id)?;
    let devices: Vec<_> = list
        .into_iter()
        .map(|(statement, signature, certificate)| json!({"statement": statement, "signature": signature, "certificate": certificate}))
        .collect();
    Ok(Json(json!({"devices": devices})))
}

/// `PUT /v1/archive/recovery`: the next recovery archive key version, signed by the account key
/// (the identical statement for the newest version again is accepted).
pub async fn put_recovery(State(state): State<AppState>, headers: HeaderMap, Json(r): Json<Signed>) -> Reply {
    let (account, device) = device_session(&state, &headers)?;
    let [header, account_key, version, x25519, created_at] = r.statement.split('\n').collect::<Vec<_>>()[..] else {
        return Err(ApiError::bad("invalid statement"));
    };
    let version = decimal(version).filter(|v| *v >= 1).ok_or_else(|| ApiError::bad("invalid version"))?;
    if header != "gwar archive recovery v1"
        || account_key != account.account_key
        || !key32(x25519)
        || decimal(created_at).is_none()
    {
        return Err(ApiError::bad("invalid statement"));
    }
    if !crypto::verify(&account.account_key, &r.statement, &r.signature) {
        return Err(ApiError::bad("the statement is not signed by the account key"));
    }
    limit_writes(&state, &account)?;
    let store = state.store.lock().expect("store lock");
    recheck(&store, &headers, &account, &device)?;
    match store.archive_recovery_newest(account.id)? {
        Some((newest, statement)) if newest == version && statement == r.statement => Ok(Json(json!({}))),
        _ if version > MAX_RECOVERY_VERSIONS => Err(quota()),
        newest if version == newest.as_ref().map_or(1, |(n, _)| n + 1) => {
            store.put_archive_recovery(account.id, version, &r.statement, &r.signature)?;
            Ok(Json(json!({})))
        }
        _ => Err(conflict("only the next recovery version")),
    }
}

/// `GET /v1/archive/recovery`: the newest recovery archive key (readers need only the current one).
pub async fn recovery(State(state): State<AppState>, headers: HeaderMap) -> Reply {
    let (account, _) = device_session(&state, &headers)?;
    let keys: Vec<_> = state
        .store
        .lock()
        .expect("store lock")
        .archive_recovery_newest_signed(account.id)?
        .into_iter()
        .map(|(statement, signature)| json!({"statement": statement, "signature": signature}))
        .collect();
    Ok(Json(json!({"keys": keys})))
}

#[derive(Deserialize)]
pub struct NewManifest {
    manifest: String,
}

/// `POST /v1/archive/manifests`: the first manifest of a generation, created by this device and
/// checked as a reader checks it on its own (statement, signature, certificate, body and its hash),
/// so a malformed one cannot take a generation.
pub async fn put_manifest(State(state): State<AppState>, headers: HeaderMap, Json(r): Json<NewManifest>) -> Reply {
    let (account, device) = device_session(&state, &headers)?;
    let bytes = unb64(&r.manifest)
        .filter(|b| b.len() <= MAX_MANIFEST_BYTES)
        .ok_or_else(|| ApiError::bad("invalid manifest"))?;
    let manifest = gwar_mls::archive::Manifest::parse(&bytes, &account.account_key)
        .map_err(|_| ApiError::bad("invalid manifest"))?;
    if manifest.creator != device {
        return Err(ApiError::bad("the manifest is not this device's"));
    }
    let generation = i64::try_from(manifest.generation).map_err(|_| ApiError::bad("invalid manifest"))?;
    limit_writes(&state, &account)?;
    let store = state.store.lock().expect("store lock");
    recheck(&store, &headers, &account, &device)?;
    // An existing generation is a conflict (the client then takes the winner), also at the quota.
    if store.archive_manifests(account.id, generation - 1, 1)?.first().is_some_and(|(g, _)| *g == generation) {
        return Err(conflict("this generation exists"));
    }
    if store.archive_counts(account.id)?.2 >= MAX_MANIFESTS {
        return Err(quota());
    }
    if !store.put_archive_manifest(account.id, generation, &r.manifest)? {
        return Err(conflict("this generation exists"));
    }
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
pub struct After {
    #[serde(default)]
    after: i64,
}

/// `GET /v1/archive/manifests?after=<generation>`
pub async fn manifests(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<After>) -> Reply {
    let (account, _) = device_session(&state, &headers)?;
    let list = state.store.lock().expect("store lock").archive_manifests(account.id, q.after, 101)?;
    let next = (list.len() > 100).then(|| list[99].0);
    let manifests: Vec<_> = list
        .into_iter()
        .take(100)
        .map(|(generation, manifest)| json!({"generation": generation, "manifest": manifest}))
        .collect();
    Ok(Json(json!({"manifests": manifests, "next": next})))
}

#[derive(Deserialize)]
pub struct NewRecord {
    generation: i64,
    record_id: String,
    data: String,
}
#[derive(Deserialize)]
pub struct NewRecords {
    records: Vec<NewRecord>,
}

/// `POST /v1/archive/records`: up to 100 records under generations Connect holds.
pub async fn put_records(State(state): State<AppState>, headers: HeaderMap, Json(r): Json<NewRecords>) -> Reply {
    let (account, device) = device_session(&state, &headers)?;
    if r.records.is_empty() || r.records.len() > 100 {
        return Err(ApiError::bad("1 to 100 records"));
    }
    for record in &r.records {
        let data = unb64(&record.data).ok_or_else(|| ApiError::bad("invalid record"))?;
        if unb64_len(&record.record_id, 16).is_none()
            || !(28..=MAX_RECORD_BYTES).contains(&data.len())
            || record.generation < 1
        {
            return Err(ApiError::bad("invalid record"));
        }
    }
    limit_writes(&state, &account)?;
    let store = state.store.lock().expect("store lock");
    recheck(&store, &headers, &account, &device)?;
    let generations: std::collections::BTreeSet<i64> = r.records.iter().map(|r| r.generation).collect();
    for generation in generations {
        if store.archive_manifests(account.id, generation - 1, 1)?.first().is_none_or(|(g, _)| *g != generation) {
            return Err(ApiError::bad("no such generation"));
        }
    }
    // Only records it does not hold count (a retried batch is accepted again).
    let rows: Vec<(i64, String, String)> = r.records.into_iter().map(|r| (r.generation, r.record_id, r.data)).collect();
    if !store.put_archive_records(account.id, &rows, MAX_RECORDS, MAX_RECORD_TOTAL)? {
        return Err(quota());
    }
    Ok(Json(json!({})))
}

/// `GET /v1/archive/records?after=<cursor>`
pub async fn records(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<After>) -> Reply {
    let (account, _) = device_session(&state, &headers)?;
    let list = state.store.lock().expect("store lock").archive_records(account.id, q.after, 1001)?;
    let next = (list.len() > 1000).then(|| list[999].0);
    let records: Vec<_> = list
        .into_iter()
        .take(1000)
        .map(|(cursor, generation, record_id, data)| json!({"cursor": cursor, "generation": generation, "record_id": record_id, "data": data}))
        .collect();
    Ok(Json(json!({"records": records, "next": next})))
}

#[derive(Deserialize)]
pub struct NewTransfer {
    transfer: String,
}

/// `POST /v1/archive/transfers`: a transfer this device signed, checked as a reader checks it
/// (statement, signature, certificate and the sealed key's hash).
pub async fn put_transfer(State(state): State<AppState>, headers: HeaderMap, Json(r): Json<NewTransfer>) -> Reply {
    let (account, device) = device_session(&state, &headers)?;
    let bytes = unb64(&r.transfer)
        .filter(|b| b.len() <= MAX_TRANSFER_BYTES)
        .ok_or_else(|| ApiError::bad("invalid transfer"))?;
    let transfer = gwar_mls::archive::Transfer::parse(&bytes, &account.account_key)
        .map_err(|_| ApiError::bad("invalid transfer"))?;
    if transfer.sender != device {
        return Err(ApiError::bad("the transfer is not this device's"));
    }
    limit_writes(&state, &account)?;
    let store = state.store.lock().expect("store lock");
    recheck(&store, &headers, &account, &device)?;
    if store.archive_counts(account.id)?.3 >= MAX_TRANSFERS {
        return Err(quota());
    }
    store.put_archive_transfer(account.id, &r.transfer)?;
    Ok(Json(json!({})))
}

/// `GET /v1/archive/transfers?after=<cursor>`
pub async fn transfers(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<After>) -> Reply {
    let (account, _) = device_session(&state, &headers)?;
    let list = state.store.lock().expect("store lock").archive_transfers(account.id, q.after, 101)?;
    let next = (list.len() > 100).then(|| list[99].0);
    let transfers: Vec<_> =
        list.into_iter().take(100).map(|(cursor, transfer)| json!({"cursor": cursor, "transfer": transfer})).collect();
    Ok(Json(json!({"transfers": transfers, "next": next})))
}
