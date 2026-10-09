use std::{
    net::{IpAddr, SocketAddr, UdpSocket},
    path::PathBuf,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;
use vc_proto::IceServer;
use vc_server::{
    Config, TeamSpeakConfig,
    core::issue_token,
    store::{ADMIN_GROUP, Store},
};

#[derive(Parser)]
#[command(version, about = "Self-hosted voice server")]
struct Cli {
    /// Directory for the database and server state.
    #[arg(long, env = "VC_DATA_DIR", default_value = "data")]
    data_dir: PathBuf,
    /// HTTP/WebSocket listen address.
    #[arg(long, env = "VC_HTTP", default_value = "0.0.0.0:8790")]
    http: SocketAddr,
    /// UDP address for WebRTC voice [default: 0.0.0.0:9987, or 0.0.0.0:9988 with --teamspeak,
    /// which leaves 9987 to TeamSpeak clients].
    #[arg(long, env = "VC_MEDIA")]
    media: Option<SocketAddr>,
    /// Public IP clients reach the media port on (default: primary local IP).
    #[arg(long, env = "VC_PUBLIC_IP")]
    public_ip: Option<IpAddr>,
    #[arg(long, env = "VC_MAX_CLIENTS", default_value_t = 128)]
    max_clients: u32,
    #[arg(long, env = "VC_SERVER_PASSWORD")]
    server_password: Option<String>,
    /// Directory with the built web client to serve at `/`.
    #[arg(long, env = "VC_WEB_ROOT")]
    web_root: Option<PathBuf>,
    /// JSON file with additional ICE (TURN) servers handed to clients.
    #[arg(long, env = "VC_ICE_SERVERS")]
    ice_servers: Option<PathBuf>,
    /// Let official TeamSpeak 3/6 clients in: downloads and runs the official
    /// TeamSpeak server and bridges it to this server. Its free license (32
    /// slots) is for non-commercial use only; you are responsible for complying.
    #[arg(long, env = "VC_TEAMSPEAK", value_parser = clap::builder::BoolishValueParser::new())]
    teamspeak: bool,
    /// Confirms you accept the TeamSpeak server license (teamspeak.com; also the
    /// LICENSE file in <data-dir>/teamspeak/server-*/ once installed).
    #[arg(long, env = "VC_ACCEPT_TEAMSPEAK_LICENSE", value_parser = clap::builder::BoolishValueParser::new())]
    accept_teamspeak_license: bool,
    /// UDP address TeamSpeak clients connect to.
    #[arg(long, env = "VC_TEAMSPEAK_VOICE", default_value = "0.0.0.0:9987")]
    teamspeak_voice: SocketAddr,
    /// Loopback port of the TeamSpeak server's ServerQuery.
    #[arg(long, env = "VC_TEAMSPEAK_QUERY_PORT", default_value_t = 10011)]
    teamspeak_query_port: u16,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Print a new one-time token granting the Admin group.
    AdminToken,
}

/// The address the OS would route public traffic from; no packets are sent.
fn primary_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    Some(socket.local_addr().ok()?.ip())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,str0m=warn")))
        .init();
    let cli = Cli::parse();
    std::fs::create_dir_all(&cli.data_dir).with_context(|| format!("create {}", cli.data_dir.display()))?;
    let database = cli.data_dir.join("vc.sqlite3");

    if let Some(Command::AdminToken) = cli.command {
        let token = issue_token(&Store::open(&database)?, ADMIN_GROUP)?;
        println!("{token}");
        return Ok(());
    }

    let ice_servers: Vec<IceServer> = match &cli.ice_servers {
        Some(path) => serde_json::from_str(&std::fs::read_to_string(path)?).context("parse ICE servers")?,
        None => Vec::new(),
    };
    if cli.teamspeak && !cli.accept_teamspeak_license {
        anyhow::bail!(
            "--teamspeak runs the official TeamSpeak server, which requires accepting the TeamSpeak \
             server license (https://www.teamspeak.com). Its free license is for non-commercial use \
             only (commercial operators need a TeamSpeak license). Add --accept-teamspeak-license to confirm."
        );
    }
    let media = cli.media.unwrap_or_else(|| ([0, 0, 0, 0], if cli.teamspeak { 9988 } else { 9987 }).into());
    if cli.teamspeak && media.port() == cli.teamspeak_voice.port() {
        anyhow::bail!("--media and --teamspeak-voice need different UDP ports");
    }
    let ip = cli
        .public_ip
        .or_else(|| (!media.ip().is_unspecified()).then(|| media.ip()))
        .or_else(primary_ip)
        .context("cannot determine media IP; pass --public-ip")?;
    let running = vc_server::start(Config {
        database: Some(database),
        http_bind: cli.http,
        media_bind: media,
        media_advertise: SocketAddr::new(ip, media.port()),
        max_clients: cli.max_clients,
        server_password: cli.server_password,
        web_root: cli.web_root,
        ice_servers,
        teamspeak: cli.teamspeak.then(|| TeamSpeakConfig {
            dir: cli.data_dir.join("teamspeak"),
            voice: cli.teamspeak_voice,
            query_port: cli.teamspeak_query_port,
            filetransfer: ([127, 0, 0, 1], 30033).into(),
        }),
    })
    .await?;
    tracing::info!(http = %running.http, media = %running.media, teamspeak = ?running.teamspeak, "server ready");
    shutdown_signal().await?;
    tracing::info!("shutting down");
    let teamspeak = running.teamspeak.is_some();
    for task in &running.tasks {
        task.abort();
    }
    if teamspeak {
        // Dropping the bridge asks its supervisor to stop the TeamSpeak server
        // gracefully; give it a moment before the runtime goes away.
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    Ok(())
}

async fn shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}
