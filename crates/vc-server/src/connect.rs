//! Revocations from Gwar Connect (docs/connect.md). Servers never ask Connect
//! who someone is; they only follow its public feed of revoked devices, and
//! believe an entry only if the account key signed it. If Connect is down,
//! nothing else is affected.

use std::time::Duration;

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
