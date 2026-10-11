//! Native file IO is confined to the connected server and its two file routes.

use reqwest::{Client, Url};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};
use tauri::{
    State,
    ipc::{InvokeBody, Request, Response},
};
use tauri_plugin_dialog::DialogExt;
use tokio::sync::{Semaphore, oneshot, watch};

const MAX_BYTES: u64 = 30 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct Server {
    /// Names one connection of the interface: a command naming another one (a stale upload or download after a
    /// reconnect, even to the same origin) is refused before anything is sent.
    session: String,
    origin: Url,
    client: Client,
    limit: u64,
    closed: watch::Sender<bool>,
}

pub struct Files {
    server: Mutex<Option<Server>>,
    requests: Mutex<HashMap<String, oneshot::Sender<()>>>,
    slots: Arc<Semaphore>,
}

impl Default for Files {
    fn default() -> Self {
        Self { server: Mutex::new(None), requests: Mutex::new(HashMap::new()), slots: Arc::new(Semaphore::new(4)) }
    }
}

impl Files {
    /// The connected server, if it is still the one of `session`.
    fn current(&self, session: &str) -> Result<Server, String> {
        let server = self.server.lock().unwrap_or_else(|e| e.into_inner()).clone().ok_or("no connected server")?;
        if server.session != session {
            return Err("the server changed".into());
        }
        Ok(server)
    }

    fn replace(&self, server: Option<Server>) {
        if let Some(old) = std::mem::replace(&mut *self.server.lock().unwrap_or_else(|e| e.into_inner()), server) {
            old.closed.send_replace(true);
        }
    }
}

fn restricted(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.octets()[0] == 0
                || ip.octets()[0] >= 240
                || (ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1]))
        }
        IpAddr::V6(ip) => ip.to_ipv4_mapped().map(|v| restricted(IpAddr::V4(v))).unwrap_or_else(|| {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
        }),
    }
}

fn origin(value: &str) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|_| "invalid server origin")?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || url.host_str().is_none()
    {
        return Err("invalid server origin".into());
    }
    Ok(url)
}

fn lan_host(host: &str) -> bool {
    host.trim_matches(['[', ']']).parse().map(restricted).unwrap_or_else(|_| {
        host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") || !host.contains('.')
    })
}

fn checked_targets(host: &str, addresses: &[SocketAddr]) -> Result<(), String> {
    if addresses.is_empty() || (!lan_host(host) && addresses.iter().any(|a| restricted(a.ip()))) {
        return Err("server resolves to a private target".into());
    }
    Ok(())
}

async fn server(value: &str, limit: u64, session: String) -> Result<Server, String> {
    if !token(&session, 64) {
        return Err("invalid file session".into());
    }
    let origin = origin(value)?;
    let host = origin.host_str().ok_or("missing host")?.trim_matches(['[', ']']);
    let addresses: Vec<_> = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::net::lookup_host((host, origin.port_or_known_default().ok_or("missing port")?)),
    )
    .await
    .map_err(|_| "DNS timed out")?
    .map_err(|e| e.to_string())?
    .collect();
    checked_targets(host, &addresses)?;
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(Duration::from_secs(10))
        .timeout(TIMEOUT)
        .resolve_to_addrs(host, &addresses)
        .build()
        .map_err(|e| e.to_string())?;
    Ok(Server { session, origin, client, limit: limit.min(MAX_BYTES), closed: watch::channel(false).0 })
}

#[tauri::command]
pub async fn files_set_server(
    files: State<'_, Files>,
    origin: Option<String>,
    upload_limit: u64,
    session: Option<String>,
) -> Result<(), String> {
    files.replace(None);
    if let Some(origin) = origin {
        files.replace(Some(server(&origin, upload_limit, session.ok_or("missing file session")?).await?));
    }
    Ok(())
}

fn decoded(path: &str) -> Result<String, String> {
    if path.len() > 2048 || path.bytes().any(|b| b.is_ascii_whitespace() || b == b'\\' || b == b'#') {
        return Err("invalid file path".into());
    }
    let mut out = Vec::new();
    let mut bytes = path.bytes();
    while let Some(b) = bytes.next() {
        out.push(if b == b'%' {
            let a = (bytes.next().ok_or("invalid escape")? as char).to_digit(16).ok_or("invalid escape")?;
            let b = (bytes.next().ok_or("invalid escape")? as char).to_digit(16).ok_or("invalid escape")?;
            (a * 16 + b) as u8
        } else {
            b
        });
    }
    let out = String::from_utf8(out).map_err(|_| "invalid file path")?;
    // Names may contain "%" or "..", but no segment may be a dot segment.
    if out.split('/').any(|seg| seg == "." || seg == "..") || out.bytes().any(|b| b.is_ascii_control() || b == b'\\') {
        return Err("invalid file path".into());
    }
    Ok(out)
}

fn token(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn file_url(origin: &Url, path: &str, put: bool) -> Result<Url, String> {
    let d = decoded(path)?;
    let valid = if put {
        d.strip_prefix("/api/files/")
            .and_then(|s| s.split_once("?token="))
            .is_some_and(|(id, t)| token(id, 64) && token(t, 128))
    } else {
        d.strip_prefix("/files/")
            .and_then(|s| s.split_once('/'))
            .is_some_and(|(id, name)| token(id, 64) && !name.is_empty() && !name.contains(['/', '?', '#']))
    };
    if !valid {
        return Err("invalid file route".into());
    }
    let url = origin.join(path).map_err(|e| e.to_string())?;
    if url.origin() != origin.origin() {
        return Err("invalid file origin".into());
    }
    Ok(url)
}

fn cap(size: u64, limit: u64) -> Result<usize, String> {
    if size == 0 || size > limit.min(MAX_BYTES) {
        return Err("file exceeds size limit".into());
    }
    Ok(size as usize)
}

async fn bytes(server: &Server, path: &str, size: u64) -> Result<Vec<u8>, String> {
    let cap = cap(size, server.limit)?;
    let url = file_url(&server.origin, path, false)?;
    let mut closed = server.closed.subscribe();
    if *closed.borrow() {
        return Err("disconnected".into());
    }
    let fetch = async {
        let mut response = server.client.get(url).send().await.map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("file HTTP {}", response.status()));
        }
        if response.content_length().is_some_and(|len| len > cap as u64) {
            return Err("file exceeds size limit".into());
        }
        let mut out = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
            if chunk.len() > cap - out.len() {
                return Err("file exceeds size limit".into());
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    };
    tokio::select! { result = fetch => result, _ = closed.changed() => Err("disconnected".into()) }
}

#[tauri::command]
pub async fn fetch_file(
    files: State<'_, Files>,
    session: String,
    path: String,
    size: u64,
    request_id: String,
) -> Result<Response, String> {
    let _permit = files.slots.try_acquire().map_err(|_| "too many file requests")?;
    let server = files.current(&session)?;
    cancellable(&files, &request_id, bytes(&server, &path, size)).await.map(Response::new)
}

#[tauri::command]
pub fn files_cancel(files: State<'_, Files>, request_id: String) {
    if let Some(tx) = files.requests.lock().unwrap_or_else(|e| e.into_inner()).remove(&request_id) {
        let _ = tx.send(());
    }
}

async fn cancellable<T>(
    files: &Files,
    request_id: &str,
    work: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    if !token(request_id, 64) {
        return Err("invalid request id".into());
    }
    let (tx, rx) = oneshot::channel();
    {
        let mut requests = files.requests.lock().unwrap_or_else(|e| e.into_inner());
        if requests.contains_key(request_id) {
            return Err("duplicate request id".into());
        }
        requests.insert(request_id.to_owned(), tx);
    }
    let result = tokio::select! { result = work => result, _ = rx => Err("cancelled".into()) };
    files.requests.lock().unwrap_or_else(|e| e.into_inner()).remove(request_id);
    result
}

fn proposed_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name).trim();
    if base.is_empty() { "file".into() } else { base.chars().filter(|c| !c.is_control()).take(120).collect() }
}

#[tauri::command]
pub async fn save_file_as(
    app: tauri::AppHandle,
    files: State<'_, Files>,
    session: String,
    path: String,
    file_name: String,
    size: u64,
    request_id: String,
) -> Result<bool, String> {
    let _permit = files.slots.try_acquire().map_err(|_| "too many file requests")?;
    let server = files.current(&session)?;
    file_url(&server.origin, &path, false)?;
    cap(size, server.limit)?;
    cancellable(&files, &request_id, async {
        let (tx, rx) = oneshot::channel();
        app.dialog().file().set_file_name(proposed_name(&file_name)).save_file(move |path| {
            let _ = tx.send(path);
        });
        let Some(destination) = rx.await.map_err(|e| e.to_string())? else { return Ok(false) };
        // Fetch before touching the chosen file. Failed/cancelled requests preserve it.
        let data = bytes(&server, &path, size).await?;
        tokio::fs::write(destination.into_path().map_err(|e| e.to_string())?, data).await.map_err(|e| e.to_string())?;
        Ok(true)
    })
    .await
}

/// Writes only the inspected bytes supplied by the local client, through a native save dialog.
#[tauri::command]
pub async fn save_blob_as(
    app: tauri::AppHandle,
    files: State<'_, Files>,
    request: Request<'_>,
) -> Result<bool, String> {
    let _permit = files.slots.try_acquire().map_err(|_| "too many file requests")?;
    let InvokeBody::Raw(data) = request.body() else { return Err("expected raw bytes".into()) };
    if data.len() as u64 > MAX_BYTES {
        return Err("file exceeds size limit".into());
    }
    let name = request.headers().get("x-file-name").and_then(|v| v.to_str().ok()).ok_or("missing file name")?;
    if name.len() > 2048 {
        return Err("file name too long".into());
    }
    let name = percent_encoding::percent_decode_str(name).decode_utf8().map_err(|e| e.to_string())?;
    let (tx, rx) = oneshot::channel();
    app.dialog().file().set_file_name(proposed_name(&name)).save_file(move |path| {
        let _ = tx.send(path);
    });
    let Some(destination) = rx.await.map_err(|e| e.to_string())? else { return Ok(false) };
    tokio::fs::write(destination.into_path().map_err(|e| e.to_string())?, data).await.map_err(|e| e.to_string())?;
    Ok(true)
}

#[tauri::command]
pub async fn upload_put(files: State<'_, Files>, request: Request<'_>) -> Result<vc_proto::Attachment, String> {
    let _permit = files.slots.try_acquire().map_err(|_| "too many file requests")?;
    let header = |name: &str| request.headers().get(name).and_then(|v| v.to_str().ok());
    // Only the connection the upload was reserved on; checked before the destination is chosen.
    let server = files.current(header("x-upload-session").ok_or("missing file session")?)?;
    let url = file_url(&server.origin, header("x-upload-path").ok_or("missing upload path")?, true)?;
    let InvokeBody::Raw(data) = request.body() else { return Err("expected raw bytes".into()) };
    cap(data.len() as u64, server.limit)?;
    let mut closed = server.closed.subscribe();
    if *closed.borrow() {
        return Err("disconnected".into());
    }
    let send = server.client.put(url).body(data.clone()).send();
    let response = tokio::select! { r = send => r.map_err(|e| e.to_string())?, _ = closed.changed() => return Err("disconnected".into()) };
    if response.status().is_redirection() {
        return Err("redirect rejected".into());
    }
    let mut response = response.error_for_status().map_err(|e| e.to_string())?;
    if response.content_length().is_some_and(|size| size > 16 * 1024) {
        return Err("upload response too large".into());
    }
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        if data.len() + chunk.len() > 16 * 1024 {
            return Err("upload response too large".into());
        }
        data.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&data).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn paths_and_methods_are_confined() {
        let origin = origin("https://voice.example").unwrap();
        assert!(file_url(&origin, "/files/a/photo%20one.png", false).is_ok());
        assert!(file_url(&origin, "/files/a/v1..2%20100%25.zip", false).is_ok());
        assert!(file_url(&origin, "/api/files/a?token=abc", true).is_ok());
        for path in [
            "http://localhost/files/a/b",
            "//localhost/files/a/b",
            "/files/a/../b",
            "/files/a/%2e%2e",
            "/files/a/%2fb",
            "/files/a/x?y",
            "/files/a/%5cb",
            "file:///etc/passwd",
            "/api/files/a?token=x",
        ] {
            assert!(file_url(&origin, path, false).is_err(), "{path}");
        }
        assert!(file_url(&origin, "/files/a/b", true).is_err());
        assert!(super::origin("https://user:pass@voice.example").is_err());
    }

    #[test]
    fn public_hosts_cannot_resolve_to_lan_targets() {
        for ip in
            ["127.0.0.1:80", "10.0.0.1:80", "169.254.169.254:80", "[::1]:80", "[::ffff:127.0.0.1]:80", "[fe80::1]:80"]
        {
            assert!(checked_targets("voice.example", &[ip.parse().unwrap()]).is_err());
        }
        assert!(checked_targets("192.168.1.2", &["192.168.1.2:80".parse().unwrap()]).is_ok());
        assert!(checked_targets("voice.example", &["1.1.1.1:80".parse().unwrap()]).is_ok());
    }

    #[test]
    fn size_caps_and_names() {
        assert!(cap(5, 4).is_err());
        assert!(cap(MAX_BYTES + 1, u64::MAX).is_err());
        assert!(cap(0, MAX_BYTES).is_err());
        assert_eq!(proposed_name("../../etc/passwd"), "passwd");
    }

    #[tokio::test]
    async fn native_requests_cancel_without_waiting_for_timeout() {
        let files = Files::default();
        let work = cancellable(&files, "request", std::future::pending::<Result<(), String>>());
        let cancel = async {
            tokio::task::yield_now().await;
            files.requests.lock().unwrap().remove("request").unwrap().send(()).unwrap();
        };
        let (result, ()) = tokio::join!(work, cancel);
        assert_eq!(result.unwrap_err(), "cancelled");
        assert!(files.requests.lock().unwrap().is_empty());
    }

    async fn reply(response: &'static [u8], timeout: Duration) -> Server {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0; 4096];
            let _ = socket.read(&mut buf).await;
            let _ = socket.write_all(response).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        Server {
            session: "s".into(),
            origin: origin(&format!("http://{address}")).unwrap(),
            client: Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(timeout)
                .build()
                .unwrap(),
            limit: MAX_BYTES,
            closed: watch::channel(false).0,
        }
    }

    fn at(session: &str, origin_: &str) -> Server {
        Server {
            session: session.into(),
            origin: origin(origin_).unwrap(),
            client: Client::new(),
            limit: MAX_BYTES,
            closed: watch::channel(false).0,
        }
    }

    #[test]
    fn file_commands_reach_only_the_connection_that_issued_them() {
        let files = Files::default();
        assert_eq!(files.current("a").err().unwrap(), "no connected server");
        files.replace(Some(at("a", "https://a.example")));
        assert_eq!(files.current("a").unwrap().origin.as_str(), "https://a.example/");
        // Another server, and a reconnect to the same one: commands of the old connection are refused.
        for next in [at("b", "https://b.example"), at("a2", "https://a.example")] {
            let stale = files.server.lock().unwrap().as_ref().unwrap().session.clone();
            let session = next.session.clone();
            files.replace(Some(next));
            assert_eq!(files.current(&stale).err().unwrap(), "the server changed");
            assert!(files.current(&session).is_ok());
        }
        files.replace(None);
        assert!(files.current("a2").is_err());
    }

    #[tokio::test]
    async fn a_server_needs_a_well_formed_session() {
        for session in ["", "has space", "x".repeat(65).as_str(), "a/b"] {
            assert_eq!(server("https://1.1.1.1", 1, session.into()).await.err().unwrap(), "invalid file session");
        }
    }

    #[tokio::test]
    async fn redirects_overruns_timeouts_and_disconnects_fail() {
        let redirect =
            reply(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/\r\nContent-Length: 0\r\n\r\n", TIMEOUT).await;
        assert!(bytes(&redirect, "/files/a/b", 4).await.unwrap_err().contains("302"));
        let large = reply(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n12345", TIMEOUT).await;
        assert!(bytes(&large, "/files/a/b", 4).await.is_err());
        let slow = reply(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n", Duration::from_millis(20)).await;
        assert!(bytes(&slow, "/files/a/b", 4).await.is_err());
        slow.closed.send_replace(true);
        assert_eq!(bytes(&slow, "/files/a/b", 4).await.unwrap_err(), "disconnected");
    }
}
