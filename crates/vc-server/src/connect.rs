//! Gwar Connect: signed device revocations and public account-handle lookups.
//! Authentication verifies certificates locally; neither HTTP path blocks hello.

use std::time::Duration;

use anyhow::{Result, ensure};
use serde::Deserialize;
use tracing::{debug, warn};

use crate::{
    core::{CoreHandle, RevokedDevice},
    identity,
};

const EVERY: Duration = Duration::from_secs(300);

#[derive(Deserialize)]
struct Feed {
    revocations: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    seq: i64,
    account_key: String,
    device_key: String,
    revoked_at: i64,
    signature: String,
}

/// Follows `{url}/v1/revocations` from position `seq`, forever.
pub async fn follow(url: String, core: CoreHandle, seq: i64) {
    follow_every(url, core, seq, EVERY).await
}

/// [`follow`], checking every `every`.
pub async fn follow_every(url: String, core: CoreHandle, mut seq: i64, every: Duration) {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(20)).build().expect("http client");
    loop {
        // Drain the feed (1000 per page), then wait.
        loop {
            match fetch(&client, &url, seq).await {
                Ok(entries) if entries.is_empty() => break,
                Ok(entries) => {
                    let last = entries.last().map(|e| e.seq).unwrap_or(seq);
                    let full = entries.len() >= 1000;
                    let devices = entries
                        .into_iter()
                        .filter(|e| {
                            let statement = identity::revoke_statement(&e.account_key, &e.device_key, e.revoked_at);
                            let ok = identity::signed_by(&e.account_key, &statement, &e.signature);
                            if !ok {
                                warn!(device = e.device_key, "ignoring a revocation not signed by its account");
                            }
                            ok
                        })
                        .map(|e| RevokedDevice {
                            device_key: e.device_key,
                            account_key: e.account_key,
                            revoked_at: e.revoked_at,
                        })
                        .collect();
                    core.revoked(devices, last).await;
                    seq = last;
                    if !full {
                        break;
                    }
                }
                Err(e) => {
                    debug!("gwar connect revocations: {e:#}");
                    break;
                }
            }
        }
        tokio::time::sleep(every).await;
    }
}

async fn fetch(client: &reqwest::Client, url: &str, since: i64) -> anyhow::Result<Vec<Entry>> {
    let url = format!("{}/v1/revocations?since={since}", url.trim_end_matches('/'));
    let feed: Feed = client.get(url).send().await?.error_for_status()?.json().await?;
    Ok(feed.revocations)
}

#[derive(Clone)]
pub(crate) struct Lookup {
    url: String,
    client: reqwest::Client,
    service_key: std::sync::Arc<tokio::sync::Mutex<Option<String>>>,
}

#[derive(Deserialize)]
struct Account {
    handle: String,
    account_key: String,
    attestation: vc_proto::HandleAttestation,
}

impl Lookup {
    async fn key(&self) -> Result<String> {
        #[derive(Deserialize)]
        struct ServiceKey {
            key: String,
        }
        let mut cached = self.service_key.lock().await;
        if let Some(key) = &*cached {
            return Ok(key.clone());
        }
        let url = format!("{}/v1/service-key", self.url.trim_end_matches('/'));
        let reply = self.client.get(url).send().await?.error_for_status()?;
        ensure!(reply.status() == reqwest::StatusCode::OK, "Connect returned status {}", reply.status());
        let ServiceKey { key } = reply.json().await?;
        use base64::Engine;
        let raw: [u8; 32] = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&key)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid Connect service key length"))?;
        ed25519_dalek::VerifyingKey::from_bytes(&raw)?;
        *cached = Some(key.clone());
        Ok(key)
    }

    pub fn new(url: String) -> Result<Self> {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?;
        Ok(Self { url, client, service_key: Default::default() })
    }

    pub async fn account(&self, key: &str) -> Result<Option<vc_proto::HandleAttestation>> {
        let url = format!("{}/v1/accounts/by-key/{key}", self.url.trim_end_matches('/'));
        let reply = self.client.get(url).send().await?;
        if reply.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let reply = reply.error_for_status()?;
        ensure!(reply.status() == reqwest::StatusCode::OK, "Connect returned status {}", reply.status());
        let account: Account = reply.json().await?;
        ensure!(account.account_key == key, "Connect returned a different account key");
        ensure!(
            (3..=32).contains(&account.handle.len())
                && account
                    .handle
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
                && !account.handle.starts_with('.')
                && !account.handle.ends_with('.'),
            "Connect returned an invalid handle"
        );
        let a = account.attestation;
        ensure!(a.account_key == key && a.handle == account.handle, "Connect returned a mismatched attestation");
        let service_key = self.key().await?;
        ensure!(
            identity::signed_by(&service_key, &vc_proto::handle_statement(&a), &a.signature),
            "Connect attestation signature is invalid"
        );
        let now = crate::core::now_ms();
        ensure!(
            a.issued_at <= now.saturating_add(5 * 60 * 1000) && a.expires_at > now && a.expires_at > a.issued_at,
            "Connect attestation dates are out of range"
        );
        Ok(Some(a))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, http::StatusCode, routing::get};
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use vc_proto::{HandleAttestation, handle_statement};

    #[tokio::test]
    async fn service_key_is_lazy_cached_and_retried_after_failure() {
        let service = SigningKey::from_bytes(&[42; 32]);
        let public = URL_SAFE_NO_PAD.encode(service.verifying_key().as_bytes());
        let calls = Arc::new(AtomicUsize::new(0));
        let failing = Arc::new(AtomicBool::new(true));
        let a = HandleAttestation {
            account_key: public.clone(),
            handle: "account".into(),
            issued_at: crate::core::now_ms(),
            expires_at: crate::core::now_ms() + 7 * 86400000,
            signature: String::new(),
        };
        let mut a = a;
        a.signature = URL_SAFE_NO_PAD.encode(service.sign(handle_statement(&a).as_bytes()).to_bytes());
        let app = Router::new()
            .route(
                "/v1/service-key",
                get({
                    let calls = calls.clone();
                    let failing = failing.clone();
                    let public = public.clone();
                    move || {
                        let calls = calls.clone();
                        let failing = failing.clone();
                        let public = public.clone();
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            if failing.load(Ordering::SeqCst) {
                                (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})))
                            } else {
                                (StatusCode::OK, Json(json!({"key": public})))
                            }
                        }
                    }
                }),
            )
            .route(
                "/v1/accounts/by-key/{key}",
                get({
                    let a = a.clone();
                    move || {
                        let a = a.clone();
                        async move { Json(json!({"handle": a.handle, "account_key": a.account_key, "attestation": a})) }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let lookup = Lookup::new(format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(lookup.account(&public).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        failing.store(false, Ordering::SeqCst);
        assert_eq!(lookup.account(&public).await.unwrap(), Some(a.clone()));
        failing.store(true, Ordering::SeqCst);
        assert_eq!(lookup.clone().account(&public).await.unwrap(), Some(a));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        task.abort();
    }
}
