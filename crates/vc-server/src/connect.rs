//! Gwar Connect: signed device revocations and public account-handle lookups.
//! Authentication verifies certificates locally; neither HTTP path blocks hello.

use std::{net::IpAddr, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, de::DeserializeOwned};
use tracing::{debug, warn};

use crate::{
    core::{CoreHandle, RevokedDevice},
    identity,
};

const EVERY: Duration = Duration::from_secs(300);
/// One request, including reading its body.
const DEADLINE: Duration = Duration::from_secs(20);
/// Larger than a full revocations page (1000 entries of ~300 bytes).
const MAX_BODY: usize = 1 << 20;
const PAGE: usize = 1000;

/// The only way this server talks to Connect: HTTPS (plain HTTP only to loopback), no redirects, a
/// bounded body read within one deadline, and nothing but `200` (or `404` where absence is an answer).
#[derive(Clone)]
struct Http {
    base: String,
    client: reqwest::Client,
    deadline: Duration,
}

impl Http {
    fn new(url: &str) -> Result<Self> {
        Self::with_deadline(url, DEADLINE)
    }

    fn with_deadline(url: &str, deadline: Duration) -> Result<Self> {
        let parsed = reqwest::Url::parse(url).with_context(|| format!("invalid Gwar Connect URL {url}"))?;
        let loopback = match parsed.host_str() {
            Some(host) if host.eq_ignore_ascii_case("localhost") => true,
            Some(host) => host.trim_matches(['[', ']']).parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()),
            None => false,
        };
        ensure!(
            parsed.scheme() == "https" || (parsed.scheme() == "http" && loopback),
            "Gwar Connect must be reached over HTTPS (plain HTTP only on loopback): {url}"
        );
        ensure!(
            parsed.username().is_empty()
                && parsed.password().is_none()
                && parsed.query().is_none()
                && parsed.fragment().is_none(),
            "Gwar Connect URL must not carry credentials, a query or a fragment: {url}"
        );
        let client = reqwest::Client::builder()
            .timeout(deadline)
            .redirect(reqwest::redirect::Policy::none())
            .https_only(parsed.scheme() == "https")
            .build()?;
        Ok(Self { base: url.trim_end_matches('/').to_owned(), client, deadline })
    }

    /// `GET {base}{path}`: `Ok(None)` for `404`, the parsed body for `200`, an error for anything else.
    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<Option<T>> {
        tokio::time::timeout(self.deadline, async {
            let mut reply = self.client.get(format!("{}{path}", self.base)).send().await?;
            match reply.status() {
                reqwest::StatusCode::OK => {}
                reqwest::StatusCode::NOT_FOUND => return Ok(None),
                status => bail!("Connect returned status {status}"),
            }
            ensure!(reply.content_length().is_none_or(|n| n <= MAX_BODY as u64), "Connect reply is too large");
            let mut body = Vec::new();
            while let Some(chunk) = reply.chunk().await? {
                ensure!(body.len() + chunk.len() <= MAX_BODY, "Connect reply is too large");
                body.extend_from_slice(&chunk);
            }
            Ok(Some(serde_json::from_slice(&body)?))
        })
        .await
        .context("Connect did not answer in time")?
    }
}

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
    let http = match Http::new(&url) {
        Ok(http) => http,
        Err(e) => return warn!("not following Gwar Connect revocations: {e:#}"),
    };
    loop {
        // Drain the feed (PAGE per page), then wait.
        loop {
            match fetch(&http, seq).await {
                Ok(entries) if entries.is_empty() => break,
                Ok(entries) => {
                    let full = entries.len() >= PAGE;
                    let (devices, last) = verified_revocations(entries, seq);
                    if last == seq {
                        break;
                    }
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

fn verified_revocations(mut entries: Vec<Entry>, mut seq: i64) -> (Vec<RevokedDevice>, i64) {
    entries.sort_by_key(|e| e.seq);
    let mut devices = Vec::new();
    for e in entries {
        if e.seq <= seq {
            continue;
        }
        seq = e.seq;
        let statement = identity::revoke_statement(&e.account_key, &e.device_key, e.revoked_at);
        if !identity::signed_by(&e.account_key, &statement, &e.signature) {
            warn!(device = e.device_key, "ignoring a revocation not signed by its account");
            continue;
        }
        devices.push(RevokedDevice {
            seq: e.seq,
            signature: e.signature,
            device_key: e.device_key,
            account_key: e.account_key,
            revoked_at: e.revoked_at,
        });
    }
    (devices, seq)
}

async fn fetch(http: &Http, since: i64) -> Result<Vec<Entry>> {
    let feed: Feed =
        http.get(&format!("/v1/revocations?since={since}")).await?.context("Connect has no revocations feed")?;
    ensure!(feed.revocations.len() <= PAGE, "Connect returned an oversized revocations page");
    Ok(feed.revocations)
}

#[derive(Clone)]
pub(crate) struct Lookup {
    http: Http,
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
        let ServiceKey { key } = self.http.get("/v1/service-key").await?.context("Connect has no service key")?;
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
        Ok(Self { http: Http::new(&url)?, service_key: Default::default() })
    }

    pub async fn account(&self, key: &str) -> Result<Option<vc_proto::HandleAttestation>> {
        ensure!(
            !key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "invalid account key"
        );
        let Some(account) = self.http.get::<Account>(&format!("/v1/accounts/by-key/{key}")).await? else {
            return Ok(None);
        };
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

    #[test]
    fn revocation_cursors_ignore_old_and_duplicate_entries_and_never_go_backwards() {
        let account = SigningKey::from_bytes(&[1; 32]);
        let key = URL_SAFE_NO_PAD.encode(account.verifying_key().as_bytes());
        let entry = |seq| Entry {
            seq,
            account_key: key.clone(),
            device_key: key.clone(),
            revoked_at: 1,
            signature: URL_SAFE_NO_PAD
                .encode(account.sign(identity::revoke_statement(&key, &key, 1).as_bytes()).to_bytes()),
        };
        let (devices, cursor) = verified_revocations(vec![entry(8), entry(4), entry(7), entry(7)], 5);
        assert_eq!(cursor, 8);
        assert_eq!(devices.iter().map(|d| d.seq).collect::<Vec<_>>(), vec![7, 8]);
        let (devices, cursor) = verified_revocations(vec![entry(1), entry(8)], cursor);
        assert!(devices.is_empty());
        assert_eq!(cursor, 8);
    }

    #[test]
    fn connect_is_reached_only_over_https_or_loopback() {
        for ok in ["https://gwar.example/connect", "http://127.0.0.1:9", "http://[::1]:9/", "http://localhost:9"] {
            assert!(Http::new(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://gwar.example/connect",
            "http://10.0.0.1",
            "ftp://gwar.example",
            "https://user:pw@gwar.example",
            "https://gwar.example/?a=b",
            "not a url",
        ] {
            assert!(Http::new(bad).is_err(), "{bad}");
        }
    }

    /// Valid Connect answers for the three things the server asks for.
    fn honest(path: &str) -> serde_json::Value {
        let service = SigningKey::from_bytes(&[42; 32]);
        let public = URL_SAFE_NO_PAD.encode(service.verifying_key().as_bytes());
        let mut a = HandleAttestation {
            account_key: public.clone(),
            handle: "account".into(),
            issued_at: crate::core::now_ms(),
            expires_at: crate::core::now_ms() + 86400000,
            signature: String::new(),
        };
        a.signature = URL_SAFE_NO_PAD.encode(service.sign(handle_statement(&a).as_bytes()).to_bytes());
        match path {
            "/v1/service-key" => json!({"key": public}),
            "/v1/revocations" => json!({"revocations": []}),
            _ => json!({"handle": a.handle, "account_key": a.account_key, "attestation": a}),
        }
    }

    async fn serve(
        reply: impl Fn(&str) -> axum::response::Response + Clone + Send + Sync + 'static,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().fallback(move |uri: axum::http::Uri| async move { reply(uri.path()) });
        (url, tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }))
    }

    /// Asks for revocations, the service key and an account; `Ok` for each request that succeeded.
    async fn ask(url: &str) -> [bool; 3] {
        let account = URL_SAFE_NO_PAD.encode(SigningKey::from_bytes(&[42; 32]).verifying_key().as_bytes());
        let lookup = Lookup::new(url.into()).unwrap();
        [
            fetch(&Http::new(url).unwrap(), 0).await.is_ok(),
            lookup.key().await.is_ok(),
            lookup.account(&account).await.is_ok_and(|a| a.is_some()),
        ]
    }

    #[tokio::test]
    async fn redirects_are_never_followed() {
        use axum::response::IntoResponse;
        let hits = Arc::new(AtomicUsize::new(0));
        let (target, target_task) = serve({
            let hits = hits.clone();
            move |path| {
                hits.fetch_add(1, Ordering::SeqCst);
                Json(honest(path)).into_response()
            }
        })
        .await;
        assert_eq!(ask(&target).await, [true; 3]);
        hits.store(0, Ordering::SeqCst);
        let (url, task) =
            serve(move |path| axum::response::Redirect::temporary(&format!("{target}{path}")).into_response()).await;
        assert_eq!(ask(&url).await, [false; 3]);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        task.abort();
        target_task.abort();
    }

    #[tokio::test]
    async fn oversized_replies_are_refused_with_or_without_a_length() {
        use axum::response::IntoResponse;
        let padded = |path: &str| {
            let mut body = honest(path);
            body["padding"] = json!("x".repeat(MAX_BODY));
            serde_json::to_vec(&body).unwrap()
        };
        let (url, task) = serve(move |path| padded(path).into_response()).await;
        assert_eq!(ask(&url).await, [false; 3]);
        task.abort();
        let (url, task) = serve(move |path| {
            let chunks = padded(path).chunks(4096).map(|c| Ok::<_, std::io::Error>(c.to_vec())).collect::<Vec<_>>();
            axum::body::Body::from_stream(futures::stream::iter(chunks)).into_response()
        })
        .await;
        assert_eq!(ask(&url).await, [false; 3]);
        task.abort();
    }

    #[tokio::test]
    async fn a_stalled_body_ends_at_the_deadline() {
        use axum::response::IntoResponse;
        let (url, task) = serve(|_| {
            let start = futures::stream::once(async { Ok::<_, std::io::Error>(b"{\"revocations\": [".to_vec()) });
            axum::body::Body::from_stream(futures::StreamExt::chain(start, futures::stream::pending())).into_response()
        })
        .await;
        let http = Http::with_deadline(&url, Duration::from_millis(300)).unwrap();
        let started = std::time::Instant::now();
        assert!(fetch(&http, 0).await.is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
        task.abort();
    }

    #[tokio::test]
    async fn oversized_revocation_pages_are_refused() {
        use axum::response::IntoResponse;
        let page = || {
            let entry = json!({"seq": 1, "account_key": "a", "device_key": "d", "revoked_at": 1, "signature": "s"});
            Json(json!({"revocations": vec![entry; PAGE + 1]})).into_response()
        };
        let (url, task) = serve(move |_| page()).await;
        assert!(fetch(&Http::new(&url).unwrap(), 0).await.is_err());
        task.abort();
    }

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
