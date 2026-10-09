//! HTTP side of uploads (see `core::files`): `PUT /api/files/{id}?token=…`
//! stores the bytes of a reserved upload, `GET /files/{id}/{name}` serves it.
//!
//! Served files can't run in the page: only sniffed image types are sent as
//! themselves, everything else is a download, and a sandboxing CSP plus
//! `nosniff` apply to all of them.

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures::StreamExt;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tracing::warn;

use crate::{core::files::Uploaded, gateway::Gateway};

/// Bytes kept from the start of an upload to recognize images.
const SNIFF: usize = 64 * 1024;

#[derive(Deserialize)]
pub struct TokenQuery {
    token: String,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Image types we serve inline (never SVG: it can carry scripts).
fn image_type(header: &[u8]) -> Option<&'static str> {
    match imagesize::image_type(header).ok()? {
        imagesize::ImageType::Png => Some("image/png"),
        imagesize::ImageType::Jpeg => Some("image/jpeg"),
        imagesize::ImageType::Gif => Some("image/gif"),
        imagesize::ImageType::Webp => Some("image/webp"),
        _ => None,
    }
}

pub async fn upload(
    State(gateway): State<Gateway>,
    Path(id): Path<String>,
    Query(query): Query<TokenQuery>,
    body: Body,
) -> Response {
    if !valid_id(&id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(slot) = gateway.core.upload_begin(id.clone(), query.token).await else {
        return (StatusCode::FORBIDDEN, "unknown or used upload").into_response();
    };
    let part = slot.path.with_extension("part");
    let result = async {
        if let Some(dir) = part.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        let mut file = tokio::fs::File::create(&part).await?;
        let mut written: u64 = 0;
        let mut header = Vec::with_capacity(SNIFF);
        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(std::io::Error::other)?;
            written += chunk.len() as u64;
            if written > slot.size {
                return Ok(Err(StatusCode::PAYLOAD_TOO_LARGE));
            }
            if header.len() < SNIFF {
                header.extend_from_slice(&chunk[..chunk.len().min(SNIFF - header.len())]);
            }
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        if written == 0 {
            return Ok(Err(StatusCode::BAD_REQUEST));
        }
        tokio::fs::rename(&part, &slot.path).await?;
        let image = image_type(&header);
        let dimensions =
            image.and_then(|_| imagesize::blob_size(&header).ok()).map(|s| (s.width as u32, s.height as u32));
        Ok::<_, std::io::Error>(Ok(Uploaded { id: id.clone(), size: written, image, dimensions }))
    }
    .await;
    match result {
        Ok(Ok(uploaded)) => {
            if gateway.core.upload_finish(uploaded).await {
                StatusCode::NO_CONTENT.into_response()
            } else {
                let _ = tokio::fs::remove_file(&slot.path).await;
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
        Ok(Err(status)) => {
            let _ = tokio::fs::remove_file(&part).await;
            status.into_response()
        }
        Err(e) => {
            warn!("upload {id}: {e}");
            let _ = tokio::fs::remove_file(&part).await;
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn download(State(gateway): State<Gateway>, Path((id, _name)): Path<(String, String)>) -> Response {
    if !valid_id(&id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some((file, path)) = gateway.core.file_info(id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(bytes) = tokio::fs::read(&path).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let content_type: &'static str = match file.mime.as_str() {
        "image/png" => "image/png",
        "image/jpeg" => "image/jpeg",
        "image/gif" => "image/gif",
        "image/webp" => "image/webp",
        _ => "application/octet-stream",
    };
    let inline = content_type != "application/octet-stream";
    let encoded: String = file
        .name
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    let disposition = format!("{}; filename*=UTF-8''{encoded}", if inline { "inline" } else { "attachment" });
    let mut response = Response::new(Body::from(bytes));
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'; sandbox"));
    // Ids are random and files immutable.
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=31536000, immutable"));
    response
}

#[cfg(test)]
mod tests {
    use super::{image_type, valid_id};

    #[test]
    fn ids_cannot_escape_the_files_directory() {
        assert!(valid_id("AbC-_09"));
        assert!(!valid_id("../x"));
        assert!(!valid_id("a/b"));
        assert!(!valid_id(""));
    }

    #[test]
    fn only_raster_images_are_recognized() {
        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R'];
        assert_eq!(image_type(&png), Some("image/png"));
        assert_eq!(image_type(b"<svg xmlns='http://www.w3.org/2000/svg'></svg>"), None);
        assert_eq!(image_type(b"<html><script>alert(1)</script>"), None);
    }
}
