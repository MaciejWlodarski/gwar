//! The TeamSpeak identity as text: what the official client exports, what the
//! Gwar Connect vault stores (docs/connect.md) and the files the desktop app
//! keeps. Pure helpers, so they are tested without a TeamSpeak server.
//!
//! The text form is `<counter>V<obfuscated key>`, the value of `identity="…"`
//! in an identity `.ini` exported by the TeamSpeak client.

use std::{io::Write, path::Path};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use sha1::{Digest, Sha1};
pub use tsclientlib::Identity;
use tsproto_types::crypto::EccKeyPrivP256;

/// Most servers require a security level of at least 8 to connect.
pub const MIN_LEVEL: u8 = 8;

/// A new identity at level [`MIN_LEVEL`], one whose text form reads back.
pub fn generate() -> Identity {
    loop {
        let identity = Identity::create();
        if parse_any(&export(&identity)).is_ok() {
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
        .ok()
        .or_else(|| decode_short_scalar(candidate))
        .ok_or_else(|| anyhow!("this is not a TeamSpeak identity (expected <number>V<key>)"))
}

/// The key TeamSpeak XORs into the first 100 bytes of an exported identity
/// (the same constant as in tsproto; it is not public there).
const OBFUSCATION: &[u8; 100] =
    b"b9dfaa7bee6ac57ac7b65f1094a1c155e747327bc2fe5d51c512023fe54a280201004e90ad1daaae1075d53b7d571c30e063";

/// Decodes `<counter>V<obfuscated key>` like tsproto, except that a private
/// scalar shorter than 32 bytes is padded. DER drops leading zero bytes, so one
/// identity in 256, including ones the official client made, has such a
/// scalar, and tsproto refuses it.
fn decode_short_scalar(text: &str) -> Option<Identity> {
    let (counter, key) = text.split_once('V')?;
    let counter = counter.parse().ok()?;
    let mut data = BASE64.decode(key).ok()?;
    if data.len() < 20 {
        return None;
    }
    // XOR the first 20 bytes with the SHA-1 of what follows them up to the first zero byte.
    let end = data[20..].iter().position(|b| *b == 0).map_or(data.len(), |p| 20 + p);
    let hash = Sha1::digest(&data[20..end]);
    data.iter_mut().zip(hash.iter()).for_each(|(b, h)| *b ^= h);
    data.iter_mut().zip(OBFUSCATION.iter()).for_each(|(b, o)| *b ^= o);
    let der = BASE64.decode(std::str::from_utf8(&data).ok()?).ok()?;
    let scalar = tomcrypt_private_scalar(&der)?;
    if scalar.len() > 32 {
        return None;
    }
    let mut padded = [0u8; 32];
    padded[32 - scalar.len()..].copy_from_slice(scalar);
    Some(Identity::new(EccKeyPrivP256::from_short(&padded).ok()?, counter))
}

/// The private scalar of a libtomcrypt key: `SEQUENCE { BIT STRING flags,
/// INTEGER size, INTEGER x, INTEGER y, INTEGER private }`, or, with two flag
/// bits (TS3AudioBot), `SEQUENCE { BIT STRING, INTEGER size, INTEGER private }`.
fn tomcrypt_private_scalar(der: &[u8]) -> Option<&[u8]> {
    let (tag, sequence, rest) = der_element(der)?;
    if tag != 0x30 || !rest.is_empty() {
        return None;
    }
    let (tag, flags, mut rest) = der_element(sequence)?;
    let (&unused, bits) = flags.split_first()?;
    if tag != 0x03 || bits.first()? & 0x80 == 0 {
        return None;
    }
    let private_index = match bits.len() * 8 - unused as usize {
        1 => 3,
        2 => 1,
        _ => return None,
    };
    let mut integers = Vec::new();
    while !rest.is_empty() {
        let (tag, content, next) = der_element(rest)?;
        if tag != 0x02 {
            return None;
        }
        integers.push(content);
        rest = next;
    }
    let scalar = *integers.get(private_index)?;
    // A positive DER integer may carry a leading zero byte for its sign.
    Some(scalar.iter().position(|b| *b != 0).map_or(&scalar[scalar.len()..], |start| &scalar[start..]))
}

/// One DER element: its tag, its content and what follows it.
fn der_element(data: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, data) = data.split_first()?;
    let (&first, mut data) = data.split_first()?;
    let len = if first < 0x80 {
        first as usize
    } else {
        let count = (first & 0x7f) as usize;
        if count == 0 || count > 4 || data.len() < count {
            return None;
        }
        let (bytes, rest) = data.split_at(count);
        data = rest;
        bytes.iter().fold(0usize, |len, b| (len << 8) | *b as usize)
    };
    (data.len() >= len).then(|| (tag, &data[..len], &data[len..]))
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

/// What a claim signs (docs/teamspeak-import.md, "Claim"); [`sign_claim`] signs nothing else.
const CLAIM_PREFIX: &str = "gwar teamspeak claim v1\n";

/// Signs a Gwar server's claim statement with the identity's key, the way a TeamSpeak client signs:
/// returns the public key in TeamSpeak's form (base64 libtomcrypt DER) and the signature (base64
/// DER ECDSA). Only `gwar teamspeak claim v1\n{server_key}\n{uid}\n{nonce}` with plain base64url
/// fields is signed, so the key cannot be made to sign anything else.
pub fn sign_claim(identity: &Identity, statement: &str) -> Result<(String, String)> {
    let fields = statement.strip_prefix(CLAIM_PREFIX).context("not a claim statement")?;
    let fields: Vec<&str> = fields.split('\n').collect();
    let plain = |f: &&str| {
        !f.is_empty() && f.len() <= 128 && f.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    };
    if fields.len() != 3 || !fields.iter().all(plain) {
        bail!("not a claim statement");
    }
    let key = identity.key().to_pub().to_ts();
    let signature = identity.key().clone().sign(statement.as_bytes());
    Ok((key, BASE64.encode(signature)))
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

    #[test]
    fn a_claim_is_signed_verifiably_and_nothing_else_is() {
        let identity = generate();
        let statement = "gwar teamspeak claim v1\nU2VydmVyS2V5\nAbCd-_09\nbm9uY2U";
        let (key, signature) = sign_claim(&identity, statement).unwrap();
        let public = tsproto_types::crypto::EccKeyPubP256::from_ts(&key).unwrap();
        assert_eq!(public.get_uid(), uid(&identity));
        public.verify(statement.as_bytes(), &BASE64.decode(signature).unwrap()).unwrap();
        for other in [
            "gwar teamspeak claim v2\na\nb\nc",
            "gwar teamspeak claim v1\na\nb",
            "gwar teamspeak claim v1\na\nb\nc\nd",
            "gwar teamspeak claim v1\na\nb\n",
            "gwar teamspeak claim v1\na b\nb\nc",
            "gwar teamspeak claim v1\na=\nb\nc",
            "clientinitiv alpha=x",
        ] {
            assert!(sign_claim(&identity, other).is_err(), "{other:?}");
        }
        assert!(sign_claim(&identity, &format!("gwar teamspeak claim v1\n{}\nb\nc", "a".repeat(129))).is_err());
    }

    /// The web app signs claims itself (`apps/web/src/connect/ts-key.ts`); its signature for this
    /// identity and statement (deterministic, RFC 6979) passes the server's check.
    #[test]
    fn a_claim_signed_in_the_browser_verifies() {
        let identity = parse("10VdE9XURaL9YZFTbfGpp0+S8X4YKpvVlVXUXItaTpbYwdTTX1/QGRtKFdwB1VnJ1NDFGZaQ3tQWywufgAie3paYglfeVdTK0hTElddERQPJFIGVFJlN2RyIX0+DVN5MkFIE3FGZlJDTWNBaUJTMEI2M0diYzgxWkh5SnltNmpzRXBOcEUvdVN3K1JQdkJCTFVXZ04wbU9BPT0=").unwrap();
        let statement = "gwar teamspeak claim v1\nU2VydmVyS2V5\nAbCd-_09\nbm9uY2U";
        let key =
            "MEwDAgcAAgEgAiEAiNaZZ0dghFLXYjV26zIHsVZJ4Ba0RCfrwSkqKbhJKK4CIBjP9nIggNqcs3luunE77dePSQACJZ8dHQrxvApURCMc";
        let signature =
            "MEQCIGHD38YdCiiiE7yaXNPC0N9J7DXGjaWW3W6nVcJhatzAAiBlHxiRTattwKQHVAxa19/2bL64Y6W2/GXpaF1am//rUg==";
        let public = tsproto_types::crypto::EccKeyPubP256::from_ts(key).unwrap();
        assert_eq!(public.get_uid(), uid(&identity));
        public.verify(statement.as_bytes(), &BASE64.decode(signature).unwrap()).unwrap();
    }

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
    fn keys_whose_scalar_starts_with_zero_survive_the_text_form() {
        let identity = std::iter::repeat_with(Identity::create).find(|i| i.key().to_short()[0] == 0).unwrap();
        assert!(Identity::new_from_ts_str(&export(&identity)).is_err(), "tsproto alone refuses it");
        let back = parse(&export(&identity)).unwrap();
        assert_eq!(uid(&back), uid(&identity));
        assert_eq!(back.counter(), identity.counter());
        // The regular path and the fallback agree on ordinary keys too.
        let ordinary = fresh();
        assert_eq!(uid(&decode_short_scalar(&export(&ordinary)).unwrap()), uid(&ordinary));
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

}
