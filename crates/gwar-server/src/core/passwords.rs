//! Channel/server password hashing. Argon2id is deliberately slow, so callers
//! run these on the blocking pool, never on the core actor itself.

use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};

pub fn hash(password: &str) -> String {
    let salt: [u8; 16] = rand::random();
    let salt = SaltString::encode_b64(&salt).expect("16 bytes is a valid salt length");
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .expect("argon2 with default params cannot fail")
        .to_string()
}

/// The form TeamSpeak clients send instead of the plain password.
///
/// Channel and server passwords are always stored as a hash of this form so
/// that a password set from a TeamSpeak client (which never reveals the plain
/// text) and one set from our clients are interchangeable.
pub fn ts_wire_form(password: &str) -> String {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use sha1::{Digest, Sha1};
    STANDARD.encode(Sha1::digest(password.as_bytes()))
}

/// Normalizes a submitted password: TeamSpeak sessions already send the wire form.
pub fn to_wire_form(submitted: &str, teamspeak: bool) -> String {
    if teamspeak { submitted.to_owned() } else { ts_wire_form(submitted) }
}

/// Hashes a submitted channel/server password (see [`to_wire_form`]).
pub fn hash_secret(submitted: &str, teamspeak: bool) -> String {
    hash(&to_wire_form(submitted, teamspeak))
}

pub fn verify_secret(submitted: &str, stored: &str, teamspeak: bool) -> bool {
    verify(&to_wire_form(submitted, teamspeak), stored)
}

pub fn verify(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|parsed| Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok())
}

#[cfg(test)]
mod tests {
    #[test]
    fn round_trip() {
        let h = super::hash("sekret");
        assert!(super::verify("sekret", &h));
        assert!(!super::verify("Sekret", &h));
        assert!(!super::verify("sekret", "garbage"));
    }

    #[test]
    fn teamspeak_wire_form() {
        // base64(sha1("sekret")), as TS3 sends in `clientmove cpw=`.
        assert_eq!(super::ts_wire_form("sekret"), "obmJJhGVaqE6WrnM8B9JZiWD8tI=");
        let stored = super::hash_secret("sekret", false);
        assert!(super::verify_secret("sekret", &stored, false));
        assert!(super::verify_secret(&super::ts_wire_form("sekret"), &stored, true));
        assert!(!super::verify_secret("sekret", &stored, true));
        let from_ts = super::hash_secret(&super::ts_wire_form("x"), true);
        assert!(super::verify_secret("x", &from_ts, false));
    }
}
