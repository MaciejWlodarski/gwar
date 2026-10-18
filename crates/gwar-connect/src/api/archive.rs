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
use crate::store::Account;

const MAX_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_TRANSFER_BYTES: usize = 4096;
const MAX_RECORD_BYTES: usize = 12 + 512 + 16;
const MAX_RECORDS: i64 = 200_000;
const MAX_RECORD_TOTAL: i64 = 64 * 1024 * 1024;
const MAX_MANIFESTS: i64 = 1000;
const MAX_TRANSFERS: i64 = 10_000;

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
/// Exactly `count` netstrings.
fn netstrings(mut input: &[u8], count: usize) -> Option<Vec<&[u8]>> {
    let mut fields = Vec::with_capacity(count);
    while !input.is_empty() {
        if fields.len() == count {
            return None;
        }
        let colon = input.iter().position(|&b| b == b':')?;
        let len = decimal(std::str::from_utf8(&input[..colon]).ok()?)? as usize;
        let rest = &input[colon + 1..];
        if rest.len() <= len || rest[len] != b',' {
            return None;
        }
        fields.push(&rest[..len]);
        input = &rest[len + 1..];
    }
    (fields.len() == count).then_some(fields)
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
    let store = state.store.lock().expect("store lock");
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
    let (account, _) = device_session(&state, &headers)?;
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
    let store = state.store.lock().expect("store lock");
    match store.archive_recovery_newest(account.id)? {
        Some((newest, statement)) if newest == version && statement == r.statement => Ok(Json(json!({}))),
        newest if version == newest.as_ref().map_or(1, |(n, _)| n + 1) => {
            store.put_archive_recovery(account.id, version, &r.statement, &r.signature)?;
            Ok(Json(json!({})))
        }
        _ => Err(conflict("only the next recovery version")),
    }
}

/// `GET /v1/archive/recovery`
pub async fn recovery(State(state): State<AppState>, headers: HeaderMap) -> Reply {
    let (account, _) = device_session(&state, &headers)?;
    let keys: Vec<_> = state
        .store
        .lock()
        .expect("store lock")
        .archive_recovery(account.id)?
        .into_iter()
        .map(|(statement, signature)| json!({"statement": statement, "signature": signature}))
        .collect();
    Ok(Json(json!({"keys": keys})))
}

#[derive(Deserialize)]
pub struct NewManifest {
    manifest: String,
}

/// `POST /v1/archive/manifests`: the first manifest of a generation, created by this device.
pub async fn put_manifest(State(state): State<AppState>, headers: HeaderMap, Json(r): Json<NewManifest>) -> Reply {
    let (account, device) = device_session(&state, &headers)?;
    let bytes = unb64(&r.manifest)
        .filter(|b| b.len() <= MAX_MANIFEST_BYTES)
        .ok_or_else(|| ApiError::bad("invalid manifest"))?;
    let [statement, signature, _certificate, _body] =
        netstrings(&bytes, 4).ok_or_else(|| ApiError::bad("invalid manifest"))?[..]
    else {
        return Err(ApiError::bad("invalid manifest"));
    };
    let statement = std::str::from_utf8(statement).map_err(|_| ApiError::bad("invalid manifest"))?;
    let fields: Vec<&str> = statement.split('\n').collect();
    let (Some(&"gwar archive manifest v1"), Some(&account_key), Some(generation), Some(&creator)) =
        (fields.first(), fields.get(1), fields.get(2).and_then(|g| decimal(g)), fields.get(4))
    else {
        return Err(ApiError::bad("invalid manifest"));
    };
    if fields.len() != 10 || account_key != account.account_key || generation < 1 || creator != device {
        return Err(ApiError::bad("invalid manifest"));
    }
    let signature = std::str::from_utf8(signature).map_err(|_| ApiError::bad("invalid manifest"))?;
    if !crypto::verify(&device, statement, signature) {
        return Err(ApiError::bad("the manifest is not signed by this device"));
    }
    let store = state.store.lock().expect("store lock");
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
    let (account, _) = device_session(&state, &headers)?;
    if r.records.is_empty() || r.records.len() > 100 {
        return Err(ApiError::bad("1 to 100 records"));
    }
    let mut size = 0;
    for record in &r.records {
        let data = unb64(&record.data).ok_or_else(|| ApiError::bad("invalid record"))?;
        if unb64_len(&record.record_id, 16).is_none()
            || !(28..=MAX_RECORD_BYTES).contains(&data.len())
            || record.generation < 1
        {
            return Err(ApiError::bad("invalid record"));
        }
        size += record.data.len() as i64;
    }
    let store = state.store.lock().expect("store lock");
    let generations: std::collections::BTreeSet<i64> = r.records.iter().map(|r| r.generation).collect();
    for generation in generations {
        if store.archive_manifests(account.id, generation - 1, 1)?.first().is_none_or(|(g, _)| *g != generation) {
            return Err(ApiError::bad("no such generation"));
        }
    }
    let (count, total, _, _) = store.archive_counts(account.id)?;
    if count + r.records.len() as i64 > MAX_RECORDS || total + size > MAX_RECORD_TOTAL {
        return Err(quota());
    }
    let rows: Vec<(i64, String, String)> = r.records.into_iter().map(|r| (r.generation, r.record_id, r.data)).collect();
    store.put_archive_records(account.id, &rows)?;
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

/// `POST /v1/archive/transfers`: a transfer this device signed.
pub async fn put_transfer(State(state): State<AppState>, headers: HeaderMap, Json(r): Json<NewTransfer>) -> Reply {
    let (account, device) = device_session(&state, &headers)?;
    let bytes = unb64(&r.transfer)
        .filter(|b| b.len() <= MAX_TRANSFER_BYTES)
        .ok_or_else(|| ApiError::bad("invalid transfer"))?;
    let [statement, signature, _certificate, _sealed] =
        netstrings(&bytes, 4).ok_or_else(|| ApiError::bad("invalid transfer"))?[..]
    else {
        return Err(ApiError::bad("invalid transfer"));
    };
    let statement = std::str::from_utf8(statement).map_err(|_| ApiError::bad("invalid transfer"))?;
    let fields: Vec<&str> = statement.split('\n').collect();
    if fields.len() != 9
        || fields[0] != "gwar archive transfer v1"
        || fields[1] != account.account_key
        || decimal(fields[2]).is_none_or(|g| g < 1)
        || fields[4] != device
    {
        return Err(ApiError::bad("invalid transfer"));
    }
    let signature = std::str::from_utf8(signature).map_err(|_| ApiError::bad("invalid transfer"))?;
    if !crypto::verify(&device, statement, signature) {
        return Err(ApiError::bad("the transfer is not signed by this device"));
    }
    let store = state.store.lock().expect("store lock");
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
