//! Client identities: an Ed25519 key pair generated and kept by the client.
//!
//! There are no central accounts. A user *is* their key, like a TeamSpeak
//! identity, so any self-hosted server can recognise returning users and
//! attach groups to them without a third party.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, VerifyingKey};
use gwar_proto::{DeviceCertificate, ErrorCode, Hello, Uid, certificate_hash, decode_fixed, hello_statement};
pub use gwar_proto::{device_statement, signed_by, uid_for_key};
use rand::RngCore;

pub fn valid_hash(value: &str, bytes: usize) -> bool {
    URL_SAFE_NO_PAD.decode(value).is_ok_and(|raw| raw.len() == bytes && URL_SAFE_NO_PAD.encode(raw) == value)
}

pub fn new_nonce() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Verifies a `hello` signature and returns the identity's uid.
pub fn verify_hello(hello: &Hello) -> Option<Uid> {
    let key = decode_fixed::<32>(&hello.client_signing_pk)?;
    let signature = decode_fixed::<64>(&hello.signature)?;
    let verifying = VerifyingKey::from_bytes(&key).ok()?;
    verifying.verify_strict(hello_statement(hello)?.as_bytes(), &Signature::from_bytes(&signature)).ok()?;
    Some(uid_for_key(&key))
}

/// Device certificates last at most this long (docs/connect.md).
const MAX_CERT_MS: i64 = 90 * 24 * 3600 * 1000;
const SKEW_MS: i64 = 5 * 60 * 1000;

pub fn revoke_statement(account_key: &str, device_key: &str, revoked_at: i64) -> String {
    format!("gwar revoke v1\n{account_key}\n{device_key}\n{revoked_at}")
}

/// Checks a Gwar Connect device certificate for the device key that signed
/// `hello`; the identity is then the account's: returns its uid.
pub fn verify_device(c: &DeviceCertificate, public_key: &str, now: i64) -> Result<Uid, (ErrorCode, &'static str)> {
    let invalid = |message| (ErrorCode::NotAuthenticated, message);
    if c.device_key != public_key {
        return Err(invalid("the certificate is for another device"));
    }
    if certificate_hash(Some(c)).is_none()
        || c.issued_at > now + SKEW_MS
        || c.expires_at.saturating_sub(c.issued_at) > MAX_CERT_MS
    {
        return Err(invalid("the device certificate is not valid"));
    }
    if !signed_by(&c.account_key, &device_statement(c), &c.signature) {
        return Err(invalid("the device certificate is not signed by the account"));
    }
    // Checked after the signature, so only a genuine certificate is reported as expired.
    if c.expires_at <= now {
        return Err((ErrorCode::CertificateExpired, "the device certificate has expired; renew it in Gwar Connect"));
    }
    let key: [u8; 32] =
        URL_SAFE_NO_PAD.decode(&c.account_key).ok().and_then(|k| k.try_into().ok()).ok_or(invalid("bad key"))?;
    Ok(uid_for_key(&key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uid_vectors_match_web() {
        let ascending = std::array::from_fn(|i| i as u8);
        for (key, expected) in [
            ([0; 32], "Zmh6rfhivXdsj8GLjp-OIAiXFIU"),
            ([1; 32], "cs1uhCLEB_ttCYaQ8RMLfe1-wvc"),
            (ascending, "Yw3NKWbEM2aRElRIu7JbT_QSpJw"),
        ] {
            let uid = uid_for_key(&key);
            println!("{} {uid}", URL_SAFE_NO_PAD.encode(key));
            assert_eq!(uid, expected);
        }
    }
}
