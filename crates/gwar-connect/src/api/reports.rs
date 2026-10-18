//! Bug reports from the apps (alpha): what happened, diagnostics the app collected, and up to five
//! screenshots. Stored privately for the operator (`gwar-connect reports`); anyone may send one, a
//! device session names its account.

use std::{net::SocketAddr, time::Duration};

use axum::{
    Json,
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode},
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiError, AppState, Reply, bearer, client_ip, now_ms, token_hash};
use crate::crypto::unb64;

const MAX_TEXT_CHARS: usize = 10_000;
const MAX_DIAGNOSTICS_BYTES: usize = 64 * 1024;
const MAX_IMAGES: usize = 5;
const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;
/// All reports together: past this, new ones are refused until the operator deletes some.
const MAX_STORED_BYTES: i64 = 2 * 1024 * 1024 * 1024;
const PER_ADDRESS: usize = 10;
const PER_ADDRESS_WINDOW: Duration = Duration::from_secs(3600);
/// A request carries at most five base64 screenshots and the rest.
pub const MAX_BODY_BYTES: usize = MAX_IMAGES * MAX_IMAGE_BYTES * 4 / 3 + 512 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewReport {
    text: String,
    diagnostics: Value,
    #[serde(default)]
    images: Vec<NewImage>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewImage {
    mime: String,
    data: String,
}

/// The image type its first bytes show, which must be the one claimed.
fn image_type(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some("image/gif")
    } else {
        None
    }
}

/// `POST /v1/reports` → `{id}`.
pub async fn create(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(r): Json<NewReport>,
) -> Reply {
    let ip = client_ip(&state, peer, &headers);
    if !state.allow(format!("report:{ip}"), PER_ADDRESS, PER_ADDRESS_WINDOW) {
        return Err(ApiError::limited());
    }
    let text = r.text.trim();
    if text.is_empty() || text.chars().count() > MAX_TEXT_CHARS {
        return Err(ApiError::bad("describe what happened (at most 10,000 characters)"));
    }
    let diagnostics = serde_json::to_string(&r.diagnostics).map_err(|_| ApiError::bad("invalid diagnostics"))?;
    if !r.diagnostics.is_object() || diagnostics.len() > MAX_DIAGNOSTICS_BYTES {
        return Err(ApiError::bad("invalid diagnostics"));
    }
    if r.images.len() > MAX_IMAGES {
        return Err(ApiError::bad("at most five screenshots"));
    }
    let mut images = Vec::with_capacity(r.images.len());
    for image in r.images {
        let data = unb64(&image.data).ok_or_else(|| ApiError::bad("invalid screenshot"))?;
        if data.len() > MAX_IMAGE_BYTES || image_type(&data) != Some(image.mime.as_str()) {
            return Err(ApiError::bad("screenshots must be PNG, JPEG, WebP or GIF of at most 4 MiB"));
        }
        images.push((image.mime, data));
    }
    let store = state.store.lock().expect("store lock");
    // A device session names the account; anything else stays anonymous.
    let account = match bearer(&headers) {
        Some(token) => store.session(&token_hash(token), now_ms())?,
        None => None,
    };
    let Some(id) = store.put_report(account, text, &diagnostics, &images, MAX_STORED_BYTES)? else {
        return Err(ApiError(StatusCode::INSUFFICIENT_STORAGE, "quota_exceeded", "reports are full".into()));
    };
    tracing::info!(id, "bug report received");
    Ok(Json(json!({"id": id})))
}

#[cfg(test)]
mod tests {
    use super::image_type;

    #[test]
    fn screenshots_are_recognized_by_their_bytes() {
        assert_eq!(image_type(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(image_type(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(image_type(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(image_type(b"GIF89a"), Some("image/gif"));
        assert_eq!(image_type(b"<svg>"), None);
    }
}
