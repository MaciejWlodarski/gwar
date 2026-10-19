//! A TeamSpeak import into the running server (docs/teamspeak-import.md, "Running it"): the plan is
//! applied to the live database in one transaction, the core reloads what it keeps in memory, and
//! everyone connects again, as after a restart that is not one: Gwar clients reconnect by themselves
//! and get a fresh welcome, and the TeamSpeak bridge reattaches and syncs from scratch.

use super::*;
use crate::ts_import::{
    apply::{self, Applied, ApplyOptions},
    plan::Plan,
};

impl CoreHandle {
    /// Applies an import plan to the running server; see [`Core::import_live`].
    pub async fn import_live(&self, plan: Plan, source: String, options: ApplyOptions) -> Result<Applied, String> {
        let (tx, rx) = oneshot::channel();
        let msg = CoreMsg::Import { plan: Box::new(plan), source, options, reply: tx };
        self.tx.send(msg).await.map_err(|_| "the server is stopping".to_owned())?;
        rx.await.map_err(|_| "the server is stopping".to_owned())?
    }
}

impl Core {
    /// Applies `plan` atomically, reloads roles, channels, the server's settings and password, then
    /// has everyone connect again. The actor is busy meanwhile (hashing protected channels' passwords
    /// takes most of it): requests wait, as they would during a restart.
    pub(super) fn import_live(
        &mut self,
        plan: &Plan,
        source: &str,
        mut options: ApplyOptions,
    ) -> Result<Applied, String> {
        if options.replace_channels {
            return Err("--replace-channels needs the server stopped".into());
        }
        // Without a bridge nothing would ever take a typed password from the queue: none is kept.
        options.for_bridge &= self.info.teamspeak;
        // What the core keeps is read inside the same transaction: a failure there undoes the import too.
        let (applied, reloaded) = self
            .store
            .atomically(|store| Ok((apply::apply(store, plan, source, options)?, Reloaded::read(store)?)))
            .map_err(|e| format!("nothing was changed: {e:#}"))?;
        self.install(reloaded);
        // The bridge goes first, so nothing that follows reaches it (it would kick TeamSpeak users, who
        // stay on their server and come back through the new bridge the supervisor attaches).
        if self.bridge.take().is_some() {
            info!("TeamSpeak bridge restarts to take the import");
        }
        // Each Gwar app is told before anything else is queued for it, and then nothing more is sent to
        // anyone while everyone leaves: they reconnect and see the server as it is now.
        for s in self.sessions.values_mut() {
            if let Some(out) = s.out.take() {
                let _ = out.try_send(Outbound::Close(LeaveReason::ServerShutdown));
            }
        }
        let ids: Vec<SessionId> = self.sessions.keys().copied().collect();
        for id in ids {
            self.remove(id, LeaveReason::ServerShutdown);
        }
        info!(
            roles = applied.roles_created + applied.roles_updated,
            channels = applied.channels_created + applied.channels_updated,
            members = applied.members_created + applied.members_updated,
            bans = applied.bans,
            "TeamSpeak import applied to the running server"
        );
        Ok(applied)
    }

    /// Takes over what the database has now (nobody is connected to be told: everyone reconnects).
    fn install(&mut self, r: Reloaded) {
        self.groups = r.groups;
        self.channels = r.channels;
        let info = &mut self.info;
        info.name = r.name.unwrap_or_else(|| info.name.clone());
        info.welcome = r.welcome.unwrap_or_else(|| info.welcome.clone());
        info.max_clients = r.max_clients.unwrap_or(info.max_clients);
        info.default_channel = r
            .default_channel
            .filter(|id| self.channels.get(id).is_some_and(|c| !c.encrypted && !c.private))
            .unwrap_or(info.default_channel);
        // A password the import set (or cleared) is the server's now; one never stored stays as started.
        if let Some(stored) = r.password_hash {
            self.password.store(Arc::new(Some(stored).filter(|h| !h.is_empty()).map(Into::into)));
        }
    }
}

/// What the core keeps of the database, read after an import.
struct Reloaded {
    groups: BTreeMap<GroupId, Group>,
    channels: BTreeMap<ChannelId, ChannelRow>,
    name: Option<String>,
    welcome: Option<String>,
    max_clients: Option<u32>,
    default_channel: Option<ChannelId>,
    password_hash: Option<String>,
}

impl Reloaded {
    fn read(store: &Store) -> anyhow::Result<Self> {
        Ok(Self {
            groups: store.groups()?.into_iter().map(|g| (g.id, g)).collect(),
            channels: store.channels()?.into_iter().map(|c| (c.id, c)).collect(),
            name: store.meta("name")?,
            welcome: store.meta("welcome")?,
            max_clients: store.meta("max_clients")?.and_then(|v| v.parse().ok()),
            default_channel: store.meta("default_channel")?.and_then(|v| v.parse().ok()),
            password_hash: store.meta("password_hash")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::bridge::{BridgeMsg, RemoteClient};
    use super::super::expiry_tests::{on_core, test_core, test_core_teamspeak};
    use super::*;
    use crate::ts_import::{
        plan::Options,
        tests::{BRIDGE, plan, source},
    };

    async fn gwar_session(core: &CoreHandle) -> mpsc::Receiver<Outbound> {
        let (out, frames) = mpsc::channel(OUTBOUND_QUEUE);
        core.connect(ConnectRequest {
            request_id: 1,
            hello_key: String::new(),
            certificate: None,
            uid: crate::identity::uid_for_key(&[1; 32]),
            public_key: URL_SAFE_NO_PAD.encode([1; 32]),
            nickname: "alice".into(),
            platform: Platform::Web,
            out,
            ip: None,
            device: None,
            device_expires_at: None,
            invite: None,
            password_ok: true,
        })
        .await
        .unwrap();
        frames
    }

    async fn remote(core: &CoreHandle, uid: &str, channel: ChannelId) {
        let (tx, rx) = oneshot::channel();
        let client = RemoteClient {
            uid: uid.into(),
            nickname: uid.into(),
            channel,
            platform: Platform::Ts3,
            muted: false,
            deafened: false,
            away: None,
        };
        core.bridge(BridgeMsg::Join(client, tx)).await;
        rx.await.unwrap().unwrap();
    }

    /// The fixture with a server password, and that password typed for the import.
    fn with_server_password() -> (crate::ts_import::plan::Plan, &'static str) {
        let mut s = source();
        s.server.insert("virtualserver_flag_password".into(), "1".into());
        let passwords = [("@server", "top"), ("Secret", "hunter2")];
        let options = Options {
            passwords: passwords.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect(),
            ..Default::default()
        };
        (plan(&s, &options), "top")
    }

    #[tokio::test]
    async fn a_live_import_is_applied_whole_and_everyone_reconnects_to_it() {
        let core = test_core_teamspeak(Store::in_memory().unwrap()).unwrap();
        let (notes, mut bridge) = mpsc::unbounded_channel();
        core.bridge(BridgeMsg::Attach(notes)).await;
        let mut frames = gwar_session(&core).await;
        let lobby = core.info().await.unwrap().default_channel;
        remote(&core, "ts:ModUid=", lobby).await;

        while bridge.try_recv().is_ok() {}
        let (planned, password) = with_server_password();
        let applied = core.import_live(planned, "src".into(), BRIDGE).await.unwrap();
        assert_eq!((applied.roles_created, applied.channels_created, applied.members_created), (2, 5, 3));
        // Everyone was let go to come back; the Gwar client is told it is a restart (it reconnects).
        let mut closed = None;
        while let Ok(frame) = frames.try_recv() {
            if let Outbound::Close(reason) = frame {
                closed = Some(reason);
            }
        }
        assert_eq!(closed, Some(LeaveReason::ServerShutdown));
        // The bridge is let go before anyone leaves: it hears no departure (it would kick them on
        // TeamSpeak), and its TeamSpeak users come back through the next one.
        let notes = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let mut notes = Vec::new();
            while let Some(note) = bridge.recv().await {
                notes.push(note);
            }
            notes
        })
        .await
        .expect("the bridge was not let go");
        assert!(!notes.iter().any(|n| matches!(n, BridgeNote::Event(e) if matches!(**e, Event::ClientLeft { .. }))));
        // What the core keeps is what the database has now, the server password included.
        let (sessions, roles, channels, hash) = on_core(&core, |core| {
            (
                core.sessions.len(),
                core.groups.values().map(|g| g.name.clone()).collect::<Vec<_>>(),
                core.channels.values().map(|c| c.name.clone()).collect::<Vec<_>>(),
                core.password.load_full().as_ref().as_ref().map(|h| h.to_string()),
            )
        })
        .await;
        assert_eq!(sessions, 0);
        assert!(roles.contains(&"Moderator".to_string()), "{roles:?}");
        assert!(channels.contains(&"Secret".to_string()), "{channels:?}");
        assert!(passwords::verify_secret(password, &hash.expect("a server password"), false));
    }

    #[tokio::test]
    async fn a_failed_live_import_changes_nothing_and_lets_nobody_go() {
        let store = Store::in_memory().unwrap();
        // The bans come last: failing there tests that the roles and channels before go back too.
        store
            .raw()
            .execute_batch("CREATE TRIGGER no_bans BEFORE INSERT ON bans BEGIN SELECT RAISE(ABORT, 'test'); END;")
            .unwrap();
        let core = test_core_teamspeak(store).unwrap();
        let (notes, mut bridge) = mpsc::unbounded_channel();
        core.bridge(BridgeMsg::Attach(notes)).await;
        let mut frames = gwar_session(&core).await;
        let before = on_core(&core, |core| (core.groups.len(), core.channels.len())).await;
        let refused = core.import_live(with_server_password().0, "src".into(), BRIDGE).await;
        assert!(refused.unwrap_err().starts_with("nothing was changed"));
        let after = on_core(&core, |core| {
            let stored = (core.store.groups().unwrap().len(), core.store.channels().unwrap().len());
            (
                (core.groups.len(), core.channels.len()),
                stored,
                core.sessions.len(),
                core.store.meta("password_hash").unwrap(),
            )
        })
        .await;
        assert_eq!(after, (before, before, 1, None));
        assert!(!std::iter::from_fn(|| frames.try_recv().ok()).any(|f| matches!(f, Outbound::Close(_))));
        while bridge.try_recv().is_ok() {}
        assert!(matches!(bridge.try_recv(), Err(mpsc::error::TryRecvError::Empty)), "the bridge stays");
        // --replace-channels needs the server stopped.
        let replace = ApplyOptions { replace_channels: true, ..BRIDGE };
        assert!(
            core.import_live(plan(&source(), &Options::default()), "src".into(), replace)
                .await
                .unwrap_err()
                .contains("stopped")
        );
    }

    #[tokio::test]
    async fn without_teamspeak_a_live_import_keeps_no_typed_password() {
        let core = test_core(Store::in_memory().unwrap(), None);
        core.import_live(with_server_password().0, "src".into(), BRIDGE).await.unwrap();
        assert!(on_core(&core, |core| core.store.bridge_passwords().unwrap().is_empty()).await);
        // With TeamSpeak it waits for the bridge.
        let core = test_core_teamspeak(Store::in_memory().unwrap()).unwrap();
        core.import_live(with_server_password().0, "src".into(), BRIDGE).await.unwrap();
        assert_eq!(on_core(&core, |core| core.store.bridge_passwords().unwrap().len()).await, 1);
    }
}
