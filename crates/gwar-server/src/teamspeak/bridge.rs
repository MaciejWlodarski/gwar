//! The bridge between our core and the official TeamSpeak server.
//!
//! * Our channels, server name and welcome message are mirrored onto the
//!   TeamSpeak server through ServerQuery; ours stay the authority.
//! * Every TeamSpeak user becomes a remote session in our core (presence,
//!   channel, mute flags, chat), learned from ServerQuery notifications.
//! * Every user of ours gets a puppet connection (see [`super::puppet`]) that
//!   carries their voice and chat to TeamSpeak and hears TeamSpeak users.
//! * Dedicated query listeners hear channel chat even when no puppet is there.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use gwar_proto::{
    Channel, ChannelId, ChatMessage, ChatTarget, Client, Event, FileAccess, LeaveReason, Platform, ServerInfo,
    SessionId,
};
use rand::Rng;
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};
use tsclientlib::{Identity, MessageTarget};

use super::{
    channels::{self, BRIDGE_TALK_POWER, ChannelMap, TsChannel},
    install,
    listeners::{self, ChannelListeners},
    process::{ProcessConfig, TsProcess},
    puppet::{self, ChannelText, PuppetCmd, PuppetEvent, PuppetHandle, PuppetSpec, Shared},
    query::{Cmd, Query, QueryError, Record},
};
use crate::{
    TeamSpeakConfig,
    core::{
        CoreHandle, WatchUrl,
        bridge::{BridgeMsg, BridgeNote, RemoteClient, RemoteUpdate, ServerPassword},
        now_ms,
    },
    media::{AudioRoute, MediaPlane},
};

/// Name of the TeamSpeak server group that exempts puppets from flood limits.
const PUPPET_GROUP: &str = "Voice bridge";
/// TeamSpeak client description of every puppet.
const PUPPET_DESCRIPTION: &str = "Gwar user (bridged)";
/// Flood points one address may collect before TeamSpeak blocks it (default 250).
const PUPPET_IP_BLOCK: u32 = 2500;
/// How long a retired puppet stays down before the same user gets a new one.
const PUPPET_COOLDOWN: Duration = Duration::from_secs(3);
/// Connections one puppet identity may have at once (one per device of a member).
const PUPPET_CLONES: u32 = 100;
/// Powers of TeamSpeak's stock Server Admin group (ServerQuery has 100).
const SERVER_ADMIN_POWER: u32 = 75;
/// ServerQuery's own powers.
const QUERY_POWER: u32 = 100;
/// Join power needed for an encrypted channel: above every TeamSpeak group's.
const ENCRYPTED_JOIN_POWER: u32 = 1_000_000;
/// The fixed description of an encrypted channel on TeamSpeak (never who is in it).
const ENCRYPTED_DESCRIPTION: &str = "End-to-end encrypted. Join from Gwar.";

/// Installs, runs and bridges the TeamSpeak server for as long as the process
/// lives. `ready` is true while the bridge is fully in sync.
pub async fn run(config: TeamSpeakConfig, core: CoreHandle, plane: MediaPlane, ready: watch::Sender<bool>) {
    loop {
        if let Err(e) = supervise(&config, &core, &plane, &ready).await {
            warn!("teamspeak: {e:#}");
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

async fn supervise(
    config: &TeamSpeakConfig,
    core: &CoreHandle,
    plane: &MediaPlane,
    ready: &watch::Sender<bool>,
) -> Result<()> {
    std::fs::create_dir_all(&config.dir).with_context(|| format!("create {}", config.dir.display()))?;
    let installed = install::ensure_installed(&config.dir).await.context("install the TeamSpeak server")?;
    let password = secret(&config.dir.join("query-password"))?;
    let process = TsProcess::spawn(ProcessConfig {
        installed,
        state_dir: config.dir.join("state"),
        voice: config.voice,
        query_port: config.query_port,
        filetransfer: config.filetransfer,
        admin_password: password.clone(),
    })
    .await
    .context("start the TeamSpeak server")?;
    info!(voice = %config.voice, "TeamSpeak server running");
    let mut generation = process.generation();
    let mut identities = Identities::load(&config.dir.join("identities.json"))?;
    let voice_ip = match config.voice.ip() {
        ip if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        ip => ip,
    };
    let link = Link {
        query: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), config.query_port),
        password,
        voice: SocketAddr::new(voice_ip, config.voice.port()),
        map: config.dir.join("channels.json"),
        server_password: config.dir.join("server-password.json"),
        public_url: config.public_url.clone(),
    };
    loop {
        generation.borrow_and_update();
        let shared = Arc::new(Shared::new(core.clone(), plane.clone()));
        let result = Bridge::run(&link, core, shared, &mut identities, ready).await;
        ready.send_replace(false);
        if let Err(e) = result {
            warn!("teamspeak bridge: {e:#}");
        }
        tokio::select! {
            _ = generation.changed() => {}
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
    }
}

struct Link {
    public_url: Option<String>,
    query: SocketAddr,
    password: String,
    voice: SocketAddr,
    map: PathBuf,
    /// The server password set on TeamSpeak ([`SavedPassword`]).
    server_password: PathBuf,
}

/// The plain text of the server password the bridge set on TeamSpeak, and the hash of ours it
/// stands for: our puppets need it to connect, also after a restart, and TeamSpeak keeps only its
/// own hash. Readable only by us, like the ServerQuery password next to it.
#[derive(serde::Serialize, serde::Deserialize)]
struct SavedPassword {
    hash: String,
    password: String,
}

/// Reads or creates a random secret file readable only by us.
fn secret(path: &Path) -> Result<String> {
    if let Ok(value) = std::fs::read_to_string(path) {
        return Ok(value.trim().to_owned());
    }
    let value: String = rand::rng().sample_iter(rand::distr::Alphanumeric).take(24).map(char::from).collect();
    write_private(path, value.as_bytes())?;
    Ok(value)
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    std::io::Write::write_all(&mut options.open(&tmp)?, data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// TeamSpeak identities of our users' puppets, by our uid.
struct Identities {
    path: PathBuf,
    by_uid: BTreeMap<String, Identity>,
}

impl Identities {
    fn load(path: &Path) -> Result<Self> {
        let by_uid = match std::fs::read(path) {
            Ok(data) => serde_json::from_slice(&data).context("invalid puppet identities file")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self { path: path.to_owned(), by_uid })
    }

    fn get(&mut self, uid: &str) -> Result<Identity> {
        if let Some(identity) = self.by_uid.get(uid) {
            return Ok(identity.clone());
        }
        let identity = Identity::create();
        self.by_uid.insert(uid.to_owned(), identity.clone());
        write_private(&self.path, &serde_json::to_vec(&self.by_uid)?)?;
        Ok(identity)
    }

    /// TeamSpeak uids of every puppet we ever created.
    fn ts_uids(&self) -> HashSet<String> {
        self.by_uid.values().map(ts_uid).collect()
    }
}

fn ts_uid(identity: &Identity) -> String {
    identity.key().to_pub().get_uid()
}

/// One of our users, as mirrored on TeamSpeak.
struct Local {
    client: Client,
    ts_uid: String,
    puppet: Option<PuppetHandle>,
    /// Bumped per puppet, so events of a puppet being torn down are ignored.
    generation: u64,
    /// Bumped per puppet and per move: channel text is queued for one route.
    route: u64,
    clid: Option<u16>,
    retry_at: Option<Instant>,
    /// Since when the puppet has had nothing to do (see [`LINGER`]).
    idle_since: Option<Instant>,
    /// A private conversation with a TeamSpeak user keeps the puppet until then.
    chatting_until: Option<Instant>,
    /// In the puppets' group (which gives the talk power), retried until it is.
    grouped: bool,
}

/// How one of our users appears on TeamSpeak.
///
/// Slots are scarce (32 with the free license, shared with TeamSpeak users),
/// so only users who share a channel with TeamSpeak users (or talk to one
/// privately) get a puppet; everyone else is listed in the channel
/// description. A later layer can add a mixed stand-in for when the server
/// is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Presence {
    Puppet,
    Listed,
    /// On our server but not in voice: nothing on TeamSpeak.
    Absent,
}

/// How long an unneeded puppet stays, so people moving around don't make it flap.
const LINGER: Duration = Duration::from_secs(20);
/// How long a private conversation keeps a puppet alive after the last message.
const CHAT_KEEP: Duration = Duration::from_secs(600);

/// A share's TeamSpeak watch link, as the bridge keeps it: in memory only, to tell TeamSpeak users
/// who enter the channel later (docs/screen-share.md, "TeamSpeak watch links").
struct WatchPost {
    channel: ChannelId,
    /// The post: `🖥 {name} is sharing their screen. Watch: {url}`.
    text: String,
    /// TeamSpeak users who were in the channel when it was posted, or were told since.
    told: HashSet<u16>,
}

/// A TeamSpeak user, mirrored as a remote session in our core.
struct Remote {
    session: SessionId,
    nickname: String,
    cid: u64,
    muted: bool,
    deafened: bool,
    away: Option<String>,
    /// Whether TeamSpeak has them as a talker, as far as we set it (`None`: unknown, after a move).
    talker: Option<bool>,
    /// Their TeamSpeak database id: whispering other TeamSpeak users is a permission on it.
    cldbid: Option<u64>,
}

struct Bridge<'a> {
    query: Query,
    core: CoreHandle,
    shared: Arc<Shared>,
    voice: SocketAddr,
    public_url: Option<String>,
    identities: &'a mut Identities,
    puppet_uids: HashSet<String>,
    map: ChannelMap,
    ts: HashMap<u64, TsChannel>,
    channels: BTreeMap<ChannelId, Channel>,
    server: Option<ServerInfo>,
    locals: HashMap<SessionId, Local>,
    remotes: HashMap<u16, Remote>,
    puppet_clids: HashMap<u16, SessionId>,
    puppet_events: mpsc::Sender<PuppetEvent>,
    puppet_group: Option<u64>,
    listeners: ChannelListeners,
    /// Last description written per TeamSpeak channel (who is here without a puppet).
    descriptions: HashMap<u64, String>,
    /// Our own ServerQuery client id, once looked up.
    query_clid: Option<u64>,
    /// Encrypted channels whose join power was set since the bridge started.
    locked: HashSet<u64>,
    /// The server password set on TeamSpeak, for our puppets to connect with.
    server_password: Option<String>,
    password_file: PathBuf,
    /// Who of the TeamSpeak users may talk, and in which of our channels, as our side last said
    /// ([`BridgeNote::Talker`]).
    talkers: HashMap<SessionId, (ChannelId, bool)>,
    /// The whisper right as last set per TeamSpeak identity (see `sync_whisper`).
    whispers: HashMap<u64, bool>,
    /// TeamSpeak users a refused move left in place, with that channel: a second refusal there
    /// means our side will not take them in it either.
    held: HashMap<u16, u64>,
    /// Working watch links of shares in bridged channels, by share.
    links: HashMap<String, WatchPost>,
}

/// Runs `channelcreate` and lets the stock Server Admin power manage the
/// channel's permissions. A channel takes its creator's powers as the ones
/// needed to manage it, and ServerQuery's 100 would lock out every TeamSpeak
/// admin, so this sets what one of them would have; deleting stays ours (it
/// would only come back), so its needed power remains. An encrypted channel's
/// permissions stay ServerQuery's (see [`lock_encrypted`]). A channel whose power
/// cannot be set is deleted again, and the next sync creates it anew.
async fn create_channel(query: &Query, create: Cmd, encrypted: bool) -> Result<u64> {
    let created = query.call(create).await?;
    let cid = created.first().and_then(|r| r.get("cid")?.parse().ok()).context("channelcreate returned no cid")?;
    let result = if encrypted {
        lock_encrypted(query, cid).await
    } else {
        let perm = Cmd::new("channeladdperm")
            .arg("cid", cid)
            .arg("permsid", "i_channel_needed_permission_modify_power")
            .arg("permvalue", SERVER_ADMIN_POWER);
        query.call(perm).await.map(drop).context("set the permission power of a new TeamSpeak channel")
    };
    if let Err(e) = result {
        let _ = query.call(Cmd::new("channeldelete").arg("cid", cid).arg("force", 1)).await;
        return Err(e);
    }
    Ok(cid)
}

/// A puppet's TeamSpeak channel and whether it speaks, from the user's channel
/// (encrypted, its TeamSpeak channel) and the default channel's.
fn target_of(own: Option<(bool, Option<u64>)>, default: Option<u64>) -> (u64, bool) {
    match own {
        Some((false, Some(cid))) => (cid, true),
        _ => (default.unwrap_or(0), false),
    }
}

/// Keeps TeamSpeak users out of an encrypted channel: its chat is not bridged.
/// Only ServerQuery may change its permissions, so no TeamSpeak admin can lower the lock.
async fn lock_encrypted(query: &Query, cid: u64) -> Result<()> {
    let perm = Cmd::new("channeladdperm")
        .arg("cid", cid)
        .arg("permsid", "i_channel_needed_permission_modify_power")
        .arg("permvalue", QUERY_POWER);
    query.call(perm).await.context("keep the permissions of an encrypted TeamSpeak channel to ServerQuery")?;
    let perm = Cmd::new("channeladdperm")
        .arg("cid", cid)
        .arg("permsid", "i_channel_needed_join_power")
        .arg("permvalue", ENCRYPTED_JOIN_POWER);
    query.call(perm).await.context("set the join power of an encrypted TeamSpeak channel")?;
    Ok(())
}

/// One of the virtual server's properties (`serverinfo`).
async fn server_field(query: &Query, name: &str) -> Result<String> {
    let info = query.call(Cmd::new("serverinfo")).await?;
    Ok(info.first().and_then(|r| r.get(name)).cloned().unwrap_or_default())
}

/// Makes sure the virtual server takes clients only once the bridge has set its password: it
/// never starts by itself, and one that did (its first run, or set up before this) stops now,
/// sending away whoever it let in. Until the bridge starts it, TeamSpeak is closed, also when
/// setting the bridge up fails.
async fn seal(query: &Query) -> Result<()> {
    if server_field(query, "virtualserver_autostart").await? == "0" {
        return Ok(());
    }
    if server_field(query, "virtualserver_status").await? == "online" {
        query.call(Cmd::new("serverstop").arg("sid", 1)).await?;
        query.call(Cmd::new("use").arg("sid", 1).flag("-virtual")).await?;
    }
    query.call(Cmd::new("serveredit").arg("virtualserver_autostart", 0)).await?;
    Ok(())
}

impl<'a> Bridge<'a> {
    async fn run(
        link: &Link,
        core: &CoreHandle,
        shared: Arc<Shared>,
        identities: &'a mut Identities,
        ready: &watch::Sender<bool>,
    ) -> Result<()> {
        let (query, mut notifications) =
            // The virtual server waits, stopped, for its password (`set_server_password`).
            Query::connect_stopped(link.query, "serveradmin", &link.password, 1).await.context("connect ServerQuery")?;
        // First of all, before anything here can fail: a server that started by itself stops.
        seal(&query).await.context("stop the TeamSpeak server until its password is set")?;
        let (puppet_events, mut puppet_rx) = mpsc::channel(256);
        let (listener_events, mut listener_rx) = mpsc::channel(256);
        let mut bridge = Bridge {
            query,
            core: core.clone(),
            shared,
            voice: link.voice,
            public_url: link.public_url.clone(),
            puppet_uids: identities.ts_uids(),
            identities,
            map: ChannelMap::load(&link.map)?,
            ts: HashMap::new(),
            channels: BTreeMap::new(),
            server: None,
            locals: HashMap::new(),
            remotes: HashMap::new(),
            puppet_clids: HashMap::new(),
            puppet_events,
            puppet_group: None,
            listeners: ChannelListeners::new(link.query, link.password.clone(), listener_events),
            descriptions: HashMap::new(),
            query_clid: None,
            locked: HashSet::new(),
            server_password: None,
            password_file: link.server_password.clone(),
            held: HashMap::new(),
            talkers: HashMap::new(),
            whispers: HashMap::new(),
            links: HashMap::new(),
        };
        let result = bridge.serve(&mut notifications, &mut puppet_rx, &mut listener_rx, ready).await;
        // The watch links' only copies go with this bridge, and the links stop working.
        bridge.links.clear();
        // Hand the TeamSpeak users back; the next attach starts from scratch.
        for (clid, remote) in bridge.remotes.drain() {
            bridge.shared.forget(clid);
            bridge.core.bridge(BridgeMsg::Leave(remote.session, LeaveReason::Timeout)).await;
        }
        bridge.core.bridge(BridgeMsg::Detached).await;
        result
    }

    async fn serve(
        &mut self,
        notifications: &mut mpsc::Receiver<super::query::Notification>,
        puppet_rx: &mut mpsc::Receiver<PuppetEvent>,
        listener_rx: &mut mpsc::Receiver<listeners::Text>,
        ready: &watch::Sender<bool>,
    ) -> Result<()> {
        let (tx, mut notes) = mpsc::unbounded_channel();
        self.core.bridge(BridgeMsg::Attach(tx)).await;
        let Some(BridgeNote::Snapshot { server, channels, clients, password }) = notes.recv().await else {
            bail!("core did not send a snapshot");
        };
        // Before anyone connects: TeamSpeak starts only with it, and our puppets need it.
        self.set_server_password(password, true).await?;
        self.configure_server(&server).await?;
        self.sync_channels(channels).await?;
        for event in ["server", "textserver"] {
            self.query.call(Cmd::new("servernotifyregister").arg("event", event)).await?;
        }
        self.query.call(Cmd::new("servernotifyregister").arg("event", "channel").arg("id", 0)).await?;
        self.load_remotes().await?;
        for client in clients {
            self.add_local(client);
        }
        self.reconcile(Duration::from_millis(150));
        info!(channels = self.channels.len(), remotes = self.remotes.len(), "TeamSpeak bridge ready");
        ready.send_replace(true);

        let mut sweep = tokio::time::interval(Duration::from_millis(100));
        let mut poll = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                note = notes.recv() => match note {
                    Some(BridgeNote::ServerPassword(password)) => {
                        // Not set, TeamSpeak may be open (or keep an old one): the next bridge tries
                        // again, with the queued plain text.
                        self.set_server_password(password, false).await.context("set the TeamSpeak server password")?;
                    }
                    Some(note) => self.on_note(note).await,
                    None => bail!("core stopped"),
                },
                notification = notifications.recv() => match notification {
                    Some(n) => self.on_notification(n).await,
                    None => bail!("ServerQuery connection closed"),
                },
                Some(event) = puppet_rx.recv() => self.on_puppet(event).await,
                Some(text) = listener_rx.recv() => self.on_listener(text).await,
                _ = sweep.tick() => self.shared.sweep(),
                _ = poll.tick() => self.poll().await,
            }
        }
    }

    // ---------------------------------------------------------------- setup

    async fn configure_server(&mut self, server: &ServerInfo) -> Result<()> {
        self.query
            .call(
                Cmd::new("serveredit")
                    .arg("virtualserver_name", &server.name)
                    .arg("virtualserver_welcomemessage", &server.welcome)
                    .arg("virtualserver_weblist_enabled", 0),
            )
            .await?;
        self.server = Some(server.clone());
        // TeamSpeak moves to another port when ours is taken and remembers it;
        // insist on the configured one so clients find the server.
        let mut info = self.query.call(Cmd::new("serverinfo")).await?;
        let port = |info: &[Record]| info.first().and_then(|r| r.get("virtualserver_port")?.parse::<u16>().ok());
        if port(&info) != Some(self.voice.port()) {
            warn!(port = self.voice.port(), "moving the TeamSpeak server back to its configured port");
            self.query.call(Cmd::new("serveredit").arg("virtualserver_port", self.voice.port())).await?;
            // The new port only takes effect when the virtual server restarts.
            self.query.call(Cmd::new("serverstop").arg("sid", 1)).await?;
            let started = self.query.call(Cmd::new("serverstart").arg("sid", 1)).await;
            started.with_context(|| format!("TeamSpeak cannot use UDP port {}", self.voice.port()))?;
            self.query.call(Cmd::new("use").arg("sid", 1)).await?;
            info = self.query.call(Cmd::new("serverinfo")).await?;
        }
        let online = info.first().and_then(|r| r.get("virtualserver_status")).map(String::as_str) == Some("online");
        if port(&info) != Some(self.voice.port()) || !online {
            bail!("TeamSpeak cannot use UDP port {} (is another program using it?)", self.voice.port());
        }
        // Channels are managed on our side; TeamSpeak guests may not create any.
        // TeamSpeak's flood guard adds up everyone behind one address, and all
        // our puppets share ours: a few users joining at once would get it
        // blocked. Raise the per-address threshold; per-client limits stay.
        let ip_block = info
            .first()
            .and_then(|r| r.get("virtualserver_antiflood_points_needed_ip_block")?.parse::<u32>().ok())
            .unwrap_or(0);
        if ip_block < PUPPET_IP_BLOCK {
            self.query
                .call(Cmd::new("serveredit").arg("virtualserver_antiflood_points_needed_ip_block", PUPPET_IP_BLOCK))
                .await?;
        }
        let guest = info
            .first()
            .and_then(|r| r.get("virtualserver_default_server_group")?.parse::<u64>().ok())
            .context("serverinfo has no default server group")?;
        for perm in ["b_channel_create_temporary", "b_channel_create_semi_permanent", "b_channel_create_permanent"] {
            ignore_missing(
                self.query.call(Cmd::new("servergroupdelperm").arg("sgid", guest).arg("permsid", perm)).await,
            )?;
        }
        // Whispering a TeamSpeak user takes the bridge's whisper power, which only talkers
        // (see `apply_talker`) and our puppets have: a user who may not speak whispers nobody there.
        self.query
            .call(
                Cmd::new("servergroupaddperm")
                    .arg("sgid", guest)
                    .arg("permsid", "i_client_needed_whisper_power")
                    .arg("permvalue", BRIDGE_TALK_POWER)
                    .arg("permnegated", 0)
                    .arg("permskip", 0),
            )
            .await?;
        // A client in any server group loses the default (guest) group, so the
        // puppets' group gets the guest permissions copied in on every start
        // (keeping its members), plus the exemptions below.
        let groups = self.query.call(Cmd::new("servergrouplist")).await?;
        let existing = groups
            .iter()
            .find(|g| {
                g.get("name").map(String::as_str) == Some(PUPPET_GROUP)
                    && g.get("type").map(String::as_str) == Some("1")
            })
            .and_then(|g| g.get("sgid")?.parse::<u64>().ok());
        let copy = Cmd::new("servergroupcopy")
            .arg("ssgid", guest)
            .arg("tsgid", existing.unwrap_or(0))
            .arg("name", PUPPET_GROUP)
            .arg("type", 1);
        let copied = self.query.call(copy).await?;
        let sgid: u64 = match existing {
            Some(sgid) => sgid,
            None => {
                copied.first().and_then(|r| r.get("sgid")?.parse().ok()).context("servergroupcopy returned no sgid")?
            }
        };
        // Puppets all connect from our address; a flood ban triggered by our
        // users coming and going must not lock them out. Our core already
        // checked channel passwords and limits before a puppet follows its user.
        for (perm, value) in [
            ("b_client_ignore_antiflood", 1),
            ("b_client_ignore_bans", 1),
            ("b_channel_join_ignore_password", 1),
            ("b_channel_join_ignore_maxclients", 1),
            // Our users may write to everyone.
            ("b_client_server_textmessage_send", 1),
            // Mirrored channels need more talk power than anyone else has; our core
            // already decided who of our users is heard. The same for whispers, and anyone
            // may whisper a puppet (our core decides whether that reaches its user).
            ("i_client_talk_power", BRIDGE_TALK_POWER),
            ("i_client_whisper_power", BRIDGE_TALK_POWER),
            ("i_client_needed_whisper_power", 0),
            // Every device of a member has a puppet with the member's identity,
            // and a guest identity may be connected only once.
            ("i_client_max_clones_uid", PUPPET_CLONES),
        ] {
            self.query
                .call(
                    Cmd::new("servergroupaddperm")
                        .arg("sgid", sgid)
                        .arg("permsid", perm)
                        .arg("permvalue", value)
                        .arg("permnegated", 0)
                        .arg("permskip", 0),
                )
                .await?;
        }
        self.puppet_group = Some(sgid);
        Ok(())
    }

    /// Gives TeamSpeak the server password ours has. Without the plain text, the one set before
    /// stays if it is still ours; otherwise TeamSpeak gets a random one: closed until the password
    /// is set again on our side, never open where ours is not.
    ///
    /// At `startup` the bridge also starts the virtual server, which never starts by itself (no
    /// autostart, see [`seal`]): it takes clients only once its password is ours.
    async fn set_server_password(&mut self, ours: ServerPassword, startup: bool) -> Result<()> {
        let (password, typed) = match ours.hash.clone() {
            None => (None, false),
            Some(hash) => {
                let saved = std::fs::read(&self.password_file)
                    .ok()
                    .and_then(|data| serde_json::from_slice::<SavedPassword>(&data).ok())
                    .filter(|saved| saved.hash == hash)
                    .map(|saved| saved.password);
                let typed = ours.plaintext.is_some();
                let password = ours.plaintext.clone().or(saved).unwrap_or_else(|| {
                    warn!(
                        "the server has a password TeamSpeak does not know: TeamSpeak users are kept out \
                         until it is set again in the server settings"
                    );
                    random_password()
                });
                (Some(password), typed)
            }
        };
        // Stopped unless the bridge started it before (see `seal`).
        let running = startup && server_field(&self.query, "virtualserver_status").await? == "online";
        let edit = Cmd::new("serveredit")
            .arg("virtualserver_password", password.as_deref().unwrap_or(""))
            .arg("virtualserver_autostart", 0);
        self.query.call(edit).await?;
        self.server_password = password.clone();
        match (&ours.hash, &password) {
            (Some(hash), Some(password)) => write_private(
                &self.password_file,
                &serde_json::to_vec(&SavedPassword { hash: hash.clone(), password: password.clone() })?,
            )?,
            _ => match std::fs::remove_file(&self.password_file) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            },
        }
        if startup && !running {
            self.query.call(Cmd::new("serverstart").arg("sid", 1)).await.context("start the TeamSpeak server")?;
            self.query.call(Cmd::new("use").arg("sid", 1)).await?;
        }
        if let (true, Some(hash), Some(password)) = (typed, ours.hash, password) {
            self.core.bridge(BridgeMsg::ServerPasswordSet { hash, password }).await;
        }
        Ok(())
    }

    async fn sync_channels(&mut self, channels: Vec<Channel>) -> Result<()> {
        self.channels = channels.into_iter().map(|c| (c.id, c)).collect();
        let list = self
            .query
            .call(Cmd::new("channellist").flag("-topic").flag("-flags").flag("-limits").flag("-voice"))
            .await?;
        self.ts = list.iter().filter_map(TsChannel::from_record).map(|c| (c.cid, c)).collect();
        let (channels, ts) = (&self.channels, &self.ts);
        self.map.retain(|ours, cid| channels.contains_key(&ours) && ts.contains_key(&cid));
        if let (Some(server), Some(default)) = (&self.server, self.ts.values().find(|c| c.default)) {
            self.map.link(server.default_channel, default.cid);
        }
        for id in channels::tree_order(&self.channels) {
            let channel = self.channels[&id].clone();
            if let Err(e) = self.ensure_channel(&channel, None).await {
                warn!(channel = id, "TeamSpeak channel sync: {e:#}");
            }
        }
        // Channels someone created on the TeamSpeak side do not exist for us.
        let stray: Vec<u64> =
            self.ts.values().filter(|c| self.map.ours(c.cid).is_none() && !c.default).map(|c| c.cid).collect();
        for cid in stray {
            ignore_missing(self.query.call(Cmd::new("channeldelete").arg("cid", cid).arg("force", 1)).await)?;
            self.ts.remove(&cid);
        }
        self.map.save()
    }

    /// Creates or updates the TeamSpeak channel for `channel`. `password` is
    /// the plaintext when it changed on our side.
    async fn ensure_channel(&mut self, channel: &Channel, password: Option<Option<String>>) -> Result<()> {
        let parent = channel.parent.and_then(|p| self.map.ts(p)).unwrap_or(0);
        let siblings = channels::siblings(&self.channels, channel.parent);
        let order = siblings
            .iter()
            .position(|&c| c == channel.id)
            .and_then(|i| i.checked_sub(1))
            .and_then(|i| self.map.ts(siblings[i]))
            .unwrap_or(0);
        let mut want = channels::desired(channel, parent, order);
        // Without the plaintext a protected channel stays locked on TeamSpeak
        // until its password is set again on our side.
        let password = password.or_else(|| match self.map.ts(channel.id).and_then(|cid| self.ts.get(&cid)) {
            Some(current) if current.password == channel.has_password => None,
            _ => Some(channel.has_password.then(random_password)),
        });

        let Some(cid) = self.map.ts(channel.id) else {
            let mut cmd = Cmd::new("channelcreate")
                .arg("channel_name", &want.name)
                .arg("channel_topic", &want.topic)
                .arg("channel_flag_permanent", 1)
                .arg("cpid", parent)
                .arg("channel_order", order)
                .arg("channel_codec", 4)
                .arg("channel_codec_quality", 10)
                .arg("channel_needed_talk_power", want.talk_power);
            for (key, value) in channels::max_clients_args(want.max_clients) {
                cmd = cmd.arg(key, value);
            }
            if let Some(Some(p)) = &password {
                cmd = cmd.arg("channel_password", p);
            }
            let cid = create_channel(&self.query, cmd, channel.encrypted).await?;
            want.cid = cid;
            want.password = matches!(password, Some(Some(_)));
            self.ts.insert(cid, want);
            self.map.link(channel.id, cid);
            return self.map.save();
        };

        let current = self.ts.get(&cid).cloned().unwrap_or_default();
        if channel.encrypted && !self.locked.contains(&cid) {
            lock_encrypted(&self.query, cid).await?;
            self.locked.insert(cid);
        }
        // On its own: a rejected rename or limit must not leave the channel open to every talker.
        if current.talk_power != want.talk_power {
            let cmd = Cmd::new("channeledit").arg("cid", cid).arg("channel_needed_talk_power", want.talk_power);
            if let Err(e) = self.query.call(cmd).await {
                warn!(cid, "TeamSpeak channel talk power: {e:#}");
                // Tried again on the next change or sync.
                want.talk_power = current.talk_power;
            }
        }
        let mut args = channels::edits(&current, &want);
        if let Some(p) = &password {
            args.push(("channel_password", p.clone().unwrap_or_default()));
        }
        if !args.is_empty() {
            let mut cmd = Cmd::new("channeledit").arg("cid", cid);
            for (key, value) in args {
                cmd = cmd.arg(key, value);
            }
            self.query.call(cmd).await?;
        }
        if !current.default && (current.parent != want.parent || current.order != want.order) {
            self.query.call(Cmd::new("channelmove").arg("cid", cid).arg("cpid", parent).arg("order", order)).await?;
        }
        want.cid = cid;
        want.default = current.default;
        want.password = password.map_or(current.password, |p| p.is_some());
        self.ts.insert(cid, want);
        Ok(())
    }

    async fn delete_channel(&mut self, channel: ChannelId) {
        self.channels.remove(&channel);
        let Some(cid) = self.map.unlink_ours(channel) else { return };
        self.ts.remove(&cid);
        if let Err(e) = ignore_missing(self.query.call(Cmd::new("channeldelete").arg("cid", cid).arg("force", 1)).await)
        {
            warn!(channel, "TeamSpeak channel delete: {e:#}");
        }
        let _ = self.map.save();
        self.reconcile(Duration::ZERO);
    }

    async fn load_remotes(&mut self) -> Result<()> {
        let list =
            self.query.call(Cmd::new("clientlist").flag("-uid").flag("-away").flag("-voice").flag("-info")).await?;
        for record in list {
            self.add_remote(&record).await;
        }
        Ok(())
    }

    // -------------------------------------------------------------- remotes

    async fn add_remote(&mut self, r: &Record) {
        let Some(clid) = r.get("clid").and_then(|v| v.parse::<u16>().ok()) else { return };
        let uid = r.get("client_unique_identifier").cloned().unwrap_or_default();
        if r.get("client_type").map(String::as_str) != Some("0")
            || self.puppet_uids.contains(&uid)
            || self.remotes.contains_key(&clid)
        {
            return;
        }
        let cid = r.get("cid").or_else(|| r.get("ctid")).and_then(|v| v.parse().ok()).unwrap_or(0);
        let remote = Remote {
            session: 0,
            nickname: r.get("client_nickname").map(|n| shown(n)).unwrap_or_default(),
            cid,
            muted: flag(r, "client_input_muted"),
            deafened: flag(r, "client_output_muted"),
            away: away_of(r),
            talker: None,
            cldbid: r.get("client_database_id").and_then(|v| v.parse().ok()),
        };
        let client = RemoteClient {
            uid: format!("ts:{uid}"),
            nickname: remote.nickname.clone(),
            channel: self.our_channel(cid),
            platform: platform_of(r.get("client_version").map(String::as_str).unwrap_or("")),
            muted: remote.muted,
            deafened: remote.deafened,
            away: remote.away.clone(),
        };
        let Some(session) = self.core.remote_join(client).await else {
            // Banned on our side.
            let cmd = Cmd::new("clientkick").arg("clid", clid).arg("reasonid", 5).arg("reasonmsg", "banned");
            let _ = self.query.call(cmd).await;
            return;
        };
        if let Some(origin) = self.core.audio_origin(session).await {
            self.shared.mirror(clid, cid, origin);
        }
        self.remotes.insert(clid, Remote { session, ..remote });
        self.reconcile(Duration::ZERO);
        self.tell_watch_links(clid, cid).await;
    }

    fn our_channel(&self, cid: u64) -> ChannelId {
        self.map.ours(cid).or_else(|| self.server.as_ref().map(|s| s.default_channel)).unwrap_or_default()
    }

    async fn remove_remote(&mut self, clid: u16, reason: LeaveReason) {
        self.held.remove(&clid);
        if let Some(remote) = self.remotes.remove(&clid) {
            self.talkers.remove(&remote.session);
            if let Some(cldbid) = remote.cldbid {
                self.sync_whisper(cldbid).await;
            }
            self.shared.forget(clid);
            self.core.bridge(BridgeMsg::Leave(remote.session, reason)).await;
            self.reconcile(Duration::ZERO);
        }
    }

    /// Marks a TeamSpeak user a talker or not, as our side last said ([`BridgeNote::Talker`]) for
    /// the channel they are in. Without such a decision (a move not answered yet, a channel our
    /// side does not have them in) they are silent. A failure is retried by [`Self::poll`].
    async fn apply_talker(&mut self, clid: u16) {
        let Some(remote) = self.remotes.get(&clid) else { return };
        let here = self.our_channel(remote.cid);
        let want = matches!(self.talkers.get(&remote.session), Some(&(channel, true)) if channel == here);
        let cldbid = remote.cldbid;
        if remote.talker != Some(want) {
            let cmd = Cmd::new("clientedit").arg("clid", clid).arg("client_is_talker", u8::from(want));
            let done = self.query.call(cmd).await;
            if let Err(e) = &done {
                warn!(clid, "TeamSpeak talker: {e:#}");
            }
            if let Some(remote) = self.remotes.get_mut(&clid) {
                remote.talker = done.is_ok().then_some(want);
            }
        }
        if let Some(cldbid) = cldbid {
            self.sync_whisper(cldbid).await;
        }
    }

    /// Whispering other TeamSpeak users takes the bridge's whisper power (their needed one),
    /// which TeamSpeak keeps per identity: given only while every connection of that identity is
    /// a talker, so one that may not speak never whispers through another. A failure is
    /// retried by [`Self::poll`].
    async fn sync_whisper(&mut self, cldbid: u64) {
        let want = {
            let mut connections = self.remotes.values().filter(|r| r.cldbid == Some(cldbid)).peekable();
            connections.peek().is_some() && connections.all(|r| r.talker == Some(true))
        };
        if self.whispers.get(&cldbid) == Some(&want) {
            return;
        }
        let done = if want {
            let cmd = Cmd::new("clientaddperm")
                .arg("cldbid", cldbid)
                .arg("permsid", "i_client_whisper_power")
                .arg("permvalue", BRIDGE_TALK_POWER)
                .arg("permskip", 0);
            self.query.call(cmd).await.map(drop)
        } else {
            let cmd = Cmd::new("clientdelperm").arg("cldbid", cldbid).arg("permsid", "i_client_whisper_power");
            ignore_missing(self.query.call(cmd).await)
        };
        match done {
            Ok(()) => {
                self.whispers.insert(cldbid, want);
            }
            Err(e) => {
                warn!(cldbid, "TeamSpeak whisper right: {e:#}");
                // Possibly still the other way: the next sync tries again.
                self.whispers.insert(cldbid, !want);
            }
        }
    }

    fn remote_by_session(&self, session: SessionId) -> Option<(u16, &Remote)> {
        self.remotes.iter().find(|(_, r)| r.session == session).map(|(clid, r)| (*clid, r))
    }

    /// Picks up mute, away and nickname changes, which ServerQuery does not push.
    async fn poll(&mut self) {
        self.reconcile(Duration::ZERO);
        self.describe_channels().await;
        if self.remotes.is_empty() {
            return;
        }
        let list = match self.query.call(Cmd::new("clientlist").flag("-away").flag("-voice")).await {
            Ok(list) => list,
            Err(e) => {
                debug!("clientlist: {e:#}");
                return;
            }
        };
        for r in list {
            let Some(clid) = r.get("clid").and_then(|v| v.parse::<u16>().ok()) else { continue };
            let Some(remote) = self.remotes.get_mut(&clid) else { continue };
            let mut update = RemoteUpdate::default();
            if let Some(nickname) = r.get("client_nickname").map(|n| shown(n)).filter(|n| *n != remote.nickname) {
                remote.nickname = nickname.clone();
                update.nickname = Some(nickname);
            }
            let (muted, deafened, away) =
                (flag(&r, "client_input_muted"), flag(&r, "client_output_muted"), away_of(&r));
            if muted != remote.muted {
                remote.muted = muted;
                update.muted = Some(muted);
            }
            if deafened != remote.deafened {
                remote.deafened = deafened;
                update.deafened = Some(deafened);
            }
            if away != remote.away {
                remote.away = away.clone();
                update.away = Some(away);
            }
            if update.nickname.is_some() || update.muted.is_some() || update.deafened.is_some() || update.away.is_some()
            {
                let session = remote.session;
                self.core.bridge(BridgeMsg::Update(session, update)).await;
            }
        }
        // Talker flags and whisper rights that did not take (also of those who left), and
        // puppets not yet in their group.
        let clids: Vec<u16> = self.remotes.keys().copied().collect();
        for clid in clids {
            self.apply_talker(clid).await;
        }
        let identities: Vec<u64> = self.whispers.keys().copied().collect();
        for cldbid in identities {
            self.sync_whisper(cldbid).await;
        }
        let ungrouped: Vec<(SessionId, String)> = self
            .locals
            .iter()
            .filter(|(_, l)| l.clid.is_some() && !l.grouped)
            .map(|(session, l)| (*session, l.ts_uid.clone()))
            .collect();
        for (session, ts_uid) in ungrouped {
            match self.join_puppet_group(&ts_uid).await {
                Ok(()) => {
                    if let Some(local) = self.locals.get_mut(&session) {
                        local.grouped = true;
                    }
                }
                Err(e) => warn!(session, "puppet group: {e:#}"),
            }
        }
    }

    // --------------------------------------------------------------- locals

    fn add_local(&mut self, client: Client) {
        let identity = match self.identities.get(&client.uid) {
            Ok(identity) => identity,
            Err(e) => {
                warn!(session = client.id, "puppet identity: {e:#}");
                return;
            }
        };
        let ts_uid = ts_uid(&identity);
        self.puppet_uids.insert(ts_uid.clone());
        let local = Local {
            client,
            ts_uid,
            puppet: None,
            generation: 0,
            route: 0,
            clid: None,
            retry_at: None,
            idle_since: None,
            chatting_until: None,
            grouped: false,
        };
        self.locals.insert(local.client.id, local);
    }

    fn presence(&self, local: &Local, now: Instant) -> Presence {
        let chatting = local.chatting_until.is_some_and(|until| until > now);
        let channel = local
            .client
            .channel
            .map(|c| (self.is_encrypted(c), self.remotes.values().any(|r| self.our_channel(r.cid) == c)));
        presence_of(channel, chatting)
    }

    /// Keeps listeners in occupied channels and starts or retires puppets.
    /// `stagger` spreads simultaneous starts so the flood guard stays calm.
    fn reconcile(&mut self, stagger: Duration) {
        self.listeners.reconcile(listener_channels(&self.remotes, &self.map));
        let now = Instant::now();
        let decisions: Vec<_> = self.locals.values().map(|l| (l.client.id, self.presence(l, now))).collect();
        let mut started = 0u32;
        for (session, presence) in decisions {
            let Some(local) = self.locals.get_mut(&session) else { continue };
            match presence {
                Presence::Puppet => {
                    local.idle_since = None;
                    if local.puppet.is_none() && local.retry_at.is_none_or(|at| at <= now) {
                        info!(session, channel = ?local.client.channel, "puppet starts");
                        self.spawn_puppet(session, stagger * started);
                        started += 1;
                    }
                }
                Presence::Listed if local.puppet.is_some() => {
                    let idle = *local.idle_since.get_or_insert(now);
                    if now.duration_since(idle) >= LINGER {
                        info!(session, channel = ?local.client.channel, "puppet retires: no TeamSpeak user shares its channel");
                        self.retire_puppet(session);
                    }
                }
                Presence::Listed => local.idle_since = None,
                Presence::Absent => {
                    if local.puppet.is_some() {
                        info!(session, channel = ?local.client.channel, "puppet retires: not in a bridged channel");
                        // Someone going in and out of an encrypted channel would otherwise connect
                        // a new puppet each time, which TeamSpeak's flood guard punishes.
                        local.retry_at = Some(now + PUPPET_COOLDOWN);
                    }
                    self.retire_puppet(session);
                }
            }
        }
    }

    fn is_encrypted(&self, channel: ChannelId) -> bool {
        self.channels.get(&channel).is_some_and(|c| c.encrypted)
    }

    /// The TeamSpeak channel of a user's puppet and whether their voice goes there:
    /// their own channel, or (silent) the default one when they are not in voice or
    /// in an encrypted channel (a puppet for direct messages only).
    fn puppet_target(&self, client: &Client) -> (u64, bool) {
        let own = client.channel.map(|c| (self.is_encrypted(c), self.map.ts(c)));
        target_of(own, self.server.as_ref().and_then(|s| self.map.ts(s.default_channel)))
    }

    /// Moves the user's puppet where it belongs, starting a new text route. A puppet
    /// whose queue cannot take the move starts over there instead, so it never keeps
    /// speaking or routing text for the old channel.
    fn move_puppet(&mut self, session: SessionId) {
        let Some(local) = self.locals.get(&session) else { return };
        let (cid, voice) = self.puppet_target(&local.client);
        let Some(local) = self.locals.get_mut(&session) else { return };
        let Some(puppet) = &local.puppet else { return };
        local.route += 1;
        let origin = voice.then(|| self.shared.plane.routing.load().origin(session)).flatten();
        let origin = origin.filter(|o| o.bridgeable() && Some(o.channel) == local.client.channel);
        let audio = AudioRoute::new(local.route, cid, origin);
        if !puppet.send(PuppetCmd::Move { cid, route: local.route, audio, voice }) {
            warn!(session, "puppet busy; reconnecting it in its new channel");
            self.retire_puppet(session);
            self.spawn_puppet(session, Duration::ZERO);
        }
    }

    fn spawn_puppet(&mut self, session: SessionId, delay: Duration) {
        let Some(local) = self.locals.get(&session) else { return };
        let Ok(identity) = self.identities.get(&local.client.uid) else { return };
        let (channel, voice) = self.puppet_target(&local.client);
        let Some(local) = self.locals.get_mut(&session) else { return };
        local.generation += 1;
        local.route += 1;
        let c = &local.client;
        let spec = PuppetSpec {
            session,
            generation: local.generation,
            nickname: c.nickname.clone(),
            identity,
            channel,
            route: local.route,
            // Only audio from the channel this snapshot places the puppet for: a lagging snapshot
            // never pairs a public destination with a current private (or other) origin.
            audio: AudioRoute::new(
                local.route,
                channel,
                voice
                    .then(|| self.shared.plane.routing.load().origin(session))
                    .flatten()
                    .filter(|o| o.bridgeable() && Some(o.channel) == c.channel),
            ),
            voice,
            muted: c.muted,
            deafened: c.deafened,
            away: c.away.clone(),
            server_password: self.server_password.clone(),
            delay,
        };
        local.puppet = Some(puppet::spawn(self.voice, spec, self.shared.clone(), self.puppet_events.clone()));
        local.retry_at = None;
        local.idle_since = None;
    }

    /// Disconnects the user's puppet; they stay listed in the channel description.
    fn retire_puppet(&mut self, session: SessionId) {
        let Some(local) = self.locals.get_mut(&session) else { return };
        local.puppet = None;
        local.idle_since = None;
        if let Some(clid) = local.clid.take() {
            self.puppet_clids.remove(&clid);
        }
    }

    fn remove_local(&mut self, session: SessionId) {
        self.retire_puppet(session);
        self.locals.remove(&session);
    }

    /// Lists our users without a puppet in their TeamSpeak channel's description.
    async fn describe_channels(&mut self) {
        let mut listed: BTreeMap<u64, Vec<String>> = self.ts.keys().map(|cid| (*cid, Vec::new())).collect();
        for local in self.locals.values().filter(|l| l.clid.is_none()) {
            let channel = local.client.channel.filter(|c| !self.is_encrypted(*c));
            if let Some(names) = channel.and_then(|c| self.map.ts(c)).and_then(|cid| listed.get_mut(&cid)) {
                // The marker only: the watch link never goes in a description, which everyone on
                // the TeamSpeak server can read.
                let name = &local.client.nickname;
                names.push(if local.client.sharing { format!("{name} · sharing screen") } else { name.clone() });
            }
        }
        for (cid, mut names) in listed {
            names.sort_by_key(|n| n.to_lowercase());
            let encrypted = self.map.ours(cid).is_some_and(|c| self.is_encrypted(c));
            let text = if encrypted { ENCRYPTED_DESCRIPTION.to_owned() } else { describe(&names) };
            debug_assert!(!encrypted || names.is_empty(), "nobody is listed in an encrypted channel");
            if self.descriptions.get(&cid) == Some(&text) {
                continue;
            }
            let cmd = Cmd::new("channeledit").arg("cid", cid).arg("channel_description", &text);
            match self.query.call(cmd).await {
                Ok(_) => {
                    self.descriptions.insert(cid, text);
                }
                Err(e) => debug!(cid, "channel description: {e:#}"),
            }
        }
    }

    fn update_local(&mut self, client: Client) {
        let Some(local) = self.locals.get_mut(&client.id) else { return };
        let session = client.id;
        let before = std::mem::replace(&mut local.client, client);
        if before.channel != local.client.channel {
            self.reconcile(Duration::ZERO);
        }
        let Some(local) = self.locals.get(&session) else { return };
        if before.channel != local.client.channel || before.voice_binding != local.client.voice_binding {
            self.move_puppet(session);
        }
        let Some(local) = self.locals.get(&session) else { return };
        let Some(puppet) = &local.puppet else { return };
        let after = &local.client;
        if (before.muted, before.deafened) != (after.muted, after.deafened) {
            puppet.send(PuppetCmd::Flags { muted: after.muted, deafened: after.deafened });
        }
        if before.nickname != after.nickname {
            puppet.send(PuppetCmd::Nickname(after.nickname.clone()));
        }
        if before.away != after.away {
            puppet.send(PuppetCmd::Away(after.away.clone()));
        }
    }

    // --------------------------------------------------------------- events

    async fn on_note(&mut self, note: BridgeNote) {
        match note {
            BridgeNote::Snapshot { .. } => warn!("unexpected second snapshot"),
            // Handled in `serve`: a failure there restarts the bridge, to set it again.
            BridgeNote::ServerPassword(_) => {}
            BridgeNote::Channel { channel, password, .. } => {
                self.channels.insert(channel.id, channel.clone());
                // Which typed password was set: a queued one goes only once this very one is.
                let typed = password.clone().flatten();
                match self.ensure_channel(&channel, password).await {
                    Ok(()) => {
                        if let Some(typed) = typed {
                            self.core.bridge(BridgeMsg::PasswordSet(channel.id, typed)).await;
                        }
                    }
                    Err(e) => warn!(channel = channel.id, "TeamSpeak channel sync: {e:#}"),
                }
                self.reconcile(Duration::ZERO);
            }
            BridgeNote::Event(event) => self.on_event(*event).await,
            BridgeNote::Unban { uid } => self.unban(&uid).await,
            BridgeNote::Talker { session, channel, talks } => {
                self.talkers.insert(session, (channel, talks));
                if let Some((clid, _)) = self.remote_by_session(session) {
                    self.apply_talker(clid).await;
                }
            }
            BridgeNote::WatchLink { share, session, channel, url } => {
                self.post_watch_link(share, session, channel, url).await
            }
            BridgeNote::WatchLinkGone { share } => {
                self.links.remove(&share);
            }
        }
    }

    /// Posts a share's watch link in its TeamSpeak channel the way a relayed attachment link
    /// goes: through the sharer's puppet, else signed by ServerQuery when TeamSpeak users are
    /// there to read it. Never in an encrypted channel, nor where TeamSpeak users get only file
    /// names (`file_access: members`); the core refuses both too.
    async fn post_watch_link(&mut self, share: String, session: SessionId, channel: ChannelId, url: WatchUrl) {
        let links = self.server.as_ref().is_some_and(|s| s.file_access == FileAccess::Link);
        if !links || self.is_encrypted(channel) {
            return;
        }
        let Some(cid) = self.map.ts(channel) else { return };
        let Some(local) = self.locals.get(&session) else { return };
        let text = watch_text(&local.client.nickname, &url.0);
        // Everyone in the channel now reads the post; who comes later is told privately.
        let told: HashSet<u16> = self.remotes.iter().filter(|(_, r)| r.cid == cid).map(|(clid, _)| *clid).collect();
        let readers = !told.is_empty();
        let puppet = local
            .puppet
            .as_ref()
            .filter(|_| local.client.channel == Some(channel))
            .map(|p| (p, ChannelText { cid, encrypted: false, route: local.route }));
        if let Some((puppet, route)) = puppet {
            for part in split_for_teamspeak(&text) {
                puppet.send(PuppetCmd::Text { mode: 2, target: 0, text: part, channel: Some(route) });
            }
        } else if readers {
            let signed = signed_text(&local.client.nickname, &text);
            for part in split_for_teamspeak(&signed) {
                if let Err(e) = self.query_channel_message(cid, part).await {
                    warn!(share, "watch link post: {e:#}");
                }
            }
        }
        self.links.insert(share, WatchPost { channel, text, told });
    }

    /// A TeamSpeak user entered channel `cid`: the watch links of shares running there reach them
    /// once, as a private message from ServerQuery.
    async fn tell_watch_links(&mut self, clid: u16, cid: u64) {
        let mut texts = Vec::new();
        for post in self.links.values_mut() {
            if self.map.ts(post.channel) == Some(cid) && post.told.insert(clid) {
                texts.push(post.text.clone());
            }
        }
        for text in texts {
            for part in split_for_teamspeak(&text) {
                let cmd = Cmd::new("sendtextmessage").arg("targetmode", 1).arg("target", clid).arg("msg", part);
                if let Err(e) = self.query.call(cmd).await {
                    debug!(clid, "watch link message: {e:#}");
                }
            }
        }
    }

    async fn on_event(&mut self, event: Event) {
        match event {
            Event::ServerUpdated(server) => {
                let cmd = Cmd::new("serveredit")
                    .arg("virtualserver_name", &server.name)
                    .arg("virtualserver_welcomemessage", &server.welcome);
                if let Err(e) = self.query.call(cmd).await {
                    warn!("TeamSpeak server update: {e:#}");
                }
                self.server = Some(server);
            }
            Event::ChannelDeleted { channel } => self.delete_channel(channel).await,
            Event::ClientJoined(client) if !is_remote(&client) => {
                self.add_local(client);
                self.reconcile(Duration::ZERO);
            }
            Event::ClientUpdated(client) if self.locals.contains_key(&client.id) => self.update_local(client),
            Event::ClientUpdated(client) => {
                // An admin of ours moved a TeamSpeak user.
                let Some((clid, remote)) = self.remote_by_session(client.id) else { return };
                let (session, current) = (remote.session, remote.cid);
                let Some(cid) = client.channel.and_then(|c| self.map.ts(c)).filter(|cid| *cid != current) else {
                    return;
                };
                self.shared.forget(clid);
                if let Err(e) = self.query.call(Cmd::new("clientmove").arg("clid", clid).arg("cid", cid)).await {
                    warn!(clid, "TeamSpeak move: {e:#}");
                    // A refusal emits no notifyclientmoved. Read the confirmed
                    // location (it may have changed remotely while we waited),
                    // reconcile core participation and mint a fresh mirror route.
                    match self.query.call(Cmd::new("clientinfo").arg("clid", clid)).await {
                        Ok(records) => {
                            let Some(cid) = records.first().and_then(|r| r.get("cid")).and_then(|v| v.parse().ok())
                            else {
                                return;
                            };
                            if self.held.get(&clid) == Some(&cid) {
                                // Our side refused this channel and TeamSpeak refuses the way
                                // back: out of the channel, unheard meanwhile (no mirror).
                                self.held.remove(&clid);
                                let kick = Cmd::new("clientkick").arg("clid", clid).arg("reasonid", 4);
                                if let Err(e) = self.query.call(kick).await {
                                    warn!(clid, "TeamSpeak channel kick: {e:#}");
                                }
                            } else {
                                self.held.insert(clid, cid);
                                self.client_moved(clid, cid).await;
                                // Taken in there after all (an admin's move undone): nothing held.
                                if self.core.audio_origin(session).await.map(|o| o.channel)
                                    == Some(self.our_channel(cid))
                                {
                                    self.held.remove(&clid);
                                }
                            }
                        }
                        Err(e) => warn!(clid, "TeamSpeak refused move reconciliation: {e:#}"),
                    }
                }
            }
            Event::ClientLeft { client, reason } => {
                if self.locals.contains_key(&client) {
                    self.remove_local(client);
                } else if let Some((clid, _)) = self.remote_by_session(client) {
                    // Our side already removed the session; make TeamSpeak follow.
                    let cldbid = self.remotes.remove(&clid).and_then(|r| r.cldbid);
                    self.talkers.remove(&client);
                    if let Some(cldbid) = cldbid {
                        self.sync_whisper(cldbid).await;
                    }
                    self.shared.forget(clid);
                    self.reconcile(Duration::ZERO);
                    let cmd = match &reason {
                        LeaveReason::Banned { reason, until, .. } => {
                            // TeamSpeak keeps the ban itself (0 = permanent).
                            let secs = until.map(|t| ((t - now_ms()) / 1000).max(1)).unwrap_or(0);
                            Cmd::new("banclient")
                                .arg("clid", clid)
                                .arg("time", secs)
                                .arg("banreason", reason.clone().unwrap_or_default())
                        }
                        LeaveReason::Kicked { reason, .. } => Cmd::new("clientkick")
                            .arg("clid", clid)
                            .arg("reasonid", 5)
                            .arg("reasonmsg", reason.clone().unwrap_or_default()),
                        _ => Cmd::new("clientkick").arg("clid", clid).arg("reasonid", 5).arg("reasonmsg", ""),
                    };
                    if let Err(e) = ignore_missing(self.query.call(cmd).await) {
                        warn!(clid, "TeamSpeak kick/ban: {e:#}");
                    }
                }
            }
            Event::ChatMessage(message) => self.relay_chat(message).await,
            _ => {}
        }
    }

    /// A message written on our side, delivered to TeamSpeak.
    async fn relay_chat(&mut self, m: ChatMessage) {
        if m.target == ChatTarget::Server && self.server.as_ref().is_some_and(|s| s.server_chat.encrypted) {
            return;
        }
        if let ChatTarget::Channel(channel) = m.target
            && self.is_encrypted(channel)
        {
            return;
        }
        let (mode, target) = match m.target {
            ChatTarget::Channel(_) => (2, 0),
            ChatTarget::Server => (3, 0),
            ChatTarget::Client(to) => match self.remote_by_session(to) {
                Some((clid, _)) => (1, clid),
                None => return, // between two of our users
            },
        };
        if mode == 1
            && let Some(local) = self.locals.get_mut(&m.author)
        {
            // Talking privately to a TeamSpeak user needs a puppet for them to answer.
            local.chatting_until = Some(Instant::now() + CHAT_KEEP);
            self.reconcile(Duration::ZERO);
        }
        let author = self.locals.get(&m.author);
        // A puppet writes in its own channel only; a starting one queues the
        // message until it is connected. Channel text keeps its channel and the
        // puppet's current route, so the puppet drops it after any move.
        let channel_text = match m.target {
            ChatTarget::Channel(channel) => author
                .filter(|l| l.client.channel == Some(channel))
                .and_then(|l| Some(ChannelText { cid: self.map.ts(channel)?, encrypted: false, route: l.route })),
            _ => None,
        };
        let puppet = author
            .and_then(|l| l.puppet.as_ref())
            .filter(|_| !matches!(m.target, ChatTarget::Channel(_)) || channel_text.is_some());
        // Links open only for Gwar members on a members-only server: TeamSpeak users get the names.
        let links = self.server.as_ref().is_none_or(|s| s.file_access == FileAccess::Link);
        let body = ts_text(&m, self.public_url.as_deref().filter(|_| links));
        if let Some(puppet) = puppet {
            for text in split_for_teamspeak(&body) {
                puppet.send(PuppetCmd::Text { mode, target, text, channel: channel_text });
            }
            return;
        }
        // Otherwise ServerQuery (invisible to TeamSpeak users) posts it, signed.
        let signed = signed_text(&m.author_name, &body);
        for text in split_for_teamspeak(&signed) {
            let result = match m.target {
                ChatTarget::Server => {
                    self.query
                        .call(Cmd::new("sendtextmessage").arg("targetmode", 3).arg("target", 1).arg("msg", text))
                        .await
                }
                ChatTarget::Channel(channel) => {
                    let Some(cid) = self.map.ts(channel) else { return };
                    // Nobody on TeamSpeak would read it.
                    if !self.remotes.values().any(|r| r.cid == cid) {
                        return;
                    }
                    self.query_channel_message(cid, text).await
                }
                ChatTarget::Client(_) => return,
            };
            if let Err(e) = result {
                warn!("relayed message: {e:#}");
            }
        }
    }

    /// Lifts TeamSpeak's own ban on a user whose ban was removed on our side.
    async fn unban(&mut self, uid: &str) {
        let Some(ts_uid) = uid.strip_prefix("ts:") else { return };
        let Ok(bans) = self.query.call(Cmd::new("banlist")).await else { return };
        for ban in bans.iter().filter(|b| b.get("uid").map(String::as_str) == Some(ts_uid)) {
            if let Some(id) = ban.get("banid") {
                let _ = self.query.call(Cmd::new("bandel").arg("banid", id)).await;
            }
        }
    }

    /// Posts `text` in a TeamSpeak channel as the (hidden) ServerQuery client,
    /// which has to be in that channel to write there.
    async fn query_channel_message(&mut self, cid: u64, text: String) -> Result<Vec<Record>> {
        if self.query_clid.is_none() {
            let me = self.query.call(Cmd::new("whoami")).await?;
            self.query_clid = me.first().and_then(|r| r.get("client_id")?.parse().ok());
        }
        let clid = self.query_clid.context("whoami returned no client id")?;
        let moved = self.query.call(Cmd::new("clientmove").arg("clid", clid).arg("cid", cid)).await;
        if let Err(e) = moved
            && query_error(&e) != Some(ERR_ALREADY_IN_CHANNEL)
        {
            return Err(e);
        }
        self.query.call(Cmd::new("sendtextmessage").arg("targetmode", 2).arg("target", cid).arg("msg", text)).await
    }

    async fn on_notification(&mut self, n: super::query::Notification) {
        match n.name.as_str() {
            "notifycliententerview" => {
                for r in &n.records {
                    self.add_remote(r).await;
                }
            }
            "notifyclientleftview" => {
                for r in &n.records {
                    let Some(clid) = r.get("clid").and_then(|v| v.parse::<u16>().ok()) else { continue };
                    let reason = match r.get("reasonid").map(String::as_str) {
                        Some("5") | Some("6") => LeaveReason::Kicked {
                            by: r.get("invokername").map(|n| shown(n)).unwrap_or_default(),
                            reason: r.get("reasonmsg").filter(|m| !m.is_empty()).map(|m| shown(m)),
                        },
                        Some("3") => LeaveReason::Timeout,
                        Some("11") => LeaveReason::ServerShutdown,
                        _ => LeaveReason::Quit,
                    };
                    self.remove_remote(clid, reason).await;
                }
            }
            "notifyclientmoved" => {
                for r in &n.records {
                    let (Some(clid), Some(cid)) = (
                        r.get("clid").and_then(|v| v.parse::<u16>().ok()),
                        r.get("ctid").and_then(|v| v.parse::<u64>().ok()),
                    ) else {
                        continue;
                    };
                    self.held.remove(&clid);
                    self.client_moved(clid, cid).await;
                }
            }
            "notifytextmessage" => {
                let Some(r) = n.records.first() else { return };
                let invoker = r.get("invokerid").and_then(|v| v.parse::<u16>().ok()).unwrap_or(0);
                if r.get("targetmode").map(String::as_str) == Some("3")
                    && let Some(remote) = self.remotes.get(&invoker)
                {
                    let text = r.get("msg").cloned().unwrap_or_default();
                    self.core.bridge(BridgeMsg::Chat(remote.session, ChatTarget::Server, text)).await;
                }
            }
            other => debug!(notification = other, "ignored TeamSpeak notification"),
        }
    }

    async fn client_moved(&mut self, clid: u16, cid: u64) {
        if let Some(remote) = self.remotes.get_mut(&clid) {
            self.shared.forget(clid);
            remote.cid = cid;
            let session = remote.session;
            let channel = self.our_channel(cid);
            let update = RemoteUpdate { channel: Some(channel), ..Default::default() };
            self.core.bridge(BridgeMsg::Update(session, update)).await;
            if let Some(origin) = self.core.audio_origin(session).await {
                self.shared.mirror(clid, cid, origin);
            }
            // TeamSpeak takes a talker's flag away on a move, though not on a refused one: silent
            // until our side's decision for this move comes (it always sends one).
            self.talkers.remove(&session);
            if let Some(remote) = self.remotes.get_mut(&clid) {
                remote.talker = None;
            }
            self.apply_talker(clid).await;
            self.reconcile(Duration::ZERO);
            self.tell_watch_links(clid, cid).await;
        } else if let Some(&session) = self.puppet_clids.get(&clid) {
            // Someone moved a puppet on TeamSpeak; our user decides where they are.
            // Either way, text queued before is not sent in the new channel.
            self.move_puppet(session);
        }
    }

    async fn on_puppet(&mut self, event: PuppetEvent) {
        match event {
            PuppetEvent::Connected { session, generation, clid } => {
                let Some(local) = self.locals.get_mut(&session).filter(|l| l.generation == generation) else { return };
                local.clid = Some(clid);
                self.puppet_clids.insert(clid, session);
                let ts_uid = local.ts_uid.clone();
                // The bridge group first: it exempts the puppet from TeamSpeak's flood guard, which
                // otherwise refuses a new client's first commands (the move, the nickname).
                match self.join_puppet_group(&ts_uid).await {
                    Ok(()) => {
                        if let Some(local) = self.locals.get_mut(&session) {
                            local.grouped = true;
                        }
                    }
                    // Without the group the puppet is silent: `poll` tries again.
                    Err(e) => warn!(session, "puppet group: {e:#}"),
                }
                // The channel may have changed while connecting.
                self.move_puppet(session);
                // Say what this client is, so nobody mistakes it for a TeamSpeak app.
                let describe = Cmd::new("clientedit").arg("clid", clid).arg("client_description", PUPPET_DESCRIPTION);
                if let Err(e) = self.query.call(describe).await {
                    debug!(session, "puppet description: {e:#}");
                }
            }
            PuppetEvent::Text { session, generation, origin, source_route, receiver_route, target, text } => {
                if !source_route.permits(&origin)
                    || !receiver_route.is_current()
                    || !self.core.audio_binding_matches(&origin)
                    || self.locals.get(&session).is_none_or(|local| local.generation != generation)
                {
                    return;
                }
                if matches!(target, MessageTarget::Client(_) | MessageTarget::Poke(_))
                    && let Some(local) = self.locals.get_mut(&session)
                {
                    local.chatting_until = Some(Instant::now() + CHAT_KEEP);
                }
                let author = origin.session;
                let Some(target) = puppet_chat_target(session, target) else { return };
                self.core.bridge(BridgeMsg::Chat(author, target, text)).await;
            }
            PuppetEvent::Gone { session, generation, error } => {
                // Ignore puppets we retired ourselves or that were already replaced.
                let Some(local) =
                    self.locals.get_mut(&session).filter(|l| l.generation == generation && l.puppet.is_some())
                else {
                    return;
                };
                if let Some(e) = &error {
                    warn!(session, "puppet: {e}");
                }
                if let Some(clid) = local.clid.take() {
                    self.puppet_clids.remove(&clid);
                }
                local.puppet = None;
                local.retry_at = Some(Instant::now() + Duration::from_secs(10));
            }
        }
    }

    async fn join_puppet_group(&mut self, ts_uid: &str) -> Result<()> {
        let Some(sgid) = self.puppet_group else { return Ok(()) };
        let found = self.query.call(Cmd::new("clientgetdbidfromuid").arg("cluid", ts_uid)).await?;
        let cldbid = found.first().and_then(|r| r.get("cldbid")).context("no database id")?;
        ignore_duplicate(
            self.query.call(Cmd::new("servergroupaddclient").arg("sgid", sgid).arg("cldbid", cldbid)).await,
        )
    }

    async fn on_listener(&mut self, text: listeners::Text) {
        if self.listeners.accepts(&text)
            && let Some(message) = channel_chat(&self.remotes, &self.map, text.cid, text.invoker, text.text)
        {
            self.core.bridge(message).await;
        }
    }
}

/// How a user appears on TeamSpeak, from their voice channel (`(encrypted, shared
/// with a TeamSpeak user)`) and whether they talk privately to a TeamSpeak user.
/// Nobody has a puppet in an encrypted channel: a private conversation keeps one
/// in the default channel, for text only.
fn presence_of(channel: Option<(bool, bool)>, chatting: bool) -> Presence {
    match channel {
        Some((false, shared)) if shared || chatting => Presence::Puppet,
        Some((false, _)) => Presence::Listed,
        _ if chatting => Presence::Puppet,
        _ => Presence::Absent,
    }
}

fn listener_channels(remotes: &HashMap<u16, Remote>, map: &ChannelMap) -> BTreeSet<u64> {
    remotes.values().filter(|r| map.ours(r.cid).is_some()).map(|r| r.cid).collect()
}

/// Only remote sessions may author TeamSpeak chat; our puppets and query
/// clients never enter `remotes`, so our outgoing messages cannot echo back.
fn channel_chat(
    remotes: &HashMap<u16, Remote>,
    map: &ChannelMap,
    cid: u64,
    invoker: u16,
    text: String,
) -> Option<BridgeMsg> {
    let remote = remotes.get(&invoker)?;
    let channel = map.ours(cid)?;
    Some(BridgeMsg::Chat(remote.session, ChatTarget::Channel(channel), text))
}

fn puppet_chat_target(session: SessionId, target: MessageTarget) -> Option<ChatTarget> {
    match target {
        MessageTarget::Client(_) | MessageTarget::Poke(_) => Some(ChatTarget::Client(session)),
        // Channel and server messages have one source: their query listener.
        MessageTarget::Channel | MessageTarget::Server => None,
    }
}

/// Text from our side as plain text on TeamSpeak, which renders BBCode in chat and
/// channel descriptions: Gwar shows `[img]…` literally, so TeamSpeak must too (no
/// images fetched from TeamSpeak users' machines, no disguised links, no styled
/// names). A zero-width space after each `[` keeps any tag from being recognised.
fn plain(text: &str) -> String {
    text.replace('[', "[\u{200B}")
}

/// A relayed message posted by ServerQuery, under its author's name: `[name] text`. The
/// brackets are ours, so they are neutralised too (a name like `hr` would make a tag).
fn signed_text(author: &str, body: &str) -> String {
    format!("{} {body}", plain(&format!("[{author}]")))
}

/// How a message of ours reads on TeamSpeak: the text, then a link per
/// attachment (TeamSpeak chats show plain text only; both TS3 and TS6 make
/// URLs clickable). Mentions are already readable as `@nickname`.
fn ts_text(m: &ChatMessage, public_url: Option<&str>) -> String {
    let mut text = plain(&m.text);
    for file in &m.attachments {
        if !text.is_empty() {
            text.push('\n');
        }
        match public_url {
            Some(base) => {
                text.push_str(&format!("📎 {} — {}{}", plain(&file.name), base.trim_end_matches('/'), file.url))
            }
            None => text.push_str(&format!("📎 {}", plain(&file.name))),
        }
    }
    text
}

/// How a watch link reads on TeamSpeak. The URL is ours (base64url token after `#`).
fn watch_text(name: &str, url: &str) -> String {
    format!("🖥 {} is sharing their screen. Watch: {url}", plain(name))
}

/// TeamSpeak caps a text message at 1024 characters; long ones go in parts.
fn split_for_teamspeak(text: &str) -> Vec<String> {
    const LIMIT: usize = 1000;
    let mut parts = Vec::new();
    let mut current = String::new();
    for line in text.split_inclusive('\n') {
        for c in line.chars() {
            if current.chars().count() >= LIMIT {
                parts.push(std::mem::take(&mut current));
            }
            current.push(c);
        }
    }
    if !current.trim().is_empty() {
        parts.push(current);
    }
    parts
}

/// Channel description listing our users who have no puppet there.
fn describe(names: &[String]) -> String {
    if names.is_empty() {
        return String::new();
    }
    let names: Vec<_> = names.iter().map(|n| plain(n)).collect();
    format!("[b]In the app ({}):[/b] {}", names.len(), names.join(", "))
}

fn is_remote(client: &Client) -> bool {
    client.uid.starts_with("ts:")
}

fn flag(r: &Record, key: &str) -> bool {
    r.get(key).map(String::as_str) == Some("1")
}

fn away_of(r: &Record) -> Option<String> {
    flag(r, "client_away").then(|| r.get("client_away_message").map(|a| shown(a)).unwrap_or_default())
}

/// A TeamSpeak user's nickname or away message as Gwar shows it: bidi formatting goes (it
/// reorders the text around it) and control characters become spaces. TeamSpeak mode cleans
/// the same way (`display_text` in gwar-client, `displayText` in apps/web/src/lib/text.ts).
fn shown(text: &str) -> String {
    text.chars().filter(|&c| !super::bidi(c)).map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// TeamSpeak 5/6 clients report their major version first; TS3 reports 3.x.
fn platform_of(version: &str) -> Platform {
    match version.trim().chars().next() {
        Some('5') | Some('6') => Platform::Ts6,
        _ => Platform::Ts3,
    }
}

fn random_password() -> String {
    rand::rng().sample_iter(rand::distr::Alphanumeric).take(32).map(char::from).collect()
}

/// TeamSpeak query error ids for "nothing to do".
const ERR_DATABASE_EMPTY: u32 = 1281;
const ERR_INVALID_CLIENT: u32 = 512;
const ERR_INVALID_CHANNEL: u32 = 768;
const ERR_PERMISSION_NOT_FOUND: u32 = 2562;
const ERR_DUPLICATE_ENTRY: u32 = 2561;
const ERR_ALREADY_IN_CHANNEL: u32 = 770;

fn query_error(e: &anyhow::Error) -> Option<u32> {
    e.downcast_ref::<QueryError>().map(|q| q.id)
}

fn ignore_missing<T>(result: Result<T>) -> Result<()> {
    match result {
        Ok(_) => Ok(()),
        Err(e)
            if matches!(
                query_error(&e),
                Some(ERR_DATABASE_EMPTY | ERR_INVALID_CLIENT | ERR_INVALID_CHANNEL | ERR_PERMISSION_NOT_FOUND)
            ) =>
        {
            Ok(())
        }
        Err(e) => Err(e),
    }
}

fn ignore_duplicate<T>(result: Result<T>) -> Result<()> {
    match result {
        Err(e) if query_error(&e) == Some(ERR_DUPLICATE_ENTRY) => Ok(()),
        other => other.map(|_| ()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn teamspeak_names_lose_bidi_and_control_characters() {
        assert_eq!(super::shown("Gar\u{202e}den\u{2066}"), "Garden");
        assert_eq!(super::shown("a\tb\nc\u{7}"), "a b c ");
        assert_eq!(super::shown("  spaced  "), "  spaced  ");
    }

    use super::*;

    fn remote(session: SessionId, cid: u64) -> Remote {
        Remote {
            session,
            nickname: "TeamSpeak user".into(),
            cid,
            muted: false,
            deafened: false,
            away: None,
            talker: None,
            cldbid: None,
        }
    }

    /// A ServerQuery that answers `channelcreate` with cid 42 and refuses
    /// `channeladdperm` when `refuse` is set; returns the commands it got.
    async fn channel_query(refuse: bool) -> (Query, Arc<std::sync::Mutex<Vec<String>>>) {
        channel_query_at(refuse, 10).await
    }

    /// The same, with `clientinfo` placing the client in `cid`.
    async fn channel_query_at(refuse: bool, cid: u64) -> (Query, Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::{
            io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let commands = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = commands.clone();
        tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let (read, mut write) = sock.into_split();
            write.write_all(b"TS3\n\rWelcome\n\r").await.unwrap();
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let name = line.split(' ').next().unwrap_or_default().to_owned();
                seen.lock().unwrap().push(line);
                let reply = match name.as_str() {
                    "clientmove" if refuse => "error id=2568 msg=insufficient\\sclient\\spermissions\n\r",
                    "clientinfo" => &format!("cid={cid}\n\rerror id=0 msg=ok\n\r"),
                    "whoami" => "client_id=1\n\rerror id=0 msg=ok\n\r",
                    "channelcreate" => "cid=42\n\rerror id=0 msg=ok\n\r",
                    "channeladdperm" if refuse => "error id=2568 msg=insufficient\\sclient\\spermissions\n\r",
                    _ => "error id=0 msg=ok\n\r",
                };
                write.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        let (query, _notes) = Query::connect(addr, "serveradmin", "p", 1).await.unwrap();
        (query, commands)
    }

    #[tokio::test]
    async fn refused_remote_move_restores_confirmed_source_route() {
        use crate::{
            core::expiry_tests::test_core_teamspeak,
            media::{MediaPlane, Routing},
            store::Store,
        };

        let store = Store::in_memory().unwrap();
        store.init_server_chat(false).unwrap();
        let core = test_core_teamspeak(store).unwrap();
        let (notes, mut receiver) = mpsc::unbounded_channel();
        core.bridge(BridgeMsg::Attach(notes)).await;
        let BridgeNote::Snapshot { server, .. } = receiver.recv().await.unwrap() else { panic!("snapshot") };
        let (webrtc, _media) = mpsc::channel(8);
        let plane = MediaPlane {
            routing: Arc::new(arc_swap::ArcSwap::from_pointee(Routing::default())),
            sinks: Arc::default(),
            uplinks: Arc::default(),
            webrtc,
        };
        let (query, commands) = channel_query(true).await;
        commands.lock().unwrap().clear();
        let dir = tempfile::tempdir().unwrap();
        let mut identities = Identities::load(&dir.path().join("identities.json")).unwrap();
        let (puppet_events, _puppets) = mpsc::channel(8);
        let (listener_events, _listeners) = mpsc::channel(8);
        let mut bridge = Bridge {
            query,
            core: core.clone(),
            shared: Arc::new(Shared::new(core.clone(), plane)),
            voice: "127.0.0.1:1".parse().unwrap(),
            public_url: None,
            identities: &mut identities,
            puppet_uids: HashSet::new(),
            map: ChannelMap::load(&dir.path().join("channels.json")).unwrap(),
            ts: HashMap::new(),
            channels: BTreeMap::new(),
            server: Some(server),
            locals: HashMap::new(),
            remotes: HashMap::new(),
            puppet_clids: HashMap::new(),
            puppet_events,
            puppet_group: None,
            listeners: ChannelListeners::new("127.0.0.1:1".parse().unwrap(), "p".into(), listener_events),
            descriptions: HashMap::new(),
            query_clid: None,
            locked: HashSet::new(),
            server_password: None,
            password_file: dir.path().join("server-password.json"),
            held: HashMap::new(),
            talkers: HashMap::new(),
            whispers: HashMap::new(),
            links: HashMap::new(),
        };
        bridge.map.link(1, 10);
        bridge.map.link(2, 20);
        let session = core
            .remote_join(RemoteClient {
                uid: "ts:refused".into(),
                nickname: "remote".into(),
                channel: 1,
                platform: Platform::Desktop,
                muted: false,
                deafened: false,
                away: None,
            })
            .await
            .unwrap();
        bridge.remotes.insert(7, remote(session, 10));
        let original = core.audio_origin(session).await.unwrap();
        bridge.shared.mirror(7, 10, original.clone());
        let old = bridge.shared.mirrors.read().unwrap()[&7].clone();
        core.bridge(BridgeMsg::Update(session, RemoteUpdate { channel: Some(2), ..Default::default() })).await;
        let moved = core.audio_origin(session).await.unwrap(); // actor barrier
        assert_eq!(moved.channel, 2);
        // Bridge-origin updates suppress echo; supply the same administrative
        // event a local move would produce, without replacing the bridge.
        bridge
            .on_event(Event::ClientUpdated(Client {
                id: session,
                uid: "ts:refused".into(),
                nickname: "remote".into(),
                channel: Some(2),
                groups: vec![],
                platform: Platform::Desktop,
                muted: false,
                deafened: false,
                away: None,
                talking: false,
                voice: true,
                voice_binding: Some(moved.binding.clone()),
                server_muted: false,
                server_deafened: false,
                sharing: false,
                channel_group: None,
                avatar: None,
            }))
            .await;
        let restored = core.audio_origin(session).await.unwrap();
        assert_eq!(restored.channel, 1);
        assert_ne!(restored.binding, original.binding);
        assert!(!old.is_current());
        let fresh = bridge.shared.mirrors.read().unwrap()[&7].clone();
        assert!(fresh.generation > old.generation);
        assert!(fresh.permits(&restored));
        assert_eq!(bridge.remotes[&7].cid, 10);
        assert!(commands.lock().unwrap().iter().any(|c| c.starts_with("clientinfo clid=7")));
    }

    #[tokio::test]
    async fn a_refused_way_back_ends_in_a_channel_kick_not_a_loop() {
        use crate::{
            core::expiry_tests::test_core_teamspeak,
            media::{MediaPlane, Routing},
            store::{MEMBER_GROUP, Store},
        };

        let store = Store::in_memory().unwrap();
        store.init_server_chat(false).unwrap();
        let deny = gwar_proto::ChannelOverride {
            role: Some(MEMBER_GROUP),
            user: None,
            allow: vec![],
            deny: vec![gwar_proto::Permission::Connect],
        };
        store.set_channel_overrides(2, &[deny]).unwrap();
        let core = test_core_teamspeak(store).unwrap();
        let (notes, mut receiver) = mpsc::unbounded_channel();
        core.bridge(BridgeMsg::Attach(notes)).await;
        let BridgeNote::Snapshot { server, .. } = receiver.recv().await.unwrap() else { panic!("snapshot") };
        let (webrtc, _media) = mpsc::channel(8);
        let plane = MediaPlane {
            routing: Arc::new(arc_swap::ArcSwap::from_pointee(Routing::default())),
            sinks: Arc::default(),
            uplinks: Arc::default(),
            webrtc,
        };
        // TeamSpeak refuses every move and keeps the user in 20 (our 2, closed to them).
        let (query, commands) = channel_query_at(true, 20).await;
        let dir = tempfile::tempdir().unwrap();
        let mut identities = Identities::load(&dir.path().join("identities.json")).unwrap();
        let (puppet_events, _puppets) = mpsc::channel(8);
        let (listener_events, _listeners) = mpsc::channel(8);
        let mut bridge = Bridge {
            query,
            core: core.clone(),
            shared: Arc::new(Shared::new(core.clone(), plane)),
            voice: "127.0.0.1:1".parse().unwrap(),
            public_url: None,
            identities: &mut identities,
            puppet_uids: HashSet::new(),
            map: ChannelMap::load(&dir.path().join("channels.json")).unwrap(),
            ts: HashMap::new(),
            channels: BTreeMap::new(),
            server: Some(server),
            locals: HashMap::new(),
            remotes: HashMap::new(),
            puppet_clids: HashMap::new(),
            puppet_events,
            puppet_group: None,
            listeners: ChannelListeners::new("127.0.0.1:1".parse().unwrap(), "p".into(), listener_events),
            descriptions: HashMap::new(),
            query_clid: None,
            locked: HashSet::new(),
            server_password: None,
            password_file: dir.path().join("server-password.json"),
            held: HashMap::new(),
            talkers: HashMap::new(),
            whispers: HashMap::new(),
            links: HashMap::new(),
        };
        bridge.map.link(1, 10);
        bridge.map.link(2, 20);
        let session = core
            .remote_join(RemoteClient {
                uid: "ts:held".into(),
                nickname: "remote".into(),
                channel: 1,
                platform: Platform::Desktop,
                muted: false,
                deafened: false,
                away: None,
            })
            .await
            .unwrap();
        bridge.remotes.insert(7, remote(session, 10));
        commands.lock().unwrap().clear();
        // They walk into 20 on TeamSpeak: our side refuses and asks for them back, twice at most.
        bridge.client_moved(7, 20).await;
        let mut corrections = 0;
        while let Ok(Some(note)) = tokio::time::timeout(Duration::from_millis(200), receiver.recv()).await {
            if let BridgeNote::Event(event) = note
                && matches!(*event, Event::ClientUpdated(ref c) if c.id == session)
            {
                corrections += 1;
                assert!(corrections <= 2, "the correction loops");
                bridge.on_event(*event).await;
            }
        }
        assert_eq!(corrections, 2);
        assert_eq!(core.audio_origin(session).await.unwrap().channel, 1);
        assert!(commands.lock().unwrap().iter().any(|c| c.starts_with("clientkick clid=7 reasonid=4")));
        // Unheard in the meantime: no route mirrors them from 20 as if they were in 1.
        assert!(!bridge.shared.mirrors.read().unwrap().contains_key(&7));
    }

    #[tokio::test]
    async fn talkers_follow_our_side_and_are_silent_without_a_decision_for_their_channel() {
        use crate::{
            core::expiry_tests::test_core_teamspeak,
            media::{MediaPlane, Routing},
            store::Store,
        };

        let store = Store::in_memory().unwrap();
        store.init_server_chat(false).unwrap();
        let core = test_core_teamspeak(store).unwrap();
        let (notes, mut receiver) = mpsc::unbounded_channel();
        core.bridge(BridgeMsg::Attach(notes)).await;
        let BridgeNote::Snapshot { server, .. } = receiver.recv().await.unwrap() else { panic!("snapshot") };
        let (webrtc, _media) = mpsc::channel(8);
        let plane = MediaPlane {
            routing: Arc::new(arc_swap::ArcSwap::from_pointee(Routing::default())),
            sinks: Arc::default(),
            uplinks: Arc::default(),
            webrtc,
        };
        let (query, commands) = channel_query(false).await;
        let dir = tempfile::tempdir().unwrap();
        let mut identities = Identities::load(&dir.path().join("identities.json")).unwrap();
        let (puppet_events, _puppets) = mpsc::channel(8);
        let (listener_events, _listeners) = mpsc::channel(8);
        let mut bridge = Bridge {
            query,
            core: core.clone(),
            shared: Arc::new(Shared::new(core.clone(), plane)),
            voice: "127.0.0.1:1".parse().unwrap(),
            public_url: None,
            identities: &mut identities,
            puppet_uids: HashSet::new(),
            map: ChannelMap::load(&dir.path().join("channels.json")).unwrap(),
            ts: HashMap::new(),
            channels: BTreeMap::new(),
            server: Some(server),
            locals: HashMap::new(),
            remotes: HashMap::new(),
            puppet_clids: HashMap::new(),
            puppet_events,
            puppet_group: None,
            listeners: ChannelListeners::new("127.0.0.1:1".parse().unwrap(), "p".into(), listener_events),
            descriptions: HashMap::new(),
            query_clid: None,
            locked: HashSet::new(),
            server_password: None,
            password_file: dir.path().join("server-password.json"),
            held: HashMap::new(),
            talkers: HashMap::new(),
            whispers: HashMap::new(),
            links: HashMap::new(),
        };
        bridge.map.link(1, 10);
        bridge.map.link(2, 20);
        bridge.remotes.insert(7, Remote { cldbid: Some(5), ..remote(3, 10) });
        let sent = |commands: &Arc<std::sync::Mutex<Vec<String>>>| std::mem::take(&mut *commands.lock().unwrap());
        commands.lock().unwrap().clear();

        // A decision for another channel than theirs: silent, and no whispering either.
        bridge.on_note(BridgeNote::Talker { session: 3, channel: 2, talks: true }).await;
        let got = sent(&commands);
        assert!(got.iter().any(|c| c == "clientedit clid=7 client_is_talker=0"), "{got:?}");
        assert!(got.iter().any(|c| c.starts_with("clientdelperm cldbid=5 permsid=i_client_whisper_power")), "{got:?}");
        // Their own channel: a talker, who may whisper; the same decision again sends nothing.
        bridge.on_note(BridgeNote::Talker { session: 3, channel: 1, talks: true }).await;
        let got = sent(&commands);
        assert!(got.iter().any(|c| c == "clientedit clid=7 client_is_talker=1"), "{got:?}");
        assert!(got.iter().any(|c| c.starts_with("clientaddperm cldbid=5 permsid=i_client_whisper_power")), "{got:?}");
        bridge.apply_talker(7).await;
        assert!(sent(&commands).is_empty());
        // A move: silent until our side decides for the new channel.
        bridge.client_moved(7, 20).await;
        let got = sent(&commands);
        assert!(got.iter().any(|c| c == "clientedit clid=7 client_is_talker=0"), "{got:?}");
        assert!(got.iter().any(|c| c.starts_with("clientdelperm cldbid=5")), "{got:?}");
        assert!(!bridge.talkers.contains_key(&3));
        bridge.on_note(BridgeNote::Talker { session: 3, channel: 2, talks: true }).await;
        assert!(sent(&commands).iter().any(|c| c.starts_with("clientaddperm cldbid=5")));
        // A second connection of the same identity that may not talk: nobody of it whispers.
        bridge.remotes.insert(8, Remote { cldbid: Some(5), ..remote(4, 10) });
        bridge.on_note(BridgeNote::Talker { session: 4, channel: 1, talks: false }).await;
        assert!(sent(&commands).iter().any(|c| c.starts_with("clientdelperm cldbid=5")));
        // When it leaves, the talker left whispers again; when that one leaves too, nobody does.
        bridge.remove_remote(8, LeaveReason::Quit).await;
        assert!(sent(&commands).iter().any(|c| c.starts_with("clientaddperm cldbid=5")));
        bridge.remove_remote(7, LeaveReason::Quit).await;
        assert!(sent(&commands).iter().any(|c| c.starts_with("clientdelperm cldbid=5")));
    }

    #[tokio::test]
    async fn relay_server_chat_checks_its_permanent_mode_before_sending_to_query() {
        use crate::{
            core::expiry_tests::test_core_teamspeak,
            media::{MediaPlane, Routing},
            store::Store,
        };

        let store = Store::in_memory().unwrap();
        store.init_server_chat(true).unwrap();
        let core = test_core_teamspeak(store).unwrap();
        let (notes, mut receiver) = mpsc::unbounded_channel();
        core.bridge(BridgeMsg::Attach(notes)).await;
        let BridgeNote::Snapshot { server, .. } = receiver.recv().await.unwrap() else { panic!("snapshot") };
        let (webrtc, _media) = mpsc::channel(8);
        let plane = MediaPlane {
            routing: Arc::new(arc_swap::ArcSwap::from_pointee(Routing::default())),
            sinks: Arc::default(),
            uplinks: Arc::default(),
            webrtc,
        };
        let (query, commands) = channel_query(false).await;
        commands.lock().unwrap().clear();
        let dir = tempfile::tempdir().unwrap();
        let mut identities = Identities::load(&dir.path().join("identities.json")).unwrap();
        let (puppet_events, _puppets) = mpsc::channel(8);
        let (listener_events, _listeners) = mpsc::channel(8);
        let mut bridge = Bridge {
            query,
            core: core.clone(),
            shared: Arc::new(Shared::new(core, plane)),
            voice: "127.0.0.1:1".parse().unwrap(),
            public_url: None,
            identities: &mut identities,
            puppet_uids: HashSet::new(),
            map: ChannelMap::load(&dir.path().join("channels.json")).unwrap(),
            ts: HashMap::new(),
            channels: BTreeMap::new(),
            server: Some(server),
            locals: HashMap::new(),
            remotes: HashMap::new(),
            puppet_clids: HashMap::new(),
            puppet_events,
            puppet_group: None,
            listeners: ChannelListeners::new("127.0.0.1:1".parse().unwrap(), "p".into(), listener_events),
            descriptions: HashMap::new(),
            query_clid: None,
            locked: HashSet::new(),
            server_password: None,
            password_file: dir.path().join("server-password.json"),
            held: HashMap::new(),
            talkers: HashMap::new(),
            whispers: HashMap::new(),
            links: HashMap::new(),
        };
        let message = ChatMessage {
            id: 1,
            target: ChatTarget::Server,
            author: 1,
            author_uid: "u".into(),
            author_name: "Ann".into(),
            text: "server text".into(),
            sent_at: 0,
            mentions: vec![],
            attachments: vec![],
            edited_at: None,
            signature: None,
        };
        bridge.relay_chat(message.clone()).await;
        assert!(commands.lock().unwrap().is_empty(), "encrypted server chat must never reach TeamSpeak");
        bridge.server.as_mut().unwrap().server_chat = Default::default();
        bridge.relay_chat(message).await;
        let commands = commands.lock().unwrap();
        assert_eq!(commands.len(), 1);
        assert!(commands[0].starts_with("sendtextmessage targetmode=3 target=1 "), "{commands:?}");
    }

    #[tokio::test]
    async fn new_channels_get_the_server_admin_permission_power_or_go() {
        let (query, commands) = channel_query(false).await;
        assert_eq!(
            create_channel(&query, Cmd::new("channelcreate").arg("channel_name", "A"), false).await.unwrap(),
            42
        );
        let sent = commands.lock().unwrap().clone();
        assert!(sent.contains(
            &"channeladdperm cid=42 permsid=i_channel_needed_permission_modify_power permvalue=75".to_owned()
        ));
        assert!(!sent.iter().any(|c| c.contains("i_channel_needed_join_power")), "{sent:?}");
        assert!(!sent.iter().any(|c| c.starts_with("channeldelete")), "{sent:?}");

        // An encrypted channel is closed to every TeamSpeak group, and only ServerQuery may reopen it.
        let (query, commands) = channel_query(false).await;
        assert_eq!(create_channel(&query, Cmd::new("channelcreate").arg("channel_name", "E"), true).await.unwrap(), 42);
        let sent = commands.lock().unwrap().clone();
        assert_eq!(
            sent[3..],
            [
                "channeladdperm cid=42 permsid=i_channel_needed_permission_modify_power permvalue=100",
                "channeladdperm cid=42 permsid=i_channel_needed_join_power permvalue=1000000"
            ],
            "{sent:?}"
        );

        let (query, commands) = channel_query(true).await;
        assert!(create_channel(&query, Cmd::new("channelcreate").arg("channel_name", "A"), false).await.is_err());
        let sent = commands.lock().unwrap().clone();
        assert_eq!(sent.last().unwrap(), "channeldelete cid=42 force=1", "{sent:?}");
    }

    #[tokio::test]
    async fn listener_relays_channel_text_to_remote_author_without_puppet_duplicates() {
        use crate::teamspeak::query::tests::{ChannelEvent, channel_server};

        let dir = tempfile::tempdir().unwrap();
        let mut map = ChannelMap::load(&dir.path().join("channels.json")).unwrap();
        map.link(3, 42);
        let remotes = HashMap::from([(7, remote(123, 42))]);
        let mut server = channel_server().await;
        let (tx, mut rx) = mpsc::channel(16);
        let mut pool = ChannelListeners::new(server.addr, "p ss".into(), tx);
        pool.reconcile(listener_channels(&remotes, &map));
        assert!(matches!(server.event().await, ChannelEvent::Ready { cid: 42, .. }));
        // Identical successive messages are separate messages, not duplicates.
        for _ in 0..2 {
            let body = "[b]Hi[/b] | / \\ 日本\nagain";
            server.message(42, 2, 7, body);
            let text = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await.unwrap().unwrap();
            assert!(pool.accepts(&text));
            let message = channel_chat(&remotes, &map, text.cid, text.invoker, text.text).unwrap();
            let BridgeMsg::Chat(author, target, text) = message else { panic!("expected channel chat") };
            assert_eq!(author, 123);
            assert_eq!(target, ChatTarget::Channel(3));
            assert_eq!(text, body);
            assert_eq!(puppet_chat_target(456, MessageTarget::Channel), None);
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn channel_chat_ignores_own_puppet_query_and_unknown_invokers() {
        let dir = tempfile::tempdir().unwrap();
        let mut map = ChannelMap::load(&dir.path().join("channels.json")).unwrap();
        map.link(3, 42);
        let remotes = HashMap::from([(7, remote(123, 42))]);
        // Puppets (8), the posting query (9) and listeners (100) are not remotes.
        for invoker in [8, 9, 100, 0] {
            assert!(channel_chat(&remotes, &map, 42, invoker, "echo".into()).is_none());
        }
        assert!(channel_chat(&remotes, &map, 99, 7, "unbridged".into()).is_none());
        let moved = HashMap::from([(7, remote(123, 55))]);
        let message = channel_chat(&moved, &map, 42, 7, "sent before moving".into()).unwrap();
        assert!(matches!(message, BridgeMsg::Chat(123, ChatTarget::Channel(3), _)));
    }

    #[test]
    fn listener_channels_follow_remote_membership_and_ignore_unbridged_channels() {
        let dir = tempfile::tempdir().unwrap();
        let mut map = ChannelMap::load(&dir.path().join("channels.json")).unwrap();
        map.link(3, 42);
        map.link(4, 55);
        let mut remotes = HashMap::from([(7, remote(123, 42)), (8, remote(124, 42)), (9, remote(125, 99))]);
        assert_eq!(listener_channels(&remotes, &map), BTreeSet::from([42]));
        remotes.get_mut(&7).unwrap().cid = 55;
        assert_eq!(listener_channels(&remotes, &map), BTreeSet::from([42, 55]));
        remotes.remove(&8);
        assert_eq!(listener_channels(&remotes, &map), BTreeSet::from([55]));
        remotes.remove(&7);
        assert!(listener_channels(&remotes, &map).is_empty());
    }

    #[test]
    fn puppets_relay_private_messages_and_pokes_only() {
        assert_eq!(
            puppet_chat_target(123, MessageTarget::Client(tsclientlib::ClientId(7))),
            Some(ChatTarget::Client(123))
        );
        assert_eq!(
            puppet_chat_target(123, MessageTarget::Poke(tsclientlib::ClientId(7))),
            Some(ChatTarget::Client(123))
        );
        assert_eq!(puppet_chat_target(123, MessageTarget::Channel), None);
        assert_eq!(puppet_chat_target(123, MessageTarget::Server), None);
    }

    #[test]
    fn long_messages_are_split_and_attachments_become_links() {
        let mut m = ChatMessage {
            signature: None,
            id: 1,
            target: ChatTarget::Server,
            author: 1,
            author_uid: "u".into(),
            author_name: "Ann".into(),
            text: "look".into(),
            sent_at: 0,
            mentions: vec![],
            attachments: vec![gwar_proto::Attachment {
                sha256: String::new(),
                id: "f1".into(),
                name: "cat.png".into(),
                mime: "image/png".into(),
                size: 10,
                url: "/files/f1/cat.png".into(),
                width: None,
                height: None,
                sealed: None,
            }],
            edited_at: None,
        };
        assert_eq!(
            ts_text(&m, Some("https://gwar.example/")),
            "look\n📎 cat.png — https://gwar.example/files/f1/cat.png"
        );
        m.text = "[img]https://tracker.example/p.png[/img] [url=https://evil.example]bank[/url]".into();
        m.attachments[0].name = "[b]invoice[/b].pdf".into();
        let relayed = ts_text(&m, None);
        assert_eq!(relayed.matches('[').count(), relayed.matches("[\u{200B}").count(), "{relayed:?}");
        // Nor does the name a relayed message is signed with: `[hr]` is a tag on its own.
        assert_eq!(signed_text("hr", "hi"), "[\u{200B}hr] hi");
        assert_eq!(
            relayed.replace('\u{200B}', ""),
            "[img]https://tracker.example/p.png[/img] [url=https://evil.example]bank[/url]\n📎 [b]invoice[/b].pdf"
        );
        m.text = "x".repeat(2500);
        let parts = split_for_teamspeak(&m.text);
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| p.chars().count() <= 1000));
    }

    #[test]
    fn nobody_has_a_puppet_in_an_encrypted_channel_unless_chatting_privately() {
        assert_eq!(presence_of(Some((false, true)), false), Presence::Puppet);
        assert_eq!(presence_of(Some((false, false)), false), Presence::Listed);
        assert_eq!(presence_of(Some((false, false)), true), Presence::Puppet);
        // Encrypted, even with a TeamSpeak user there (who could not have joined): no puppet, not listed.
        assert_eq!(presence_of(Some((true, true)), false), Presence::Absent);
        assert_eq!(presence_of(Some((true, false)), true), Presence::Puppet);
        assert_eq!(presence_of(None, true), Presence::Puppet);
        assert_eq!(presence_of(None, false), Presence::Absent);
    }

    #[test]
    fn a_puppet_speaks_only_in_its_users_plaintext_channel() {
        assert_eq!(target_of(Some((false, Some(5))), Some(1)), (5, true));
        // In an encrypted channel or out of voice: silent, in the default channel.
        assert_eq!(target_of(Some((true, Some(7))), Some(1)), (1, false));
        assert_eq!(target_of(None, Some(1)), (1, false));
        // Moving from the default channel into an encrypted one keeps the TeamSpeak
        // channel but still changes the target, so the puppet is told to fall silent.
        assert_ne!(target_of(Some((false, Some(1))), Some(1)), target_of(Some((true, Some(7))), Some(1)));
    }

    #[test]
    fn description_lists_users_without_puppets() {
        assert_eq!(describe(&[]), "");
        assert_eq!(describe(&["Ania".into(), "Bartek".into()]), "[b]In the app (2):[/b] Ania, Bartek");
        // Nicknames are names, not markup.
        assert_eq!(describe(&["[img]x[/img]".into()]), "[b]In the app (1):[/b] [\u{200B}img]x[\u{200B}/img]");
    }

    #[test]
    fn platform_follows_the_client_major_version() {
        assert_eq!(platform_of("3.6.2 [Build: 1695203293]"), Platform::Ts3);
        assert_eq!(platform_of("6.0.0-beta3"), Platform::Ts6);
        assert_eq!(platform_of(""), Platform::Ts3);
    }

    // ------------------------------------------------------------- watch links

    /// A bridge over `query` with channels 1 → 10 and 2 → 20 (2 encrypted), and our user alice
    /// (session 500) in 1, sharing.
    async fn watch_bridge<'a>(
        query: Query,
        dir: &Path,
        identities: &'a mut Identities,
        file_access: FileAccess,
    ) -> (Bridge<'a>, CoreHandle) {
        use crate::{
            core::expiry_tests::test_core_teamspeak,
            media::{MediaPlane, Routing},
            store::Store,
        };
        let store = Store::in_memory().unwrap();
        store.init_server_chat(false).unwrap();
        let core = test_core_teamspeak(store).unwrap();
        let (notes, mut receiver) = mpsc::unbounded_channel();
        core.bridge(BridgeMsg::Attach(notes)).await;
        let BridgeNote::Snapshot { mut server, .. } = receiver.recv().await.unwrap() else { panic!("snapshot") };
        server.file_access = file_access;
        let (webrtc, _media) = mpsc::channel(8);
        let plane = MediaPlane {
            routing: Arc::new(arc_swap::ArcSwap::from_pointee(Routing::default())),
            sinks: Arc::default(),
            uplinks: Arc::default(),
            webrtc,
        };
        let (puppet_events, _puppets) = mpsc::channel(8);
        let (listener_events, _listeners) = mpsc::channel(8);
        let mut bridge = Bridge {
            query,
            core: core.clone(),
            shared: Arc::new(Shared::new(core.clone(), plane)),
            voice: "127.0.0.1:1".parse().unwrap(),
            public_url: None,
            identities,
            puppet_uids: HashSet::new(),
            map: ChannelMap::load(&dir.join("channels.json")).unwrap(),
            ts: HashMap::new(),
            channels: BTreeMap::new(),
            server: Some(server),
            locals: HashMap::new(),
            remotes: HashMap::new(),
            puppet_clids: HashMap::new(),
            puppet_events,
            puppet_group: None,
            listeners: ChannelListeners::new("127.0.0.1:1".parse().unwrap(), "p".into(), listener_events),
            descriptions: HashMap::new(),
            query_clid: None,
            locked: HashSet::new(),
            server_password: None,
            password_file: dir.join("server-password.json"),
            held: HashMap::new(),
            talkers: HashMap::new(),
            whispers: HashMap::new(),
            links: HashMap::new(),
        };
        bridge.map.link(1, 10);
        bridge.map.link(2, 20);
        for (id, encrypted) in [(1, false), (2, true)] {
            bridge.channels.insert(
                id,
                Channel {
                    id,
                    parent: None,
                    name: format!("c{id}"),
                    topic: String::new(),
                    position: 0,
                    has_password: false,
                    max_clients: None,
                    max_family_clients: None,
                    encrypted,
                    history: false,
                    private: false,
                    access: None,
                    synced: true,
                    overrides: Vec::new(),
                },
            );
        }
        let client = Client {
            id: 500,
            uid: "alice-uid".into(),
            nickname: "[b]alice".into(),
            channel: Some(1),
            groups: vec![],
            platform: Platform::Web,
            muted: false,
            deafened: false,
            away: None,
            talking: false,
            voice: true,
            voice_binding: None,
            server_muted: false,
            server_deafened: false,
            sharing: true,
            channel_group: None,
        };
        let local = Local {
            client,
            ts_uid: "puppet-uid".into(),
            puppet: None,
            generation: 0,
            route: 3,
            clid: None,
            // No real puppet starts in these tests.
            retry_at: Some(Instant::now() + Duration::from_secs(3600)),
            idle_since: None,
            chatting_until: None,
            grouped: false,
        };
        bridge.locals.insert(500, local);
        (bridge, core)
    }

    const LINK: &str = "https://gwar.test/watch#c2VjcmV0LXRva2VuLWZvci10aGUtdGVzdHMtb25seQ";

    fn watch_link(channel: ChannelId) -> BridgeNote {
        BridgeNote::WatchLink { share: "S".into(), session: 500, channel, url: WatchUrl(LINK.into()) }
    }

    /// The `sendtextmessage` commands sent so far, as (target mode, target, decoded text).
    fn messages(commands: &std::sync::Mutex<Vec<String>>) -> Vec<(String, String, String)> {
        commands
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.starts_with("sendtextmessage "))
            .map(|c| {
                let field = |name: &str| {
                    c.split(' ').find_map(|f| f.strip_prefix(&format!("{name}="))).unwrap_or_default().to_owned()
                };
                let msg = field("msg").replace("\\s", " ").replace("\\/", "/").replace("\\p", "|");
                (field("targetmode"), field("target"), msg)
            })
            .collect()
    }

    #[tokio::test]
    async fn a_watch_link_goes_through_the_sharers_puppet() {
        let (query, commands) = channel_query(false).await;
        let dir = tempfile::tempdir().unwrap();
        let mut identities = Identities::load(&dir.path().join("identities.json")).unwrap();
        let (mut bridge, _core) = watch_bridge(query, dir.path(), &mut identities, FileAccess::Link).await;
        let (cmds, mut sent) = mpsc::channel(8);
        bridge.locals.get_mut(&500).unwrap().puppet = Some(PuppetHandle::for_test(cmds));
        bridge.remotes.insert(7, remote(70, 10));
        bridge.on_note(watch_link(1)).await;
        let Ok(PuppetCmd::Text { mode, target, text, channel }) = sent.try_recv() else { panic!("a puppet post") };
        assert_eq!((mode, target), (2, 0));
        assert_eq!(text, format!("🖥 [\u{200B}b]alice is sharing their screen. Watch: {LINK}"));
        assert_eq!(channel, Some(ChannelText { cid: 10, encrypted: false, route: 3 }));
        assert!(messages(&commands).is_empty(), "the puppet posts, not ServerQuery");
        assert!(bridge.links.contains_key("S"));
    }

    #[tokio::test]
    async fn without_a_puppet_serverquery_signs_the_link_only_for_readers_and_late_joiners_get_it_once() {
        let (query, commands) = channel_query(false).await;
        let dir = tempfile::tempdir().unwrap();
        let mut identities = Identities::load(&dir.path().join("identities.json")).unwrap();
        let (mut bridge, core) = watch_bridge(query, dir.path(), &mut identities, FileAccess::Link).await;
        // Nobody on TeamSpeak in the channel: nothing is posted, but the link is kept.
        bridge.on_note(watch_link(1)).await;
        assert!(messages(&commands).is_empty());
        assert!(bridge.links.contains_key("S"));
        // A TeamSpeak user enters: one private message from ServerQuery.
        let session = core
            .remote_join(RemoteClient {
                uid: "ts:late".into(),
                nickname: "late".into(),
                channel: 2,
                platform: Platform::Ts3,
                muted: false,
                deafened: false,
                away: None,
            })
            .await
            .unwrap();
        bridge.remotes.insert(7, remote(session, 20));
        bridge.client_moved(7, 10).await;
        let text = format!("🖥 [\u{200B}b]alice is sharing their screen. Watch: {LINK}");
        assert_eq!(messages(&commands), vec![("1".into(), "7".into(), text.clone())]);
        // Out and back in: told already.
        bridge.client_moved(7, 20).await;
        bridge.client_moved(7, 10).await;
        assert_eq!(messages(&commands).len(), 1, "a late joiner gets the link once");

        // A second share, posted while a TeamSpeak user reads the channel: signed by ServerQuery.
        commands.lock().unwrap().clear();
        let second = BridgeNote::WatchLink { share: "T".into(), session: 500, channel: 1, url: WatchUrl(LINK.into()) };
        bridge.on_note(second).await;
        let signed = format!("[\u{200B}[\u{200B}b]alice] {text}");
        assert_eq!(messages(&commands), vec![("2".into(), "10".into(), signed)]);
        // Those who read it in the channel are not told again.
        bridge.client_moved(7, 20).await;
        bridge.client_moved(7, 10).await;
        assert_eq!(messages(&commands).len(), 1);

        // Gone: nobody is told any more.
        bridge.on_note(BridgeNote::WatchLinkGone { share: "S".into() }).await;
        bridge.on_note(BridgeNote::WatchLinkGone { share: "T".into() }).await;
        assert!(bridge.links.is_empty(), "the bridge's copy is gone");
        bridge.remotes.insert(8, remote(session, 20));
        bridge.client_moved(8, 10).await;
        assert_eq!(messages(&commands).len(), 1);
    }

    #[tokio::test]
    async fn no_watch_link_reaches_an_encrypted_channel_or_a_members_only_server() {
        for (channel, access) in [(2, FileAccess::Link), (1, FileAccess::Members)] {
            let (query, commands) = channel_query(false).await;
            let dir = tempfile::tempdir().unwrap();
            let mut identities = Identities::load(&dir.path().join("identities.json")).unwrap();
            let (mut bridge, _core) = watch_bridge(query, dir.path(), &mut identities, access).await;
            let (cmds, mut sent) = mpsc::channel(8);
            let local = bridge.locals.get_mut(&500).unwrap();
            local.puppet = Some(PuppetHandle::for_test(cmds));
            local.client.channel = Some(channel);
            bridge.remotes.insert(7, remote(70, if channel == 2 { 20 } else { 10 }));
            bridge.on_note(watch_link(channel)).await;
            assert!(sent.try_recv().is_err(), "channel {channel}, {access:?}: no puppet post");
            assert!(messages(&commands).is_empty(), "channel {channel}, {access:?}: no ServerQuery post");
            assert!(bridge.links.is_empty(), "channel {channel}, {access:?}: nothing kept");
        }
    }

    #[tokio::test]
    async fn the_description_marks_a_sharer_but_never_carries_the_link() {
        let (query, commands) = channel_query(false).await;
        let dir = tempfile::tempdir().unwrap();
        let mut identities = Identities::load(&dir.path().join("identities.json")).unwrap();
        let (mut bridge, _core) = watch_bridge(query, dir.path(), &mut identities, FileAccess::Link).await;
        bridge.ts.insert(10, TsChannel::default());
        bridge.on_note(watch_link(1)).await;
        bridge.describe_channels().await;
        let description = bridge.descriptions[&10].clone();
        assert_eq!(description, "[b]In the app (1):[/b] [\u{200B}b]alice · sharing screen");
        assert!(!commands.lock().unwrap().iter().any(|c| c.contains("watch")), "no link in any command");
    }
}
