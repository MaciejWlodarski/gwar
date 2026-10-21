//! Icons and avatars of a TeamSpeak server, for TeamSpeak mode.
//!
//! TeamSpeak clients fetch them through the server's file transfer, from channel 0: a group's,
//! channel's or the server's icon is `/icon_<id>`, a client's avatar `/avatar_<uid>` (the uid
//! spelled as [`avatar_name`] does). The adapter does the same, with tight bounds: a size limit
//! per kind, [`CONCURRENT`] transfers at a time, a [`TIMEOUT`] per transfer, and a per-connection
//! cache of at most [`CACHE_BYTES`] in memory, nothing on disk. An icon is keyed by its id and an
//! avatar by its owner's uid and avatar hash (`client_flag_avatar`), so an image is fetched again
//! only when that changes. Every file must be a PNG, JPEG, GIF or WebP image by its first bytes
//! ([`sniff`]); nothing else is kept, SVG included, whatever the server says the file is.
//!
//! The proto shapes carry a token per image ([`ImageKey::token`]) once it is here, never before,
//! and the UI asks the connection for the bytes by token (`TsHandle::image`).

use std::{
    collections::{BTreeSet, HashMap},
    sync::{Arc, Mutex},
    time::Duration,
};

use sha1::{Digest, Sha1};
use tokio::{io::AsyncReadExt, time::Instant};

/// The largest icon kept. TeamSpeak's icons are 16×16 px, a few hundred bytes.
pub const ICON_LIMIT: u64 = 64 * 1024;
/// The largest avatar kept. TeamSpeak's client uploads avatars of at most 300×300 px.
pub const AVATAR_LIMIT: u64 = 300 * 1024;
/// Transfers at a time; the rest wait for a free one.
pub const CONCURRENT: usize = 4;
/// From asking for a file to having all of it.
pub const TIMEOUT: Duration = Duration::from_secs(15);
/// All images of a connection together, in bytes.
pub const CACHE_BYTES: usize = 32 << 20;
/// Images of a connection, at most.
pub const CACHE_ENTRIES: usize = 2048;
/// Files that failed are not asked for again; past this many, the list starts over.
const FAILED_ENTRIES: usize = 4096;
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

/// Reads a file of `size` bytes TeamSpeak announced from its transfer `stream`: all of it, and
/// at most `limit`, within [`TIMEOUT`].
pub async fn read_file(
    stream: impl tokio::io::AsyncRead + Unpin,
    size: u64,
    limit: u64,
) -> Result<Vec<u8>, &'static str> {
    read_within(stream, size, limit, TIMEOUT).await
}

async fn read_within(
    stream: impl tokio::io::AsyncRead + Unpin,
    size: u64,
    limit: u64,
    time: Duration,
) -> Result<Vec<u8>, &'static str> {
    if size == 0 || size > limit {
        return Err("too large or empty");
    }
    let mut data = Vec::with_capacity(size as usize);
    // Exactly the announced bytes: TeamSpeak may keep the connection open after them.
    let mut file = stream.take(size);
    match tokio::time::timeout(time, file.read_to_end(&mut data)).await {
        Err(_) => Err("timed out"),
        Ok(Err(_)) => Err("transfer failed"),
        Ok(Ok(_)) if data.len() as u64 != size => Err("not the announced size"),
        Ok(Ok(_)) => Ok(data),
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

/// A transfer under way.
struct Flight {
    key: ImageKey,
    /// The `ftinitdownload` command's answer, until it comes.
    command: Option<tsclientlib::MessageHandle>,
    since: Instant,
    /// The file is being read.
    reading: bool,
}

/// What the adapter knows of a connection's images: the ones ready (in the shared [`Store`]),
/// the transfers under way, and the files that failed.
pub struct Images {
    store: Arc<Mutex<Store>>,
    wanted: BTreeSet<ImageKey>,
    flights: HashMap<u16, Flight>,
    failed: BTreeSet<ImageKey>,
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
            failed: BTreeSet::new(),
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

    /// The images the view shows now; the transfers to start for them (by transfer id).
    pub fn want(&mut self, wanted: BTreeSet<ImageKey>) -> Vec<(u16, ImageKey)> {
        self.wanted = wanted;
        self.queued()
    }

    /// Transfers to start, as free ones allow: images wanted, not here, not under way and not failed.
    pub fn queued(&mut self) -> Vec<(u16, ImageKey)> {
        let free = CONCURRENT.saturating_sub(self.flights.len());
        let store = self.lock();
        let busy: BTreeSet<&ImageKey> = self.flights.values().map(|f| &f.key).collect();
        let start: Vec<ImageKey> = self
            .wanted
            .iter()
            .filter(|k| !store.has(k) && !busy.contains(k) && !self.failed.contains(k))
            .take(free)
            .cloned()
            .collect();
        drop(store);
        start
            .into_iter()
            .map(|key| {
                // Ids are ours alone: tsclientlib numbers only the transfers it starts itself.
                self.next_id = self.next_id.wrapping_add(1);
                while self.flights.contains_key(&self.next_id) {
                    self.next_id = self.next_id.wrapping_add(1);
                }
                let id = self.next_id;
                self.flights
                    .insert(id, Flight { key: key.clone(), command: None, since: Instant::now(), reading: false });
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

    /// TeamSpeak answered a command; true if it was one of ours. A refusal fails the transfer.
    pub fn answered(&mut self, command: tsclientlib::MessageHandle, ok: bool) -> bool {
        let Some((&id, flight)) = self.flights.iter_mut().find(|(_, f)| f.command == Some(command)) else {
            return false;
        };
        flight.command = None;
        if !ok {
            self.fail(id);
        }
        true
    }

    /// TeamSpeak opened transfer `id` for a file of `size` bytes: the most to read, or `None` if
    /// it is not ours or too large (then it failed).
    pub fn opened(&mut self, id: u16, size: u64) -> Option<u64> {
        let flight = self.flights.get_mut(&id)?;
        let limit = flight.key.limit();
        if size == 0 || size > limit || flight.reading {
            self.fail(id);
            return None;
        }
        flight.reading = true;
        Some(limit)
    }

    /// Transfer `id` failed (refused, missing file, closed connection).
    pub fn fail(&mut self, id: u16) {
        if let Some(flight) = self.flights.remove(&id) {
            self.failed(flight.key);
        }
    }

    fn failed(&mut self, key: ImageKey) {
        if self.failed.len() >= FAILED_ENTRIES {
            self.failed.clear();
        }
        self.failed.insert(key);
    }

    /// Transfer `id` ended with `data`: kept if it is an image and fits. True if the view changes.
    pub fn finished(&mut self, id: u16, data: Result<Vec<u8>, &'static str>) -> bool {
        let Some(Flight { key, reading: true, .. }) = self.flights.remove(&id) else { return false };
        let data = match data {
            Ok(data) if data.len() as u64 > key.limit() => Err("too large"),
            Ok(data) if sniff(&data).is_none() => Err("not a PNG, JPEG, GIF or WebP image"),
            other => other,
        };
        let kept = match data {
            Ok(data) => {
                let kept = self.lock().insert(key.clone(), data, &self.wanted);
                if !kept {
                    tracing::debug!(path = %key.path(), "no room for another TeamSpeak image");
                }
                kept
            }
            Err(error) => {
                tracing::debug!(path = %key.path(), error, "TeamSpeak image refused");
                false
            }
        };
        if !kept {
            self.failed(key);
            return false;
        }
        self.wanted.contains(&key)
    }

    /// Fails transfers older than [`TIMEOUT`] (and a little more, for the reading that has its
    /// own); true if any did, so others can start.
    pub fn expire(&mut self, now: Instant) -> bool {
        let old: Vec<u16> = self
            .flights
            .iter()
            .filter(|(_, f)| now.saturating_duration_since(f.since) > TIMEOUT * 2)
            .map(|(&id, _)| id)
            .collect();
        for &id in &old {
            self.fail(id);
        }
        !old.is_empty()
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
        assert_eq!(images.opened(first, PNG.len() as u64), Some(ICON_LIMIT));
        assert!(images.finished(first, Ok(PNG.to_vec())));
        assert_eq!(images.token(Some(key.clone())), Some(key.token()));
        assert_eq!(images.store().lock().unwrap().get(&key.token()).as_deref(), Some(PNG));
        let next = images.queued();
        assert_eq!(next.len(), 1);
        assert!(next.iter().all(|(_, k)| *k != key));

        // Refused by TeamSpeak, too large, not an image, timed out: failed, and not asked for again.
        let (refused, _) = started[1].clone();
        images.sent(refused, handle(2));
        assert!(images.answered(handle(2), false));
        let (large, _) = started[2].clone();
        assert_eq!(images.opened(large, ICON_LIMIT + 1), None);
        let (svg, svg_key) = started[3].clone();
        assert!(images.opened(svg, 10).is_some());
        assert!(!images.finished(svg, Ok(b"<svg onload=alert(1)>".to_vec())));
        assert_eq!(images.token(Some(svg_key)), None);
        let (late, _) = next[0].clone();
        assert!(!images.expire(Instant::now()));
        assert!(images.expire(Instant::now() + TIMEOUT * 3));
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
            images.fail(id);
        }
        assert_eq!(images.queued(), vec![(images.next_id, ImageKey::Icon(2000))]);
        // A transfer TeamSpeak opens twice, or never asked for, is refused.
        assert_eq!(images.opened(images.next_id, 10), Some(ICON_LIMIT));
        assert_eq!(images.opened(images.next_id, 10), None);
        assert_eq!(images.opened(4242, 10), None);
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
        assert_eq!(read_file(&b"abcdef"[..], 6, 10).await, Ok(b"abcdef".to_vec()));
        assert_eq!(read_file(&b"abc"[..], 6, 10).await, Err("not the announced size"));
        // What follows the announced bytes is not read.
        assert_eq!(read_file(&b"abcdefgh"[..], 6, 10).await, Ok(b"abcdef".to_vec()));
        assert_eq!(read_file(&b"abcdef"[..], 11, 10).await, Err("too large or empty"));
        assert_eq!(read_file(&b""[..], 0, 10).await, Err("too large or empty"));
        // A server that never finishes sending.
        let (_tx, rx) = tokio::io::duplex(64);
        assert_eq!(read_within(rx, 6, 10, Duration::from_millis(20)).await, Err("timed out"));
    }
}
