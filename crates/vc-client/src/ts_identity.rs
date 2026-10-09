//! The TeamSpeak identity as text: what the official client exports, what the
//! Gwar Connect vault stores (docs/connect.md) and the files the desktop app
//! keeps. Pure helpers, so they are tested without a TeamSpeak server.
//!
//! The text form is `<counter>V<obfuscated key>`, the value of `identity="…"`
//! in an identity `.ini` exported by the TeamSpeak client.

use std::{io::Write, path::Path};

use anyhow::{Context, Result, anyhow, bail};
pub use tsclientlib::Identity;

/// Most servers require a security level of at least 8 to connect.
pub const MIN_LEVEL: u8 = 8;

/// A new identity at level [`MIN_LEVEL`]. tsproto cannot read back a key whose private scalar starts with
/// a zero byte (one in 256: its DER form is a byte short), so such a key would be lost the first time it is
/// stored; this makes another one instead.
pub fn generate() -> Identity {
    loop {
        let identity = Identity::create();
        let stored = serde_json::to_vec(&identity).ok().and_then(|json| serde_json::from_slice::<Identity>(&json).ok());
        if stored.is_some() && parse_any(&export(&identity)).is_ok() {
            return identity;
        }
    }
}

/// `<counter>V<obfuscated key>`, as the TeamSpeak client exports it.
pub fn export(identity: &Identity) -> String {
    format!("{}V{}", identity.counter(), identity.key().to_ts_obfuscated())
}

/// The TeamSpeak unique id (base64 of the SHA-1 of the public key).
pub fn uid(identity: &Identity) -> String {
    identity.key().to_pub().get_uid()
}

/// The `identity="…"` value of an identity `.ini`; the value may be quoted.
fn ini_value(text: &str) -> Option<&str> {
    text.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        (name.trim() == "identity").then(|| value.trim().trim_matches('"').trim())
    })
}

/// Reads an identity from a bare `<counter>V<key>` string or from the whole
/// contents of an identity `.ini`, whatever its security level. Fails on anything else.
pub fn parse_any(text: &str) -> Result<Identity> {
    let text = text.trim();
    // The key itself ends in `=` padding, so only a line named `identity` counts as an .ini.
    let candidate = match ini_value(text) {
        Some(value) => value,
        None if text.contains('\n') => {
            bail!("no identity=\"…\" line found; paste the identity string or the whole .ini file")
        }
        None => text.trim_matches('"'),
    };
    if candidate.is_empty() {
        bail!("the identity is empty");
    }
    Identity::new_from_ts_str(candidate)
        .map_err(|_| anyhow!("this is not a TeamSpeak identity (expected <number>V<key>)"))
}

/// Like [`parse_any`], but also fails on identities below [`MIN_LEVEL`], which most servers would refuse.
pub fn parse(text: &str) -> Result<Identity> {
    let identity = parse_any(text)?;
    let level = identity.level();
    if level < MIN_LEVEL {
        bail!(
            "the identity has security level {level}, but most servers need at least {MIN_LEVEL}; raise it in the TeamSpeak client first"
        );
    }
    Ok(identity)
}

/// The identity stored at `path`, if there is a file.
pub fn read(path: &Path) -> Result<Option<Identity>> {
    match std::fs::read(path) {
        Ok(data) => Ok(Some(serde_json::from_slice(&data).context("invalid TeamSpeak identity file")?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Writes `identity` to `path` (private to the user), replacing what is there.
pub fn write(path: &Path, identity: &Identity) -> Result<()> {
    write_private(path, &serde_json::to_vec(identity)?)
}

/// Writes `data` to `path` readable by the user only, replacing what is there.
/// It goes through a temporary file, so a crash never leaves half a file.
pub(crate) fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = Path::new(&temp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(temp)?;
    file.write_all(data)?;
    file.sync_all()?;
    std::fs::rename(temp, path)?;
    Ok(())
}

/// A fresh key with a counter that gives it a level below [`MIN_LEVEL`] (a key reaches 8 at counter 0 once in 256).
#[cfg(test)]
pub(crate) fn low_level() -> Identity {
    let mut identity = generate();
    let mut low = 0;
    while {
        identity.set_counter(low);
        identity.level() >= MIN_LEVEL
    } {
        low += 1;
    }
    identity
}

/// A directory under the system temp dir that is removed when dropped.
#[cfg(test)]
pub(crate) struct TempDir(pub std::path::PathBuf);

#[cfg(test)]
impl TempDir {
    pub fn new(label: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "gwar-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    pub fn join(&self, name: &str) -> std::path::PathBuf {
        self.0.join(name)
    }
}

#[cfg(test)]
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Identity {
        generate()
    }

    #[test]
    fn generated_identities_survive_storing() {
        for _ in 0..400 {
            let identity = generate();
            assert!(identity.level() >= MIN_LEVEL);
            assert_eq!(uid(&parse(&export(&identity)).unwrap()), uid(&identity));
        }
    }

    #[test]
    fn export_then_parse_keeps_the_uid_and_level() {
        let identity = fresh();
        let text = export(&identity);
        assert!(text.contains('V'));
        let back = parse(&text).unwrap();
        assert_eq!(uid(&back), uid(&identity));
        assert_eq!(back.counter(), identity.counter());
        assert!(back.level() >= MIN_LEVEL);
        assert_eq!(export(&back), text);
    }

    #[test]
    fn reads_the_ini_of_the_official_client() {
        let identity = fresh();
        let text = export(&identity);
        let ini = format!("[Identity]\nid=Default\nidentity=\"{text}\"\nnickname=Me\nphonetic_nickname=\n");
        assert_eq!(uid(&parse(&ini).unwrap()), uid(&identity));
        // CRLF line endings, an unquoted value and spaces around the equals sign.
        let ini = format!("[Identity]\r\nid=Default\r\nidentity = {text}\r\n");
        assert_eq!(uid(&parse(&ini).unwrap()), uid(&identity));
        // A quoted bare string.
        assert_eq!(uid(&parse(&format!("  \"{text}\"\n")).unwrap()), uid(&identity));
    }

    #[test]
    fn rejects_garbage_with_a_clear_message() {
        for bad in ["", "   ", "hello", "12Vnot-a-key", "V", "x=1\ny=2", "[Identity]\nnickname=Me\n", "identity=\"\""] {
            assert!(parse(bad).is_err(), "{bad:?} should be rejected");
        }
        let message = parse("[Identity]\nnickname=Me\n").unwrap_err().to_string();
        assert!(message.contains("identity"), "{message}");
    }

    #[test]
    fn parse_any_accepts_a_low_level_identity() {
        let identity = low_level();
        let text = export(&identity);
        assert!(parse(&text).is_err());
        let back = parse_any(&text).unwrap();
        assert_eq!(uid(&back), uid(&identity));
        assert!(back.level() < MIN_LEVEL);
    }

    #[test]
    fn rejects_a_low_level_identity() {
        let identity = low_level();
        let message = parse(&export(&identity)).unwrap_err().to_string();
        assert!(message.contains("security level"), "{message}");
    }

    #[test]
    fn files_round_trip_and_are_private() {
        let dir = std::env::temp_dir().join(format!("gwar-ts-identity-{}", std::process::id()));
        let path = dir.join("identity.json");
        assert!(read(&path).unwrap().is_none());
        let first = fresh();
        write(&path, &first).unwrap();
        let second = fresh();
        write(&path, &second).unwrap();
        assert_eq!(uid(&read(&path).unwrap().unwrap()), uid(&second));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
