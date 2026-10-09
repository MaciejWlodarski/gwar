//! The user's Ed25519 identity, persisted as a 32-byte seed.

use std::{fs, io::Write, path::Path};

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signer, SigningKey};
use rand::RngCore;
use vc_proto::challenge_message;

#[derive(Clone)]
pub struct Identity(SigningKey);

impl Identity {
    pub fn generate() -> Self {
        let mut seed = [0u8; 32];
        rand::rng().fill_bytes(&mut seed);
        Self(SigningKey::from_bytes(&seed))
    }

    /// Loads the identity at `path`, creating it (mode 0600) on first use.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => Self::import(text.trim()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let identity = Self::generate();
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut options = fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let mut file = options.open(path).with_context(|| format!("create {}", path.display()))?;
                file.write_all(identity.export().as_bytes())?;
                file.sync_all()?;
                Ok(identity)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Portable text form (base64url seed) for backup and moving between devices.
    pub fn export(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0.to_bytes())
    }

    pub fn import(text: &str) -> Result<Self> {
        let bytes = URL_SAFE_NO_PAD.decode(text).context("identity is not base64url")?;
        let Ok(seed) = <[u8; 32]>::try_from(bytes.as_slice()) else { bail!("identity must be 32 bytes") };
        Ok(Self(SigningKey::from_bytes(&seed)))
    }

    pub fn public_key(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0.verifying_key().as_bytes())
    }

    pub fn sign_challenge(&self, nonce: &str) -> String {
        let message = challenge_message(nonce, &self.public_key());
        URL_SAFE_NO_PAD.encode(self.0.sign(&message).to_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_round_trip_and_persistence() {
        let id = Identity::generate();
        assert_eq!(Identity::import(&id.export()).unwrap().public_key(), id.public_key());
        let dir = std::env::temp_dir().join(format!("vc-identity-{}", rand::random::<u64>()));
        let path = dir.join("identity");
        let first = Identity::load_or_create(&path).unwrap();
        let second = Identity::load_or_create(&path).unwrap();
        assert_eq!(first.public_key(), second.public_key());
        fs::remove_dir_all(dir).unwrap();
    }
}
