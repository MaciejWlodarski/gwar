//! A TeamSpeak import into the running server (docs/teamspeak-import.md, "Running it"): the plan is
//! applied to the live database, then everything the core keeps in memory follows and everyone is
//! told, as if each change had been made by hand.

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
    /// Applies `plan` to the live database (in steps: a failure leaves what was done, and a re-run
    /// finishes it), then reloads channels, roles and the server's settings, refreshes everyone
    /// connected and has the TeamSpeak bridge reconnect to take the new channels and passwords.
    pub(super) fn import_live(&mut self, plan: &Plan, source: &str, options: ApplyOptions) -> Result<Applied, String> {
        if options.replace_channels {
            return Err("--replace-channels needs the server stopped".into());
        }
        let before = self.admissions();
        let applied = apply::apply(&self.store, plan, source, options).map_err(|e| format!("{e:#}"));
        if let Err(e) = self.reload_after_import() {
            warn!("reloading after an import: {e:#}");
        }
        self.announce_imported_members(plan);
        if let Err(e) = self.refresh_sessions_after_import() {
            warn!("refreshing sessions after an import: {e:#}");
        }
        self.reconcile_admissions(before);
        self.enforce_connect_everywhere();
        self.publish_routing();
        // The bridge reconnects (the TeamSpeak supervisor attaches a new one) and syncs from scratch.
        if self.bridge.take().is_some() {
            info!("TeamSpeak bridge restarts to take the import");
        }
        if let Ok(applied) = &applied {
            info!(
                roles = applied.roles_created + applied.roles_updated,
                channels = applied.channels_created + applied.channels_updated,
                members = applied.members_created + applied.members_updated,
                bans = applied.bans,
                "TeamSpeak import applied to the running server"
            );
        }
        applied
    }

    /// Makes the core's copies of channels, roles and the server's settings match the database again,
    /// telling everyone what changed, and gives connected people their current roles.
    fn reload_after_import(&mut self) -> anyhow::Result<()> {
        // Roles: new, changed, gone, and their order.
        let stored: BTreeMap<GroupId, Group> = self.store.groups()?.into_iter().map(|g| (g.id, g)).collect();
        let order = |groups: &BTreeMap<GroupId, Group>| {
            let mut ids: Vec<_> =
                groups.values().filter(|g| g.id != MEMBER_GROUP).map(|g| (g.position, g.id)).collect();
            ids.sort_unstable();
            ids.into_iter().map(|(_, id)| id).collect::<Vec<_>>()
        };
        let reordered = order(&stored) != order(&self.groups);
        let gone: Vec<GroupId> = self.groups.keys().filter(|id| !stored.contains_key(id)).copied().collect();
        let old = std::mem::replace(&mut self.groups, stored);
        for id in gone {
            self.broadcast(Event::GroupDeleted { group: id }, |_| true);
        }
        let groups: Vec<Group> = self.groups.values().cloned().collect();
        for group in groups {
            match old.get(&group.id) {
                None => self.broadcast(Event::GroupCreated(group), |_| true),
                Some(before) if *before != group => self.broadcast(Event::GroupUpdated(group), |_| true),
                Some(_) => {}
            }
        }
        if reordered {
            let groups = order(&self.groups);
            self.broadcast(Event::GroupsReordered { groups }, |_| true);
        }

        // Channels: an import only adds and changes them (it removes none while the server runs).
        let stored: BTreeMap<ChannelId, ChannelRow> = self.store.channels()?.into_iter().map(|c| (c.id, c)).collect();
        for (id, row) in stored {
            let view = channel_view(&row);
            let changed = match self.channels.get(&id) {
                None => Some(true),
                Some(before) if channel_view(before) != view || before.password_hash != row.password_hash => {
                    Some(false)
                }
                Some(_) => None,
            };
            self.channels.insert(id, row);
            match changed {
                Some(true) => self.broadcast_channel(Event::ChannelCreated(view)),
                Some(false) => self.broadcast_channel(Event::ChannelUpdated(view)),
                None => {}
            }
        }

        // The server's name, welcome, size and default channel.
        let mut info = self.info.clone();
        info.name = self.store.meta("name")?.unwrap_or(info.name);
        info.welcome = self.store.meta("welcome")?.unwrap_or(info.welcome);
        info.max_clients = self.store.meta("max_clients")?.and_then(|v| v.parse().ok()).unwrap_or(info.max_clients);
        info.default_channel = self
            .store
            .meta("default_channel")?
            .and_then(|v| v.parse().ok())
            .filter(|id| self.channels.get(id).is_some_and(|c| !c.encrypted && !c.private))
            .unwrap_or(info.default_channel);
        if info != self.info {
            self.info = info;
            self.broadcast(Event::ServerUpdated(self.info.clone()), |_| true);
        }

        Ok(())
    }

    /// Tells everyone about the members an import made or changed (as many as a welcome lists).
    fn announce_imported_members(&mut self, plan: &Plan) {
        for m in plan.members.iter().take(MAX_MEMBERS as usize) {
            match self.store.user_by_uid(&m.uid) {
                Ok(Some((_, member))) => self.broadcast(Event::MemberUpdated(member), |_| true),
                Ok(None) => {}
                Err(e) => warn!("store: {e:#}"),
            }
        }
    }

    /// Gives everyone connected their roles now (a TeamSpeak user may have become a member) and
    /// applies new bans.
    fn refresh_sessions_after_import(&mut self) -> anyhow::Result<()> {
        let ids: Vec<SessionId> = self.sessions.keys().copied().collect();
        for id in ids {
            let s = &self.sessions[&id];
            let banned = self
                .ban_for(Some(&s.uid), s.ip)
                .or_else(|| (s.member != s.uid).then(|| self.ban_for(Some(&s.member), None)).flatten());
            if let Some(ban) = banned {
                self.remove(id, LeaveReason::Banned { by: ban.by, reason: ban.reason, until: ban.expires_at });
                continue;
            }
            let (uid, member, user_id) = (s.uid.clone(), s.member.clone(), s.user_id);
            let known = if user_id != 0 {
                self.store.user_by_uid(&member)?
            } else {
                // An unknown TeamSpeak user the import made a member of.
                let linked = self.store.teamspeak_member(&uid)?.unwrap_or_else(|| uid.clone());
                self.store.user_by_uid(&linked)?
            };
            let Some((user_id, known)) = known else { continue };
            let s = self.sessions.get_mut(&id).expect("listed above");
            if s.user_id == user_id && s.groups == known.groups {
                continue;
            }
            if s.user_id == 0 {
                s.nickname = known.nickname.clone();
            }
            s.user_id = user_id;
            s.member = known.uid.clone();
            s.groups = known.groups.clone();
            self.client_updated(id);
        }
        Ok(())
    }
}
