//! The bridge between our core and the official TeamSpeak server.
//!
//! * Our channels, server name and welcome message are mirrored onto the
//!   TeamSpeak server through ServerQuery; ours stay the authority.
//! * Every TeamSpeak user becomes a remote session in our core (presence,
//!   channel, mute flags, chat), learned from ServerQuery notifications.
//! * Every user of ours gets a puppet connection (see [`super::puppet`]) that
//!   carries their voice and chat to TeamSpeak and hears TeamSpeak users.

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use rand::Rng;
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};
use tsclientlib::{Identity, MessageTarget};
use vc_proto::{
    Channel, ChannelId, ChatMessage, ChatTarget, Client, Event, LeaveReason, Platform, ServerInfo, SessionId,
};

use super::{
    channels::{self, ChannelMap, TsChannel},
    install,
    process::{ProcessConfig, TsProcess},
    puppet::{self, PuppetCmd, PuppetEvent, PuppetHandle, PuppetSpec, Shared},
    query::{Cmd, Query, QueryError, Record},
};
use crate::{
    TeamSpeakConfig,
    core::{
        CoreHandle,
        bridge::{BridgeMsg, BridgeNote, RemoteClient, RemoteUpdate},
    },
    media::MediaPlane,
};

/// Name of the TeamSpeak server group that exempts puppets from flood limits.
const PUPPET_GROUP: &str = "Voice bridge";
/// TeamSpeak client description of every puppet.
const PUPPET_DESCRIPTION: &str = "Gwar user (bridged)";
/// Flood points one address may collect before TeamSpeak blocks it (default 250).
const PUPPET_IP_BLOCK: u32 = 2500;

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
    query: SocketAddr,
    password: String,
    voice: SocketAddr,
    map: PathBuf,
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
    clid: Option<u16>,
    retry_at: Option<Instant>,
    /// Since when the puppet has had nothing to do (see [`LINGER`]).
    idle_since: Option<Instant>,
    /// A private conversation with a TeamSpeak user keeps the puppet until then.
    chatting_until: Option<Instant>,
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

/// A TeamSpeak user, mirrored as a remote session in our core.
struct Remote {
    session: SessionId,
    nickname: String,
    cid: u64,
    muted: bool,
    deafened: bool,
    away: Option<String>,
}

struct Bridge<'a> {
    query: Query,
    core: CoreHandle,
    shared: Arc<Shared>,
    voice: SocketAddr,
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
    /// Channel messages already relayed: every puppet in the channel reports them.
    relayed: VecDeque<(u16, u64, Instant)>,
    /// Last description written per TeamSpeak channel (who is here without a puppet).
    descriptions: HashMap<u64, String>,
    /// Our own ServerQuery client id, once looked up.
    query_clid: Option<u64>,
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
            Query::connect(link.query, "serveradmin", &link.password, 1).await.context("connect ServerQuery")?;
        let (puppet_events, mut puppet_rx) = mpsc::channel(256);
        let mut bridge = Bridge {
            query,
            core: core.clone(),
            shared,
            voice: link.voice,
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
            relayed: VecDeque::new(),
            descriptions: HashMap::new(),
            query_clid: None,
        };
        let result = bridge.serve(&mut notifications, &mut puppet_rx, ready).await;
        // Hand the TeamSpeak users back; the next attach starts from scratch.
        for (clid, remote) in bridge.remotes.drain() {
            bridge.shared.forget(clid);
            bridge.core.bridge(BridgeMsg::Leave(remote.session, LeaveReason::Timeout)).await;
        }
        result
    }

    async fn serve(
        &mut self,
        notifications: &mut mpsc::Receiver<super::query::Notification>,
        puppet_rx: &mut mpsc::Receiver<PuppetEvent>,
        ready: &watch::Sender<bool>,
    ) -> Result<()> {
        let (tx, mut notes) = mpsc::unbounded_channel();
        self.core.bridge(BridgeMsg::Attach(tx)).await;
        let Some(BridgeNote::Snapshot { server, channels, clients }) = notes.recv().await else {
            bail!("core did not send a snapshot");
        };
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
                    Some(note) => self.on_note(note).await,
                    None => bail!("core stopped"),
                },
                notification = notifications.recv() => match notification {
                    Some(n) => self.on_notification(n).await,
                    None => bail!("ServerQuery connection closed"),
                },
                Some(event) = puppet_rx.recv() => self.on_puppet(event).await,
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
        for perm in [
            "b_client_ignore_antiflood",
            "b_client_ignore_bans",
            "b_channel_join_ignore_password",
            "b_channel_join_ignore_maxclients",
            // Our users may write to everyone.
            "b_client_server_textmessage_send",
        ] {
            self.query
                .call(
                    Cmd::new("servergroupaddperm")
                        .arg("sgid", sgid)
                        .arg("permsid", perm)
                        .arg("permvalue", 1)
                        .arg("permnegated", 0)
                        .arg("permskip", 0),
                )
                .await?;
        }
        self.puppet_group = Some(sgid);
        Ok(())
    }

    async fn sync_channels(&mut self, channels: Vec<Channel>) -> Result<()> {
        self.channels = channels.into_iter().map(|c| (c.id, c)).collect();
        let list = self.query.call(Cmd::new("channellist").flag("-topic").flag("-flags").flag("-limits")).await?;
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
                .arg("channel_codec_quality", 10);
            for (key, value) in channels::max_clients_args(want.max_clients) {
                cmd = cmd.arg(key, value);
            }
            if let Some(Some(p)) = &password {
                cmd = cmd.arg("channel_password", p);
            }
            let created = self.query.call(cmd).await?;
            let cid =
                created.first().and_then(|r| r.get("cid")?.parse().ok()).context("channelcreate returned no cid")?;
            want.cid = cid;
            want.password = matches!(password, Some(Some(_)));
            self.ts.insert(cid, want);
            self.map.link(channel.id, cid);
            return self.map.save();
        };

        let current = self.ts.get(&cid).cloned().unwrap_or_default();
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
            nickname: r.get("client_nickname").cloned().unwrap_or_default(),
            cid,
            muted: flag(r, "client_input_muted"),
            deafened: flag(r, "client_output_muted"),
            away: away_of(r),
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
        let Some(session) = self.core.remote_join(client).await else { return };
        self.shared.mirrors.write().expect("mirrors lock").insert(clid, session);
        self.remotes.insert(clid, Remote { session, ..remote });
        self.reconcile(Duration::ZERO);
    }

    fn our_channel(&self, cid: u64) -> ChannelId {
        self.map.ours(cid).or_else(|| self.server.as_ref().map(|s| s.default_channel)).unwrap_or_default()
    }

    async fn remove_remote(&mut self, clid: u16, reason: LeaveReason) {
        if let Some(remote) = self.remotes.remove(&clid) {
            self.shared.forget(clid);
            self.core.bridge(BridgeMsg::Leave(remote.session, reason)).await;
            self.reconcile(Duration::ZERO);
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
            if let Some(nickname) = r.get("client_nickname").filter(|n| **n != remote.nickname) {
                remote.nickname = nickname.clone();
                update.nickname = Some(nickname.clone());
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
            clid: None,
            retry_at: None,
            idle_since: None,
            chatting_until: None,
        };
        self.locals.insert(local.client.id, local);
    }

    fn presence(&self, local: &Local, now: Instant) -> Presence {
        let chatting = local.chatting_until.is_some_and(|until| until > now);
        let Some(channel) = local.client.channel else {
            // A private conversation still needs a puppet (in the default channel).
            return if chatting { Presence::Puppet } else { Presence::Absent };
        };
        let shares_channel = self.remotes.values().any(|r| self.our_channel(r.cid) == channel);
        if shares_channel || chatting { Presence::Puppet } else { Presence::Listed }
    }

    /// Starts puppets for users who need one now and retires idle ones.
    /// `stagger` spreads simultaneous starts so the flood guard stays calm.
    fn reconcile(&mut self, stagger: Duration) {
        let now = Instant::now();
        let decisions: Vec<_> = self.locals.values().map(|l| (l.client.id, self.presence(l, now))).collect();
        let mut started = 0u32;
        for (session, presence) in decisions {
            let Some(local) = self.locals.get_mut(&session) else { continue };
            match presence {
                Presence::Puppet => {
                    local.idle_since = None;
                    if local.puppet.is_none() && local.retry_at.is_none_or(|at| at <= now) {
                        self.spawn_puppet(session, stagger * started);
                        started += 1;
                    }
                }
                Presence::Listed if local.puppet.is_some() => {
                    let idle = *local.idle_since.get_or_insert(now);
                    if now.duration_since(idle) >= LINGER {
                        self.retire_puppet(session);
                    }
                }
                Presence::Listed => local.idle_since = None,
                Presence::Absent => self.retire_puppet(session),
            }
        }
    }

    fn spawn_puppet(&mut self, session: SessionId, delay: Duration) {
        let Some(local) = self.locals.get(&session) else { return };
        let Ok(identity) = self.identities.get(&local.client.uid) else { return };
        let Some(local) = self.locals.get_mut(&session) else { return };
        local.generation += 1;
        let c = &local.client;
        let spec = PuppetSpec {
            session,
            generation: local.generation,
            nickname: c.nickname.chars().take(30).collect(),
            identity,
            channel: c.channel.and_then(|c| self.map.ts(c)).unwrap_or(0),
            muted: c.muted,
            deafened: c.deafened,
            away: c.away.clone(),
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
            if let Some(names) = local.client.channel.and_then(|c| self.map.ts(c)).and_then(|cid| listed.get_mut(&cid))
            {
                names.push(local.client.nickname.clone());
            }
        }
        for (cid, mut names) in listed {
            names.sort_by_key(|n| n.to_lowercase());
            let text = describe(&names);
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
        let Some(puppet) = &local.puppet else { return };
        let after = &local.client;
        if before.channel != after.channel
            && let Some(cid) = after.channel.and_then(|c| self.map.ts(c))
        {
            puppet.send(PuppetCmd::Move(cid));
        }
        if (before.muted, before.deafened) != (after.muted, after.deafened) {
            puppet.send(PuppetCmd::Flags { muted: after.muted, deafened: after.deafened });
        }
        if before.nickname != after.nickname {
            puppet.send(PuppetCmd::Nickname(after.nickname.chars().take(30).collect()));
        }
        if before.away != after.away {
            puppet.send(PuppetCmd::Away(after.away.clone()));
        }
    }

    // --------------------------------------------------------------- events

    async fn on_note(&mut self, note: BridgeNote) {
        match note {
            BridgeNote::Snapshot { .. } => warn!("unexpected second snapshot"),
            BridgeNote::Channel { channel, password, .. } => {
                self.channels.insert(channel.id, channel.clone());
                if let Err(e) = self.ensure_channel(&channel, password).await {
                    warn!(channel = channel.id, "TeamSpeak channel sync: {e:#}");
                }
            }
            BridgeNote::Event(event) => self.on_event(event).await,
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
                let Some(cid) = client.channel.and_then(|c| self.map.ts(c)).filter(|cid| *cid != remote.cid) else {
                    return;
                };
                if let Err(e) = self.query.call(Cmd::new("clientmove").arg("clid", clid).arg("cid", cid)).await {
                    warn!(clid, "TeamSpeak move: {e:#}");
                }
            }
            Event::ClientLeft { client, reason } => {
                if self.locals.contains_key(&client) {
                    self.remove_local(client);
                } else if let Some((clid, _)) = self.remote_by_session(client) {
                    // Our side already removed the session; make TeamSpeak follow.
                    self.remotes.remove(&clid);
                    self.shared.forget(clid);
                    let message = match &reason {
                        LeaveReason::Kicked { reason: Some(r), .. } => r.clone(),
                        _ => String::new(),
                    };
                    let cmd = Cmd::new("clientkick").arg("clid", clid).arg("reasonid", 5).arg("reasonmsg", message);
                    if let Err(e) = ignore_missing(self.query.call(cmd).await) {
                        warn!(clid, "TeamSpeak kick: {e:#}");
                    }
                }
            }
            Event::ChatMessage(message) => self.relay_chat(message).await,
            _ => {}
        }
    }

    /// A message written on our side, delivered to TeamSpeak.
    async fn relay_chat(&mut self, m: ChatMessage) {
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
        // message until it is connected.
        let puppet = author.and_then(|l| l.puppet.as_ref()).filter(|_| match m.target {
            ChatTarget::Channel(channel) => author.is_some_and(|l| l.client.channel == Some(channel)),
            _ => true,
        });
        if let Some(puppet) = puppet {
            puppet.send(PuppetCmd::Text { mode, target, text: m.text });
            return;
        }
        // Otherwise ServerQuery (invisible to TeamSpeak users) posts it, signed.
        let text = format!("[{}] {}", m.author_name, m.text);
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
            debug!("relayed message: {e:#}");
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
                            by: r.get("invokername").cloned().unwrap_or_default(),
                            reason: r.get("reasonmsg").filter(|m| !m.is_empty()).cloned(),
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
            remote.cid = cid;
            let session = remote.session;
            let channel = self.our_channel(cid);
            let update = RemoteUpdate { channel: Some(channel), ..Default::default() };
            self.core.bridge(BridgeMsg::Update(session, update)).await;
            self.reconcile(Duration::ZERO);
        } else if let Some(local) = self.puppet_clids.get(&clid).and_then(|s| self.locals.get(s)) {
            // Someone moved a puppet on TeamSpeak; our user decides where they are.
            if let (Some(want), Some(puppet)) = (local.client.channel.and_then(|c| self.map.ts(c)), &local.puppet)
                && want != cid
            {
                puppet.send(PuppetCmd::Move(want));
            }
        }
    }

    async fn on_puppet(&mut self, event: PuppetEvent) {
        match event {
            PuppetEvent::Connected { session, generation, clid } => {
                let Some(local) = self.locals.get_mut(&session).filter(|l| l.generation == generation) else { return };
                local.clid = Some(clid);
                self.puppet_clids.insert(clid, session);
                let ts_uid = local.ts_uid.clone();
                // The channel may have changed while connecting.
                if let (Some(cid), Some(puppet)) = (local.client.channel.and_then(|c| self.map.ts(c)), &local.puppet) {
                    puppet.send(PuppetCmd::Move(cid));
                }
                if let Err(e) = self.join_puppet_group(&ts_uid).await {
                    debug!(session, "puppet group: {e:#}");
                }
                // Say what this client is, so nobody mistakes it for a TeamSpeak app.
                let describe = Cmd::new("clientedit").arg("clid", clid).arg("client_description", PUPPET_DESCRIPTION);
                if let Err(e) = self.query.call(describe).await {
                    debug!(session, "puppet description: {e:#}");
                }
            }
            PuppetEvent::Text { session, invoker, target, text } => {
                let Some(remote) = self.remotes.get(&invoker) else { return };
                if matches!(target, MessageTarget::Client(_) | MessageTarget::Poke(_))
                    && let Some(local) = self.locals.get_mut(&session)
                {
                    local.chatting_until = Some(Instant::now() + CHAT_KEEP);
                }
                let (author, cid) = (remote.session, remote.cid);
                let target = match target {
                    MessageTarget::Channel => {
                        if !self.first_report(invoker, &text) {
                            return;
                        }
                        ChatTarget::Channel(self.our_channel(cid))
                    }
                    MessageTarget::Client(_) | MessageTarget::Poke(_) => ChatTarget::Client(session),
                    // Server messages arrive through ServerQuery.
                    MessageTarget::Server => return,
                };
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

    /// Whether this channel message is new (each puppet in the channel reports it).
    fn first_report(&mut self, invoker: u16, text: &str) -> bool {
        use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
        let hash = BuildHasherDefault::<DefaultHasher>::default().hash_one(text);
        let now = Instant::now();
        while self.relayed.front().is_some_and(|(_, _, at)| now.duration_since(*at) > Duration::from_secs(2)) {
            self.relayed.pop_front();
        }
        if self.relayed.iter().any(|(i, h, _)| *i == invoker && *h == hash) {
            return false;
        }
        self.relayed.push_back((invoker, hash, now));
        true
    }
}

/// Channel description listing our users who have no puppet there.
fn describe(names: &[String]) -> String {
    if names.is_empty() {
        return String::new();
    }
    format!("[b]In the app ({}):[/b] {}", names.len(), names.join(", "))
}

fn is_remote(client: &Client) -> bool {
    client.uid.starts_with("ts:")
}

fn flag(r: &Record, key: &str) -> bool {
    r.get(key).map(String::as_str) == Some("1")
}

fn away_of(r: &Record) -> Option<String> {
    flag(r, "client_away").then(|| r.get("client_away_message").cloned().unwrap_or_default())
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
    use super::*;

    #[test]
    fn description_lists_users_without_puppets() {
        assert_eq!(describe(&[]), "");
        assert_eq!(describe(&["Ania".into(), "Bartek".into()]), "[b]In the app (2):[/b] Ania, Bartek");
    }

    #[test]
    fn platform_follows_the_client_major_version() {
        assert_eq!(platform_of("3.6.2 [Build: 1695203293]"), Platform::Ts3);
        assert_eq!(platform_of("6.0.0-beta3"), Platform::Ts6);
        assert_eq!(platform_of(""), Platform::Ts3);
    }
}
