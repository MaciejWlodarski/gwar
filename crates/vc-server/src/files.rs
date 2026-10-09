//! HTTP side of uploads (see `core::files`): `PUT /api/files/{id}?token=…`
//! stores the bytes of a reserved upload, `GET /files/{id}/{name}` serves it.
//!
//! Served files can't run in the page: only sniffed raster images, audio and
//! video are sent as themselves (so chats can show and play them), everything
//! else is a download, and a sandboxing CSP plus `nosniff` apply to all of
//! them. Range requests make video seekable (Safari needs them to play).

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures::StreamExt;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tracing::warn;

use crate::{core::files::Uploaded, gateway::Gateway};

/// Bytes kept from the start of an upload to recognize its type.
const SNIFF: usize = 64 * 1024;

#[derive(Deserialize)]
pub struct TokenQuery {
    token: String,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Content types served inline; nothing here can run script (no SVG, no HTML).
const INLINE: [&str; 12] = [
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "video/mp4",
    "video/webm",
    "video/ogg",
    "audio/mpeg",
    "audio/ogg",
    "audio/wav",
    "audio/flac",
    "audio/mp4",
];

/// The type of a file from its first bytes, if it is one we show inline.
fn media_type(header: &[u8]) -> Option<&'static str> {
    // Other image types (HEIF, SVG, …) fall through: MP4 shares HEIF's `ftyp` box.
    match imagesize::image_type(header) {
        Ok(imagesize::ImageType::Png) => return Some("image/png"),
        Ok(imagesize::ImageType::Jpeg) => return Some("image/jpeg"),
        Ok(imagesize::ImageType::Gif) => return Some("image/gif"),
        Ok(imagesize::ImageType::Webp) => return Some("image/webp"),
        _ => {}
    }
    let at = |offset: usize, magic: &[u8]| header.get(offset..offset + magic.len()) == Some(magic);
    if at(4, b"ftyp") {
        // ISO base media: audio-only brands are M4A/M4B, the rest is video.
        return Some(if at(8, b"M4A ") || at(8, b"M4B ") { "audio/mp4" } else { "video/mp4" });
    }
    if at(0, &[0x1A, 0x45, 0xDF, 0xA3]) {
        return Some("video/webm");
    }
    if at(0, b"OggS") {
        // Theora streams are video; Vorbis/Opus are audio.
        return Some(if header.windows(7).any(|w| w == b"\x80theora") { "video/ogg" } else { "audio/ogg" });
    }
    if at(0, b"RIFF") && at(8, b"WAVE") {
        return Some("audio/wav");
    }
    if at(0, b"fLaC") {
        return Some("audio/flac");
    }
    if at(0, b"ID3") || (header.len() > 1 && header[0] == 0xFF && header[1] & 0xE0 == 0xE0) {
        return Some("audio/mpeg");
    }
    None
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
        let media = media_type(&header);
        let dimensions = media
            .filter(|m| m.starts_with("image/"))
            .and_then(|_| imagesize::blob_size(&header).ok())
            .map(|s| (s.width as u32, s.height as u32));
        Ok::<_, std::io::Error>(Ok(Uploaded { id: id.clone(), size: written, media, dimensions }))
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

/// `bytes=a-b`, `bytes=a-` or `bytes=-n` within `len`, as an inclusive range.
fn byte_range(headers: &HeaderMap, len: usize) -> Option<(usize, usize)> {
    let spec = headers.get(header::RANGE)?.to_str().ok()?.strip_prefix("bytes=")?;
    let (start, end) = spec.split_once('-')?;
    let (start, end) = match (start.trim(), end.trim()) {
        ("", n) => (len.checked_sub(n.parse().ok()?)?, len.checked_sub(1)?),
        (a, "") => (a.parse().ok()?, len.checked_sub(1)?),
        (a, b) => (a.parse().ok()?, b.parse::<usize>().ok()?.min(len.checked_sub(1)?)),
    };
    (start <= end && end < len).then_some((start, end))
}

pub async fn download(
    State(gateway): State<Gateway>,
    Path((id, _name)): Path<(String, String)>,
    request: HeaderMap,
) -> Response {
    if !valid_id(&id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some((file, path)) = gateway.core.file_info(id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(bytes) = tokio::fs::read(&path).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let content_type = INLINE.iter().copied().find(|t| *t == file.mime).unwrap_or("application/octet-stream");
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
    let len = bytes.len();
    let range = byte_range(&request, len);
    let mut response = match range {
        Some((start, end)) => {
            let mut partial = Response::new(Body::from(bytes[start..=end].to_vec()));
            *partial.status_mut() = StatusCode::PARTIAL_CONTENT;
            if let Ok(value) = HeaderValue::from_str(&format!("bytes {start}-{end}/{len}")) {
                partial.headers_mut().insert(header::CONTENT_RANGE, value);
            }
            partial
        }
        None if request.contains_key(header::RANGE) => {
            let mut unsatisfiable = Response::new(Body::empty());
            *unsatisfiable.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
            return unsatisfiable;
        }
        None => Response::new(Body::from(bytes)),
    };
    let headers = response.headers_mut();
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
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
    use axum::http::{HeaderMap, HeaderValue, header};

    use super::{byte_range, media_type, valid_id};

    #[test]
    fn ids_cannot_escape_the_files_directory() {
        assert!(valid_id("AbC-_09"));
        assert!(!valid_id("../x"));
        assert!(!valid_id("a/b"));
        assert!(!valid_id(""));
    }

    #[test]
    fn only_raster_images_audio_and_video_are_recognized() {
        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R'];
        assert_eq!(media_type(&png), Some("image/png"));
        assert_eq!(media_type(b"\0\0\0\x20ftypisom\0\0\x02\0"), Some("video/mp4"));
        assert_eq!(media_type(b"\0\0\0\x20ftypM4A \0\0\x02\0"), Some("audio/mp4"));
        assert_eq!(media_type(&[0x1A, 0x45, 0xDF, 0xA3, 0x9F]), Some("video/webm"));
        assert_eq!(media_type(b"ID3\x04\0\0\0\0\0\0"), Some("audio/mpeg"));
        assert_eq!(media_type(b"RIFF\0\0\0\0WAVEfmt "), Some("audio/wav"));
        assert_eq!(media_type(b"OggS\0\x02\0\0\0\0\0\0\0\0\x01\x1eOpusHead"), Some("audio/ogg"));
        assert_eq!(media_type(b"<svg xmlns='http://www.w3.org/2000/svg'></svg>"), None);
        assert_eq!(media_type(b"<html><script>alert(1)</script>"), None);
    }

    #[test]
    fn ranges_are_parsed_within_the_file() {
        let range = |spec: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::RANGE, HeaderValue::from_str(spec).unwrap());
            byte_range(&headers, 100)
        };
        assert_eq!(range("bytes=0-9"), Some((0, 9)));
        assert_eq!(range("bytes=90-"), Some((90, 99)));
        assert_eq!(range("bytes=-10"), Some((90, 99)));
        assert_eq!(range("bytes=50-500"), Some((50, 99)));
        assert_eq!(range("bytes=100-"), None);
        assert_eq!(range("items=0-1"), None);
    }
}
