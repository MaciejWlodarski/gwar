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
/// contents of an identity `.ini`. Fails on anything else and on identities
/// below [`MIN_LEVEL`], which most servers would refuse.
pub fn parse(text: &str) -> Result<Identity> {
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
    let identity = Identity::new_from_ts_str(candidate)
        .map_err(|_| anyhow!("this is not a TeamSpeak identity (expected <number>V<key>)"))?;
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
/// It goes through a temporary file, so a crash never leaves half an identity.
pub fn write(path: &Path, identity: &Identity) -> Result<()> {
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
    file.write_all(&serde_json::to_vec(identity)?)?;
    file.sync_all()?;
    std::fs::rename(temp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Identity {
        let mut identity = Identity::create();
        identity.upgrade_level(MIN_LEVEL);
        identity
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
    fn rejects_a_low_level_identity() {
        // The same key with counter 0 has level 0 (a key at level 8 or more at counter 0 is a one in 256 chance).
        let mut identity = fresh();
        let mut low = 0;
        while {
            identity.set_counter(low);
            identity.level() >= MIN_LEVEL
        } {
            low += 1;
        }
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
