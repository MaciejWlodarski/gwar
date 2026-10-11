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

/// How long a reserved upload waits for its bytes to start coming. Once started, it stays
/// reserved until it finishes or is given up, however long writing it takes.
const UPLOAD_WINDOW: Duration = Duration::from_secs(600);
/// How long the bytes may take once they start coming (enforced by the HTTP side).
pub const UPLOAD_DEADLINE: Duration = Duration::from_secs(600);
/// Uploads not attached to a message within this time are removed.
const UNATTACHED_KEEP_MS: i64 = 24 * 3600 * 1000;
const MAX_NAME: usize = 120;

pub(super) struct PendingUpload {
    uploader: String,
    name: String,
    mime: String,
    size: u64,
    token: String,
    sha256: String,
    until: Instant,
    /// Its token was used: bytes may be on disk until `Finish` or `Abort`.
    started: bool,
}

impl PendingUpload {
    /// Whether it still holds its space at `now`.
    fn live(&self, now: Instant) -> bool {
        self.started || self.until > now
    }
}

/// What the HTTP side needs to accept one upload.
pub struct UploadSlot {
    pub path: PathBuf,
    pub size: u64,
    pub sha256: String,
    /// When the bytes must be complete.
    pub deadline: Instant,
}

/// A finished upload as checked by the HTTP side.
pub struct Uploaded {
    pub id: String,
    pub size: u64,
    pub sha256: String,
    /// Image, audio or video type sniffed from the bytes, if any.
    pub media: Option<&'static str>,
    pub dimensions: Option<(u32, u32)>,
}

pub enum FileMsg {
    Begin {
        id: String,
        token: String,
        reply: oneshot::Sender<Option<UploadSlot>>,
    },
    Finish {
        upload: Uploaded,
        reply: oneshot::Sender<bool>,
    },
    /// The upload this token started ended without finishing: its bytes go, then its
    /// reservation. Sent by whoever presented the token, whether or not it got to start.
    Abort {
        id: String,
        token: String,
    },
    Info {
        id: String,
        reply: oneshot::Sender<Option<(Attachment, PathBuf)>>,
    },
}

impl CoreHandle {
    /// Claims a reserved upload (the token works once).
    pub async fn upload_begin(&self, id: String, token: String) -> Option<UploadSlot> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(CoreMsg::Files(FileMsg::Begin { id, token, reply })).await.ok()?;
        rx.await.ok().flatten()
    }

    /// Gives up an upload (see [`FileMsg::Abort`]); harmless once it has finished.
    pub async fn upload_abort(&self, id: String, token: String) {
        let _ = self.tx.send(CoreMsg::Files(FileMsg::Abort { id, token })).await;
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

    pub(super) fn file_upload(
        &mut self,
        session: SessionId,
        name: String,
        size: u64,
        mime: String,
        sha256: String,
    ) -> Reply {
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
        if !crate::identity::valid_hash(&sha256, 32) {
            return Err(err(ErrorCode::BadRequest, "invalid attachment hash"));
        }
        let now = Instant::now();
        self.uploads.retain(|_, u| u.live(now));
        // One user can't reserve unbounded space.
        let uploader = self.sessions[&session].uid.clone();
        if self.uploads.values().filter(|u| u.uploader == uploader).count() >= 10 {
            return Err(err(ErrorCode::RateLimited, "too many uploads in progress"));
        }
        // Identities are free, so a per-member quota alone cannot protect the disk: the total is capped too.
        // Reserved uploads count as if they were already stored.
        for (limit, who, message) in [
            (self.upload_quota, Some(uploader.as_str()), "your upload quota is used up"),
            (self.storage_limit, None, "the server's file storage is full"),
        ] {
            if limit == 0 {
                continue;
            }
            let reserved: u64 =
                self.uploads.values().filter(|u| who.is_none_or(|w| u.uploader == w)).map(|u| u.size).sum();
            let stored = self.store.stored_bytes(who).map_err(storage_error)?;
            if stored.saturating_add(reserved).saturating_add(size) > limit {
                return Err(err(ErrorCode::QuotaExceeded, message));
            }
        }
        let id = random_id(16);
        let token = random_id(24);
        let mime = mime.chars().filter(|c| c.is_ascii_graphic()).take(100).collect();
        let upload_url = format!("/api/files/{id}?token={token}");
        self.uploads.insert(
            id.clone(),
            PendingUpload {
                uploader,
                name: clean_name(&name),
                mime,
                size,
                token,
                sha256,
                until: now + UPLOAD_WINDOW,
                started: false,
            },
        );
        Ok(Response::Upload { file: id, upload_url })
    }

    pub(super) fn file_msg(&mut self, msg: FileMsg) {
        match msg {
            FileMsg::Begin { id, token, reply } => {
                let now = Instant::now();
                let deadline = now + UPLOAD_DEADLINE;
                let slot = self
                    .uploads
                    .get_mut(&id)
                    .filter(|u| !u.started && u.until > now && u.token == token)
                    .map(|u| {
                        // The token works once; the upload stays reserved until it finishes or is given up.
                        u.started = true;
                        (u.size, u.sha256.clone())
                    })
                    .map(|(size, sha256)| UploadSlot { path: self.files_dir.join(&id), size, sha256, deadline });
                let _ = reply.send(slot);
            }
            FileMsg::Finish { upload, reply } => {
                // A refused upload stays reserved until its `Abort` removes the bytes.
                let Some(pending) = self.uploads.get(&upload.id).filter(|u| u.started) else {
                    let _ = reply.send(false);
                    return;
                };
                if upload.sha256 != pending.sha256 || upload.size != pending.size {
                    let _ = reply.send(false);
                    return;
                }
                // Only sniffed media types are ever served as themselves.
                let mime = upload.media.map(str::to_owned).unwrap_or_else(|| pending.mime.clone());
                let stored = self.store.insert_file(
                    &upload.id,
                    &pending.uploader,
                    &pending.name,
                    &mime,
                    upload.size,
                    &upload.sha256,
                    upload.dimensions,
                    now_ms(),
                );
                match &stored {
                    Ok(()) => drop(self.uploads.remove(&upload.id)),
                    Err(e) => warn!("store: {e:#}"),
                }
                let _ = reply.send(stored.is_ok());
            }
            FileMsg::Abort { id, token } => {
                // Only the upload this token started; a finished one is gone from `uploads` (its
                // `Finish` came first) and its bytes are the stored file's.
                if !self.uploads.get(&id).is_some_and(|u| u.started && u.token == token) {
                    return;
                }
                if !matches!(self.store.file(&id), Ok(None)) {
                    return;
                }
                let path = self.file_path(&id);
                let paths = [path.with_extension("part"), path];
                let Some(tx) = self.me.upgrade() else { return };
                tokio::task::spawn_blocking(move || {
                    for path in paths {
                        let _ = std::fs::remove_file(path);
                    }
                    // The space is free once the bytes are gone, not before.
                    let release = move |core: &mut Core| drop(core.uploads.remove(&id));
                    let _ = tx.blocking_send(CoreMsg::Resume(Box::new(release)));
                });
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
        self.uploads.retain(|_, u| u.live(now));
        let mut gone = self.store.expire_unattached(now_ms() - UNATTACHED_KEEP_MS).unwrap_or_default();
        gone.extend(self.store.expire_orphans().unwrap_or_default());
        self.remove_files(gone);
        self.touch_online();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        core::expiry_tests::{on_core, test_core_storing},
        store::Store,
    };

    /// A reservation for 3 bytes with token "t", started or not.
    fn pending(started: bool) -> PendingUpload {
        PendingUpload {
            uploader: "u".into(),
            name: "f".into(),
            mime: "x".into(),
            size: 3,
            token: "t".into(),
            sha256: "h".into(),
            until: Instant::now() + UPLOAD_WINDOW,
            started,
        }
    }

    /// Reserves `id` (not started, token "t").
    pub(crate) async fn reserve(core: &CoreHandle, id: &'static str) {
        on_core(core, move |core| drop(core.uploads.insert(id.into(), pending(false)))).await;
    }

    async fn reserved(core: &CoreHandle, id: &'static str) -> bool {
        on_core(core, move |core| core.uploads.contains_key(id)).await
    }

    /// Waits for the core to finish giving `id` up.
    pub(crate) async fn released(core: &CoreHandle, id: &'static str) -> bool {
        for _ in 0..100 {
            if !reserved(core, id).await {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    #[tokio::test]
    async fn a_started_upload_holds_its_space_until_it_ends() {
        let dir = tempfile::tempdir().unwrap();
        let core = test_core_storing(Store::in_memory().unwrap(), None, dir.path().into(), 100);
        on_core(&core, |core| {
            let past = Instant::now() - Duration::from_secs(1);
            core.uploads.insert("waiting".into(), PendingUpload { until: past, ..pending(false) });
            core.uploads.insert("writing".into(), PendingUpload { until: past, ..pending(true) });
            core.uploads.insert("refused".into(), pending(true));
            core.tick();
        })
        .await;
        assert!(!reserved(&core, "waiting").await, "an unused reservation expires");
        assert!(reserved(&core, "writing").await, "however long the bytes take");
        // Only the token that started it gives it up.
        core.upload_abort("writing".into(), "other".into()).await;
        assert!(!released(&core, "writing").await);
        // A refused finish keeps the space until the bytes are removed.
        std::fs::write(dir.path().join("refused"), "abc").unwrap();
        let wrong = Uploaded { id: "refused".into(), size: 3, sha256: "x".into(), media: None, dimensions: None };
        assert!(!core.upload_finish(wrong).await);
        assert!(reserved(&core, "refused").await);
        core.upload_abort("refused".into(), "t".into()).await;
        assert!(released(&core, "refused").await);
        assert!(!dir.path().join("refused").exists());
    }

    /// A handler that goes away after its upload finished must not take the stored bytes with
    /// it; one that goes away before removes them and only then frees the space.
    #[tokio::test]
    async fn abandoning_an_upload_removes_only_unfinished_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let core = test_core_storing(Store::in_memory().unwrap(), None, dir.path().into(), 100);
        on_core(&core, |core| {
            core.uploads.insert("done".into(), pending(true));
            core.uploads.insert("half".into(), pending(true));
        })
        .await;
        std::fs::write(dir.path().join("done"), "abc").unwrap();
        std::fs::write(dir.path().join("half.part"), "a").unwrap();
        let done = Uploaded { id: "done".into(), size: 3, sha256: "h".into(), media: None, dimensions: None };
        assert!(core.upload_finish(done).await);
        core.upload_abort("done".into(), "t".into()).await;
        core.upload_abort("half".into(), "t".into()).await;
        assert!(released(&core, "half").await, "space never freed");
        assert!(!dir.path().join("half.part").exists());
        assert_eq!(std::fs::read(dir.path().join("done")).unwrap(), b"abc");
        assert!(core.file_info("done".into()).await.is_some());
    }

    #[test]
    fn names_lose_paths_and_control_characters() {
        assert_eq!(clean_name("../../etc/passwd"), "passwd");
        assert_eq!(clean_name("C:\\Users\\me\\photo.png"), "photo.png");
        assert_eq!(clean_name("a\nb.txt"), "ab.txt");
        assert_eq!(clean_name(".hidden"), "hidden");
        assert_eq!(clean_name(""), "file");
    }
}
