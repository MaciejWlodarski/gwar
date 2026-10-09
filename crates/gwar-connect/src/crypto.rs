//! Encodings and the signed statements of docs/connect.md.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, VerifyingKey};

/// Device certificates last at most this long.
pub const MAX_CERT_MS: i64 = 400 * 24 * 3600 * 1000;
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
    if issued_at > now + SKEW_MS || expires_at <= now || expires_at - issued_at > MAX_CERT_MS {
        return Err("certificate dates are out of range");
    }
    if !verify(account_key, &device_statement(account_key, device_key, issued_at, expires_at), signature) {
        return Err("certificate signature does not match the account key");
    }
    Ok(())
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
    fn handles_are_normalized_and_restricted() {
        assert_eq!(handle("@Maciej"), Some("maciej".into()));
        assert_eq!(handle("a.b_c9"), Some("a.b_c9".into()));
        assert_eq!(handle("ab"), None);
        assert_eq!(handle("has space"), None);
        assert_eq!(handle(".dot"), None);
        assert_eq!(handle("ąę"), None);
    }
}
