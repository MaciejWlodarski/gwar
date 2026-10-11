//! The server's persistent identity key.

use std::{fs, io::Write, path::Path};

use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;
pub const KEY_FILE: &str = "server-key.ed25519";

pub fn generate() -> SigningKey {
    SigningKey::from_bytes(&rand::random())
}

/// Publishes a complete seed atomically, without replacing an existing key.
pub fn load(data_dir: &Path) -> Result<SigningKey> {
    fs::create_dir_all(data_dir).context("create server key directory")?;
    let path = data_dir.join(KEY_FILE);
    if !path.exists() {
        let temporary = data_dir.join(format!(".server-key-{:016x}", rand::random::<u64>()));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).context("create server key")?;
        let result = (|| -> Result<()> {
            file.write_all(&generate().to_bytes()).context("write server key")?;
            file.sync_all().context("sync server key")?;
            match fs::hard_link(&temporary, &path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
                Err(e) => Err(e).context("publish server key"),
            }
        })();
        fs::remove_file(&temporary).context("remove temporary server key")?;
        result?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).context("secure server key")?;
    }
    let seed: [u8; 32] = fs::read(&path)
        .context("read server key")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid server key length"))?;
    Ok(SigningKey::from_bytes(&seed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_persisted_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let first = load(dir.path()).unwrap();
        assert_eq!(first.to_bytes(), load(dir.path()).unwrap().to_bytes());
        assert_eq!(fs::read(dir.path().join(KEY_FILE)).unwrap(), first.to_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(dir.path().join(KEY_FILE)).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn invalid_key_is_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(KEY_FILE), b"bad").unwrap();
        assert!(load(dir.path()).is_err());
        assert_eq!(fs::read(dir.path().join(KEY_FILE)).unwrap(), b"bad");
    }
}
