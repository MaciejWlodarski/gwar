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
    tls::Tls,
};

#[derive(Parser)]
#[command(version, about = "Self-hosted voice server")]
struct Cli {
    /// Directory for the database and server state.
    #[arg(long, env = "VC_DATA_DIR", default_value = "data")]
    data_dir: PathBuf,
    /// HTTP(S)/WebSocket listen address [default: 0.0.0.0:443 with HTTPS, else 0.0.0.0:8790].
    #[arg(long, env = "VC_HTTP")]
    http: Option<SocketAddr>,
    /// Serve HTTPS for this domain with a certificate from Let's Encrypt,
    /// obtained and renewed automatically (repeat for more names).
    #[arg(long = "domain", env = "VC_DOMAIN", value_delimiter = ',')]
    domains: Vec<String>,
    /// Contact address for Let's Encrypt (expiry notices).
    #[arg(long, env = "VC_ACME_EMAIL")]
    acme_email: Option<String>,
    /// Use Let's Encrypt's staging environment (for testing; untrusted certificates).
    #[arg(long, env = "VC_ACME_STAGING", value_parser = clap::builder::BoolishValueParser::new())]
    acme_staging: bool,
    /// Serve HTTPS with this PEM certificate chain (instead of --domain).
    #[arg(long, env = "VC_TLS_CERT", requires = "tls_key", conflicts_with = "domains")]
    tls_cert: Option<PathBuf>,
    /// Private key for --tls-cert (PEM).
    #[arg(long, env = "VC_TLS_KEY", requires = "tls_cert")]
    tls_key: Option<PathBuf>,
    /// With HTTPS, don't redirect plain HTTP on port 80.
    #[arg(long, env = "VC_NO_HTTP_REDIRECT", value_parser = clap::builder::BoolishValueParser::new())]
    no_http_redirect: bool,
    /// Gwar Connect service whose revoked devices are refused.
    #[arg(long, env = "VC_CONNECT_URL", default_value = vc_server::OFFICIAL_CONNECT_URL)]
    connect_url: String,
    /// Don't follow Gwar Connect (devices revoked there keep working here).
    #[arg(long, env = "VC_NO_CONNECT", value_parser = clap::builder::BoolishValueParser::new())]
    no_connect: bool,
    /// Extra web app origins allowed to upload files (the official one always is).
    #[arg(long = "web-origin", env = "VC_WEB_ORIGINS", value_delimiter = ',')]
    web_origins: Vec<String>,
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
    /// Also serve a built web client at `/` (development or private networks;
    /// people normally use the official web app or the desktop app).
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
    /// Public base URL of the web origin, e.g. https://voice.example.com
    /// (used in links to uploads posted to TeamSpeak).
    #[arg(long, env = "VC_PUBLIC_URL")]
    public_url: Option<String>,
    /// Largest upload in MiB (0 disables uploads).
    #[arg(long, env = "VC_UPLOAD_LIMIT_MB", default_value_t = 25)]
    upload_limit_mb: u64,
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
    /// Write a consistent copy of the database to this file (safe while running).
    Backup { path: PathBuf },
    /// List or clean up the people this server has seen.
    Members {
        #[command(subcommand)]
        command: MembersCommand,
    },
}

#[derive(Subcommand)]
enum MembersCommand {
    /// Print uid, last nickname, last seen, groups and message count of every member
    /// (safe while the server runs).
    List {
        /// Only members not seen for this many days.
        #[arg(long)]
        inactive_days: Option<u32>,
    },
    /// Remove members not seen for a while. Not a ban: they can join again as new members.
    /// Changes the database directly, so the server must be stopped (preview with --dry-run
    /// at any time). The server's own `member.prune` request does the same from the web app
    /// while it runs.
    Prune {
        /// Remove members not seen for at least this many days.
        #[arg(long)]
        inactive_days: u32,
        /// Spare members who hold a role besides the default one (the default).
        #[arg(long, conflicts_with = "include_grouped")]
        keep_groups: bool,
        /// Also remove members who hold roles.
        #[arg(long)]
        include_grouped: bool,
        /// Also delete their messages and files.
        #[arg(long)]
        delete_messages: bool,
        /// Only show who would be removed.
        #[arg(long)]
        dry_run: bool,
    },
}

fn members(data_dir: &std::path::Path, database: &std::path::Path, command: MembersCommand) -> Result<()> {
    use vc_server::members::{self, PruneOptions};
    match command {
        MembersCommand::List { inactive_days } => {
            let store = Store::open(database)?;
            let rows = members::list(&store, inactive_days)?;
            print!("{}", members::format_list(&rows, &store.groups()?, vc_server::core::now_ms()));
            eprintln!("{} members", rows.len());
        }
        MembersCommand::Prune { inactive_days, keep_groups: _, include_grouped, delete_messages, dry_run } => {
            // Only the real thing needs the server out of the way.
            let _lock = (!dry_run).then(|| members::lock_data_dir(data_dir)).transpose()?;
            let store = Store::open(database)?;
            let options = PruneOptions { inactive_days, include_grouped, delete_messages, dry_run };
            let report = members::prune(&store, &data_dir.join("files"), &options)?;
            print!("{}", members::format_list(&report.members, &store.groups()?, vc_server::core::now_ms()));
            let verb = if report.dry_run { "would remove" } else { "removed" };
            eprintln!("{verb} {} members", report.members.len());
        }
    }
    Ok(())
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

    match cli.command {
        Some(Command::AdminToken) => {
            let token = issue_token(&Store::open(&database)?, ADMIN_GROUP)?;
            println!("{token}");
            return Ok(());
        }
        Some(Command::Backup { path }) => {
            Store::open(&database)?.backup_to(&path)?;
            println!("{}", path.display());
            return Ok(());
        }
        Some(Command::Members { command }) => return members(&cli.data_dir, &database, command),
        None => {}
    }
    // Held while the server runs, so `members prune` can tell.
    let _data_dir_lock = vc_server::members::lock_data_dir(&cli.data_dir)?;

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
    let tls = match (&cli.tls_cert, &cli.tls_key, cli.domains.is_empty()) {
        (Some(cert), Some(key), _) => Some(Tls::Files { cert: cert.clone(), key: key.clone() }),
        (_, _, false) => Some(Tls::Acme {
            domains: cli.domains.clone(),
            email: cli.acme_email.clone(),
            cache: cli.data_dir.join("acme"),
            staging: cli.acme_staging,
        }),
        _ => None,
    };
    let http_bind = cli.http.unwrap_or_else(|| ([0, 0, 0, 0], if tls.is_some() { 443 } else { 8790 }).into());
    let redirect_http = (tls.is_some() && !cli.no_http_redirect).then(|| SocketAddr::from(([0, 0, 0, 0], 80)));
    // Links to uploads (posted to TeamSpeak) need the public origin.
    let public_url = cli.public_url.clone().or_else(|| cli.domains.first().map(|d| format!("https://{d}")));
    let mut web_origins = vec![vc_server::OFFICIAL_WEB_ORIGIN.to_owned()];
    web_origins.extend(public_url.clone());
    web_origins.extend(cli.web_origins.iter().map(|o| o.trim_end_matches('/').to_owned()));
    let ip = cli
        .public_ip
        .or_else(|| (!media.ip().is_unspecified()).then(|| media.ip()))
        .or_else(primary_ip)
        .context("cannot determine media IP; pass --public-ip")?;
    let running = vc_server::start(Config {
        database: Some(database),
        http_bind,
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
            public_url: public_url.clone(),
        }),
        public_url: public_url.clone(),
        upload_limit: cli.upload_limit_mb * 1024 * 1024,
        files_dir: Some(cli.data_dir.join("files")),
        web_origins,
        tls,
        redirect_http,
        connect_url: (!cli.no_connect).then(|| cli.connect_url.clone()),
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
