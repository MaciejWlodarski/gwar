//! The running server's local control socket, `data/admin.sock` (Unix only): maintenance commands
//! that would otherwise need the server stopped hand their work to it here. Only the server's own
//! system user can open it (mode 0600), and it takes one request per connection:
//! `{"import": {plan, source, options}}` answers `{"ok": applied}` or `{"error": message}`.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::ts_import::{
    apply::{Applied, ApplyOptions},
    plan::Plan,
};

/// The socket's name in the data directory.
pub const SOCKET: &str = "admin.sock";
/// The largest request accepted: a plan with every client of a big TeamSpeak server.
const MAX_REQUEST: u64 = 64 << 20;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ControlRequest {
    Import { plan: Plan, source: String, options: ApplyOptions },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ControlReply {
    Ok(Applied),
    Error(String),
}

/// Listens on `data_dir/admin.sock` for as long as the server runs.
#[cfg(unix)]
pub async fn serve(data_dir: &Path, core: crate::core::CoreHandle) -> Result<tokio::task::JoinHandle<()>> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let path = data_dir.join(SOCKET);
    // The data directory's lock is ours, so a socket left there is a stale one.
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("remove {}", path.display())),
    }
    let listener = tokio::net::UnixListener::bind(&path).with_context(|| format!("bind {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else { continue };
            let core = core.clone();
            tokio::spawn(async move {
                let mut text = Vec::new();
                let reply = match (&mut stream).take(MAX_REQUEST + 1).read_to_end(&mut text).await {
                    Ok(_) if text.len() as u64 > MAX_REQUEST => ControlReply::Error("request too large".into()),
                    Ok(_) => match serde_json::from_slice::<ControlRequest>(&text) {
                        Ok(ControlRequest::Import { plan, source, options }) => {
                            match core.import_live(plan, source, options).await {
                                Ok(applied) => ControlReply::Ok(applied),
                                Err(e) => ControlReply::Error(e),
                            }
                        }
                        Err(e) => ControlReply::Error(format!("bad request: {e}")),
                    },
                    Err(e) => ControlReply::Error(format!("read: {e}")),
                };
                if let Ok(bytes) = serde_json::to_vec(&reply) {
                    let _ = stream.write_all(&bytes).await;
                    let _ = stream.shutdown().await;
                }
            });
        }
    }))
}

/// Whether a server runs on `data_dir` with a control socket to talk to.
pub fn available(data_dir: &Path) -> bool {
    cfg!(unix) && data_dir.join(SOCKET).exists()
}

/// Hands an import to the server running on `data_dir`.
#[cfg(unix)]
pub async fn import(data_dir: &Path, plan: Plan, source: String, options: ApplyOptions) -> Result<Applied> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let path = data_dir.join(SOCKET);
    let mut stream = tokio::net::UnixStream::connect(&path)
        .await
        .with_context(|| format!("connect to the running server at {}", path.display()))?;
    let request = serde_json::to_vec(&ControlRequest::Import { plan, source, options })?;
    stream.write_all(&request).await?;
    stream.shutdown().await?;
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).await?;
    match serde_json::from_slice(&reply).context("the server's answer")? {
        ControlReply::Ok(applied) => Ok(applied),
        ControlReply::Error(e) => bail!("the running server refused the import: {e}"),
    }
}

#[cfg(not(unix))]
pub async fn import(_: &Path, _: Plan, _: String, _: ApplyOptions) -> Result<Applied> {
    bail!("stop the server first: importing into a running server needs a Unix control socket")
}
