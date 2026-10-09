//! Client identities: an Ed25519 key pair generated and kept by the client.
//!
//! There are no central accounts. A user *is* their key, like a TeamSpeak
//! identity, so any self-hosted server can recognise returning users and
//! attach groups to them without a third party.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use rand::RngCore;
use sha2::{Digest, Sha256};
use vc_proto::{Uid, challenge_message};

pub fn new_nonce() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Stable, printable user id: first 20 bytes of SHA-256 over the raw public key.
pub fn uid_for_key(public_key: &[u8; 32]) -> Uid {
    URL_SAFE_NO_PAD.encode(&Sha256::digest(public_key)[..20])
}

/// Verifies a `hello` signature and returns the identity's uid.
pub fn verify_hello(nonce: &str, public_key: &str, signature: &str) -> Option<Uid> {
    let key: [u8; 32] = URL_SAFE_NO_PAD.decode(public_key).ok()?.try_into().ok()?;
    let signature: [u8; 64] = URL_SAFE_NO_PAD.decode(signature).ok()?.try_into().ok()?;
    let verifying = VerifyingKey::from_bytes(&key).ok()?;
    verifying.verify(&challenge_message(nonce, public_key), &Signature::from_bytes(&signature)).ok()?;
    Some(uid_for_key(&key))
}

#[cfg(test)]
pub mod test_support {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    pub struct TestIdentity(pub SigningKey);

    impl TestIdentity {
        pub fn generate() -> Self {
            let mut seed = [0u8; 32];
            rand::rng().fill_bytes(&mut seed);
            Self(SigningKey::from_bytes(&seed))
        }

        pub fn public_key(&self) -> String {
            URL_SAFE_NO_PAD.encode(self.0.verifying_key().as_bytes())
        }

        pub fn sign(&self, nonce: &str) -> String {
            let message = challenge_message(nonce, &self.public_key());
            URL_SAFE_NO_PAD.encode(self.0.sign(&message).to_bytes())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::TestIdentity;
    use super::*;

    #[test]
    fn accepts_valid_and_rejects_replayed_signature() {
        let id = TestIdentity::generate();
        let nonce = new_nonce();
        let signature = id.sign(&nonce);
        let uid = verify_hello(&nonce, &id.public_key(), &signature).expect("valid");
        assert_eq!(uid.len(), 27);
        assert!(verify_hello(&new_nonce(), &id.public_key(), &signature).is_none());
        let other = TestIdentity::generate();
        assert!(verify_hello(&nonce, &other.public_key(), &signature).is_none());
        assert!(verify_hello(&nonce, "not-base64!", &signature).is_none());
    }
}
