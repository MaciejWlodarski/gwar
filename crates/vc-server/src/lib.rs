//! Voice server core.
//!
//! ```text
//!  browser / desktop ──WS vc/3──▶ gateway ──▶ core actor ──▶ SQLite
//!                                                │ routing snapshot
//!  browser / desktop ◀─WebRTC Opus─▶ media SFU ◀─┤
//!  official TeamSpeak server ◀─query + puppets─▶ teamspeak bridge (optional)
//! ```

pub mod connect;
pub mod core;
pub mod files;
pub mod gateway;
pub mod identity;
pub mod media;
pub mod members;
mod preauth;
pub mod server_key;
use base64::Engine;
pub mod store;
#[cfg(feature = "teamspeak")]
pub mod teamspeak;
pub mod tls;

use std::{collections::HashSet, net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use axum::http::{HeaderValue, Method, header};
use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{get, put},
};
use tokio::{
    net::{TcpListener, UdpSocket},
    sync::mpsc,
};
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    services::{ServeDir, ServeFile},
};
use vc_proto::{IceServer, endpoint_from_public_url, normalize_endpoint};

use crate::{
    core::{CoreConfig, CoreHandle},
    gateway::Gateway,
    media::webrtc::{self, WebRtcConfig},
    store::Store,
};

pub struct Config {
    /// Persistent server identity directory; None uses an ephemeral key (tests).
    pub data_dir: Option<PathBuf>,
    /// Explicit public IP, never the inferred media address.
    pub public_ip: Option<std::net::IpAddr>,
    /// Isolated development only: accept any canonical signed endpoint.
    pub insecure_any_host: bool,
    /// SQLite database path; `None` keeps state in memory (tests).
    pub database: Option<PathBuf>,
    pub http_bind: SocketAddr,
    pub media_bind: SocketAddr,
    /// Address advertised to WebRTC clients. Port 0 means "use the bound port".
    pub media_advertise: SocketAddr,
    pub max_clients: u32,
    pub server_password: Option<String>,
    pub web_root: Option<PathBuf>,
    pub ice_servers: Vec<IceServer>,
    /// Official TeamSpeak server to run and bridge; `None` disables TS compatibility.
    pub teamspeak: Option<TeamSpeakConfig>,
    /// Public base URL of the web origin (e.g. `https://voice.example.com`),
    /// used in links to uploads posted to TeamSpeak and to derive an external `/ws` endpoint.
    pub public_url: Option<String>,
    /// Extra accepted external WebSocket endpoints. Never derived from request headers.
    pub public_endpoints: Vec<String>,
    /// Largest upload in bytes; 0 disables uploads.
    pub upload_limit: u64,
    /// Where uploads are stored; `None` uses a temporary directory (tests).
    pub files_dir: Option<PathBuf>,
    /// Web app origins allowed to upload cross-origin (see [`OFFICIAL_WEB_ORIGIN`]).
    pub web_origins: Vec<String>,
    /// HTTPS for the HTTP listener; `None` serves plain HTTP (behind a proxy, or tests).
    pub tls: Option<tls::Tls>,
    /// Also answer plain HTTP here with a redirect to HTTPS.
    pub redirect_http: Option<SocketAddr>,
    /// Gwar Connect, for revocations and confirmed account handles (see [`connect`]).
    pub connect_url: Option<String>,
}

/// The project's Gwar Connect service.
pub const OFFICIAL_CONNECT_URL: &str = "https://gwar.maciejwlodarski.com/connect";

/// Where the project hosts the web app that connects to every Gwar server.
pub const OFFICIAL_WEB_ORIGIN: &str = "https://gwar.maciejwlodarski.com";

/// The official TeamSpeak server to run and bridge (needs the `teamspeak` feature).
pub struct TeamSpeakConfig {
    /// Install and state directory, e.g. `<data-dir>/teamspeak`.
    pub dir: PathBuf,
    /// UDP address TeamSpeak clients connect to.
    pub voice: SocketAddr,
    /// Loopback port for ServerQuery.
    pub query_port: u16,
    pub filetransfer: SocketAddr,
    /// See [`Config::public_url`].
    pub public_url: Option<String>,
}

pub struct Running {
    pub http: SocketAddr,
    pub media: SocketAddr,
    pub core: CoreHandle,
    pub plane: media::MediaPlane,
    /// TeamSpeak voice address, when enabled.
    pub teamspeak: Option<SocketAddr>,
    /// Whether the TeamSpeak bridge is up and in sync (false until then).
    pub teamspeak_ready: Option<tokio::sync::watch::Receiver<bool>>,
    /// One-time admin token, present only when this start created a fresh database.
    pub admin_token: Option<String>,
    pub tasks: Vec<tokio::task::JoinHandle<()>>,
}

/// Only global unicast addresses can add an automatic public endpoint.
/// Special-purpose exclusions follow https://doc.rust-lang.org/std/net/struct.Ipv4Addr.html#method.is_global
/// and the IPv6 registry; multicast, deprecated 6to4 and unallocated IPv6 ranges stay excluded.
pub fn global_unicast(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            let [a, b, c, d] = ip.octets();
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && b == 168)
                || (a == 192 && b == 0 && ((c == 0 && d != 9 && d != 10) || c == 2))
                || (a == 192 && b == 88 && c == 99)
                || (a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
                || (a == 203 && b == 0 && c == 113))
        }
        std::net::IpAddr::V6(ip) => {
            let segments = ip.segments();
            (segments[0] & 0xe000) == 0x2000
                && !(segments[0] == 0x2001 && segments[1] == 0xdb8)
                && !(segments[0] == 0x2001
                    && segments[1] < 0x200
                    && !(matches!(segments, [0x2001, 1, 0, 0, 0, 0, 0, 1 | 2])
                        || segments[1] == 3
                        || (segments[1] == 4 && segments[2] == 0x112)
                        || (0x20..=0x3f).contains(&segments[1])))
                && segments[0] != 0x2002
                && !(segments[0] == 0x3fff && segments[1] < 0x1000)
        }
    }
}

pub fn accepted_endpoints(config: &Config) -> Result<HashSet<String>> {
    let mut endpoints = HashSet::new();
    for endpoint in &config.public_endpoints {
        endpoints.insert(normalize_endpoint(endpoint).with_context(|| format!("invalid public endpoint: {endpoint}"))?);
    }
    if let Some(url) = &config.public_url {
        endpoints.insert(endpoint_from_public_url(url).context("invalid public URL")?);
    }
    if let Some(tls::Tls::Acme { domains, .. }) = &config.tls {
        for domain in domains {
            endpoints.insert(normalize_endpoint(&format!("wss://{domain}/ws")).context("invalid public domain")?);
        }
    }
    if let Some(ip) = config.public_ip.filter(|ip| config.tls.is_some() && global_unicast(*ip)) {
        let host = match ip {
            std::net::IpAddr::V4(_) => ip.to_string(),
            std::net::IpAddr::V6(_) => format!("[{ip}]"),
        };
        let port = if config.http_bind.port() != 443 { format!(":{}", config.http_bind.port()) } else { String::new() };
        endpoints.insert(normalize_endpoint(&format!("wss://{host}{port}/ws")).context("invalid public IP endpoint")?);
    }
    Ok(endpoints)
}

pub async fn start(config: Config) -> Result<Running> {
    let public_endpoints = Arc::new(accepted_endpoints(&config)?);
    anyhow::ensure!(
        !public_endpoints.is_empty() || config.insecure_any_host,
        "no accepted public endpoints; configure --domain, --public-url or --public-endpoint (isolated tests only: --insecure-any-host)"
    );
    if config.insecure_any_host {
        tracing::warn!(
            "INSECURE --insecure-any-host: login relay protection is DISABLED; isolated development/tests ONLY"
        );
    }
    let server_key = Arc::new(match config.data_dir.as_deref() {
        Some(dir) => server_key::load(dir)?,
        None => server_key::generate(),
    });
    let store = match &config.database {
        Some(path) => Store::open(path)?,
        None => Store::in_memory()?,
    };
    let admin_token = core::ensure_first_admin_token(&store)?;
    let connect_seq: i64 = store.meta("connect_seq")?.and_then(|s| s.parse().ok()).unwrap_or(0);
    // A password set in the server settings (even "none") wins over the command line.
    let hash = match store.meta("password_hash")? {
        Some(stored) => Some(stored).filter(|h| !h.is_empty()),
        None => match config.server_password.clone().filter(|p| !p.is_empty()) {
            Some(p) => Some(tokio::task::spawn_blocking(move || core::hash_secret(&p, false)).await?),
            None => None,
        },
    };
    let password: core::ServerPassword = Arc::new(ArcSwap::from_pointee(hash.map(Arc::<str>::from)));
    let files_dir = config.files_dir.clone().unwrap_or_else(|| {
        std::env::temp_dir().join(format!("gwar-files-{}-{}", std::process::id(), rand::random::<u32>()))
    });
    std::fs::create_dir_all(&files_dir).with_context(|| format!("create {}", files_dir.display()))?;
    let routing = Arc::new(ArcSwap::from_pointee(media::Routing::default()));
    let (media_tx, media_rx) = mpsc::channel(256);
    let core = core::spawn(
        store,
        CoreConfig {
            max_clients: config.max_clients,
            ice_servers: config.ice_servers,
            version: env!("CARGO_PKG_VERSION").into(),
            password: password.clone(),
            upload_limit: config.upload_limit,
            files_dir,
            connect_url: config.connect_url.clone(),
            server_key: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(server_key.verifying_key().as_bytes()),
        },
        routing.clone(),
        media_tx.clone(),
    )?;

    let udp = Arc::new(UdpSocket::bind(config.media_bind).await.context("bind media UDP socket")?);
    let media_addr = udp.local_addr()?;
    let mut advertise = config.media_advertise;
    if advertise.port() == 0 {
        advertise.set_port(media_addr.port());
    }
    let sinks = Arc::new(media::Sinks::default());
    let uplinks = Arc::new(media::Sinks::default());
    let plane = media::MediaPlane {
        routing: routing.clone(),
        sinks: sinks.clone(),
        uplinks: uplinks.clone(),
        webrtc: media_tx,
    };
    let mut tasks = vec![tokio::spawn(webrtc::run(
        WebRtcConfig { socket: udp, advertise },
        routing.clone(),
        sinks,
        uplinks,
        core.clone(),
        media_rx,
    ))];

    // Installing and starting TeamSpeak can take a while; serve our clients meanwhile.
    let (teamspeak, teamspeak_ready) = match config.teamspeak {
        #[cfg(not(feature = "teamspeak"))]
        Some(_) => anyhow::bail!("this build has no TeamSpeak support (enable the `teamspeak` feature)"),
        #[cfg(feature = "teamspeak")]
        Some(ts) => {
            let voice = ts.voice;
            let (ready, watch) = tokio::sync::watch::channel(false);
            tasks.push(tokio::spawn(teamspeak::run(ts, core.clone(), plane.clone(), ready)));
            (Some(voice), Some(watch))
        }
        None => (None, None),
    };
    let gateway = Gateway {
        core: core.clone(),
        preauth: Default::default(),
        password,
        public_endpoints,
        server_key,
        insecure_any_host: config.insecure_any_host,
    };
    // The web app runs on another origin than this server: let it upload and
    // fetch files. Uploads are authorized by one-time tokens, not cookies.
    let origins: Vec<HeaderValue> = config.web_origins.iter().filter_map(|o| o.parse().ok()).collect();
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_methods([Method::GET, Method::PUT])
        .allow_headers([header::CONTENT_TYPE, header::RANGE])
        .max_age(std::time::Duration::from_secs(3600));
    let files_routes = Router::new()
        // Uploads enforce their own reserved size.
        .route("/api/files/{id}", put(files::upload).layer(DefaultBodyLimit::disable()))
        .route("/files/{id}/{name}", get(files::download))
        .layer(cors);
    let mut app = Router::new()
        .route("/ws", get(gateway::upgrade))
        .route("/health", get(|| async { "ok" }))
        .merge(files_routes)
        .with_state(gateway);
    if let Some(root) = config.web_root {
        let index = root.join("index.html");
        app = app.fallback_service(ServeDir::new(root).fallback(ServeFile::new(index)));
    }
    if let Some(url) = config.connect_url {
        tasks.push(tokio::spawn(connect::follow(url, core.clone(), connect_seq)));
    }
    let listener = TcpListener::bind(config.http_bind).await.context("bind HTTP listener")?;
    let http = listener.local_addr()?;
    tasks.extend(tls::serve(listener, app, config.tls).await?);
    if let Some(bind) = config.redirect_http {
        match tls::redirect_to_https(bind).await {
            Ok(task) => tasks.push(task),
            // Port 80 is optional (certificates come over 443).
            Err(e) => tracing::warn!("{e:#}"),
        }
    }
    Ok(Running { http, media: advertise, core, plane, teamspeak, teamspeak_ready, admin_token, tasks })
}
