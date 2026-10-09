//! Groups and roles, bans, invites and server settings.
//!
//! One rule throughout: nobody can hand out, or act against someone holding,
//! a permission they don't have themselves. That keeps moderators from
//! promoting themselves or demoting admins.

use std::{collections::BTreeSet, net::IpAddr};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use vc_proto::{
    Ban, BanCreate, BanId, ErrorCode, Event, Group, GroupCreate, GroupId, GroupUpdate, Invite, InviteCreate,
    LeaveReason, Permission, Response, ServerUpdate, SessionId, Uid,
};

use super::{Core, Reply, bridge::BridgeNote, clean, err, now_ms, passwords, storage_error};
use crate::store::{ADMIN_GROUP, MEMBER_GROUP};

const MAX_GROUP_NAME: usize = 32;
const MAX_REASON: usize = 200;
const MAX_INVITE_USES: u32 = 10_000;
const MAX_DURATION_SECS: u64 = 10 * 365 * 24 * 3600;

pub(super) fn ban_message(ban: &Ban) -> String {
    let mut message = String::from("you are banned from this server");
    if let Some(until) = ban.expires_at {
        message.push_str(&format!(" until {until} (unix ms)"));
    }
    if let Some(reason) = &ban.reason {
        message.push_str(&format!(": {reason}"));
    }
    message
}

fn valid_color(color: &str) -> bool {
    color.len() == 7 && color.starts_with('#') && color[1..].chars().all(|c| c.is_ascii_hexdigit())
}

fn invite_code() -> String {
    let mut bytes = [0u8; 9];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

impl Core {
    fn my_permissions(&self, session: SessionId) -> BTreeSet<Permission> {
        self.permissions(&self.sessions[&session])
    }

    fn group_permissions(&self, groups: &[GroupId]) -> BTreeSet<Permission> {
        groups.iter().filter_map(|g| self.groups.get(g)).flat_map(|g| g.permissions.iter().copied()).collect()
    }

    /// Whether everything `permissions` allows is also allowed to `session`.
    fn covers(&self, session: SessionId, permissions: &BTreeSet<Permission>) -> bool {
        permissions.is_subset(&self.my_permissions(session))
    }

    // ---------------------------------------------------------------- groups

    pub(super) fn group_create(&mut self, session: SessionId, c: GroupCreate) -> Reply {
        self.require(session, Permission::GroupManage)?;
        let name = clean(&c.name, MAX_GROUP_NAME, "group name")?;
        let permissions: BTreeSet<_> = c.permissions.into_iter().collect();
        if !self.covers(session, &permissions) {
            return Err(err(ErrorCode::Forbidden, "cannot grant permissions you do not have"));
        }
        let color = c.color.filter(|c| !c.is_empty());
        if color.as_deref().is_some_and(|c| !valid_color(c)) {
            return Err(err(ErrorCode::BadRequest, "color must look like #a1b2c3"));
        }
        let permissions: Vec<_> = permissions.into_iter().collect();
        let id = self.store.insert_group(&name, &permissions, color.as_deref()).map_err(storage_error)?;
        let group = Group { id, name, permissions, color };
        self.groups.insert(id, group.clone());
        self.broadcast(Event::GroupCreated(group.clone()), |_| true);
        Ok(Response::Group { group })
    }

    pub(super) fn group_update(&mut self, session: SessionId, u: GroupUpdate) -> Reply {
        self.require(session, Permission::GroupManage)?;
        let Some(mut group) = self.groups.get(&u.group).cloned() else {
            return Err(err(ErrorCode::NotFound, "no such group"));
        };
        if !self.covers(session, &group.permissions.iter().copied().collect()) {
            return Err(err(ErrorCode::Forbidden, "this group has permissions you do not have"));
        }
        if let Some(name) = &u.name {
            group.name = clean(name, MAX_GROUP_NAME, "group name")?;
        }
        if let Some(permissions) = u.permissions {
            if group.id == ADMIN_GROUP {
                return Err(err(ErrorCode::BadRequest, "the Admin group always has every permission"));
            }
            let permissions: BTreeSet<_> = permissions.into_iter().collect();
            if !self.covers(session, &permissions) {
                return Err(err(ErrorCode::Forbidden, "cannot grant permissions you do not have"));
            }
            group.permissions = permissions.into_iter().collect();
        }
        match u.color.as_deref() {
            Some("") => group.color = None,
            Some(c) if !valid_color(c) => return Err(err(ErrorCode::BadRequest, "color must look like #a1b2c3")),
            Some(c) => group.color = Some(c.to_owned()),
            None => {}
        }
        self.store.update_group(&group).map_err(storage_error)?;
        self.groups.insert(group.id, group.clone());
        self.broadcast(Event::GroupUpdated(group.clone()), |_| true);
        Ok(Response::Group { group })
    }

    pub(super) fn group_delete(&mut self, session: SessionId, id: GroupId) -> Reply {
        self.require(session, Permission::GroupManage)?;
        let Some(group) = self.groups.get(&id) else {
            return Err(err(ErrorCode::NotFound, "no such group"));
        };
        if id == ADMIN_GROUP || id == MEMBER_GROUP {
            return Err(err(ErrorCode::BadRequest, "the built-in groups cannot be deleted"));
        }
        if !self.covers(session, &group.permissions.iter().copied().collect()) {
            return Err(err(ErrorCode::Forbidden, "this group has permissions you do not have"));
        }
        self.store.delete_group(id).map_err(storage_error)?;
        self.groups.remove(&id);
        let affected: Vec<_> = self.sessions.values().filter(|s| s.groups.contains(&id)).map(|s| s.id).collect();
        for s in affected {
            if let Some(session) = self.sessions.get_mut(&s) {
                session.groups.retain(|g| *g != id);
            }
            self.client_updated(s);
        }
        self.broadcast(Event::GroupDeleted { group: id }, |_| true);
        Ok(Response::Empty {})
    }

    pub(super) fn member_groups(&mut self, session: SessionId, uid: Uid, groups: Vec<GroupId>) -> Reply {
        self.require(session, Permission::GroupManage)?;
        let Some((user_id, mut member)) = self.store.user_by_uid(&uid).map_err(storage_error)? else {
            return Err(err(ErrorCode::NotFound, "no such member"));
        };
        let mut wanted: Vec<GroupId> = groups.into_iter().collect::<BTreeSet<_>>().into_iter().collect();
        if wanted.iter().any(|g| !self.groups.contains_key(g)) {
            return Err(err(ErrorCode::NotFound, "no such group"));
        }
        if wanted.is_empty() {
            wanted.push(MEMBER_GROUP);
        }
        let me = &self.sessions[&session];
        if me.uid != uid && !self.covers(session, &self.group_permissions(&member.groups)) {
            return Err(err(ErrorCode::Forbidden, "this member has permissions you do not have"));
        }
        // Every group added or removed must be within your own permissions.
        let changed: Vec<GroupId> = wanted
            .iter()
            .filter(|g| !member.groups.contains(g))
            .chain(member.groups.iter().filter(|g| !wanted.contains(g)))
            .copied()
            .collect();
        if !self.covers(session, &self.group_permissions(&changed)) {
            return Err(err(ErrorCode::Forbidden, "cannot grant or remove permissions you do not have"));
        }
        if me.uid == uid && member.groups.contains(&ADMIN_GROUP) && !wanted.contains(&ADMIN_GROUP) {
            return Err(err(ErrorCode::BadRequest, "you cannot remove your own Admin group"));
        }
        self.store.set_user_groups(user_id, &wanted).map_err(storage_error)?;
        member.groups = wanted.clone();
        let online: Vec<_> = self.sessions.values().filter(|s| s.user_id == user_id).map(|s| s.id).collect();
        for s in online {
            if let Some(session) = self.sessions.get_mut(&s) {
                session.groups = wanted.clone();
            }
            self.client_updated(s);
        }
        self.broadcast(Event::MemberUpdated(member), |_| true);
        Ok(Response::Groups { groups: wanted })
    }

    /// Checks an invite at connect: `Some(group)` if valid. It is only used
    /// up when it does something: admits a newcomer or grants a new group.
    pub(super) fn admit_by_invite(
        &self,
        code: &str,
        known: Option<&vc_proto::Member>,
    ) -> anyhow::Result<Option<Option<GroupId>>> {
        let now = now_ms();
        let Some(group) = self.store.peek_invite(code, now)? else { return Ok(None) };
        let useful = match known {
            None => true,
            Some(member) => group.is_some_and(|g| !member.groups.contains(&g)),
        };
        if !useful {
            return Ok(Some(None));
        }
        self.store.use_invite(code, now)
    }

    // ------------------------------------------------------------------ bans

    /// The ban in force for this identity or address, if any.
    pub(super) fn ban_for(&self, uid: Option<&str>, ip: Option<IpAddr>) -> Option<Ban> {
        let ip = ip.map(|ip| ip.to_string());
        self.store
            .bans(now_ms())
            .ok()?
            .into_iter()
            .find(|b| (uid.is_some() && b.uid.as_deref() == uid) || (ip.is_some() && b.ip.is_some() && b.ip == ip))
    }

    pub(super) fn ban_create(&mut self, session: SessionId, b: BanCreate) -> Reply {
        self.require(session, Permission::ClientBan)?;
        let reason = b.reason.map(|r| r.trim().chars().take(MAX_REASON).collect::<String>()).filter(|r| !r.is_empty());
        if b.duration.is_some_and(|d| d == 0 || d > MAX_DURATION_SECS) {
            return Err(err(ErrorCode::BadRequest, "duration must be 1 second to 10 years"));
        }
        // Who: an online client, or a known member by uid.
        let (uid, nickname, ip, groups) = match (b.client, b.uid) {
            (Some(client), _) => {
                let Some(s) = self.sessions.get(&client) else {
                    return Err(err(ErrorCode::NotFound, "no such client"));
                };
                (s.uid.clone(), s.nickname.clone(), s.ip.filter(|_| b.ip), s.groups.clone())
            }
            (None, Some(uid)) => {
                let Some((_, member)) = self.store.user_by_uid(&uid).map_err(storage_error)? else {
                    return Err(err(ErrorCode::NotFound, "no such member"));
                };
                (uid, member.nickname, None, member.groups)
            }
            (None, None) => return Err(err(ErrorCode::BadRequest, "say whom to ban")),
        };
        let me = &self.sessions[&session];
        if me.uid == uid {
            return Err(err(ErrorCode::BadRequest, "you cannot ban yourself"));
        }
        if !self.covers(session, &self.group_permissions(&groups)) {
            return Err(err(ErrorCode::Forbidden, "this member has permissions you do not have"));
        }
        let now = now_ms();
        let mut ban = Ban {
            id: 0,
            uid: Some(uid.clone()),
            ip: ip.map(|ip| ip.to_string()),
            nickname,
            reason,
            by: me.nickname.clone(),
            created_at: now,
            expires_at: b.duration.map(|d| now + (d as i64) * 1000),
        };
        ban.id = self.store.insert_ban(&ban).map_err(storage_error)?;
        // Everyone the ban covers leaves now.
        let banned: Vec<_> = self
            .sessions
            .values()
            .filter(|s| s.uid == uid || (ban.ip.is_some() && s.ip.map(|ip| ip.to_string()) == ban.ip))
            .map(|s| s.id)
            .collect();
        let reason = LeaveReason::Banned { by: ban.by.clone(), reason: ban.reason.clone(), until: ban.expires_at };
        for s in banned {
            self.remove(s, reason.clone());
        }
        Ok(Response::Ban { ban })
    }

    pub(super) fn ban_list(&mut self, session: SessionId) -> Reply {
        self.require(session, Permission::ClientBan)?;
        Ok(Response::Bans { bans: self.store.bans(now_ms()).map_err(storage_error)? })
    }

    pub(super) fn ban_delete(&mut self, session: SessionId, id: BanId) -> Reply {
        self.require(session, Permission::ClientBan)?;
        let Some(ban) = self.store.delete_ban(id, now_ms()).map_err(storage_error)? else {
            return Err(err(ErrorCode::NotFound, "no such ban"));
        };
        // A TeamSpeak user's ban also lives on the TeamSpeak server.
        if let Some(uid) = ban.uid.filter(|uid| uid.starts_with("ts:")) {
            self.notify_bridge(|| BridgeNote::Unban { uid });
        }
        Ok(Response::Empty {})
    }

    // --------------------------------------------------------------- invites

    pub(super) fn invite_create(&mut self, session: SessionId, i: InviteCreate) -> Reply {
        self.require(session, Permission::InviteCreate)?;
        if i.max_uses.is_some_and(|n| n == 0 || n > MAX_INVITE_USES) {
            return Err(err(ErrorCode::BadRequest, "max uses must be 1–10000"));
        }
        if i.expires_in.is_some_and(|d| d == 0 || d > MAX_DURATION_SECS) {
            return Err(err(ErrorCode::BadRequest, "expiry must be 1 second to 10 years"));
        }
        if let Some(group) = i.group {
            let Some(g) = self.groups.get(&group) else {
                return Err(err(ErrorCode::NotFound, "no such group"));
            };
            if !self.covers(session, &g.permissions.iter().copied().collect()) {
                return Err(err(ErrorCode::Forbidden, "cannot grant permissions you do not have"));
            }
        }
        let now = now_ms();
        let invite = Invite {
            code: invite_code(),
            uses: 0,
            max_uses: i.max_uses,
            expires_at: i.expires_in.map(|d| now + (d as i64) * 1000),
            group: i.group.filter(|g| *g != MEMBER_GROUP),
            created_by: self.sessions[&session].nickname.clone(),
            created_at: now,
        };
        self.store.insert_invite(&invite).map_err(storage_error)?;
        Ok(Response::Invite { invite })
    }

    /// Everyone with `invite_create` sees the invites they made; managers see all.
    pub(super) fn invite_list(&mut self, session: SessionId) -> Reply {
        self.require(session, Permission::InviteCreate)?;
        let all = self.has(session, Permission::ServerManage);
        let me = self.sessions[&session].nickname.clone();
        let invites =
            self.store.invites().map_err(storage_error)?.into_iter().filter(|i| all || i.created_by == me).collect();
        Ok(Response::Invites { invites })
    }

    pub(super) fn invite_delete(&mut self, session: SessionId, code: &str) -> Reply {
        self.require(session, Permission::InviteCreate)?;
        let mine = self
            .store
            .invites()
            .map_err(storage_error)?
            .into_iter()
            .any(|i| i.code == code && i.created_by == self.sessions[&session].nickname);
        if !mine && !self.has(session, Permission::ServerManage) {
            return Err(err(ErrorCode::Forbidden, "you can only delete your own invites"));
        }
        if !self.store.delete_invite(code).map_err(storage_error)? {
            return Err(err(ErrorCode::NotFound, "no such invite"));
        }
        Ok(Response::Empty {})
    }

    // -------------------------------------------------------------- settings

    pub(super) fn server_update(&mut self, session: SessionId, id: u32, u: ServerUpdate) -> Option<Reply> {
        let checked = (|| {
            self.require(session, Permission::ServerManage)?;
            let name = u.name.as_deref().map(|n| clean(n, 64, "server name")).transpose()?;
            if u.welcome.as_ref().is_some_and(|w| w.chars().count() > 1000) {
                return Err(err(ErrorCode::BadRequest, "welcome message too long"));
            }
            if u.default_channel.is_some_and(|c| !self.channels.contains_key(&c)) {
                return Err(err(ErrorCode::NotFound, "no such channel"));
            }
            if u.max_clients.is_some_and(|m| m == 0 || m > 100_000) {
                return Err(err(ErrorCode::BadRequest, "max clients must be 1–100000"));
            }
            if u.password.as_ref().is_some_and(|p| p.len() > 128) {
                return Err(err(ErrorCode::BadRequest, "password too long"));
            }
            Ok(name)
        })();
        let name = match checked {
            Ok(name) => name,
            Err(e) => return Some(Err(e)),
        };
        let apply = move |core: &mut Core, password_hash: Option<Option<String>>| -> Reply {
            if let Some(name) = name {
                core.store.set_meta("name", &name).map_err(storage_error)?;
                core.info.name = name;
            }
            if let Some(welcome) = u.welcome {
                let welcome = welcome.trim().to_owned();
                core.store.set_meta("welcome", &welcome).map_err(storage_error)?;
                core.info.welcome = welcome;
            }
            if let Some(channel) = u.default_channel.filter(|c| core.channels.contains_key(c)) {
                core.store.set_meta("default_channel", &channel.to_string()).map_err(storage_error)?;
                core.info.default_channel = channel;
            }
            if let Some(max) = u.max_clients {
                core.store.set_meta("max_clients", &max.to_string()).map_err(storage_error)?;
                core.info.max_clients = max;
            }
            if let Some(hash) = password_hash {
                // Stored even when empty: an explicit "no password" outlives the command line.
                core.store.set_meta("password_hash", hash.as_deref().unwrap_or("")).map_err(storage_error)?;
                core.password.store(std::sync::Arc::new(hash.map(Into::into)));
            }
            core.broadcast(Event::ServerUpdated(core.info.clone()), |_| true);
            Ok(Response::Empty {})
        };
        match u.password {
            None => Some(apply(self, None)),
            // Hashing is slow: do it off the actor, then apply everything together.
            Some(password) => {
                self.defer(
                    move || (!password.is_empty()).then(|| passwords::hash_secret(&password, false)),
                    move |core, hash| {
                        if !core.sessions.contains_key(&session) {
                            return;
                        }
                        let result = apply(core, Some(hash));
                        core.reply(session, id, result);
                    },
                );
                None
            }
        }
    }

    /// Stores revocations from Gwar Connect and signs those devices out.
    pub(super) fn revoked(&mut self, devices: Vec<super::RevokedDevice>, seq: i64) {
        for d in &devices {
            if let Err(e) = self.store.revoke_device(&d.device_key, &d.account_key, d.revoked_at) {
                tracing::warn!("store: {e:#}");
            }
        }
        if let Err(e) = self.store.set_meta("connect_seq", &seq.to_string()) {
            tracing::warn!("store: {e:#}");
        }
        let gone: Vec<_> = self
            .sessions
            .values()
            .filter(|s| s.device.as_ref().is_some_and(|d| devices.iter().any(|r| &r.device_key == d)))
            .map(|s| s.id)
            .collect();
        let reason =
            LeaveReason::Kicked { by: "Gwar Connect".into(), reason: Some("this device was signed out".into()) };
        for session in gone {
            self.remove(session, reason.clone());
        }
    }
}
