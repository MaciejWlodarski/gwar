use std::{net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use gwar_connect::{Config, DESKTOP_ORIGINS, store::Store};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about = "Gwar Connect: accounts that keep one identity across devices")]
struct Cli {
    /// Directory for the database and service signing key.
    #[arg(long, env = "GWAR_CONNECT_DATA_DIR", default_value = "connect-data")]
    data_dir: PathBuf,
    /// Listen address (put HTTPS in front, e.g. a reverse proxy).
    #[arg(long, env = "GWAR_CONNECT_HTTP", default_value = "127.0.0.1:8900")]
    http: SocketAddr,
    /// Web app origins allowed to use the API from a browser (the desktop app's are always allowed).
    #[arg(
        long = "origin",
        env = "GWAR_CONNECT_ORIGINS",
        value_delimiter = ',',
        default_value = "https://gwar.app"
    )]
    origins: Vec<String>,
    /// Header your reverse proxy sets to the client's address, e.g. `x-real-ip`; believed
    /// only from loopback. Without it every proxied client shares the proxy's address.
    #[arg(long, env = "GWAR_CONNECT_CLIENT_IP_HEADER")]
    client_ip_header: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Writes a consistent copy of the database to this file.
    Backup { path: PathBuf },
    /// Prints the public handle signing key (creates it on first use).
    ServiceKey,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    let cli = Cli::parse();
    std::fs::create_dir_all(&cli.data_dir).with_context(|| format!("create {}", cli.data_dir.display()))?;
    let database = cli.data_dir.join("connect.sqlite3");
    match cli.command {
        Some(Command::Backup { path }) => {
            Store::open(&database)?.backup_to(&path)?;
            println!("{}", path.display());
            return Ok(());
        }
        Some(Command::ServiceKey) => {
            let key = gwar_connect::service_key::load(&cli.data_dir)?;
            println!("{}", gwar_connect::crypto::b64(key.verifying_key().as_bytes()));
            return Ok(());
        }
        None => {}
    }
    let mut origins = cli.origins;
    origins.extend(DESKTOP_ORIGINS.iter().map(|o| o.to_string()));
    let config = Config { database: Some(database), bind: cli.http, origins, client_ip_header: cli.client_ip_header };
    let addr = gwar_connect::start(config).await?;
    tracing::info!(%addr, "Gwar Connect ready");
    tokio::signal::ctrl_c().await?;
    Ok(())
}
