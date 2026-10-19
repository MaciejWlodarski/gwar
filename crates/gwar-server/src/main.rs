use std::{
    net::{IpAddr, SocketAddr, UdpSocket},
    path::PathBuf,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use gwar_proto::IceServer;
use gwar_server::{
    Config, TeamSpeakConfig,
    core::issue_token,
    store::{ADMIN_GROUP, Store},
    tls::Tls,
};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about = "Self-hosted voice server")]
struct Cli {
    /// Directory for the database and server state.
    #[arg(long, env = "GWAR_DATA_DIR", default_value = "data")]
    data_dir: PathBuf,
    /// HTTP(S)/WebSocket listen address [default: 0.0.0.0:443 with HTTPS, else 0.0.0.0:8790].
    #[arg(long, env = "GWAR_HTTP")]
    http: Option<SocketAddr>,
    /// Serve HTTPS for this domain with a certificate from Let's Encrypt,
    /// obtained and renewed automatically (repeat for more names).
    #[arg(long = "domain", env = "GWAR_DOMAIN", value_delimiter = ',')]
    domains: Vec<String>,
    /// Contact address for Let's Encrypt (expiry notices).
    #[arg(long, env = "GWAR_ACME_EMAIL")]
    acme_email: Option<String>,
    /// Use Let's Encrypt's staging environment (for testing; untrusted certificates).
    #[arg(long, env = "GWAR_ACME_STAGING", value_parser = clap::builder::BoolishValueParser::new())]
    acme_staging: bool,
    /// Serve HTTPS with this PEM certificate chain (instead of --domain).
    #[arg(long, env = "GWAR_TLS_CERT", requires = "tls_key", conflicts_with = "domains")]
    tls_cert: Option<PathBuf>,
    /// Private key for --tls-cert (PEM).
    #[arg(long, env = "GWAR_TLS_KEY", requires = "tls_cert")]
    tls_key: Option<PathBuf>,
    /// With HTTPS, don't redirect plain HTTP on port 80.
    #[arg(long, env = "GWAR_NO_HTTP_REDIRECT", value_parser = clap::builder::BoolishValueParser::new())]
    no_http_redirect: bool,
    /// Gwar Connect service whose revoked devices are refused.
    #[arg(long, env = "GWAR_CONNECT_URL", default_value = gwar_server::OFFICIAL_CONNECT_URL)]
    connect_url: String,
    /// Don't follow Gwar Connect (devices revoked there keep working here).
    #[arg(long, env = "GWAR_NO_CONNECT", value_parser = clap::builder::BoolishValueParser::new())]
    no_connect: bool,
    /// Extra web app origins allowed to upload files (the official one always is).
    #[arg(long = "web-origin", env = "GWAR_WEB_ORIGINS", value_delimiter = ',')]
    web_origins: Vec<String>,
    /// UDP address for WebRTC voice [default: 0.0.0.0:9987, or 0.0.0.0:9988 with --teamspeak,
    /// which leaves 9987 to TeamSpeak clients].
    #[arg(long, env = "GWAR_MEDIA")]
    media: Option<SocketAddr>,
    /// Public IP clients reach the media port on (default: primary local IP).
    #[arg(long, env = "GWAR_PUBLIC_IP")]
    public_ip: Option<IpAddr>,
    #[arg(long, env = "GWAR_MAX_CLIENTS", default_value_t = 128)]
    max_clients: u32,
    #[arg(long, env = "GWAR_SERVER_PASSWORD")]
    server_password: Option<String>,
    /// Also serve a built web client at `/` (development or private networks;
    /// people normally use the official web app or the desktop app).
    #[arg(long, env = "GWAR_WEB_ROOT")]
    web_root: Option<PathBuf>,
    /// JSON file with additional ICE (TURN) servers handed to clients.
    #[arg(long, env = "GWAR_ICE_SERVERS")]
    ice_servers: Option<PathBuf>,
    /// Let official TeamSpeak 3/6 clients in: downloads and runs the official
    /// TeamSpeak server and bridges it to this server. Its free license (32
    /// slots) is for non-commercial use only; you are responsible for complying.
    #[arg(long, env = "GWAR_TEAMSPEAK", value_parser = clap::builder::BoolishValueParser::new())]
    teamspeak: bool,
    /// Encrypt server-wide chat with history, only when setting up a new server.
    #[arg(long, env = "GWAR_ENCRYPTED_SERVER_CHAT", value_parser = clap::builder::BoolishValueParser::new())]
    encrypted_server_chat: bool,
    /// Confirms you accept the TeamSpeak server license (teamspeak.com; also the
    /// LICENSE file in <data-dir>/teamspeak/server-*/ once installed).
    #[arg(long, env = "GWAR_ACCEPT_TEAMSPEAK_LICENSE", value_parser = clap::builder::BoolishValueParser::new())]
    accept_teamspeak_license: bool,
    /// UDP address TeamSpeak clients connect to.
    #[arg(long, env = "GWAR_TEAMSPEAK_VOICE", default_value = "0.0.0.0:9987")]
    teamspeak_voice: SocketAddr,
    /// Public base URL of the web origin, e.g. https://voice.example.com
    /// (used in links to uploads and to bind login signatures).
    #[arg(long, env = "GWAR_PUBLIC_URL")]
    public_url: Option<String>,
    /// Accepted external WebSocket URL (repeat for aliases, including the path).
    #[arg(long = "public-endpoint", env = "GWAR_PUBLIC_ENDPOINTS", value_delimiter = ',')]
    public_endpoints: Vec<String>,
    /// Accept any signed endpoint. Unsafe: isolated local development/tests only.
    #[arg(long, env = "GWAR_INSECURE_ANY_HOST", value_parser = clap::builder::BoolishValueParser::new())]
    insecure_any_host: bool,
    /// Header your reverse proxy sets to the client's address, e.g. `x-real-ip`; believed
    /// only from loopback. Without it every proxied client shares the proxy's address.
    #[arg(long, env = "GWAR_CLIENT_IP_HEADER")]
    client_ip_header: Option<String>,
    /// Largest upload in MiB (0 disables uploads).
    #[arg(long, env = "GWAR_UPLOAD_LIMIT_MB", default_value_t = 25)]
    upload_limit_mb: u64,
    /// MiB of uploads one member may keep stored (0 = no limit).
    #[arg(long, env = "GWAR_UPLOAD_QUOTA_MB", default_value_t = 1024)]
    upload_quota_mb: u64,
    /// MiB all uploads together may take on disk (0 = no limit).
    #[arg(long, env = "GWAR_STORAGE_LIMIT_MB", default_value_t = 10240)]
    storage_limit_mb: u64,
    /// TCP address of TeamSpeak file transfer (icons, avatars, channel files).
    /// TeamSpeak tells clients a specific address as is, so bind one only if
    /// clients can reach it; the default lets them use the server's address.
    #[arg(long, env = "GWAR_TEAMSPEAK_FILETRANSFER", default_value = "0.0.0.0:30033")]
    teamspeak_filetransfer: SocketAddr,
    /// Loopback port of the TeamSpeak server's ServerQuery.
    #[arg(long, env = "GWAR_TEAMSPEAK_QUERY_PORT", default_value_t = 10011)]
    teamspeak_query_port: u16,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Print the persistent server identity public key (base64url).
    ServerKey,
    /// Print a new one-time token granting the Admin group.
    AdminToken,
    /// Write a consistent copy of the database to this file (safe while running).
    Backup { path: PathBuf },
    /// List or clean up the people this server has seen.
    Members {
        #[command(subcommand)]
        command: MembersCommand,
    },
    /// Set the server's owner (docs/e2e-build.md, section 6). Changes the database directly,
    /// so the server must be stopped.
    Owner {
        #[command(subcommand)]
        command: OwnerCommand,
    },
    /// Import a TeamSpeak 3 server's channels, roles, members and bans over ServerQuery
    /// (docs/teamspeak-import.md). The TeamSpeak server is only read. Without --dry-run this
    /// changes the database, so this server must be stopped; a copy of the database is kept first.
    /// The ServerQuery password is read from GWAR_TS_QUERY_PASSWORD or, with --password-stdin, stdin.
    /// Raw ServerQuery is unencrypted: reach a remote TeamSpeak server through an SSH tunnel.
    #[cfg(feature = "teamspeak")]
    ImportTeamspeak {
        /// ServerQuery address, host:port (raw query, usually 10011); this machine's unless
        /// --insecure-query.
        #[arg(long)]
        from: String,
        /// Allow a ServerQuery address on another machine: its password crosses the network in clear.
        #[arg(long)]
        insecure_query: bool,
        #[arg(long, default_value = "serveradmin")]
        user: String,
        /// Virtual server id on that instance.
        #[arg(long, default_value_t = 1)]
        sid: u32,
        #[arg(long)]
        password_stdin: bool,
        /// Show what the import would do (tried on a copy of the database), change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Also import clients without a role.
        #[arg(long)]
        all_clients: bool,
        /// Keep spacer channels as plain channels.
        #[arg(long)]
        spacers: bool,
        /// Passwords to keep: one `name = password` per line (a channel's path, its name, or `@server`).
        #[arg(long, conflicts_with = "drop_passwords")]
        passwords: Option<PathBuf>,
        /// Import the server and its password channels without a password.
        #[arg(long)]
        drop_passwords: bool,
        /// Write every password TeamSpeak has into this new file, to fill in for --passwords (with
        /// what --passwords already gives), then stop: nothing is imported.
        #[arg(long, conflicts_with = "drop_passwords")]
        passwords_template: Option<PathBuf>,
        /// Remove this server's own top-level channels first (the seeded Lobby, General, AFK).
        #[arg(long)]
        replace_channels: bool,
        /// With --replace-channels, also channels holding messages, subchannels or encrypted history.
        #[arg(long, requires = "replace_channels")]
        force: bool,
        /// This server runs with --teamspeak: the bridge sets the kept passwords on TeamSpeak
        /// (they wait in the database in plain text until it has).
        #[arg(long)]
        for_bridge: bool,
    },
}

#[derive(Subcommand)]
enum OwnerCommand {
    /// Make a stored member the owner of a server that has none.
    Set { uid: String },
    /// Start a new owner generation for a lost owner account. Clients show a neutral notice
    /// of a changed owner and ask before accepting it.
    Reset { uid: String },
}

#[derive(Subcommand)]
enum MembersCommand {
    /// Print uid, nickname, tag, Connect handle, last seen, groups and message count of every member
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
    /// Remove these members (by uid), whatever their roles. Not a ban. Like `prune`,
    /// the server must be stopped unless it is a --dry-run.
    Remove {
        #[arg(required = true)]
        uids: Vec<String>,
        /// Also delete their messages and files.
        #[arg(long)]
        delete_messages: bool,
        /// Only show who would be removed.
        #[arg(long)]
        dry_run: bool,
    },
    /// Move unsigned messages and member records onto another, keeping the target's nickname.
    /// Signed messages retain their original author uid and mentions.
    /// Both members must exist and the server must be stopped, even for --dry-run.
    Merge {
        from_uid: String,
        into_uid: String,
        /// Show what would move and roll the transaction back.
        #[arg(long)]
        dry_run: bool,
    },
    /// Link a TeamSpeak identity (`ts:<uid>`) to a member: joining with it acts as the member, and a
    /// member `ts:<uid>` gives them its roles (docs/teamspeak-import.md). The server must be stopped.
    Link { teamspeak: String, member_uid: String },
    /// Unlink a TeamSpeak identity; the member keeps the roles. The server must be stopped.
    Unlink { teamspeak: String },
}

fn members(data_dir: &std::path::Path, database: &std::path::Path, command: MembersCommand) -> Result<()> {
    use gwar_server::members::{self, PruneOptions};
    match command {
        MembersCommand::List { inactive_days } => {
            let store = Store::open(database)?;
            let rows = members::list(&store, inactive_days)?;
            print!("{}", members::format_list(&rows, &store.groups()?, gwar_server::core::now_ms()));
            eprintln!("{} members", rows.len());
        }
        MembersCommand::Prune { inactive_days, keep_groups: _, include_grouped, delete_messages, dry_run } => {
            // Only the real thing needs the server out of the way.
            let _lock = (!dry_run).then(|| members::lock_data_dir(data_dir)).transpose()?;
            let store = Store::open(database)?;
            let options = PruneOptions { inactive_days, include_grouped, delete_messages, dry_run };
            let report = members::prune(&store, &data_dir.join("files"), &options)?;
            print!("{}", members::format_list(&report.members, &store.groups()?, gwar_server::core::now_ms()));
            let verb = if report.dry_run { "would remove" } else { "removed" };
            eprintln!("{verb} {} members", report.members.len());
        }
        MembersCommand::Remove { uids, delete_messages, dry_run } => {
            let _lock = (!dry_run).then(|| members::lock_data_dir(data_dir)).transpose()?;
            let store = Store::open(database)?;
            let report = members::remove(&store, &data_dir.join("files"), &uids, delete_messages, dry_run)?;
            print!("{}", members::format_list(&report.members, &store.groups()?, gwar_server::core::now_ms()));
            let verb = if report.dry_run { "would remove" } else { "removed" };
            eprintln!("{verb} {} members", report.members.len());
        }
        MembersCommand::Merge { from_uid, into_uid, dry_run } => {
            let _lock = members::lock_data_dir(data_dir)?;
            let store = Store::open(database)?;
            let report = members::merge(&store, &from_uid, &into_uid, dry_run)?;
            let verb = if dry_run { "would merge" } else { "merged" };
            println!(
                "{verb} {from_uid} into {into_uid}: {} unsigned messages, {} files, {} read marks, {} group memberships, {} unsigned mentions, {} bans; signed messages keep their original author uid and mentions",
                report.messages, report.files, report.read_marks, report.groups, report.mentions, report.bans
            );
        }
        MembersCommand::Link { teamspeak, member_uid } => {
            let _lock = members::lock_data_dir(data_dir)?;
            let store = Store::open(database)?;
            match store.link_teamspeak(&teamspeak, &member_uid, gwar_server::core::now_ms())? {
                gwar_server::store::Link::Linked { member: true, groups, overrides } => println!(
                    "linked {teamspeak} to {member_uid}: {groups} roles and the overrides in {} channels moved over, the member {teamspeak} removed",
                    overrides.len()
                ),
                gwar_server::store::Link::Linked { member: false, .. } => {
                    println!("linked {teamspeak} to {member_uid}")
                }
                gwar_server::store::Link::Already => println!("{teamspeak} was already linked to {member_uid}"),
                gwar_server::store::Link::TooMany => {
                    anyhow::bail!("{member_uid} has as many TeamSpeak identities linked as allowed")
                }
                gwar_server::store::Link::Elsewhere => {
                    anyhow::bail!("{teamspeak} is linked to another member: unlink it first")
                }
            }
        }
        MembersCommand::Unlink { teamspeak } => {
            let _lock = members::lock_data_dir(data_dir)?;
            let store = Store::open(database)?;
            match store.unlink_teamspeak(&teamspeak)? {
                Some(member) => println!("unlinked {teamspeak} from {member}; the roles stay with {member}"),
                None => anyhow::bail!("{teamspeak} is not linked to anyone"),
            }
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
    let database = cli.data_dir.join("gwar.sqlite3");
    // Before the rename the database was vc.sqlite3. Starting with an empty one beside it would lose
    // every account, channel and role without a word, so stop and say how to move it.
    let legacy = cli.data_dir.join("vc.sqlite3");
    if legacy.exists() && !database.exists() {
        anyhow::bail!(
            "{} is from before the rename to Gwar: stop the server, rename it (and any vc.sqlite3-wal and \
             vc.sqlite3-shm beside it) to gwar.sqlite3, delete vc.lock, then start again",
            legacy.display()
        );
    }

    match cli.command {
        Some(Command::ServerKey) => {
            use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
            println!(
                "{}",
                URL_SAFE_NO_PAD.encode(gwar_server::server_key::load(&cli.data_dir)?.verifying_key().as_bytes())
            );
            return Ok(());
        }
        Some(Command::AdminToken) => {
            let token = issue_token(&Store::open(&database)?, ADMIN_GROUP, None)?;
            println!("{token}");
            return Ok(());
        }
        Some(Command::Backup { path }) => {
            Store::open(&database)?.backup_to(&path)?;
            println!("{}", path.display());
            return Ok(());
        }
        Some(Command::Members { command }) => return members(&cli.data_dir, &database, command),
        Some(Command::Owner { command }) => {
            let _lock = gwar_server::members::lock_data_dir(&cli.data_dir)?;
            let store = Store::open(&database)?;
            match command {
                OwnerCommand::Set { uid } => {
                    store.set_owner(&uid)?;
                    println!("{uid} is the owner (generation 1)");
                }
                OwnerCommand::Reset { uid } => {
                    let generation = store.reset_owner(&uid)?;
                    println!("{uid} is the owner of the new generation {generation}");
                }
            }
            return Ok(());
        }
        #[cfg(feature = "teamspeak")]
        Some(Command::ImportTeamspeak {
            from,
            insecure_query,
            user,
            sid,
            password_stdin,
            dry_run,
            all_clients,
            spacers,
            passwords,
            drop_passwords,
            passwords_template,
            replace_channels,
            force,
            for_bridge,
        }) => {
            use gwar_server::ts_import;
            let query_password = if password_stdin {
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                line.trim_end_matches(['\r', '\n']).to_owned()
            } else {
                std::env::var("GWAR_TS_QUERY_PASSWORD").context("set GWAR_TS_QUERY_PASSWORD or use --password-stdin")?
            };
            let options = ts_import::plan::Options {
                all_clients,
                spacers,
                drop_passwords,
                passwords: passwords.as_deref().map(ts_import::read_passwords).transpose()?.unwrap_or_default(),
            };
            let passwords_given = passwords.is_some();
            let addr = tokio::net::lookup_host(&from)
                .await
                .with_context(|| format!("resolve {from}"))?
                .next()
                .with_context(|| format!("no address for {from}"))?;
            if !addr.ip().is_loopback() && !insecure_query {
                anyhow::bail!(
                    "{from} is not this machine: raw ServerQuery sends the password unencrypted. Use an SSH tunnel \
                     (ssh -N -L 20011:127.0.0.1:10011 that-host, then --from 127.0.0.1:20011) or pass --insecure-query"
                );
            }
            let source = async {
                let (query, _notes) =
                    gwar_server::teamspeak::query::Query::connect(addr, &user, &query_password, sid).await?;
                ts_import::source::read(&query).await
            }
            .await
            .map_err(|e| match ts_import::guide::query_hint(&e, &from, sid) {
                Some(hint) => e.context(hint),
                None => e,
            })?;
            let source_id = source
                .server
                .get("virtualserver_unique_identifier")
                .cloned()
                .unwrap_or_else(|| format!("{from}/{sid}"));
            let plan = ts_import::plan::plan(&source, &options)?;
            if let Some(path) = passwords_template {
                ts_import::write_passwords_template(&path, &ts_import::plan::passwords_template(&plan))?;
                println!(
                    "Wrote {} ({} passwords). Type each after its \"=\", then run the import with --passwords {}.",
                    path.display(),
                    plan.password_keys.len(),
                    path.display()
                );
                return Ok(());
            }
            print!("{}", ts_import::plan::report(&plan));
            let apply_options = ts_import::apply::ApplyOptions { replace_channels, force, for_bridge };
            let summary = |applied: &ts_import::apply::Applied| {
                let mut out = format!(
                    "{} roles new, {} updated; {} channels new, {} updated; {} members new, {} updated; {} bans.",
                    applied.roles_created,
                    applied.roles_updated,
                    applied.channels_created,
                    applied.channels_updated,
                    applied.members_created,
                    applied.members_updated,
                    applied.bans,
                );
                if !applied.deleted.is_empty() {
                    out += &format!("\nChannels removed (--replace-channels): {}.", applied.deleted.join(", "));
                }
                for note in &applied.notes {
                    out += &format!("\n  - {note}");
                }
                out
            };
            if dry_run {
                // Tried on a copy: works while this server runs.
                let applied = ts_import::import(
                    &database,
                    &plan,
                    &source_id,
                    ts_import::apply::ApplyOptions { for_bridge: false, ..apply_options },
                    false,
                )?;
                println!("\nDry run, on a copy of the database: {}\nNothing changed.", summary(&applied));
                let server_running = gwar_server::members::data_dir_in_use(&cli.data_dir);
                print!(
                    "{}",
                    ts_import::guide::after_dry_run(&ts_import::guide::DryRun {
                        plan: &plan,
                        passwords_given,
                        drop_passwords,
                        for_bridge,
                        replace_channels,
                        server_running,
                    })
                );
                return Ok(());
            }
            let _lock = match gwar_server::members::lock_data_dir(&cli.data_dir) {
                Ok(lock) => lock,
                // The server is running: try the plan on a copy, then hand it to the server.
                Err(_) if gwar_server::control::available(&cli.data_dir) => {
                    let tried = ts_import::import(
                        &database,
                        &plan,
                        &source_id,
                        ts_import::apply::ApplyOptions { for_bridge: false, ..apply_options },
                        false,
                    )?;
                    println!("\nTried on a copy of the database first: {}", summary(&tried));
                    // Applied in steps, so the database as it was is kept first.
                    let backup =
                        cli.data_dir.join(format!("gwar.sqlite3.before-import-{}", gwar_server::core::now_ms()));
                    ts_import::copy_database(&database, &backup)?;
                    let advice = ts_import::guide::after_import(&plan, true, Some(&backup.display().to_string()));
                    let applied = gwar_server::control::import(&cli.data_dir, plan, source_id, apply_options).await?;
                    println!("\nApplied to the running server: {}", summary(&applied));
                    println!("The database as it was: {}", backup.display());
                    print!("{advice}");
                    return Ok(());
                }
                Err(e) => return Err(e),
            };
            // A server that never ran has no database yet: nothing to keep.
            let backup = database
                .exists()
                .then(|| cli.data_dir.join(format!("gwar.sqlite3.before-import-{}", gwar_server::core::now_ms())));
            if let Some(backup) = &backup {
                ts_import::copy_database(&database, backup)?;
            }
            let applied = ts_import::import(&database, &plan, &source_id, apply_options, true)?;
            println!("\nImported: {}", summary(&applied));
            if let Some(backup) = &backup {
                println!("The database before the import: {}", backup.display());
            }
            let backup = backup.map(|b| b.display().to_string());
            print!("{}", ts_import::guide::after_import(&plan, false, backup.as_deref()));
            return Ok(());
        }
        None => {}
    }
    // Held while the server runs, so `members prune` can tell.
    let _data_dir_lock = gwar_server::members::lock_data_dir(&cli.data_dir)?;

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
    let mut web_origins = vec![gwar_server::OFFICIAL_WEB_ORIGIN.to_owned()];
    web_origins.extend(public_url.clone());
    web_origins.extend(cli.web_origins.iter().map(|o| o.trim_end_matches('/').to_owned()));
    let ip = cli
        .public_ip
        .or_else(|| (!media.ip().is_unspecified()).then(|| media.ip()))
        .or_else(primary_ip)
        .context("cannot determine media IP; pass --public-ip")?;
    let running = gwar_server::start(Config {
        data_dir: Some(cli.data_dir.clone()),
        public_ip: cli.public_ip,
        insecure_any_host: cli.insecure_any_host,
        database: Some(database),
        http_bind,
        media_bind: media,
        media_advertise: SocketAddr::new(ip, media.port()),
        max_clients: cli.max_clients,
        encrypted_server_chat: cli.encrypted_server_chat,
        server_password: cli.server_password,
        web_root: cli.web_root,
        ice_servers,
        teamspeak: cli.teamspeak.then(|| TeamSpeakConfig {
            dir: cli.data_dir.join("teamspeak"),
            voice: cli.teamspeak_voice,
            query_port: cli.teamspeak_query_port,
            filetransfer: cli.teamspeak_filetransfer,
            public_url: public_url.clone(),
        }),
        public_url: public_url.clone(),
        public_endpoints: cli.public_endpoints,
        client_ip_header: cli.client_ip_header,
        upload_limit: cli.upload_limit_mb * 1024 * 1024,
        upload_quota: cli.upload_quota_mb.saturating_mul(1024 * 1024),
        storage_limit: cli.storage_limit_mb.saturating_mul(1024 * 1024),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_endpoint_is_repeatable_and_comma_delimited() {
        let cli = Cli::try_parse_from([
            "gwar-server",
            "--public-endpoint",
            "wss://b.example/ws,ws://192.168.1.2:8790/ws",
            "--public-endpoint",
            "ws://[::1]:8790/ws",
        ])
        .unwrap();
        assert_eq!(cli.public_endpoints, ["wss://b.example/ws", "ws://192.168.1.2:8790/ws", "ws://[::1]:8790/ws"]);
    }
}
