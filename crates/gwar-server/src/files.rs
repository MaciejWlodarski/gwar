//! HTTP side of uploads (see `core::files`): `PUT /api/files/{id}?token=…`
//! stores the bytes of a reserved upload, `GET /files/{id}/{name}` serves it.
//! With `FileAccess::Members` a download needs `Authorization: Bearer <token>`,
//! the token from the requester's `Welcome`.
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
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tracing::warn;

use crate::{
    core::{CoreHandle, files::Uploaded},
    gateway::Gateway,
};

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

/// An upload's bytes must keep coming (at most this long between chunks) and be
/// complete within the deadline its reservation gives them.
const UPLOAD_IDLE: std::time::Duration = std::time::Duration::from_secs(30);

/// The next chunk of an upload, or `REQUEST_TIMEOUT` when the sender stalls or runs out of time.
async fn paced<S: futures::Stream + Unpin>(
    stream: &mut S,
    deadline: tokio::time::Instant,
) -> Result<Option<S::Item>, StatusCode> {
    let left = deadline.saturating_duration_since(tokio::time::Instant::now());
    // A chunk already waiting would be taken even with no time left.
    if left.is_zero() {
        return Err(StatusCode::REQUEST_TIMEOUT);
    }
    tokio::time::timeout(UPLOAD_IDLE.min(left), stream.next()).await.map_err(|_| StatusCode::REQUEST_TIMEOUT)
}

/// Until disarmed, gives the upload up however it ends: an error, a timeout or the client
/// going away (which drops the handler), from before the token is presented on. The core
/// acts only on an upload this token started and never on a finished one, so a handler
/// dropped around `Begin` or `Finish` leaves nothing behind and takes nothing stored.
struct Abandoned {
    core: CoreHandle,
    id: String,
    attempt: u64,
    armed: bool,
}

impl Drop for Abandoned {
    fn drop(&mut self) {
        if self.armed {
            let (core, id, attempt) = (self.core.clone(), std::mem::take(&mut self.id), self.attempt);
            tokio::spawn(async move { core.upload_abort(id, attempt).await });
        }
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
    receive(gateway.core, id, query.token, body).await
}

async fn receive(core: CoreHandle, id: String, token: String, body: Body) -> Response {
    // This request's own name: another request with the same token can never give up ours.
    static ATTEMPTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let attempt = ATTEMPTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut abandoned = Abandoned { core: core.clone(), id: id.clone(), attempt, armed: true };
    let Some(slot) = core.upload_begin(id.clone(), token, attempt).await else {
        return (StatusCode::FORBIDDEN, "unknown or used upload").into_response();
    };
    let part = slot.path.with_extension("part");
    let result = async {
        if let Some(dir) = part.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        let mut file = tokio::fs::File::create(&part).await?;
        let mut written: u64 = 0;
        let mut hash = Sha256::new();
        let mut header = Vec::with_capacity(SNIFF);
        let mut stream = body.into_data_stream();
        // A slow or stalled sender cannot hold the upload (and its file) open for long.
        let deadline = tokio::time::Instant::from_std(slot.deadline);
        loop {
            let next = match paced(&mut stream, deadline).await {
                Ok(next) => next,
                Err(status) => return Ok(Err(status)),
            };
            let Some(chunk) = next else { break };
            let chunk = chunk.map_err(std::io::Error::other)?;
            written += chunk.len() as u64;
            if written > slot.size {
                return Ok(Err(StatusCode::PAYLOAD_TOO_LARGE));
            }
            if !slot.sealed && header.len() < SNIFF {
                header.extend_from_slice(&chunk[..chunk.len().min(SNIFF - header.len())]);
            }
            hash.update(&chunk);
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        let sha256 = URL_SAFE_NO_PAD.encode(hash.finalize());
        if written != slot.size || sha256 != slot.sha256 {
            return Ok(Err(StatusCode::BAD_REQUEST));
        }
        tokio::fs::rename(&part, &slot.path).await?;
        let media = if slot.sealed { None } else { media_type(&header) };
        let dimensions = media
            .filter(|m| m.starts_with("image/"))
            .and_then(|_| imagesize::blob_size(&header).ok())
            .map(|s| (s.width as u32, s.height as u32));
        Ok::<_, std::io::Error>(Ok(Uploaded { id: id.clone(), size: written, sha256, media, dimensions }))
    }
    .await;
    match result {
        Ok(Ok(uploaded)) => {
            if !core.upload_finish(uploaded).await {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            abandoned.armed = false;
            match core.file_info(id.clone()).await {
                Some((file, _)) => axum::Json(file).into_response(),
                None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            }
        }
        Ok(Err(status)) => status.into_response(),
        Err(e) => {
            warn!("upload {id}: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `bytes=a-b`, `bytes=a-` or `bytes=-n` within `len`, as an inclusive range.
fn byte_range(headers: &HeaderMap, len: u64) -> Option<(u64, u64)> {
    let spec = headers.get(header::RANGE)?.to_str().ok()?.strip_prefix("bytes=")?;
    let (start, end) = spec.split_once('-')?;
    let (start, end) = match (start.trim(), end.trim()) {
        ("", n) => (len.checked_sub(n.parse().ok()?)?, len.checked_sub(1)?),
        (a, "") => (a.parse().ok()?, len.checked_sub(1)?),
        (a, b) => (a.parse().ok()?, b.parse::<u64>().ok()?.min(len.checked_sub(1)?)),
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
    let access = request
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_owned);
    // A file the requester may not see is answered like a missing one.
    let Some((file, path, reading)) = gateway.core.file_open(id, access).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Opened off the runtime by a task that owns the lease, so even a request cancelled
    // meanwhile keeps the lease until the handle is closed.
    let opened = tokio::task::spawn_blocking(move || {
        let held = Held { file: std::fs::File::open(&path)?, _lease: reading };
        let len = held.file.metadata()?.len();
        Ok::<_, std::io::Error>((held, len))
    });
    let Ok(Ok((held, len))) = opened.await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    serve_file(held, len, &file.mime, &file.name, request).await
}

/// Up to `max` bytes; empty at the end. Retries reads interrupted by a signal.
fn read_chunk(reader: &mut impl std::io::Read, max: usize) -> std::io::Result<Vec<u8>> {
    let mut chunk = vec![0; max];
    loop {
        match reader.read(&mut chunk) {
            Ok(n) => {
                chunk.truncate(n);
                return Ok(chunk);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// An open file with the lease that keeps its bytes on disk. Fields drop in order: the
/// handle closes before the lease ends.
struct Held<L> {
    file: std::fs::File,
    _lease: L,
}

/// Streams `len` bytes of `held`. Every read runs on a blocking task that owns `held`, so if
/// the body is dropped mid-read, the lease ends only after that read and the handle are done.
async fn serve_file<L: Send + 'static>(
    mut held: Held<L>,
    len: u64,
    mime: &str,
    name: &str,
    request: HeaderMap,
) -> Response {
    let content_type = INLINE.iter().copied().find(|t| *t == mime).unwrap_or("application/octet-stream");
    let inline = content_type != "application/octet-stream";
    let encoded: String = name
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
    let range = byte_range(&request, len);
    if range.is_none() && request.contains_key(header::RANGE) {
        let mut response = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
        response.headers_mut().insert(header::CONTENT_RANGE, HeaderValue::from_str(&format!("bytes */{len}")).unwrap());
        return response;
    }
    let (start, length) = range.map_or((0, len), |(start, end)| (start, end - start + 1));
    // Moves the offset of the freshly opened file only; no data is read here.
    if start > 0 && std::io::Seek::seek(&mut held.file, std::io::SeekFrom::Start(start)).is_err() {
        return StatusCode::NOT_FOUND.into_response();
    }
    // Read only the selected slice, at most 64 KiB per poll. No whole-file allocation.
    let stream = futures::stream::try_unfold((held, length), |(mut held, left)| async move {
        if left == 0 {
            return Ok(None);
        }
        let (held, chunk) = tokio::task::spawn_blocking(move || {
            let read = read_chunk(&mut held.file, left.min(64 * 1024) as usize);
            (held, read)
        })
        .await
        .map_err(std::io::Error::other)?;
        let chunk = chunk?;
        if chunk.is_empty() {
            return Ok(None);
        }
        let left = left - chunk.len() as u64;
        Ok::<_, std::io::Error>(Some((chunk, (held, left))))
    });
    let mut response = Response::new(Body::from_stream(stream));
    if let Some((start, end)) = range {
        *response.status_mut() = StatusCode::PARTIAL_CONTENT;
        response
            .headers_mut()
            .insert(header::CONTENT_RANGE, HeaderValue::from_str(&format!("bytes {start}-{end}/{len}")).unwrap());
    }
    response.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from_str(&length.to_string()).unwrap());
    let headers = response.headers_mut();
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'; sandbox"));
    // Bearer capabilities have no expiry. Keep them out of shared and persistent caches.
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue, header};

    use super::{Held, UPLOAD_IDLE, byte_range, media_type, paced, read_chunk, serve_file, valid_id};
    use crate::core::files::UPLOAD_DEADLINE;
    use axum::http::StatusCode;

    #[tokio::test(start_paused = true)]
    async fn stalled_or_dripping_uploads_time_out() {
        let start = tokio::time::Instant::now();
        let deadline = start + UPLOAD_DEADLINE;
        let mut stalled = futures::stream::pending::<u8>();
        assert_eq!(paced(&mut stalled, deadline).await, Err(StatusCode::REQUEST_TIMEOUT));
        assert_eq!(start.elapsed(), UPLOAD_IDLE);

        // One byte every 20 s never stalls, but the whole upload has a deadline.
        let start = tokio::time::Instant::now();
        let deadline = start + UPLOAD_DEADLINE;
        let mut drip = Box::pin(futures::stream::unfold((), |()| async {
            tokio::time::sleep(std::time::Duration::from_secs(20)).await;
            Some((1u8, ()))
        }));
        let mut received = 0;
        while received < 100 && paced(&mut drip, deadline).await.is_ok() {
            received += 1;
        }
        assert_eq!(start.elapsed(), UPLOAD_DEADLINE, "after {received} chunks");
        // Past the deadline even a chunk that is already there is refused.
        let mut ready = futures::stream::iter([1u8]);
        assert_eq!(paced(&mut ready, deadline).await, Err(StatusCode::REQUEST_TIMEOUT));
        // An honest stream ends normally.
        let mut done = futures::stream::iter([1u8]);
        assert_eq!(paced(&mut done, deadline + UPLOAD_DEADLINE).await, Ok(Some(1)));
        assert_eq!(paced(&mut done, deadline + UPLOAD_DEADLINE).await, Ok(None));
    }

    /// The client goes away while its token is with the core: the upload it started is
    /// given up all the same.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_upload_dropped_while_starting_is_given_up() {
        use crate::core::{
            expiry_tests::{on_core, test_core_storing},
            files::tests::{released, reserve},
        };
        let dir = tempfile::tempdir().unwrap();
        let core = test_core_storing(crate::store::Store::in_memory().unwrap(), None, dir.path().into(), 100);
        reserve(&core, "f").await;
        // The core is busy, so the token waits in its queue when the handler goes away.
        let busy = tokio::spawn({
            let core = core.clone();
            async move { on_core(&core, |_| std::thread::sleep(std::time::Duration::from_millis(300))).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let body = axum::body::Body::from_stream(futures::stream::pending::<Result<Vec<u8>, std::io::Error>>());
        let receiving = super::receive(core.clone(), "f".into(), "t".into(), body);
        assert!(tokio::time::timeout(std::time::Duration::from_millis(50), receiving).await.is_err());
        busy.await.unwrap();
        assert!(released(&core, "f").await, "the started upload kept its space");
    }

    #[tokio::test]
    async fn ranges_stream_only_the_slice_and_keep_security_headers() {
        use std::io::{Seek, Write};
        let mut file = tempfile::tempfile().unwrap();
        let len = 1u64 << 33;
        file.set_len(len).unwrap();
        file.seek(std::io::SeekFrom::Start(len - 1)).unwrap();
        file.write_all(b"x").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, HeaderValue::from_str(&format!("bytes={}-", len - 1)).unwrap());
        let response = serve_file(Held { file, _lease: () }, len, "image/png", "photo.png", headers).await;
        assert_eq!(response.status(), axum::http::StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "1");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(response.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(response.headers()[header::CONTENT_SECURITY_POLICY], "default-src 'none'; sandbox");
        assert!(response.headers()[header::CONTENT_DISPOSITION].to_str().unwrap().starts_with("inline;"));
        assert_eq!(&axum::body::to_bytes(response.into_body(), 1).await.unwrap()[..], b"x");
    }

    /// A download dropped while a read is still running keeps its lease until that read is
    /// done. (A FIFO stands in for a slow disk: its read waits for a writer.)
    #[cfg(unix)]
    #[tokio::test]
    async fn a_dropped_download_keeps_its_lease_until_the_read_ends() {
        use futures::StreamExt;
        use std::{
            io::Write,
            sync::{
                Arc,
                atomic::{AtomicBool, Ordering},
            },
        };
        struct Lease(Arc<AtomicBool>);
        impl Drop for Lease {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("fifo");
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        let writer = std::thread::spawn({
            let fifo = fifo.clone();
            move || std::fs::OpenOptions::new().write(true).open(fifo).unwrap()
        });
        let file = std::fs::File::open(&fifo).unwrap();
        let mut writer = writer.join().unwrap();
        let ended = Arc::new(AtomicBool::new(false));
        let response = serve_file(Held { file, _lease: Lease(ended.clone()) }, 10, "x", "f", HeaderMap::new()).await;
        let mut body = response.into_body().into_data_stream();
        assert!(tokio::time::timeout(std::time::Duration::from_millis(50), body.next()).await.is_err());
        drop(body);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!ended.load(Ordering::SeqCst), "the lease ended while a read was still running");
        writer.write_all(b"data").unwrap();
        drop(writer);
        for _ in 0..100 {
            if ended.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the lease never ended");
    }

    #[test]
    fn reads_interrupted_by_a_signal_are_retried() {
        /// Fails once with `kind`, then reads "data".
        struct Failing(Option<std::io::ErrorKind>);
        impl std::io::Read for Failing {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if let Some(kind) = self.0.take() {
                    return Err(kind.into());
                }
                buf[..4].copy_from_slice(b"data");
                Ok(4)
            }
        }
        assert_eq!(read_chunk(&mut Failing(Some(std::io::ErrorKind::Interrupted)), 8).unwrap(), b"data");
        let other = read_chunk(&mut Failing(Some(std::io::ErrorKind::Other)), 8);
        assert!(other.is_err(), "other errors still end the download");
    }

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
