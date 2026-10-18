//! The device identity at rest. With a [`KeyVault`] (the Android app passes
//! one backed by the Android Keystore) the seed is stored sealed by a key
//! that never leaves the vault, hardware-backed where the phone has it: a copy
//! of the app's files is useless without that phone. Without a vault (tests,
//! desktop tools) it is a file readable only by its owner.

use std::{fs, io::Write, path::Path};

use vc_client::Identity;
use zeroize::Zeroizing;

use crate::store::StoreError;

const PLAIN: &str = "identity";
const SEALED: &str = "identity.sealed";

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum VaultError {
    #[error("{detail}")]
    Failed { detail: String },
}

/// Seals and opens small secrets with a key the platform keeps.
#[uniffi::export(with_foreign)]
pub trait KeyVault: Send + Sync {
    fn seal(&self, plain: Vec<u8>) -> Result<Vec<u8>, VaultError>;
    fn open(&self, sealed: Vec<u8>) -> Result<Vec<u8>, VaultError>;
}

impl From<VaultError> for StoreError {
    fn from(e: VaultError) -> Self {
        StoreError::Vault(e.to_string())
    }
}

/// Loads this device's identity from `dir`, creating it on first use. A
/// sealed identity that no longer opens is an error, never replaced: a new
/// identity would be a different person on every server.
pub fn load_identity(dir: &Path, vault: Option<&dyn KeyVault>) -> Result<Identity, StoreError> {
    let Some(vault) = vault else {
        return Identity::load_or_create(&dir.join(PLAIN)).map_err(|e| StoreError::Format(e.to_string()));
    };
    let sealed_path = dir.join(SEALED);
    match fs::read(&sealed_path) {
        Ok(sealed) => return import(&Zeroizing::new(vault.open(sealed)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }

    // First start with the vault: an identity kept in plain moves in, else a new one.
    let plain_path = dir.join(PLAIN);
    let identity = match fs::read(&plain_path) {
        Ok(text) => import(&Zeroizing::new(text))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Identity::generate(),
        Err(e) => return Err(e.into()),
    };
    let export = Zeroizing::new(identity.export());
    let sealed = vault.seal(export.as_bytes().to_vec())?;
    // Only rely on a sealed copy that opens again.
    if Zeroizing::new(vault.open(sealed.clone())?).as_slice() != export.as_bytes() {
        return Err(StoreError::Vault("the sealed identity does not open to itself".into()));
    }
    let tmp = sealed_path.with_extension("sealed.tmp");
    let mut file = fs::File::create(&tmp)?;
    file.write_all(&sealed)?;
    file.sync_all()?;
    fs::rename(&tmp, &sealed_path)?;
    if plain_path.exists() {
        fs::remove_file(&plain_path)?;
    }
    Ok(identity)
}

fn import(text: &[u8]) -> Result<Identity, StoreError> {
    let text = std::str::from_utf8(text).map_err(|_| StoreError::Format("identity is not text".into()))?;
    Identity::import(text.trim()).map_err(|e| StoreError::Format(e.to_string()))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    /// XORs with a key byte and tags the result; `broken` makes opening fail.
    struct FakeVault {
        key: u8,
        broken: AtomicBool,
    }

    impl KeyVault for FakeVault {
        fn seal(&self, plain: Vec<u8>) -> Result<Vec<u8>, VaultError> {
            Ok([b"v1".as_slice(), &plain.iter().map(|b| b ^ self.key).collect::<Vec<_>>()].concat())
        }

        fn open(&self, sealed: Vec<u8>) -> Result<Vec<u8>, VaultError> {
            match sealed.strip_prefix(b"v1") {
                Some(body) if !self.broken.load(Ordering::SeqCst) => Ok(body.iter().map(|b| b ^ self.key).collect()),
                _ => Err(VaultError::Failed { detail: "key permanently invalidated".into() }),
            }
        }
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("gwar-vault-{name}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_plain_identity_moves_into_the_vault_and_stays_the_same() {
        let dir = temp_dir("move");
        let plain = load_identity(&dir, None).unwrap();
        let vault = FakeVault { key: 0x5a, broken: AtomicBool::new(false) };
        let sealed = load_identity(&dir, Some(&vault)).unwrap();
        assert_eq!(sealed.uid(), plain.uid());
        assert!(!dir.join(PLAIN).exists(), "the plain copy is gone");
        let on_disk = fs::read(dir.join(SEALED)).unwrap();
        assert!(!on_disk.windows(10).any(|w| plain.export().as_bytes().windows(10).any(|p| p == w)));
        assert_eq!(load_identity(&dir, Some(&vault)).unwrap().uid(), plain.uid());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_new_device_starts_sealed() {
        let dir = temp_dir("new");
        let vault = FakeVault { key: 0x33, broken: AtomicBool::new(false) };
        let first = load_identity(&dir, Some(&vault)).unwrap();
        assert!(dir.join(SEALED).exists() && !dir.join(PLAIN).exists());
        assert_eq!(load_identity(&dir, Some(&vault)).unwrap().uid(), first.uid());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_vault_that_no_longer_opens_is_an_error_not_a_new_identity() {
        let dir = temp_dir("broken");
        let vault = FakeVault { key: 0x11, broken: AtomicBool::new(false) };
        load_identity(&dir, Some(&vault)).unwrap();
        let sealed = fs::read(dir.join(SEALED)).unwrap();
        vault.broken.store(true, Ordering::SeqCst);
        assert!(matches!(load_identity(&dir, Some(&vault)), Err(StoreError::Vault(_))));
        assert_eq!(fs::read(dir.join(SEALED)).unwrap(), sealed, "the sealed identity is left alone");
        fs::remove_dir_all(dir).unwrap();
    }
}
