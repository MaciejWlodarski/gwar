//! Encodings and the signed statements of docs/connect.md.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::store::{Challenge, Kdf};

/// Device certificates last at most this long.
pub const MAX_CERT_MS: i64 = 90 * 24 * 3600 * 1000;
/// Clock skew tolerated on "issued at" and "revoked at".
pub const SKEW_MS: i64 = 5 * 60 * 1000;
/// AES-GCM nonce + 32-byte seed + tag.
pub const KEY_BLOB_LEN: usize = 12 + 32 + 16;
/// Largest vault (nonce, ciphertext and tag).
pub const MAX_VAULT_LEN: usize = 64 * 1024;

pub fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn unb64(text: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(text).ok()
}

/// Decodes exactly `len` bytes.
pub fn unb64_len(text: &str, len: usize) -> Option<Vec<u8>> {
    unb64(text).filter(|bytes| bytes.len() == len)
}

pub fn device_statement(account_key: &str, device_key: &str, issued_at: i64, expires_at: i64) -> String {
    format!("gwar device v1\n{account_key}\n{device_key}\n{issued_at}\n{expires_at}")
}

pub fn revoke_statement(account_key: &str, device_key: &str, revoked_at: i64) -> String {
    format!("gwar revoke v1\n{account_key}\n{device_key}\n{revoked_at}")
}

/// Whether `signature` is `public`'s Ed25519 signature of `message`.
pub fn verify(public: &str, message: &str, signature: &str) -> bool {
    let Some(public) = unb64_len(public, 32).and_then(|b| VerifyingKey::try_from(b.as_slice()).ok()) else {
        return false;
    };
    let Some(signature) = unb64_len(signature, 64).and_then(|b| Signature::from_slice(&b).ok()) else {
        return false;
    };
    public.verify_strict(message.as_bytes(), &signature).is_ok()
}

/// A device certificate's problem, if any, at time `now`.
pub fn check_certificate(
    account_key: &str,
    device_key: &str,
    issued_at: i64,
    expires_at: i64,
    signature: &str,
    now: i64,
) -> Result<(), &'static str> {
    if unb64_len(device_key, 32).is_none() {
        return Err("invalid device key");
    }
    if issued_at < 0
        || expires_at > 9_007_199_254_740_991
        || issued_at > now.saturating_add(SKEW_MS)
        || expires_at <= now
        || expires_at <= issued_at
        || expires_at - issued_at > MAX_CERT_MS
    {
        return Err("certificate dates are out of range");
    }
    if !verify(account_key, &device_statement(account_key, device_key, issued_at, expires_at), signature) {
        return Err("certificate signature does not match the account key");
    }
    Ok(())
}

pub fn hash(bytes: &[u8]) -> String {
    b64(&Sha256::digest(bytes))
}

pub fn certificate_hash(account: &str, device: &str, c: &crate::store::Certificate) -> String {
    hash(format!("{}\n{}", device_statement(account, device, c.issued_at, c.expires_at), c.signature).as_bytes())
}

pub fn session_statement(service: &str, account: &str, c: &Challenge) -> String {
    format!(
        "gwar session v1\n{service}\n{account}\n{}\n{}\n{}\n{}",
        c.certificate_hash, c.token_hash, c.nonce, c.expires_at
    )
}

pub fn kdf_hash(kdf: &Kdf) -> String {
    hash(format!("gwar kdf v1\nargon2id\n19\n{}\n{}\n{}\n{}\n64", kdf.salt, kdf.m, kdf.t, kdf.p).as_bytes())
}

pub fn credentials_statement(
    service: &str,
    account: &str,
    c: &Challenge,
    kdf_hash: &str,
    auth_hash: &str,
    blob_hash: &str,
) -> String {
    format!(
        "gwar credentials v1\n{}\n{service}\n{account}\n{}\n{}\n{}\n{}\n{kdf_hash}\n{auth_hash}\n{blob_hash}",
        c.purpose, c.token_hash, c.nonce, c.expires_at, c.credential_version
    )
}

/// Lowercased handle if valid: 3–32 of `a-z 0-9 _ .`, not starting or ending with a dot.
pub fn handle(raw: &str) -> Option<String> {
    let handle = raw.trim().trim_start_matches('@').to_lowercase();
    let ok = (3..=32).contains(&handle.len())
        && handle.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
        && !handle.starts_with('.')
        && !handle.ends_with('.');
    ok.then_some(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn certificate_lifetime_is_exactly_ninety_days() {
        use ed25519_dalek::{Signer, SigningKey};
        let account = SigningKey::from_bytes(&[1; 32]);
        let device = SigningKey::from_bytes(&[2; 32]);
        let account_key = b64(account.verifying_key().as_bytes());
        let device_key = b64(device.verifying_key().as_bytes());
        let now = 1_700_000_000_000;
        assert_eq!(MAX_CERT_MS, 90 * 86_400_000);
        for (length, accepted) in [(MAX_CERT_MS, true), (MAX_CERT_MS + 1, false), (400 * 86_400_000, false)] {
            let signature = b64(&account
                .sign(device_statement(&account_key, &device_key, now, now + length).as_bytes())
                .to_bytes());
            assert_eq!(
                check_certificate(&account_key, &device_key, now, now + length, &signature, now).is_ok(),
                accepted
            );
        }
    }

    #[test]
    fn handles_are_normalized_and_restricted() {
        assert_eq!(handle("@Maciej"), Some("maciej".into()));
        assert_eq!(handle("a.b_c9"), Some("a.b_c9".into()));
        assert_eq!(handle("ab"), None);
        assert_eq!(handle("has space"), None);
        assert_eq!(handle(".dot"), None);
        assert_eq!(handle("ąę"), None);
    }
}
