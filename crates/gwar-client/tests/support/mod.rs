//! A server with the official TeamSpeak server bridged in, for tests.
//!
//! Needs a TeamSpeak server installation: set `GWAR_TS3_DIR` to a directory
//! the server installs into (`gwar-server --teamspeak` does it on first start,
//! or any earlier run of these tests). Without it the tests are skipped.

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use gwar_client::teamspeak::{self, TsConnected, TsOptions};
use gwar_server::{Running, TeamSpeakConfig, teamspeak::install};

fn free_port(udp: bool) -> u16 {
    if udp {
        std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    } else {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }
}

/// The public URL the test server pretends to have (links to uploads use it).
pub const PUBLIC_URL: &str = "https://gwar.test";

pub struct Bridged {
    pub server: Running,
    pub ts_voice: SocketAddr,
    pub media: SocketAddr,
    /// The TeamSpeak server's ServerQuery port; see [`Bridged::query_password`].
    #[allow(dead_code)]
    pub query: SocketAddr,
    /// The TeamSpeak server's file transfer (TCP).
    #[allow(dead_code)]
    pub filetransfer: SocketAddr,
    dir: TempDir,
}

impl Bridged {
    /// The `serveradmin` password of the TeamSpeak server's ServerQuery.
    #[allow(dead_code)]
    pub fn query_password(&self) -> String {
        std::fs::read_to_string(self.dir.0.join("query-password")).unwrap().trim().to_owned()
    }
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Starts our server with TeamSpeak enabled, or `None` (skip) without `GWAR_TS3_DIR`.
pub async fn start() -> Option<Bridged> {
    // Server logs, e.g. RUST_LOG=gwar_server=debug.
    let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).try_init();
    let Some(root) = std::env::var_os("GWAR_TS3_DIR").map(PathBuf::from) else {
        eprintln!("skipped: set GWAR_TS3_DIR to run tests against the official TeamSpeak server");
        return None;
    };
    // One shared installation, a fresh server state per test.
    let installed = install::ensure_installed(&root).await.expect("install TeamSpeak server");
    let dir = std::env::temp_dir().join(format!("gwar-ts-test-{}-{}", std::process::id(), free_port(false)));
    std::fs::create_dir_all(&dir).unwrap();
    let name = installed.dir.file_name().unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&installed.dir, dir.join(name)).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&installed.dir, dir.join(name)).unwrap();

    let media: SocketAddr = format!("127.0.0.1:{}", free_port(true)).parse().unwrap();
    let ts_voice: SocketAddr = format!("127.0.0.1:{}", free_port(true)).parse().unwrap();
    let http: SocketAddr = format!("127.0.0.1:{}", free_port(false)).parse().unwrap();
    let query: SocketAddr = format!("127.0.0.1:{}", free_port(false)).parse().unwrap();
    let filetransfer: SocketAddr = format!("127.0.0.1:{}", free_port(false)).parse().unwrap();
    let server = gwar_server::start(gwar_server::Config {
        database: None,
        http_bind: http,
        media_bind: media,
        media_advertise: media,
        max_clients: 8,
        encrypted_server_chat: false,
        server_password: None,
        web_root: None,
        ice_servers: vec![],
        teamspeak: Some(TeamSpeakConfig {
            dir: dir.clone(),
            voice: ts_voice,
            query_port: query.port(),
            filetransfer,
            public_url: Some(PUBLIC_URL.into()),
        }),
        public_url: Some(PUBLIC_URL.into()),
        data_dir: None,
        public_ip: None,
        insecure_any_host: true,
        public_endpoints: vec![format!("ws://{http}/ws")],
        client_ip_header: None,
        upload_limit: 10 * 1024 * 1024,
        upload_quota: 0,
        storage_limit: 0,
        files_dir: None,
        web_origins: Vec::new(),
        tls: None,
        redirect_http: None,
        connect_url: None,
    })
    .await
    .unwrap();
    let mut ready = server.teamspeak_ready.clone().unwrap();
    tokio::time::timeout(Duration::from_secs(120), ready.wait_for(|ready| *ready))
        .await
        .expect("TeamSpeak bridge did not get ready")
        .unwrap();
    Some(Bridged { server, ts_voice, media, query, filetransfer, dir: TempDir(dir) })
}

/// Connects a TeamSpeak client once the TeamSpeak server is up (it starts in the background).
pub async fn ts_client(address: SocketAddr, nickname: &str) -> TsConnected {
    ts_client_as(address, nickname, tsclientlib::Identity::create()).await
}

/// Like [`ts_client`], with a given identity.
pub async fn ts_client_as(address: SocketAddr, nickname: &str, identity: tsclientlib::Identity) -> TsConnected {
    ts_client_into(address, nickname, identity, None).await
}

/// Like [`ts_client_as`], landing in a channel (a `Parent/Child` path) with its password.
pub async fn ts_client_into(
    address: SocketAddr,
    nickname: &str,
    identity: tsclientlib::Identity,
    channel: Option<(&str, &str)>,
) -> TsConnected {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        let options = TsOptions {
            address: address.to_string(),
            nickname: nickname.into(),
            server_password: None,
            identity: identity.clone(),
            channel: channel.map(|(path, _)| path.into()),
            channel_password: channel.map(|(_, password)| password.into()),
        };
        match teamspeak::connect(options).await {
            Ok(connected) => return connected,
            Err(e) if tokio::time::Instant::now() < deadline => {
                eprintln!("waiting for the TeamSpeak server: {e:#}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(e) => panic!("TeamSpeak server did not come up: {e:#}"),
        }
    }
}
