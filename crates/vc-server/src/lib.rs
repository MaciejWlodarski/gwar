//! Voice server core.
//!
//! ```text
//!  browser / desktop ──WS vc/1──▶ gateway ──▶ core actor ──▶ SQLite
//!                                                │ routing snapshot
//!  browser / desktop ◀─WebRTC Opus─▶ media SFU ◀─┤
//!  official TeamSpeak server ◀─query + puppets─▶ teamspeak bridge (optional)
//! ```

pub mod core;
pub mod gateway;
pub mod identity;
pub mod media;
pub mod store;
#[cfg(feature = "teamspeak")]
pub mod teamspeak;

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use axum::{Router, routing::get};
use tokio::{
    net::{TcpListener, UdpSocket},
    sync::mpsc,
};
use tower_http::services::{ServeDir, ServeFile};
use vc_proto::IceServer;

use crate::{
    core::{CoreConfig, CoreHandle},
    gateway::Gateway,
    media::webrtc::{self, WebRtcConfig},
    store::Store,
};

pub struct Config {
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
}

/// The official TeamSpeak server to run and bridge (needs the `teamspeak` feature).
pub struct TeamSpeakConfig {
    /// Install and state directory, e.g. `<data-dir>/teamspeak`.
    pub dir: PathBuf,
    /// UDP address TeamSpeak clients connect to.
    pub voice: SocketAddr,
    /// Loopback port for ServerQuery.
    pub query_port: u16,
    pub filetransfer: SocketAddr,
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

pub async fn start(config: Config) -> Result<Running> {
    let store = match &config.database {
        Some(path) => Store::open(path)?,
        None => Store::in_memory()?,
    };
    let admin_token = core::ensure_first_admin_token(&store)?;
    let routing = Arc::new(ArcSwap::from_pointee(media::Routing::default()));
    let (media_tx, media_rx) = mpsc::channel(256);
    let core = core::spawn(
        store,
        CoreConfig {
            max_clients: config.max_clients,
            ice_servers: config.ice_servers,
            version: env!("CARGO_PKG_VERSION").into(),
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

    let password_hash = match config.server_password.filter(|p| !p.is_empty()) {
        Some(p) => Some(Arc::from(tokio::task::spawn_blocking(move || core::hash_secret(&p, false)).await?)),
        None => None,
    };
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
    let gateway = Gateway { core: core.clone(), password_hash };
    let mut app =
        Router::new().route("/ws", get(gateway::upgrade)).route("/health", get(|| async { "ok" })).with_state(gateway);
    if let Some(root) = config.web_root {
        let index = root.join("index.html");
        app = app.fallback_service(ServeDir::new(root).fallback(ServeFile::new(index)));
    }
    let listener = TcpListener::bind(config.http_bind).await.context("bind HTTP listener")?;
    let http = listener.local_addr()?;
    tasks.push(tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("http server: {e}");
        }
    }));
    Ok(Running { http, media: advertise, core, plane, teamspeak, teamspeak_ready, admin_token, tasks })
}
