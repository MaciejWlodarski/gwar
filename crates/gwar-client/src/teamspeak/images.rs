//! Icons and avatars of a TeamSpeak server, for TeamSpeak mode.
//!
//! TeamSpeak clients fetch them through the server's file transfer, from channel 0: a group's,
//! channel's or the server's icon is `/icon_<id>`, a client's avatar `/avatar_<uid>` (the uid
//! spelled as [`avatar_name`] does). The adapter does the same, with tight bounds: a size limit
//! per kind, [`CONCURRENT`] transfers at a time, an absolute [`TIMEOUT`] per transfer, at most
//! [`WANTED_LIMIT`] images considered, and a per-connection cache of at most [`CACHE_BYTES`] in
//! memory, nothing on disk. An icon is keyed by its id and an avatar by its owner's uid and avatar
//! hash (`client_flag_avatar`), so an image is fetched again only when that changes. Every file
//! must be a PNG, JPEG, GIF or WebP image by its first bytes ([`sniff`]); nothing else is kept,
//! SVG included, whatever the server says the file is.
//!
//! A transfer holds its slot until its work has ended. Asked for, it waits for TeamSpeak's offer
//! (`notifystartdownload`), which tsclientlib hands over without connecting (Gwar's patch, see
//! `vendor/tsclientlib/GWAR-PATCH.md`); an offer that comes after the deadline is ignored, and
//! one that points anywhere but the server we are connected to ([`transfer_address`]) fails the
//! file. Then one task of [`Reads`] connects, sends the key and reads the file, all within the
//! deadline; it
//! keeps the slot until it has ended, and all of them end with the connection. A file that timed
//! out or whose connection broke is asked for again later, [`RETRIES`] times in all at most.
//!
//! The proto shapes carry a token per image ([`ImageKey::token`]) once it is here, never before,
//! and the UI asks the connection for the bytes by token (`TsHandle::image`).

use std::{
    collections::{BTreeSet, HashMap},
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};

use sha1::{Digest, Sha1};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    time::Instant,
};

/// The largest icon kept. TeamSpeak's icons are 16×16 px, a few hundred bytes.
pub const ICON_LIMIT: u64 = 64 * 1024;
/// The largest avatar kept. TeamSpeak's client uploads avatars of at most 300×300 px.
pub const AVATAR_LIMIT: u64 = 300 * 1024;
/// Transfers at a time; the rest wait for a free one.
pub const CONCURRENT: usize = 4;
/// From asking for a file to having all of it: a transfer's absolute deadline.
pub const TIMEOUT: Duration = Duration::from_secs(15);
/// Tries in all for a file that timed out or whose connection broke, while it is shown: the
/// second after [`RETRY_DELAY`], each further one after five times the wait before.
pub const RETRIES: u32 = 3;
/// The wait before trying a file again the first time.
pub const RETRY_DELAY: Duration = Duration::from_secs(60);
/// All images of a connection together, in bytes.
pub const CACHE_BYTES: usize = 32 << 20;
/// Images of a connection, at most.
pub const CACHE_ENTRIES: usize = 2048;
/// Images considered at a time, at most: the first this many the view shows (by key). Bounds the
/// files remembered as failed (only ones still shown are), so none is asked for twice while shown.
pub const WANTED_LIMIT: usize = CACHE_ENTRIES;
/// Icon ids below this are TeamSpeak's built-in group icons (100, 200, 300, 500, 600), which its
/// client draws from its own skin: there is no file to fetch, and Gwar shows the badge's name.
pub const BUILTIN_ICONS: u32 = 1000;

/// One image of a TeamSpeak server.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ImageKey {
    /// An icon by its id (TeamSpeak's CRC32 of the file).
    Icon(u32),
    /// A client's avatar: their uid's bytes and their avatar's hash.
    Avatar { uid: Vec<u8>, hash: String },
}

impl ImageKey {
    /// The icon `id`, if it is a file: 0 is none, and ids below [`BUILTIN_ICONS`] are built in.
    pub fn icon(id: u32) -> Option<Self> {
        (id >= BUILTIN_ICONS).then_some(Self::Icon(id))
    }

    /// The avatar of uid `uid` with hash `hash`, if they have one. TeamSpeak's hash is an MD5
    /// in hex; anything else that is not plain and short is no avatar.
    pub fn avatar(uid: &[u8], hash: &str) -> Option<Self> {
        let plain = !hash.is_empty() && hash.len() <= 64 && hash.bytes().all(|b| b.is_ascii_alphanumeric());
        (plain && !uid.is_empty() && uid.len() <= 64).then(|| Self::Avatar { uid: uid.to_vec(), hash: hash.to_owned() })
    }

    /// Where the file is, in channel 0's file transfer.
    pub fn path(&self) -> String {
        match self {
            Self::Icon(id) => format!("/icon_{id}"),
            Self::Avatar { uid, .. } => format!("/{}", avatar_name(uid)),
        }
    }

    /// The largest file accepted for it.
    pub fn limit(&self) -> u64 {
        match self {
            Self::Icon(_) => ICON_LIMIT,
            Self::Avatar { .. } => AVATAR_LIMIT,
        }
    }

    /// What the proto shapes carry for it: `i<id>` for an icon, `a<sha1 hex>` of the uid and hash
    /// for an avatar. Short, URL-safe, and a new token for every new image.
    pub fn token(&self) -> String {
        match self {
            Self::Icon(id) => format!("i{id}"),
            Self::Avatar { uid, hash } => {
                let mut digest = Sha1::new();
                digest.update((uid.len() as u32).to_be_bytes());
                digest.update(uid);
                digest.update(hash.as_bytes());
                let hex: String = digest.finalize().iter().map(|b| format!("{b:02x}")).collect();
                format!("a{hex}")
            }
        }
    }
}

/// TeamSpeak's file name for a client's avatar: `avatar_` and the uid's bytes in hex, with the
/// digits 0–f written as the letters a–p (what Query calls `client_base64HashClientUID`).
pub fn avatar_name(uid: &[u8]) -> String {
    let mut name = String::with_capacity(7 + uid.len() * 2);
    name.push_str("avatar_");
    for byte in uid {
        name.push(char::from(b'a' + (byte >> 4)));
        name.push(char::from(b'a' + (byte & 0x0f)));
    }
    name
}

/// The image formats kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Png,
    Jpeg,
    Gif,
    Webp,
}

/// The format of `bytes` by their first bytes; `None` for anything else (SVG, HTML, scripts).
pub fn sniff(bytes: &[u8]) -> Option<Format> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(Format::Png)
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(Format::Jpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(Format::Gif)
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some(Format::Webp)
    } else {
        None
    }
}

/// Why a transfer brought no file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferError {
    /// Not done by its deadline.
    TimedOut,
    /// The connection failed or broke.
    Failed,
    /// It ended before the announced size.
    Short,
    /// Announced empty or larger than its limit.
    Size,
}

impl TransferError {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TimedOut => "timed out",
            Self::Failed => "transfer failed",
            Self::Short => "not the announced size",
            Self::Size => "too large or empty",
        }
    }

    /// Whether the same file may come another time.
    fn transient(self) -> bool {
        !matches!(self, Self::Size)
    }
}

/// Where to connect for a download TeamSpeak offered: always the server we are connected to
/// (`server`, the live connection's address, so a host name is not looked up again), at the
/// offer's `port`. The offer's `ip` may only say so: none, empty, a placeholder (`0.0.0.0`, `::`)
/// or that same address, alone or as a comma-separated list of such. Any other host or address,
/// or port 0, is `None`: nothing is connected to.
pub fn transfer_address(server: SocketAddr, ip: Option<&str>, port: u16) -> Option<SocketAddr> {
    let own = server.ip().to_canonical();
    let ours = |entry: &str| {
        let entry = entry.trim();
        entry.is_empty() || entry.parse::<IpAddr>().is_ok_and(|ip| ip.is_unspecified() || ip.to_canonical() == own)
    };
    // The server's own address with the offer's port: an IPv6 peer keeps its scope (interface).
    (port != 0 && ip.is_none_or(|ip| ip.split(',').all(ours))).then(|| {
        let mut address = server;
        address.set_port(port);
        address
    })
}

/// Downloads a file of `size` bytes TeamSpeak offered: connects (`connect`), sends the transfer's
/// `key`, and reads all of the file and at most `limit`, everything by `deadline`.
pub async fn download<S>(
    connect: impl Future<Output = std::io::Result<S>>,
    key: String,
    size: u64,
    limit: u64,
    deadline: Instant,
) -> Result<Vec<u8>, TransferError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if size == 0 || size > limit {
        return Err(TransferError::Size);
    }
    // `timeout_at` polls its future once before looking at the clock: past the deadline, not even
    // the connect is started.
    if Instant::now() >= deadline {
        return Err(TransferError::TimedOut);
    }
    let transfer = async {
        let mut stream = connect.await.map_err(|_| TransferError::Failed)?;
        stream.write_all(key.as_bytes()).await.map_err(|_| TransferError::Failed)?;
        stream.flush().await.map_err(|_| TransferError::Failed)?;
        let mut data = Vec::with_capacity(size as usize);
        // Exactly the announced bytes: TeamSpeak may keep the connection open after them.
        let mut file = stream.take(size);
        file.read_to_end(&mut data).await.map_err(|_| TransferError::Failed)?;
        if data.len() as u64 != size {
            return Err(TransferError::Short);
        }
        Ok(data)
    };
    tokio::time::timeout_at(deadline, transfer).await.unwrap_or(Err(TransferError::TimedOut))
}

/// A transfer's end: its id and its file.
pub type Transferred = (u16, Result<Vec<u8>, TransferError>);

/// The transfers under way, each on a task of its own that ends by its deadline ([`download`]).
/// Dropping this (the connection ended) aborts them all.
#[derive(Default)]
pub struct Reads {
    tasks: tokio::task::JoinSet<Transferred>,
    ids: HashMap<tokio::task::Id, u16>,
}

impl Reads {
    /// Downloads transfer `id`'s file (see [`download`]).
    pub fn start<S>(
        &mut self,
        id: u16,
        connect: impl Future<Output = std::io::Result<S>> + Send + 'static,
        key: String,
        size: u64,
        limit: u64,
        deadline: Instant,
    ) where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let task = self.tasks.spawn(async move { (id, download(connect, key, size, limit, deadline).await) });
        self.ids.insert(task.id(), id);
    }

    /// Transfers under way.
    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// The next transfer to end, once its task has. Never, while none runs. Cancel safe.
    pub async fn next(&mut self) -> Transferred {
        loop {
            match self.tasks.join_next_with_id().await {
                None => std::future::pending::<()>().await,
                Some(Ok((task, done))) => {
                    self.ids.remove(&task);
                    return done;
                }
                Some(Err(error)) => {
                    if let Some(id) = self.ids.remove(&error.id()) {
                        return (id, Err(TransferError::Failed));
                    }
                }
            }
        }
    }
}

/// The images ready for the UI, by key and by token; shared with the connection's handle.
#[derive(Default)]
pub struct Store {
    images: HashMap<ImageKey, Arc<[u8]>>,
    tokens: HashMap<String, ImageKey>,
    bytes: usize,
}

impl Store {
    /// The image `token` names, if it is here.
    pub fn get(&self, token: &str) -> Option<Arc<[u8]>> {
        self.images.get(self.tokens.get(token)?).cloned()
    }

    fn has(&self, key: &ImageKey) -> bool {
        self.images.contains_key(key)
    }

    /// The bytes and entries there are, or could be made by dropping images no longer `wanted`.
    fn room(&self, wanted: &BTreeSet<ImageKey>) -> (usize, usize) {
        let unwanted = self.images.iter().filter(|(k, _)| !wanted.contains(k));
        let (bytes, entries) = unwanted.fold((0, 0), |(b, n), (_, image)| (b + image.len(), n + 1));
        (CACHE_BYTES.saturating_sub(self.bytes) + bytes, CACHE_ENTRIES.saturating_sub(self.images.len()) + entries)
    }

    fn remove(&mut self, key: &ImageKey) {
        if let Some(old) = self.images.remove(key) {
            self.bytes -= old.len();
            self.tokens.remove(&key.token());
        }
    }

    /// Keeps `bytes` for `key` if they fit, making room by dropping images no longer `wanted`.
    fn insert(&mut self, key: ImageKey, bytes: Vec<u8>, wanted: &BTreeSet<ImageKey>) -> bool {
        self.remove(&key);
        let fits = |s: &Self| s.bytes + bytes.len() <= CACHE_BYTES && s.images.len() < CACHE_ENTRIES;
        if !fits(self) {
            let unwanted: Vec<ImageKey> = self.images.keys().filter(|k| !wanted.contains(k)).cloned().collect();
            for key in unwanted {
                if fits(self) {
                    break;
                }
                self.remove(&key);
            }
        }
        if !fits(self) {
            return false;
        }
        self.bytes += bytes.len();
        self.tokens.insert(key.token(), key.clone());
        self.images.insert(key, bytes.into());
        true
    }
}

/// Where a transfer is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// Asked for: waiting for TeamSpeak's answer and offer. Nothing of it runs here.
    Asked,
    /// Offered: a task of [`Reads`] downloads it, by the deadline.
    Running,
}

/// A transfer under way: it holds one of the [`CONCURRENT`] slots until its work has ended.
struct Flight {
    key: ImageKey,
    /// The `ftinitdownload` command's answer, until it comes.
    command: Option<tsclientlib::MessageHandle>,
    deadline: Instant,
    stage: Stage,
}

/// A shown image whose file did not come.
struct Failure {
    tries: u32,
    /// When to try again; never if the file itself was wrong (missing, refused, no image).
    retry: Option<Instant>,
}

/// What the adapter knows of a connection's images: the ones ready (in the shared [`Store`]),
/// the transfers under way, the files that failed, and the ones that found the cache full.
pub struct Images {
    store: Arc<Mutex<Store>>,
    /// The images the view shows, at most [`WANTED_LIMIT`].
    wanted: BTreeSet<ImageKey>,
    flights: HashMap<u16, Flight>,
    /// Shown images whose files did not come: asked for again only as [`Failure::retry`] says.
    failed: HashMap<ImageKey, Failure>,
    /// Shown images that found the cache full of shown images, by size: asked for again once that
    /// much room can be made.
    deferred: HashMap<ImageKey, usize>,
    next_id: u16,
}

impl Default for Images {
    fn default() -> Self {
        Self::new()
    }
}

impl Images {
    pub fn new() -> Self {
        Self {
            store: Arc::default(),
            wanted: BTreeSet::new(),
            flights: HashMap::new(),
            failed: HashMap::new(),
            deferred: HashMap::new(),
            next_id: 0,
        }
    }

    pub fn store(&self) -> Arc<Mutex<Store>> {
        self.store.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The token of `key` if its image is here.
    pub fn token(&self, key: Option<ImageKey>) -> Option<String> {
        let key = key?;
        self.lock().has(&key).then(|| key.token())
    }

    /// The images the view shows now (the first [`WANTED_LIMIT`] of them count); the transfers to
    /// start for them (by transfer id). What is no longer shown is forgotten as failed or deferred.
    pub fn want(&mut self, wanted: BTreeSet<ImageKey>) -> Vec<(u16, ImageKey)> {
        self.wanted = wanted.into_iter().take(WANTED_LIMIT).collect();
        self.failed.retain(|k, _| self.wanted.contains(k));
        self.deferred.retain(|k, _| self.wanted.contains(k));
        self.queued()
    }

    /// Transfers to start, as free slots allow: images wanted, not here, not under way, not
    /// failed (unless due to be tried again), and not deferred unless room for them can be made.
    pub fn queued(&mut self) -> Vec<(u16, ImageKey)> {
        let free = CONCURRENT.saturating_sub(self.flights.len());
        if free == 0 {
            return Vec::new();
        }
        let now = Instant::now();
        let store = self.lock();
        let (bytes, entries) = store.room(&self.wanted);
        let busy: BTreeSet<&ImageKey> = self.flights.values().map(|f| &f.key).collect();
        let start: Vec<ImageKey> = self
            .wanted
            .iter()
            .filter(|k| !store.has(k) && !busy.contains(k))
            .filter(|k| self.failed.get(*k).is_none_or(|f| f.retry.is_some_and(|at| at <= now)))
            .filter(|k| self.deferred.get(*k).is_none_or(|&size| size <= bytes && entries > 0))
            .take(free)
            .cloned()
            .collect();
        drop(store);
        let deadline = now + TIMEOUT;
        start
            .into_iter()
            .map(|key| {
                // Ids are ours alone: tsclientlib numbers only the transfers it starts itself.
                self.next_id = self.next_id.wrapping_add(1);
                while self.flights.contains_key(&self.next_id) {
                    self.next_id = self.next_id.wrapping_add(1);
                }
                let id = self.next_id;
                self.flights.insert(id, Flight { key: key.clone(), command: None, deadline, stage: Stage::Asked });
                (id, key)
            })
            .collect()
    }

    /// The `ftinitdownload` of transfer `id` went out as `command`.
    pub fn sent(&mut self, id: u16, command: tsclientlib::MessageHandle) {
        if let Some(flight) = self.flights.get_mut(&id) {
            flight.command = Some(command);
        }
    }

    /// The `ftinitdownload` of transfer `id` could not be sent: tried again later.
    pub fn unsent(&mut self, id: u16) {
        self.end_asked(id, true);
    }

    /// TeamSpeak answered a command; true if it was one of ours. A refusal fails the transfer.
    pub fn answered(&mut self, command: tsclientlib::MessageHandle, ok: bool) -> bool {
        let Some((&id, flight)) = self.flights.iter_mut().find(|(_, f)| f.command == Some(command)) else {
            return false;
        };
        flight.command = None;
        if !ok {
            self.end_asked(id, false);
        }
        true
    }

    /// TeamSpeak offered transfer `id`'s file of `size` bytes: the most to read and the deadline,
    /// for [`Reads::start`]. `None` to ignore the offer (connect nowhere): the transfer is not
    /// one asked for (unknown, timed out, or offered already), or the file is too large (then it
    /// failed).
    pub fn offered(&mut self, id: u16, size: u64) -> Option<(u64, Instant)> {
        if self.late(id) {
            return None;
        }
        let flight = self.flights.get_mut(&id)?;
        if flight.stage != Stage::Asked {
            return None;
        }
        let limit = flight.key.limit();
        if size == 0 || size > limit {
            self.end_asked(id, false);
            return None;
        }
        flight.stage = Stage::Running;
        Some((limit, flight.deadline))
    }

    /// TeamSpeak's `offer` for transfer `id`: where to connect, the most to read and the deadline,
    /// for [`Reads::start`]; `None` to ignore it and connect nowhere (see [`Images::offered`]). An
    /// offer pointing anywhere but the server we are connected to ([`transfer_address`]) fails
    /// the file like a wrong one: not asked for again while it is shown.
    pub fn accept(&mut self, id: u16, offer: &tsclientlib::FileDownloadOffer) -> Option<(SocketAddr, u64, Instant)> {
        if self.late(id) {
            return None;
        }
        let Some(address) = transfer_address(offer.server, offer.ip.as_deref(), offer.port) else {
            if let Some(flight) = self.flights.get(&id).filter(|f| f.stage == Stage::Asked) {
                tracing::debug!(path = %flight.key.path(), ip = ?offer.ip, port = offer.port, "TeamSpeak offered a file elsewhere than its server: ignored");
                self.end_asked(id, false);
            }
            return None;
        };
        let (limit, deadline) = self.offered(id, offer.size)?;
        Some((address, limit, deadline))
    }

    /// Whether transfer `id` is asked for and past its deadline, not yet expired by the tick: then
    /// it ends now, timed out (to be tried again later), and its offer is ignored.
    fn late(&mut self, id: u16) -> bool {
        let late = self.flights.get(&id).is_some_and(|f| f.stage == Stage::Asked && Instant::now() >= f.deadline);
        if late {
            self.end_asked(id, true);
        }
        late
    }

    /// TeamSpeak refused transfer `id` (a missing file, say). A running download reports itself.
    pub fn refused(&mut self, id: u16) {
        self.end_asked(id, false);
    }

    /// Ends transfer `id` if it is asked for and not running; its slot is free then.
    fn end_asked(&mut self, id: u16, transient: bool) {
        if self.flights.get(&id).is_some_and(|f| f.stage == Stage::Asked) {
            let flight = self.flights.remove(&id).expect("present");
            self.failed(flight.key, transient);
        }
    }

    fn failed(&mut self, key: ImageKey, transient: bool) {
        // Only images shown count, so this stays within [`WANTED_LIMIT`].
        if !self.wanted.contains(&key) {
            return;
        }
        let failure = self.failed.entry(key).or_insert(Failure { tries: 0, retry: None });
        failure.tries += 1;
        failure.retry =
            (transient && failure.tries < RETRIES).then(|| Instant::now() + RETRY_DELAY * 5u32.pow(failure.tries - 1));
    }

    /// Transfer `id`'s download ended (its task has) with `data`: kept if it is an image and fits.
    /// True if the view changes.
    pub fn finished(&mut self, id: u16, data: Result<Vec<u8>, TransferError>) -> bool {
        if self.flights.get(&id).is_none_or(|f| f.stage != Stage::Running) {
            return false;
        }
        let Some(Flight { key, .. }) = self.flights.remove(&id) else { return false };
        match data {
            Ok(data) if data.len() as u64 > key.limit() || sniff(&data).is_none() => {
                tracing::debug!(path = %key.path(), "TeamSpeak image refused: not a PNG, JPEG, GIF or WebP image");
                self.failed(key, false);
            }
            Ok(data) => {
                let size = data.len();
                if self.lock().insert(key.clone(), data, &self.wanted) {
                    self.failed.remove(&key);
                    self.deferred.remove(&key);
                    return self.wanted.contains(&key);
                }
                // Room only for images shown: this one waits until room can be made for it.
                tracing::debug!(path = %key.path(), "no room for another TeamSpeak image");
                if self.wanted.contains(&key) {
                    self.deferred.insert(key, size);
                }
            }
            Err(error) => {
                tracing::debug!(path = %key.path(), error = error.as_str(), "TeamSpeak image not fetched");
                self.failed(key, error.transient());
            }
        }
        false
    }

    /// Ends transfers asked for and not offered by their deadline (to be tried again later);
    /// running ones end on their own by then. True if a slot was freed or a file is due to be
    /// tried again, so transfers may start.
    pub fn expire(&mut self, now: Instant) -> bool {
        let late: Vec<u16> = self
            .flights
            .iter()
            .filter(|(_, f)| f.stage == Stage::Asked && now >= f.deadline)
            .map(|(&id, _)| id)
            .collect();
        for &id in &late {
            self.end_asked(id, true);
        }
        let due = self.failed.values().any(|f| f.retry.is_some_and(|at| at <= now));
        !late.is_empty() || due
    }

    /// Transfers holding a slot.
    pub fn busy(&self) -> usize {
        self.flights.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    #[test]
    fn only_png_jpeg_gif_and_webp_count_as_images() {
        assert_eq!(sniff(PNG), Some(Format::Png));
        assert_eq!(sniff(&[0xff, 0xd8, 0xff, 0xe0, 0, 0x10]), Some(Format::Jpeg));
        assert_eq!(sniff(b"GIF89a\x01\0\x01\0"), Some(Format::Gif));
        assert_eq!(sniff(b"GIF87a\x01\0\x01\0"), Some(Format::Gif));
        assert_eq!(sniff(b"RIFF\x1a\0\0\0WEBPVP8 "), Some(Format::Webp));
        for refused in [
            &b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>"[..],
            b"<?xml version=\"1.0\"?><svg/>",
            b"<html><script>alert(1)</script>",
            b"RIFF\x1a\0\0\0WAVEfmt ",
            b"BM\x36\0\0\0",
            b"\x89PNG",
            b"",
        ] {
            assert_eq!(sniff(refused), None, "{refused:?}");
        }
    }

    #[test]
    fn avatar_paths_spell_the_uid_like_teamspeak() {
        // What Query's `clientdbinfo` names `client_base64HashClientUID` for this uid (TeamSpeak 3.13.7).
        let uid =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, "DSKUhy4h5QDnrp+1tKh2iAxQ6Jk=").unwrap();
        let key = ImageKey::avatar(&uid, "0123456789abcdef0123456789abcdef").unwrap();
        assert_eq!(key.path(), "/avatar_anccjeihcocbofaaohkojplflekihgiiamfaoijj");
        assert_eq!(avatar_name(&[0x00, 0x1f, 0xa0, 0xff]), "avatar_aabpkapp");
        assert_eq!(ImageKey::Icon(3_000_000_000).path(), "/icon_3000000000");
    }

    #[test]
    fn keys_skip_built_in_icons_and_odd_hashes() {
        assert_eq!(ImageKey::icon(0), None);
        for builtin in [100, 200, 300, 500, 600, 999] {
            assert_eq!(ImageKey::icon(builtin), None);
        }
        assert_eq!(ImageKey::icon(1000), Some(ImageKey::Icon(1000)));
        assert!(ImageKey::avatar(b"uid", "").is_none());
        assert!(ImageKey::avatar(b"", "abc").is_none());
        assert!(ImageKey::avatar(b"uid", "../icon_1").is_none());
        assert!(ImageKey::avatar(b"uid", &"a".repeat(65)).is_none());
        assert_eq!(ImageKey::Icon(1234).limit(), ICON_LIMIT);
        assert_eq!(ImageKey::avatar(b"uid", "abc").unwrap().limit(), AVATAR_LIMIT);
    }

    #[test]
    fn tokens_are_short_url_safe_and_change_with_the_image() {
        assert_eq!(ImageKey::Icon(1234).token(), "i1234");
        let a = ImageKey::avatar(b"alice", "aa").unwrap().token();
        let b = ImageKey::avatar(b"alice", "ab").unwrap().token();
        let c = ImageKey::avatar(b"bob", "aa").unwrap().token();
        // The length prefix keeps uid and hash apart.
        let d = ImageKey::avatar(b"alicea", "a").unwrap().token();
        assert!(a.len() == 41 && a.starts_with('a') && a[1..].bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(a != b && a != c && a != d && b != c);
    }

    fn handle(n: u16) -> tsclientlib::MessageHandle {
        tsclientlib::MessageHandle(n)
    }

    #[test]
    fn transfers_are_bounded_cached_and_not_repeated() {
        let mut images = Images::new();
        let wanted: BTreeSet<_> = (1000..1010).map(ImageKey::Icon).collect();
        let started = images.want(wanted.clone());
        assert_eq!(started.len(), CONCURRENT);
        assert!(images.queued().is_empty(), "no more than {CONCURRENT} at a time");
        let (first, key) = started[0].clone();
        assert_eq!(images.token(Some(key.clone())), None, "no token before the image is here");

        // An image: kept, its token shown, never fetched again.
        images.sent(first, handle(1));
        assert!(images.answered(handle(1), true));
        assert!(!images.answered(handle(99), false), "not ours");
        assert_eq!(images.offered(first, PNG.len() as u64).map(|(limit, _)| limit), Some(ICON_LIMIT));
        assert!(images.finished(first, Ok(PNG.to_vec())));
        assert_eq!(images.token(Some(key.clone())), Some(key.token()));
        assert_eq!(images.store().lock().unwrap().get(&key.token()).as_deref(), Some(PNG));
        let next = images.queued();
        assert_eq!(next.len(), 1);
        assert!(next.iter().all(|(_, k)| *k != key));

        // Refused by TeamSpeak, too large, not an image, timed out: failed, and not asked for again
        // now (the one that timed out is, later).
        let (refused, _) = started[1].clone();
        images.sent(refused, handle(2));
        assert!(images.answered(handle(2), false));
        let (large, _) = started[2].clone();
        assert_eq!(images.offered(large, ICON_LIMIT + 1), None);
        let (svg, svg_key) = started[3].clone();
        assert!(images.offered(svg, 10).is_some());
        assert!(!images.finished(svg, Ok(b"<svg onload=alert(1)>".to_vec())));
        assert_eq!(images.token(Some(svg_key)), None);
        let (late, _) = next[0].clone();
        assert!(!images.expire(Instant::now()));
        assert!(images.expire(Instant::now() + TIMEOUT), "nothing of it runs: its slot is free at once");
        assert_eq!(images.offered(late, PNG.len() as u64), None, "a late offer is ignored: nothing connects");
        assert!(!images.finished(late, Ok(PNG.to_vec())), "a timed-out transfer is over");
        let rest = images.want(wanted.clone());
        assert_eq!(rest.len(), CONCURRENT);
        let failed: BTreeSet<_> = [&started[1].1, &started[2].1, &started[3].1, &next[0].1].into_iter().collect();
        assert!(rest.iter().all(|(_, k)| !failed.contains(k) && *k != key));
        assert!(rest.iter().all(|(id, _)| ![first, refused, large, svg, late].contains(id)), "fresh transfer ids");

        // A new id is a new image.
        let changed = images.want(BTreeSet::from([ImageKey::Icon(2000)]));
        assert!(changed.is_empty(), "all transfers busy");
        for (id, _) in rest {
            images.refused(id);
        }
        assert_eq!(images.queued(), vec![(images.next_id, ImageKey::Icon(2000))]);
        // A transfer TeamSpeak offers twice, or never asked for, is ignored; the first download goes on.
        assert_eq!(images.offered(images.next_id, 10).map(|(limit, _)| limit), Some(ICON_LIMIT));
        assert_eq!(images.offered(images.next_id, 10), None);
        assert_eq!(images.offered(4242, 10), None);
        images.refused(images.next_id);
        assert_eq!(images.busy(), 1, "a running download reports itself");
        assert!(images.finished(images.next_id, Ok(PNG.to_vec())));
    }

    /// Starts every transfer `images` wants and ends each with `file`, until none starts; how many started.
    fn deliver(images: &mut Images, wanted: BTreeSet<ImageKey>, file: &[u8]) -> usize {
        let mut started = 0;
        let mut start = images.want(wanted);
        while !start.is_empty() {
            for (id, _) in start {
                started += 1;
                images.offered(id, file.len() as u64).unwrap();
                images.finished(id, Ok(file.to_vec()));
            }
            start = images.queued();
        }
        started
    }

    type Connect = std::pin::Pin<Box<dyn Future<Output = std::io::Result<tokio::io::DuplexStream>> + Send>>;

    #[tokio::test(start_paused = true)]
    async fn a_transfer_holds_its_slot_until_its_work_has_ended() {
        let mut images = Images::new();
        let mut reads = Reads::default();
        let wanted: BTreeSet<_> = (1000..1100).map(ImageKey::Icon).collect();
        // Four offered just before their deadline: two connect and get no file, two never connect.
        let start = images.want(wanted.clone());
        tokio::time::advance(TIMEOUT - Duration::from_secs(1)).await;
        let mut servers = Vec::new();
        for (n, (id, _)) in start.into_iter().enumerate() {
            let connect: Connect = if n % 2 == 0 {
                let (client, server) = tokio::io::duplex(64);
                servers.push(server);
                Box::pin(std::future::ready(Ok(client)))
            } else {
                Box::pin(std::future::pending())
            };
            let (limit, deadline) = images.offered(id, 10).unwrap();
            reads.start(id, connect, "key".into(), 10, limit, deadline);
        }
        // Long past every deadline: no slot is free until its download (connect included) has
        // ended, so no more than four run.
        tokio::time::advance(TIMEOUT * 20).await;
        assert!(!images.expire(Instant::now()));
        assert!(images.queued().is_empty());
        assert_eq!(images.busy(), CONCURRENT);
        for _ in 0..CONCURRENT {
            let (id, data) = reads.next().await;
            assert_eq!(data, Err(TransferError::TimedOut), "each ends by its deadline, connected or not");
            assert!(!images.finished(id, data));
            assert_eq!(images.busy(), reads.len(), "a slot is free only once its download has ended");
        }
        assert_eq!(images.busy(), 0);

        // Four asked for and never offered: nothing of them runs, so at their deadline they end
        // and free their slots; a late offer is ignored, and nothing connects for it.
        let start = images.queued();
        assert_eq!(start.len(), CONCURRENT);
        tokio::time::advance(TIMEOUT).await;
        assert!(images.expire(Instant::now()));
        assert_eq!(images.busy(), 0);
        assert_eq!(images.offered(start[0].0, 10), None);
        let failed: Vec<_> = start.iter().map(|(_, k)| k).collect();
        assert!(images.queued().iter().all(|(_, k)| !failed.contains(&k)), "not asked for again at once");
    }

    #[tokio::test(start_paused = true)]
    async fn an_offer_after_the_deadline_connects_nowhere() {
        let server: SocketAddr = "192.0.2.7:9987".parse().unwrap();
        let offer = tsclientlib::FileDownloadOffer { server, ip: None, port: 30033, key: "key".into(), size: 10 };
        let mut images = Images::new();
        let start = images.want(BTreeSet::from([ImageKey::Icon(1000), ImageKey::Icon(1001)]));
        // Past the deadline, before the tick expired them: both offers are ignored at once.
        tokio::time::advance(TIMEOUT).await;
        assert_eq!(images.accept(start[0].0, &offer), None);
        assert_eq!(images.offered(start[1].0, 10), None);
        assert_eq!(images.busy(), 0, "ended, nothing of them runs");
        // Timed out: tried again later, not now.
        assert!(images.queued().is_empty());
        tokio::time::advance(RETRY_DELAY).await;
        assert!(images.expire(Instant::now()));
        assert_eq!(images.queued().len(), 2);
        // And a download whose deadline has passed does not even start to connect.
        let never = async { panic!("connected") };
        let late = Instant::now();
        assert_eq!(
            download::<tokio::io::DuplexStream>(never, "key".into(), 10, ICON_LIMIT, late).await,
            Err(TransferError::TimedOut)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn timed_out_files_are_tried_again_a_few_times_later() {
        let mut images = Images::new();
        let key = ImageKey::Icon(1000);
        let mut start = images.want(BTreeSet::from([key.clone()]));
        let mut tries = 0;
        let mut wait = RETRY_DELAY;
        while !start.is_empty() {
            tries += 1;
            tokio::time::advance(TIMEOUT).await;
            assert!(images.expire(Instant::now()));
            // Not again at once, nor before its wait ...
            assert!(images.queued().is_empty());
            tokio::time::advance(wait - Duration::from_secs(1)).await;
            assert!(!images.expire(Instant::now()));
            assert!(images.queued().is_empty());
            // ... but after it, until RETRIES tries are done.
            tokio::time::advance(Duration::from_secs(1)).await;
            images.expire(Instant::now());
            start = images.queued();
            wait *= 5;
        }
        assert_eq!(tries, RETRIES);
        tokio::time::advance(Duration::from_secs(86_400)).await;
        assert!(!images.expire(Instant::now()));
        assert!(images.queued().is_empty(), "then never again while shown");
        // No longer shown and shown again, it is tried anew.
        images.want(BTreeSet::new());
        let again = images.want(BTreeSet::from([key.clone()]));
        assert_eq!(again.len(), 1);
        // A broken connection counts like a timeout; a file that was wrong is not tried again.
        let (id, _) = again[0].clone();
        images.offered(id, 10).unwrap();
        images.finished(id, Err(TransferError::Short));
        tokio::time::advance(RETRY_DELAY).await;
        assert!(images.expire(Instant::now()));
        let (id, _) = images.queued().pop().unwrap();
        images.offered(id, 10).unwrap();
        images.finished(id, Ok(b"not an image".to_vec()));
        tokio::time::advance(Duration::from_secs(86_400)).await;
        assert!(!images.expire(Instant::now()));
        assert!(images.queued().is_empty());
    }

    #[tokio::test]
    async fn downloads_end_with_the_connection() {
        let mut reads = Reads::default();
        let (client, mut server) = tokio::io::duplex(64);
        reads.start(1, std::future::ready(Ok(client)), "key".into(), 10, ICON_LIMIT, Instant::now() + TIMEOUT);
        tokio::task::yield_now().await;
        drop(reads);
        // The download's task is gone, and its end of the connection with it.
        let mut gone = false;
        for _ in 0..100 {
            tokio::task::yield_now().await;
            if tokio::io::AsyncWriteExt::write_all(&mut server, b"x").await.is_err() {
                gone = true;
                break;
            }
        }
        assert!(gone);
    }

    #[test]
    fn offers_connect_only_to_the_server_we_are_connected_to() {
        let server: SocketAddr = "192.0.2.7:9987".parse().unwrap();
        let address: SocketAddr = "192.0.2.7:30033".parse().unwrap();
        let here = Some(address);
        let to = |ip: Option<&str>| transfer_address(server, ip, 30033);
        for ip in [None, Some(""), Some(" "), Some("0.0.0.0"), Some("::"), Some("192.0.2.7"), Some("::ffff:192.0.2.7")]
        {
            assert_eq!(to(ip), here, "{ip:?}");
        }
        assert_eq!(to(Some("0.0.0.0, ::")), here);
        assert_eq!(to(Some("0.0.0.0,192.0.2.7")), here);
        for ip in
            ["192.0.2.8", "127.0.0.1", "10.0.0.1", "::1", "[::1]", "evil.example", "0.0.0.0,10.0.0.1", "192.0.2.7:80"]
        {
            assert_eq!(to(Some(ip)), None, "{ip}");
        }
        assert_eq!(transfer_address(server, None, 0), None);
        // A link-local peer keeps its interface.
        let scoped: SocketAddr = "[fe80::1234%7]:9987".parse().unwrap();
        let to_scoped = transfer_address(scoped, Some("fe80::1234"), 30033).unwrap();
        assert_eq!(to_scoped, "[fe80::1234%7]:30033".parse::<SocketAddr>().unwrap());
        let std::net::SocketAddr::V6(to_scoped) = to_scoped else { panic!("IPv6") };
        assert_eq!((to_scoped.scope_id(), to_scoped.port()), (7, 30033));
        assert_eq!(transfer_address(scoped, None, 30033).map(|a| a.to_string()), Some("[fe80::1234%7]:30033".into()));
        let v6: SocketAddr = "[2001:db8::7]:9987".parse().unwrap();
        assert_eq!(transfer_address(v6, Some("2001:db8::7"), 30033), Some("[2001:db8::7]:30033".parse().unwrap()));
        assert_eq!(transfer_address(v6, Some("2001:db8::8"), 30033), None);

        // A foreign offer is ignored, so nothing connects, and that file is not asked for again
        // while it is shown; one without an ip connects to the server.
        let mut images = Images::new();
        let (foreign, near) = (ImageKey::Icon(1000), ImageKey::Icon(1001));
        let start = images.want(BTreeSet::from([foreign.clone(), near.clone()]));
        let offer = |ip: Option<&str>| tsclientlib::FileDownloadOffer {
            server,
            ip: ip.map(Into::into),
            port: 30033,
            key: "key".into(),
            size: 10,
        };
        assert_eq!(start.iter().map(|(_, k)| k).collect::<Vec<_>>(), [&foreign, &near]);
        assert_eq!(images.accept(start[0].0, &offer(Some("10.0.0.1"))), None);
        assert_eq!(images.busy(), 1, "its slot is free: nothing of it runs");
        assert_eq!(
            images.accept(start[1].0, &offer(None)).map(|(a, limit, _)| (a, limit)),
            Some((address, ICON_LIMIT))
        );
        assert_eq!(images.accept(start[1].0, &offer(Some("10.0.0.1"))), None, "offered already");
        assert_eq!(images.busy(), 1);
        assert!(images.finished(start[1].0, Ok(PNG.to_vec())));
        assert!(images.queued().is_empty());
        assert!(!images.expire(Instant::now() + Duration::from_secs(86_400)), "never due again");
    }

    #[test]
    fn a_full_cache_defers_an_image_until_room_can_be_made() {
        let mut images = Images::new();
        let fit = CACHE_BYTES / AVATAR_LIMIT as usize;
        let avatar = |n: usize| ImageKey::avatar(&n.to_be_bytes(), "aa").unwrap();
        let mut file = PNG.to_vec();
        file.resize(AVATAR_LIMIT as usize, 0);
        let all: BTreeSet<_> = (0..=fit).map(avatar).collect();
        assert_eq!(deliver(&mut images, all.clone(), &file), fit + 1);
        let missing: Vec<_> = all.iter().filter(|k| images.token(Some((*k).clone())).is_none()).cloned().collect();
        assert_eq!(missing.len(), 1, "one more than fits");
        // While every image is still shown there is no room: not asked for again ...
        assert!(images.want(all.clone()).is_empty());
        assert!(images.queued().is_empty());
        // ... but it did not fail: once the others leave, it is.
        let last = BTreeSet::from([missing[0].clone()]);
        assert_eq!(deliver(&mut images, last, &file), 1);
        assert!(images.token(Some(missing[0].clone())).is_some());
    }

    #[test]
    fn failed_files_are_not_asked_for_again_however_many_are_shown() {
        let mut images = Images::new();
        let wanted: BTreeSet<_> = (1000..1000 + 3 * WANTED_LIMIT as u32).map(ImageKey::Icon).collect();
        let mut asked = 0;
        let mut start = images.want(wanted.clone());
        while !start.is_empty() {
            for (id, _) in start {
                asked += 1;
                images.refused(id);
            }
            start = images.queued();
        }
        assert_eq!(asked, WANTED_LIMIT, "only the first WANTED_LIMIT are considered");
        assert!(images.want(wanted.clone()).is_empty(), "and none of them again");
        assert!(images.queued().is_empty());
        // One no longer shown is forgotten; shown again, it is asked for once more.
        let first = ImageKey::Icon(1000);
        images.want(wanted.iter().filter(|k| **k != first).cloned().collect());
        assert_eq!(images.want(wanted).into_iter().map(|(_, k)| k).collect::<Vec<_>>(), vec![first]);
    }

    #[test]
    fn the_cache_stays_within_its_budget() {
        let mut store = Store::default();
        let avatar = |n: u8| ImageKey::avatar(&[n], "aa").unwrap();
        let big = || vec![0u8; AVATAR_LIMIT as usize];
        let fit = CACHE_BYTES / AVATAR_LIMIT as usize;
        let wanted: BTreeSet<_> = (0..=fit as u8).map(avatar).collect();
        for n in 0..fit as u8 {
            assert!(store.insert(avatar(n), big(), &wanted));
        }
        // Full of images still shown: the next one is refused.
        assert!(!store.insert(avatar(fit as u8), big(), &wanted));
        assert!(store.bytes <= CACHE_BYTES);
        // Images no longer shown make room.
        let fewer: BTreeSet<_> = (1..=fit as u8).map(avatar).collect();
        assert!(store.insert(avatar(fit as u8), big(), &fewer));
        assert!(!store.has(&avatar(0)) && store.has(&avatar(1)));
        assert!(store.bytes <= CACHE_BYTES);
    }

    #[tokio::test]
    async fn files_are_read_whole_within_their_limit() {
        let later = Instant::now() + TIMEOUT;
        /// Downloads `size` bytes from a server that, keyed `key`, sends `file`, then closes if `close`.
        async fn from(
            file: &'static [u8],
            size: u64,
            deadline: Instant,
            close: bool,
        ) -> Result<Vec<u8>, TransferError> {
            let (client, mut server) = tokio::io::duplex(1024);
            let serve = async move {
                let mut key = [0u8; 3];
                server.read_exact(&mut key).await.unwrap();
                assert_eq!(&key, b"key", "the key comes first");
                server.write_all(file).await.unwrap();
                (!close).then_some(server)
            };
            let (data, _server) = tokio::join!(download(async { Ok(client) }, "key".into(), size, 10, deadline), serve);
            data
        }
        assert_eq!(from(b"abcdef", 6, later, false).await, Ok(b"abcdef".to_vec()));
        assert_eq!(from(b"abc", 6, later, true).await, Err(TransferError::Short));
        let soon = Instant::now() + Duration::from_millis(50);
        assert_eq!(from(b"abc", 6, soon, false).await, Err(TransferError::TimedOut));
        // What follows the announced bytes is not read.
        assert_eq!(from(b"abcdefgh", 6, later, false).await, Ok(b"abcdef".to_vec()));
        // Too large or empty: nowhere connected to.
        let never = async { panic!("connected") };
        assert_eq!(
            download::<tokio::io::DuplexStream>(never, "key".into(), 11, 10, later).await,
            Err(TransferError::Size)
        );
        let never = async { panic!("connected") };
        assert_eq!(
            download::<tokio::io::DuplexStream>(never, "key".into(), 0, 10, later).await,
            Err(TransferError::Size)
        );
        // A refused connection, and a server that never answers.
        let refused = async { Err(std::io::Error::from(std::io::ErrorKind::ConnectionRefused)) };
        assert_eq!(
            download::<tokio::io::DuplexStream>(refused, "key".into(), 6, 10, later).await,
            Err(TransferError::Failed)
        );
        let soon = Instant::now() + Duration::from_millis(20);
        assert_eq!(
            download(std::future::pending::<std::io::Result<tokio::io::DuplexStream>>(), "key".into(), 6, 10, soon)
                .await,
            Err(TransferError::TimedOut)
        );
    }
}
