//! What the app keeps between runs, in its private data directory: the device
//! identity, the pinned key of every server it has met and the last connection form.

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use vc_client::Identity;

use crate::vault::{KeyVault, load_identity};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerPin {
    pub server_pk: String,
    /// Unix time in milliseconds.
    pub first_seen: i64,
}

/// The connection form as last submitted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
pub struct LastServer {
    pub address: String,
    pub nickname: String,
    pub insecure_lan: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Servers {
    #[serde(default)]
    pins: BTreeMap<String, ServerPin>,
    #[serde(default)]
    last: Option<LastServer>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Format(String),
    #[error("server identity changed")]
    PinMismatch,
    /// The platform's key vault failed, e.g. its key is gone.
    #[error("the identity cannot be unlocked: {0}")]
    Vault(String),
}

pub struct Store {
    identity: Identity,
    servers_path: PathBuf,
    servers: Mutex<Servers>,
}

impl Store {
    pub fn open(dir: &Path, vault: Option<&dyn KeyVault>) -> Result<Self, StoreError> {
        fs::create_dir_all(dir)?;
        let identity = load_identity(dir, vault)?;
        let servers_path = dir.join("servers.json");
        let servers = match fs::read(&servers_path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| StoreError::Format(e.to_string()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Servers::default(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self { identity, servers_path, servers: Mutex::new(servers) })
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn pin(&self, endpoint: &str) -> Option<ServerPin> {
        self.servers.lock().unwrap().pins.get(endpoint).cloned()
    }

    /// Pins `key` on first contact; a different key already pinned is an error.
    pub fn commit_pin(&self, endpoint: &str, key: &str) -> Result<(), StoreError> {
        let mut servers = self.servers.lock().unwrap();
        match servers.pins.get(endpoint) {
            Some(pin) if pin.server_pk == key => return Ok(()),
            Some(_) => return Err(StoreError::PinMismatch),
            None => {}
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64;
        servers.pins.insert(endpoint.to_owned(), ServerPin { server_pk: key.to_owned(), first_seen: now });
        self.save(&servers)
    }

    /// Forgets a pin, so the next connection trusts the server's key on first use again.
    pub fn forget_pin(&self, endpoint: &str) -> Result<(), StoreError> {
        let mut servers = self.servers.lock().unwrap();
        if servers.pins.remove(endpoint).is_some() {
            self.save(&servers)?;
        }
        Ok(())
    }

    pub fn last_server(&self) -> Option<LastServer> {
        self.servers.lock().unwrap().last.clone()
    }

    pub fn set_last_server(&self, last: LastServer) -> Result<(), StoreError> {
        let mut servers = self.servers.lock().unwrap();
        servers.last = Some(last);
        self.save(&servers)
    }

    /// Writes beside the file and renames over it, so a crash leaves the old or the new copy.
    fn save(&self, servers: &Servers) -> Result<(), StoreError> {
        let tmp = self.servers_path.with_extension("json.tmp");
        let mut file = fs::File::create(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(servers).expect("serializable"))?;
        file.sync_all()?;
        fs::rename(&tmp, &self.servers_path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("gwar-app-store-{}", rand_suffix()))
    }

    fn rand_suffix() -> u128 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    }

    #[test]
    fn identity_pins_and_form_survive_a_restart() {
        let dir = temp_dir();
        let store = Store::open(&dir, None).unwrap();
        let uid = store.identity().uid();
        assert_eq!(store.pin("wss://a/ws"), None);
        store.commit_pin("wss://a/ws", "key-a").unwrap();
        store.commit_pin("wss://a/ws", "key-a").unwrap();
        assert!(matches!(store.commit_pin("wss://a/ws", "key-b"), Err(StoreError::PinMismatch)));
        let form = LastServer { address: "a".into(), nickname: "ania".into(), insecure_lan: false };
        store.set_last_server(form.clone()).unwrap();
        drop(store);

        let store = Store::open(&dir, None).unwrap();
        assert_eq!(store.identity().uid(), uid);
        assert_eq!(store.pin("wss://a/ws").unwrap().server_pk, "key-a");
        assert_eq!(store.last_server(), Some(form));
        store.forget_pin("wss://a/ws").unwrap();
        assert_eq!(Store::open(&dir, None).unwrap().pin("wss://a/ws"), None);
        fs::remove_dir_all(dir).unwrap();
    }
}
