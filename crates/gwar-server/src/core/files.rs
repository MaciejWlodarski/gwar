//! Uploads, in three steps: `file.upload` reserves an id and a one-time
//! token; the client `PUT`s the bytes over HTTP (see `crate::files`); the
//! finished file is attached by `chat.send` or `chat.sealed.send`. Files never
//! attached are removed after a day; files go when their message goes.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use gwar_proto::{Attachment, ErrorCode, FileAccess, Permission, Response, SessionId};
use rand::RngCore;
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

use super::{Core, CoreHandle, CoreMsg, Reply, err, mls::channel_id, now_ms, storage_error};

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
    sealed: bool,
    until: Instant,
    state: UploadState,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UploadState {
    /// Reserved; the token was not used yet.
    Waiting,
    /// One HTTP attempt took the token and may be writing bytes.
    Writing { attempt: u64 },
    /// That attempt ended without finishing: its bytes are being (or are to be) removed.
    GivenUp { removing: bool },
}

impl PendingUpload {
    /// Whether it still holds its space at `now`: once started, until it is stored or its
    /// bytes are gone, however long that takes.
    fn live(&self, now: Instant) -> bool {
        self.state != UploadState::Waiting || self.until > now
    }
}

/// Keeps a stored file's bytes on disk, and charged, while a download reads them; dropping it
/// ends the read.
pub struct Reading {
    tx: mpsc::Sender<CoreMsg>,
    id: String,
}

impl Drop for Reading {
    fn drop(&mut self) {
        let done = CoreMsg::Files(FileMsg::Done { id: std::mem::take(&mut self.id) });
        if let Err(mpsc::error::TrySendError::Full(done)) = self.tx.try_send(done)
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let tx = self.tx.clone();
            runtime.spawn(async move { tx.send(done).await });
        }
    }
}

/// What the HTTP side needs to accept one upload.
pub struct UploadSlot {
    pub path: PathBuf,
    pub size: u64,
    pub sha256: String,
    pub sealed: bool,
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
    /// `attempt` names the HTTP request presenting the token, unique per request.
    Begin {
        id: String,
        token: String,
        attempt: u64,
        reply: oneshot::Sender<Option<UploadSlot>>,
    },
    Finish {
        upload: Uploaded,
        reply: oneshot::Sender<bool>,
    },
    /// The request `attempt` ended without finishing: if it was the one that started the
    /// upload, its bytes go, then its reservation. Sent whether or not it got to start.
    Abort {
        id: String,
        attempt: u64,
    },
    Info {
        id: String,
        reply: oneshot::Sender<Option<(Attachment, PathBuf)>>,
    },
    /// A stored file to download, held by the returned [`Reading`]. `access` is the
    /// requester's bearer token; it matters only with [`FileAccess::Members`].
    Open {
        id: String,
        access: Option<String>,
        reply: oneshot::Sender<Option<(Attachment, PathBuf, Reading)>>,
    },
    /// A [`Reading`] ended.
    Done {
        id: String,
    },
}

impl CoreHandle {
    /// Claims a reserved upload for request `attempt` (the token works once).
    pub async fn upload_begin(&self, id: String, token: String, attempt: u64) -> Option<UploadSlot> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(CoreMsg::Files(FileMsg::Begin { id, token, attempt, reply })).await.ok()?;
        rx.await.ok().flatten()
    }

    /// Gives up request `attempt` (see [`FileMsg::Abort`]); harmless if it did not start the
    /// upload or the upload finished.
    pub async fn upload_abort(&self, id: String, attempt: u64) {
        let _ = self.tx.send(CoreMsg::Files(FileMsg::Abort { id, attempt })).await;
    }

    pub async fn upload_finish(&self, upload: Uploaded) -> bool {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(CoreMsg::Files(FileMsg::Finish { upload, reply })).await.is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// A stored file to download and where it is on disk; its bytes stay until the
    /// [`Reading`] is dropped, even if the file is deleted meanwhile.
    pub async fn file_open(&self, id: String, access: Option<String>) -> Option<(Attachment, PathBuf, Reading)> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(CoreMsg::Files(FileMsg::Open { id, access, reply })).await.ok()?;
        rx.await.ok().flatten()
    }

    /// A stored file and where it is on disk.
    pub async fn file_info(&self, id: String) -> Option<(Attachment, PathBuf)> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(CoreMsg::Files(FileMsg::Info { id, reply })).await.ok()?;
        rx.await.ok().flatten()
    }
}

pub(super) fn random_id(bytes: usize) -> String {
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

/// Removes a file; true once it is gone (also if it never was there).
fn unlink(path: &std::path::Path) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            warn!("remove {}: {e}", path.display());
            false
        }
    }
}

impl Core {
    pub(super) fn file_path(&self, id: &str) -> PathBuf {
        self.files_dir.join(id)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn file_upload(
        &mut self,
        session: SessionId,
        name: String,
        size: u64,
        mime: String,
        sha256: String,
        sealed: bool,
    ) -> Reply {
        // Where it may be attached is checked when it is (an override can allow it in one channel).
        if !self.can_anywhere(session, Permission::AttachFiles) {
            return Err(err(ErrorCode::Forbidden, "missing permission"));
        }
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
                name: if sealed { "file".into() } else { clean_name(&name) },
                mime: if sealed { "application/octet-stream".into() } else { mime },
                sealed,
                size,
                token,
                sha256,
                until: now + UPLOAD_WINDOW,
                state: UploadState::Waiting,
            },
        );
        Ok(Response::Upload { file: id, upload_url })
    }

    pub(super) fn file_msg(&mut self, msg: FileMsg) {
        match msg {
            FileMsg::Begin { id, token, attempt, reply } => {
                let now = Instant::now();
                let deadline = now + UPLOAD_DEADLINE;
                let slot = self
                    .uploads
                    .get_mut(&id)
                    .filter(|u| u.state == UploadState::Waiting && u.until > now && u.token == token)
                    .map(|u| {
                        // The token works once; the upload stays reserved until it finishes or is given up.
                        u.state = UploadState::Writing { attempt };
                        (u.size, u.sha256.clone(), u.sealed)
                    })
                    .map(|(size, sha256, sealed)| UploadSlot {
                        path: self.files_dir.join(&id),
                        size,
                        sha256,
                        sealed,
                        deadline,
                    });
                let _ = reply.send(slot);
            }
            FileMsg::Finish { upload, reply } => {
                // A refused upload stays reserved until its `Abort` removes the bytes.
                let Some(pending) =
                    self.uploads.get(&upload.id).filter(|u| matches!(u.state, UploadState::Writing { .. }))
                else {
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
                    pending.sealed,
                );
                match &stored {
                    Ok(()) => drop(self.uploads.remove(&upload.id)),
                    Err(e) => warn!("store: {e:#}"),
                }
                let _ = reply.send(stored.is_ok());
            }
            FileMsg::Abort { id, attempt } => {
                // Only the request that started it; a finished upload is gone from `uploads` (its
                // `Finish` came first) and its bytes are the stored file's.
                let Some(u) = self.uploads.get_mut(&id).filter(|u| u.state == UploadState::Writing { attempt }) else {
                    return;
                };
                u.state = UploadState::GivenUp { removing: false };
                self.remove_given_up(id);
            }
            FileMsg::Info { id, reply } => {
                let file = self.store.file(&id).map_err(storage_error).ok().flatten();
                let _ = reply.send(file.map(|f| (f, self.file_path(&id))));
            }
            FileMsg::Open { id, access, reply } => {
                let Some(tx) = self.me.upgrade() else { return };
                if !self.may_download(&id, access.as_deref()) {
                    return;
                }
                let file = self.store.file(&id).map_err(storage_error).ok().flatten();
                let opened = file.map(|f| {
                    *self.reading.entry(id.clone()).or_default() += 1;
                    (f, self.file_path(&id), Reading { tx, id })
                });
                // If the request is gone, the reply is dropped and its `Reading` with it.
                let _ = reply.send(opened);
            }
            FileMsg::Done { id } => {
                let Some(count) = self.reading.get_mut(&id) else { return };
                *count -= 1;
                if *count == 0 {
                    self.reading.remove(&id);
                    // Deleted while it was read: its bytes can go now.
                    self.remove_files();
                }
            }
        }
    }

    /// With [`FileAccess::Members`], only a client connected here that can read the channel or
    /// sealed conversation the file is posted in may download it. A file in no message (not posted
    /// yet, or its message or channel deleted) is for nobody: its uploader has the bytes.
    fn may_download(&self, id: &str, access: Option<&str>) -> bool {
        let (conversation, channel) = match (self.store.file_conversation(id), self.store.file_channel(id)) {
            (Ok(conversation), Ok(channel)) => (conversation, channel),
            (Err(e), _) | (_, Err(e)) => {
                warn!("store: {e:#}");
                return false;
            }
        };
        // Attachments of a private channel, plaintext or encrypted, need an admitted session in
        // every mode.
        let private = channel
            .or_else(|| conversation.as_deref().and_then(channel_id))
            .is_some_and(|c| self.channels.get(&c).is_some_and(|c| c.private));
        if self.info.file_access == FileAccess::Link && !private {
            return true;
        }
        let Some(session) = access
            .filter(|a| !a.is_empty())
            .and_then(|a| self.sessions.values().find(|s| s.out.is_some() && s.file_access == a))
        else {
            return false;
        };
        match (conversation, channel) {
            (Some(conversation), _) => self.mls_can_read(&session.uid, &conversation).unwrap_or(false),
            (None, Some(channel)) => self.can_read(session, channel),
            (None, None) => false,
        }
    }

    /// Removes a given-up upload's bytes (off the actor), then its reservation. Anything that
    /// fails keeps the reservation, and the next tick tries again.
    fn remove_given_up(&mut self, id: String) {
        if !self.uploads.get(&id).is_some_and(|u| u.state == UploadState::GivenUp { removing: false }) {
            return;
        }
        match self.store.file(&id) {
            Ok(None) => {}
            // Stored after all: the bytes are the file's.
            Ok(Some(_)) => {
                self.uploads.remove(&id);
                return;
            }
            Err(e) => {
                warn!("store: {e:#}");
                return;
            }
        }
        let Some(tx) = self.me.upgrade() else { return };
        if let Some(u) = self.uploads.get_mut(&id) {
            u.state = UploadState::GivenUp { removing: true };
        }
        let path = self.file_path(&id);
        let paths = [path.with_extension("part"), path];
        tokio::task::spawn_blocking(move || {
            let removed = paths.iter().all(|path| unlink(path));
            // The space is free once the bytes are gone, not before.
            let done = move |core: &mut Core| match removed {
                true => drop(core.uploads.remove(&id)),
                false => {
                    if let Some(u) = core.uploads.get_mut(&id) {
                        u.state = UploadState::GivenUp { removing: false };
                    }
                }
            };
            let _ = tx.blocking_send(CoreMsg::Resume(Box::new(done)));
        });
    }

    /// Deletes the bytes of removed files (off the actor), except while they are downloaded.
    /// Each stays charged to its uploader until its bytes are gone; what fails is tried again
    /// by the next tick.
    pub(super) fn remove_files(&mut self) {
        let ids = match self.store.removed_files() {
            Ok(ids) => ids,
            Err(e) => {
                warn!("store: {e:#}");
                return;
            }
        };
        let Some(tx) = self.me.upgrade() else { return };
        let ids: Vec<_> =
            ids.into_iter().filter(|id| !self.reading.contains_key(id) && self.removing.insert(id.clone())).collect();
        if ids.is_empty() {
            return;
        }
        let dir = self.files_dir.clone();
        tokio::task::spawn_blocking(move || {
            let gone: Vec<_> = ids.iter().filter(|id| unlink(&dir.join(id))).cloned().collect();
            let done = move |core: &mut Core| {
                for id in &ids {
                    core.removing.remove(id);
                }
                if let Err(e) = core.store.forget_removed(&gone) {
                    warn!("store: {e:#}");
                }
            };
            let _ = tx.blocking_send(CoreMsg::Resume(Box::new(done)));
        });
    }

    pub(super) fn tick(&mut self) {
        let now = Instant::now();
        self.uploads.retain(|_, u| u.live(now));
        let stuck: Vec<_> = self
            .uploads
            .iter()
            .filter(|(_, u)| u.state == UploadState::GivenUp { removing: false })
            .map(|(id, _)| id.clone())
            .collect();
        for id in stuck {
            self.remove_given_up(id);
        }
        if let Err(e) = self.store.expire_unattached(now_ms() - UNATTACHED_KEEP_MS) {
            warn!("store: {e:#}");
        }
        if let Err(e) = self.store.expire_orphans() {
            warn!("store: {e:#}");
        }
        self.remove_files();
        self.sweep_files();
        self.touch_online();
        self.history_prune();
        if let Err(e) = self.store.mls_prune(now_ms()).and_then(|_| self.mls_reconcile()) {
            warn!("MLS cleanup: {e:#}");
        }
    }

    /// Whether the bytes named `name` belong to no upload and no file. Ids are random and
    /// never reused, so bytes stray now stay stray; when in doubt they are kept.
    fn stray(&self, name: &str) -> bool {
        match name.strip_suffix(".part") {
            Some(id) => !self.uploads.contains_key(id),
            None => !self.uploads.contains_key(name) && matches!(self.store.file_known(name), Ok(false)),
        }
    }

    /// Deletes bytes that belong to no upload and no file, such as those of an upload a
    /// restart cut short (reservations live only in memory). Lists the directory off the actor.
    fn sweep_files(&mut self) {
        let Some(tx) = self.me.upgrade() else { return };
        let dir = self.files_dir.clone();
        tokio::task::spawn_blocking(move || {
            let Ok(entries) = std::fs::read_dir(&dir) else { return };
            let names: Vec<String> = entries
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                .filter_map(|e| e.file_name().into_string().ok())
                .collect();
            let found = move |core: &mut Core| {
                let stray: Vec<_> = names.into_iter().filter(|name| core.stray(name)).collect();
                if stray.is_empty() {
                    return;
                }
                let dir = core.files_dir.clone();
                tokio::task::spawn_blocking(move || {
                    for name in stray {
                        unlink(&dir.join(name));
                    }
                });
            };
            let _ = tx.blocking_send(CoreMsg::Resume(Box::new(found)));
        });
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        core::expiry_tests::{on_core, test_core_storing},
        store::Store,
    };

    /// A reservation for 3 bytes with token "t", started (by attempt 1) or not.
    fn pending(started: bool) -> PendingUpload {
        PendingUpload {
            uploader: "u".into(),
            name: "f".into(),
            mime: "x".into(),
            size: 3,
            token: "t".into(),
            sha256: "h".into(),
            sealed: false,
            until: Instant::now() + UPLOAD_WINDOW,
            state: if started { UploadState::Writing { attempt: 1 } } else { UploadState::Waiting },
        }
    }

    /// Reserves `id` (not started, token "t").
    pub(crate) async fn reserve(core: &CoreHandle, id: &'static str) {
        on_core(core, move |core| drop(core.uploads.insert(id.into(), pending(false)))).await;
    }

    async fn reserved(core: &CoreHandle, id: &'static str) -> bool {
        on_core(core, move |core| core.uploads.contains_key(id)).await
    }

    /// Waits (up to a second) for the core to finish giving `id` up.
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
        // Another request with the same token (refused, then gone) cannot give it up.
        assert!(core.upload_begin("writing".into(), "t".into(), 2).await.is_none());
        core.upload_abort("writing".into(), 2).await;
        assert!(!released(&core, "writing").await);
        // A refused finish keeps the space until the bytes are removed.
        std::fs::write(dir.path().join("refused"), "abc").unwrap();
        let wrong = Uploaded { id: "refused".into(), size: 3, sha256: "x".into(), media: None, dimensions: None };
        assert!(!core.upload_finish(wrong).await);
        assert!(reserved(&core, "refused").await);
        core.upload_abort("refused".into(), 1).await;
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
        core.upload_abort("done".into(), 1).await;
        core.upload_abort("half".into(), 1).await;
        assert!(released(&core, "half").await, "space never freed");
        assert!(!dir.path().join("half.part").exists());
        assert_eq!(std::fs::read(dir.path().join("done")).unwrap(), b"abc");
        assert!(core.file_info("done".into()).await.is_some());
    }

    /// When the database or the disk fails, the space stays charged and the next tick retries.
    #[cfg(unix)]
    #[tokio::test]
    async fn giving_up_is_retried_until_the_bytes_are_gone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let files = dir.path().join("files");
        std::fs::create_dir(&files).unwrap();
        let core = test_core_storing(Store::in_memory().unwrap(), None, files.clone(), 100);
        on_core(&core, |core| {
            core.uploads.insert("f".into(), pending(true));
            core.store.raw().execute_batch("ALTER TABLE files RENAME TO hidden").unwrap();
        })
        .await;
        std::fs::write(files.join("f.part"), "a").unwrap();
        core.upload_abort("f".into(), 1).await;
        assert!(!released(&core, "f").await, "freed without knowing whether it was stored");
        on_core(&core, |core| {
            core.store.raw().execute_batch("ALTER TABLE hidden RENAME TO files").unwrap();
        })
        .await;
        std::fs::set_permissions(&files, std::fs::Permissions::from_mode(0o555)).unwrap();
        // (Root may delete there anyway; then only the database half is tested.)
        if std::fs::write(files.join("probe"), "").is_err() {
            on_core(&core, |core| core.tick()).await;
            assert!(!released(&core, "f").await, "freed while its bytes are still there");
            assert!(files.join("f.part").exists());
        }
        std::fs::set_permissions(&files, std::fs::Permissions::from_mode(0o755)).unwrap();
        on_core(&core, |core| core.tick()).await;
        assert!(released(&core, "f").await);
        assert!(!files.join("f.part").exists());
    }

    /// Bytes charged to "u" once no removal is running.
    async fn charged(core: &CoreHandle) -> u64 {
        for _ in 0..100 {
            let idle = on_core(core, |core| core.removing.is_empty().then(|| core.store.stored_bytes(Some("u")))).await;
            if let Some(bytes) = idle {
                return bytes.unwrap();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the removal never finished");
    }

    /// A removed file's space is freed once its bytes are gone, and not before.
    #[cfg(unix)]
    #[tokio::test]
    async fn removed_files_stay_charged_until_their_bytes_are_gone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let files = dir.path().join("files");
        std::fs::create_dir(&files).unwrap();
        std::fs::write(files.join("old"), "abc").unwrap();
        let store = Store::in_memory().unwrap();
        store.insert_file("old", "u", "f", "x", 3, "h", None, 0, false).unwrap();
        std::fs::set_permissions(&files, std::fs::Permissions::from_mode(0o555)).unwrap();
        // (Root may delete there anyway; then only the freeing is tested.)
        let stuck = std::fs::write(files.join("probe"), "").is_err();
        let core = test_core_storing(store, None, files.clone(), 100);
        on_core(&core, |core| core.tick()).await;
        if stuck {
            assert_eq!(charged(&core).await, 3, "freed while its bytes are still there");
            assert!(files.join("old").exists());
            on_core(&core, |core| core.tick()).await;
            assert_eq!(charged(&core).await, 3);
        }
        std::fs::set_permissions(&files, std::fs::Permissions::from_mode(0o755)).unwrap();
        on_core(&core, |core| core.tick()).await;
        assert_eq!(charged(&core).await, 0);
        assert!(!files.join("old").exists());
    }

    /// After a restart, bytes of uploads that never finished go; files, removed files still
    /// charged, and uploads in progress stay.
    #[tokio::test]
    async fn stray_bytes_are_swept() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_memory().unwrap();
        for id in ["stored", "removed"] {
            store.insert_file(id, "u", "f", "x", 3, "h", None, now_ms(), false).unwrap();
        }
        let core = test_core_storing(store, None, dir.path().into(), 100);
        // The state first, then the bytes, so that the tick at startup cannot see one without the other.
        on_core(&core, |core| {
            core.store.raw().execute("DELETE FROM files WHERE id='removed'", []).unwrap();
            // Not removable just now: only the sweep could take it.
            core.removing.insert("removed".into());
            core.uploads.insert("busy".into(), pending(true));
            core.uploads.insert("busy2".into(), pending(true));
        })
        .await;
        for name in ["stored", "removed", "busy.part", "busy2", "cut.part", "cut", "stored.part"] {
            std::fs::write(dir.path().join(name), "x").unwrap();
        }
        on_core(&core, |core| core.tick()).await;
        let gone = |name: &str| !dir.path().join(name).exists();
        for _ in 0..100 {
            if ["cut.part", "cut", "stored.part"].iter().all(|n| gone(n)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        for name in ["cut.part", "cut", "stored.part"] {
            assert!(gone(name), "{name} was left");
        }
        for name in ["stored", "removed", "busy.part", "busy2"] {
            assert!(!gone(name), "{name} was swept");
        }
        on_core(&core, |core| {
            core.store.raw().execute_batch("ALTER TABLE files RENAME TO hidden").unwrap();
            assert!(!core.stray("stored"), "swept without knowing whether it is stored");
        })
        .await;
    }

    /// A file deleted while it is downloaded keeps its bytes, and its charge, until the download
    /// ends; a download whose request went away holds nothing.
    #[tokio::test]
    async fn a_file_being_read_is_removed_after_the_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_memory().unwrap();
        for id in ["f", "g"] {
            store.insert_file(id, "u", "f", "x", 3, "h", None, now_ms(), false).unwrap();
            std::fs::write(dir.path().join(id), "abc").unwrap();
        }
        let core = test_core_storing(store, None, dir.path().into(), 100);
        let (_, path, reading) = core.file_open("f".into(), None).await.unwrap();
        on_core(&core, |core| {
            let (reply, rx) = oneshot::channel();
            drop(rx);
            core.file_msg(FileMsg::Open { id: "g".into(), access: None, reply });
            core.store.raw().execute_batch("DELETE FROM files").unwrap();
            core.tick();
        })
        .await;
        for _ in 0..100 {
            if !dir.path().join("g").exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!dir.path().join("g").exists(), "an abandoned read kept the bytes");
        assert_eq!(charged(&core).await, 3, "freed while it is read");
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");
        assert!(core.file_open("f".into(), None).await.is_none(), "deleted files cannot be opened");
        drop(reading);
        for _ in 0..100 {
            if charged(&core).await == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(charged(&core).await, 0);
        assert!(!path.exists());
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
