//! Uploads, in three steps: `file.upload` reserves an id and a one-time
//! token; the client `PUT`s the bytes over HTTP (see `crate::files`); the
//! finished file is attached to a message by `chat.send`. Files never
//! attached are removed after a day; files go when their message goes.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use tokio::sync::oneshot;
use tracing::warn;
use vc_proto::{Attachment, ErrorCode, Permission, Response, SessionId};

use super::{Core, CoreHandle, CoreMsg, Reply, err, now_ms, storage_error};

/// How long a reserved upload waits for its bytes.
const UPLOAD_WINDOW: Duration = Duration::from_secs(600);
/// Uploads not attached to a message within this time are removed.
const UNATTACHED_KEEP_MS: i64 = 24 * 3600 * 1000;
const MAX_NAME: usize = 120;

pub(super) struct PendingUpload {
    uploader: String,
    name: String,
    mime: String,
    size: u64,
    token: String,
    until: Instant,
}

/// What the HTTP side needs to accept one upload.
pub struct UploadSlot {
    pub path: PathBuf,
    pub size: u64,
}

/// A finished upload as checked by the HTTP side.
pub struct Uploaded {
    pub id: String,
    pub size: u64,
    /// Image, audio or video type sniffed from the bytes, if any.
    pub media: Option<&'static str>,
    pub dimensions: Option<(u32, u32)>,
}

pub enum FileMsg {
    Begin { id: String, token: String, reply: oneshot::Sender<Option<UploadSlot>> },
    Finish { upload: Uploaded, reply: oneshot::Sender<bool> },
    Info { id: String, reply: oneshot::Sender<Option<(Attachment, PathBuf)>> },
}

impl CoreHandle {
    /// Claims a reserved upload (the token works once).
    pub async fn upload_begin(&self, id: String, token: String) -> Option<UploadSlot> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(CoreMsg::Files(FileMsg::Begin { id, token, reply })).await.ok()?;
        rx.await.ok().flatten()
    }

    pub async fn upload_finish(&self, upload: Uploaded) -> bool {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(CoreMsg::Files(FileMsg::Finish { upload, reply })).await.is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// A stored file and where it is on disk.
    pub async fn file_info(&self, id: String) -> Option<(Attachment, PathBuf)> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(CoreMsg::Files(FileMsg::Info { id, reply })).await.ok()?;
        rx.await.ok().flatten()
    }
}

fn random_id(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

/// A file name safe to store and show: no paths, no control characters.
fn clean_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base.chars().filter(|c| !c.is_control()).take(MAX_NAME).collect();
    let cleaned = cleaned.trim().trim_start_matches('.').to_owned();
    if cleaned.is_empty() { "file".to_owned() } else { cleaned }
}

impl Core {
    pub(super) fn file_path(&self, id: &str) -> PathBuf {
        self.files_dir.join(id)
    }

    pub(super) fn file_upload(&mut self, session: SessionId, name: String, size: u64, mime: String) -> Reply {
        self.require(session, Permission::FileUpload)?;
        if self.info.upload_limit == 0 {
            return Err(err(ErrorCode::Unavailable, "uploads are disabled on this server"));
        }
        if size == 0 {
            return Err(err(ErrorCode::BadRequest, "empty file"));
        }
        if size > self.info.upload_limit {
            return Err(err(ErrorCode::TooLarge, "file is too large"));
        }
        let now = Instant::now();
        self.uploads.retain(|_, u| u.until > now);
        // One user can't reserve unbounded space.
        let uploader = self.sessions[&session].uid.clone();
        if self.uploads.values().filter(|u| u.uploader == uploader).count() >= 10 {
            return Err(err(ErrorCode::RateLimited, "too many uploads in progress"));
        }
        let id = random_id(16);
        let token = random_id(24);
        let mime = mime.chars().filter(|c| c.is_ascii_graphic()).take(100).collect();
        let upload_url = format!("/api/files/{id}?token={token}");
        self.uploads.insert(
            id.clone(),
            PendingUpload { uploader, name: clean_name(&name), mime, size, token, until: now + UPLOAD_WINDOW },
        );
        Ok(Response::Upload { file: id, upload_url })
    }

    pub(super) fn file_msg(&mut self, msg: FileMsg) {
        match msg {
            FileMsg::Begin { id, token, reply } => {
                let slot = self
                    .uploads
                    .get_mut(&id)
                    .filter(|u| u.until > Instant::now() && !u.token.is_empty() && u.token == token)
                    .map(|u| {
                        // The token is spent; the upload stays reserved until it finishes.
                        u.token.clear();
                        u.size
                    })
                    .map(|size| UploadSlot { path: self.files_dir.join(&id), size });
                let _ = reply.send(slot);
            }
            FileMsg::Finish { upload, reply } => {
                let Some(pending) = self.uploads.remove(&upload.id) else {
                    let _ = reply.send(false);
                    return;
                };
                // Only sniffed media types are ever served as themselves.
                let mime = upload.media.map(str::to_owned).unwrap_or(pending.mime);
                let stored = self.store.insert_file(
                    &upload.id,
                    &pending.uploader,
                    &pending.name,
                    &mime,
                    upload.size,
                    upload.dimensions,
                    now_ms(),
                );
                if let Err(e) = &stored {
                    warn!("store: {e:#}");
                }
                let _ = reply.send(stored.is_ok());
            }
            FileMsg::Info { id, reply } => {
                let file = self.store.file(&id).map_err(storage_error).ok().flatten();
                let _ = reply.send(file.map(|f| (f, self.file_path(&id))));
            }
        }
    }

    /// Deletes stored files from disk (off the actor).
    pub(super) fn remove_files(&self, ids: Vec<String>) {
        if ids.is_empty() {
            return;
        }
        let paths: Vec<_> = ids.iter().map(|id| self.file_path(id)).collect();
        tokio::task::spawn_blocking(move || {
            for path in paths {
                let _ = std::fs::remove_file(path);
            }
        });
    }

    pub(super) fn tick(&mut self) {
        let now = Instant::now();
        self.uploads.retain(|_, u| u.until > now);
        let mut gone = self.store.expire_unattached(now_ms() - UNATTACHED_KEEP_MS).unwrap_or_default();
        gone.extend(self.store.expire_orphans().unwrap_or_default());
        self.remove_files(gone);
        self.touch_online();
    }
}

#[cfg(test)]
mod tests {
    use super::clean_name;

    #[test]
    fn names_lose_paths_and_control_characters() {
        assert_eq!(clean_name("../../etc/passwd"), "passwd");
        assert_eq!(clean_name("C:\\Users\\me\\photo.png"), "photo.png");
        assert_eq!(clean_name("a\nb.txt"), "ab.txt");
        assert_eq!(clean_name(".hidden"), "hidden");
        assert_eq!(clean_name(""), "file");
    }
}
