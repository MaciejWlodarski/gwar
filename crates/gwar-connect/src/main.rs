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
    #[arg(long = "origin", env = "GWAR_CONNECT_ORIGINS", value_delimiter = ',', default_value = "https://gwar.app")]
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
    /// Bug reports sent from the apps.
    #[command(subcommand)]
    Reports(Reports),
}

#[derive(Subcommand)]
enum Reports {
    /// Lists the newest reports.
    List {
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// Writes one report to a directory: report.txt (text and diagnostics) and its screenshots.
    Show { id: i64, out: PathBuf },
    /// Deletes a report and its screenshots.
    Delete { id: i64 },
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
        Some(Command::Reports(command)) => {
            reports(&Store::open(&database)?, command)?;
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

fn reports(store: &Store, command: Reports) -> Result<()> {
    let when = |ms: i64| {
        let secs = ms / 1000;
        format!("{} (unix {secs})", chrono_like(secs))
    };
    match command {
        Reports::List { limit } => {
            for (id, at, handle, text, images) in store.reports(limit)? {
                let first = text.lines().next().unwrap_or("").chars().take(80).collect::<String>();
                println!("#{id}  {}  @{}  {images} screenshot(s)  {first}", when(at), handle.as_deref().unwrap_or("-"));
            }
        }
        Reports::Show { id, out } => {
            let Some(((at, handle, device, text, diagnostics), images)) = store.report(id)? else {
                anyhow::bail!("no report #{id}");
            };
            std::fs::create_dir_all(&out)?;
            let diagnostics: serde_json::Value = serde_json::from_str(&diagnostics)?;
            let body = format!(
                "Report #{id}\nSent: {}\nAccount: @{}\nDevice: {}\n\n{text}\n\nDiagnostics:\n{}\n",
                when(at),
                handle.as_deref().unwrap_or("-"),
                device.as_deref().unwrap_or("-"),
                serde_json::to_string_pretty(&diagnostics)?
            );
            std::fs::write(out.join("report.txt"), body)?;
            for (i, (mime, data)) in images.iter().enumerate() {
                let ext = mime.strip_prefix("image/").unwrap_or("bin");
                std::fs::write(out.join(format!("screenshot-{}.{ext}", i + 1)), data)?;
            }
            println!("{}", out.display());
        }
        Reports::Delete { id } => {
            anyhow::ensure!(store.delete_report(id)?, "no report #{id}");
            println!("deleted #{id}");
        }
    }
    Ok(())
}

/// UTC `YYYY-MM-DD HH:MM` without a date library.
fn chrono_like(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02} UTC", rem / 3600, rem % 3600 / 60)
}
