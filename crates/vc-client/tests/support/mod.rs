//! A server with the official TeamSpeak server bridged in, for tests.
//!
//! Needs a TeamSpeak server installation: set `VC_TS3_DIR` to a directory
//! the server installs into (`vc-server --teamspeak` does it on first start,
//! or any earlier run of these tests). Without it the tests are skipped.

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use vc_client::teamspeak::{self, TsConnected, TsOptions};
use vc_server::{Running, TeamSpeakConfig, teamspeak::install};

fn free_port(udp: bool) -> u16 {
    if udp {
        std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    } else {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }
}

pub struct Bridged {
    pub server: Running,
    pub ts_voice: SocketAddr,
    pub media: SocketAddr,
    _dir: TempDir,
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Starts our server with TeamSpeak enabled, or `None` (skip) without `VC_TS3_DIR`.
pub async fn start() -> Option<Bridged> {
    // Server logs, e.g. RUST_LOG=vc_server=debug.
    let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).try_init();
    let Some(root) = std::env::var_os("VC_TS3_DIR").map(PathBuf::from) else {
        eprintln!("skipped: set VC_TS3_DIR to run tests against the official TeamSpeak server");
        return None;
    };
    // One shared installation, a fresh server state per test.
    let installed = install::ensure_installed(&root).await.expect("install TeamSpeak server");
    let dir = std::env::temp_dir().join(format!("vc-ts-test-{}-{}", std::process::id(), free_port(false)));
    std::fs::create_dir_all(&dir).unwrap();
    let name = installed.dir.file_name().unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&installed.dir, dir.join(name)).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&installed.dir, dir.join(name)).unwrap();

    let media: SocketAddr = format!("127.0.0.1:{}", free_port(true)).parse().unwrap();
    let ts_voice: SocketAddr = format!("127.0.0.1:{}", free_port(true)).parse().unwrap();
    let server = vc_server::start(vc_server::Config {
        database: None,
        http_bind: "127.0.0.1:0".parse().unwrap(),
        media_bind: media,
        media_advertise: media,
        max_clients: 8,
        server_password: None,
        web_root: None,
        ice_servers: vec![],
        teamspeak: Some(TeamSpeakConfig {
            dir: dir.clone(),
            voice: ts_voice,
            query_port: free_port(false),
            filetransfer: format!("127.0.0.1:{}", free_port(false)).parse().unwrap(),
            public_url: None,
        }),
        public_url: None,
        upload_limit: 10 * 1024 * 1024,
        files_dir: None,
    })
    .await
    .unwrap();
    let mut ready = server.teamspeak_ready.clone().unwrap();
    tokio::time::timeout(Duration::from_secs(120), ready.wait_for(|ready| *ready))
        .await
        .expect("TeamSpeak bridge did not get ready")
        .unwrap();
    Some(Bridged { server, ts_voice, media, _dir: TempDir(dir) })
}

/// Connects a TeamSpeak client once the TeamSpeak server is up (it starts in the background).
pub async fn ts_client(address: SocketAddr, nickname: &str) -> TsConnected {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        let options = TsOptions {
            address: address.to_string(),
            nickname: nickname.into(),
            server_password: None,
            identity: tsclientlib::Identity::create(),
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
