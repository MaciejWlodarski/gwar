//! Groups and roles, bans, invites and server settings.
//!
//! Rank decides who acts on whom (docs/roles.md): roles are ordered, a member's
//! rank is their best role's, and the owner is above every role. One acts only
//! on members ranked below oneself, manages and gives only roles below one's
//! rank, and never hands out a permission one does not hold.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::IpAddr,
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use gwar_proto::{
    Ban, BanCreate, BanId, ChannelId, ErrorCode, Event, FileAccess, Group, GroupCreate, GroupId, GroupUpdate, Invite,
    InviteCreate, LeaveReason, Member, MemberPrune, PRUNE_BATCH, PRUNE_PREVIEW, Permission, Response, ServerUpdate,
    SessionId, Uid,
};
use rand::RngCore;

use super::{Core, ErrorBody, Reply, bridge::BridgeNote, clean, err, now_ms, passwords, storage_error};
use crate::{
    preauth::subscriber,
    store::{ADMIN_GROUP, MEMBER_GROUP},
};

const MAX_GROUP_NAME: usize = 32;
const MAX_REASON: usize = 200;
const MAX_INVITE_USES: u32 = 10_000;
const MAX_DURATION_SECS: u64 = 10 * 365 * 24 * 3600;
const MAX_INACTIVE_DAYS: u32 = 3650;
const DAY_MS: i64 = 24 * 3600 * 1000;

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

/// Whether `ban` covers this identity or address. An IP ban covers the banned
/// address's [`subscriber`] range: one IPv6 host usually owns a whole /64.
fn bans(ban: &Ban, uid: Option<&str>, ip: Option<IpAddr>) -> bool {
    (uid.is_some() && ban.uid.as_deref() == uid)
        || ip.is_some_and(|ip| ban.ip.as_deref().and_then(|b| b.parse().ok()).map(subscriber) == Some(subscriber(ip)))
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

    /// What these roles allow, Member's included: Member is everyone's role, even someone's
    /// without a member record yet (a TeamSpeak user seen for the first time).
    pub(super) fn group_permissions(&self, groups: &[GroupId]) -> BTreeSet<Permission> {
        groups
            .iter()
            .chain(std::iter::once(&MEMBER_GROUP))
            .filter_map(|g| self.groups.get(g))
            .flat_map(|g| g.permissions.iter().copied())
            .collect()
    }

    /// Whether the member `uid`, online or not, could now make an invite or token
    /// (`kind`) for `group`. Grants made earlier lapse with their maker's rights.
    pub(super) fn could_grant(&self, uid: &str, kind: Permission, group: Option<GroupId>) -> anyhow::Result<bool> {
        let Some(groups) = self.store.groups_of(uid)? else { return Ok(false) };
        let theirs = self.expand(self.group_permissions(&groups), uid);
        let rank = self.rank_of(uid, &groups);
        Ok(theirs.contains(&kind) && group.is_none_or(|g| rank < self.position(g)))
    }

    /// Whether everything `permissions` allows is also allowed to `session`.
    pub(super) fn covers(&self, session: SessionId, permissions: &BTreeSet<Permission>) -> bool {
        permissions.is_subset(&self.my_permissions(session))
    }

    // ------------------------------------------------------------------ rank

    /// A role's place from the top (0); an unknown role ranks below every role.
    pub(super) fn position(&self, group: GroupId) -> i64 {
        self.groups.get(&group).map_or(i64::MAX, |g| i64::from(g.position))
    }

    /// The rank of a member holding `groups`: their best role's position; the owner's is above all (-1).
    pub(super) fn rank_of(&self, uid: &str, groups: &[GroupId]) -> i64 {
        if self.owner.as_ref().is_some_and(|o| o.chain.owner() == uid) {
            return -1;
        }
        groups.iter().map(|g| self.position(*g)).min().unwrap_or_else(|| self.position(MEMBER_GROUP))
    }

    pub(super) fn session_rank(&self, session: SessionId) -> i64 {
        let s = &self.sessions[&session];
        self.rank_of(&s.member, &s.groups)
    }

    /// Whether `session` ranks above a member with these roles.
    pub(super) fn outranks(&self, session: SessionId, uid: &str, groups: &[GroupId]) -> bool {
        self.session_rank(session) < self.rank_of(uid, groups)
    }

    /// Whether `group` is below `session`'s rank, so they may manage and give it.
    pub(super) fn below(&self, session: SessionId, group: GroupId) -> bool {
        self.session_rank(session) < self.position(group)
    }

    pub(super) fn group_reorder(&mut self, session: SessionId, order: Vec<GroupId>) -> Reply {
        self.require(session, Permission::ManageRoles)?;
        let mut current: Vec<&Group> = self.groups.values().filter(|g| g.id != MEMBER_GROUP).collect();
        current.sort_by_key(|g| g.position);
        let current: Vec<GroupId> = current.into_iter().map(|g| g.id).collect();
        let mut wanted = order.clone();
        wanted.sort_unstable();
        wanted.dedup();
        let mut known = current.clone();
        known.sort_unstable();
        if wanted != known || order.len() != current.len() {
            return Err(err(ErrorCode::BadRequest, "list every role but Member exactly once"));
        }
        // Roles at or above one's rank keep their places.
        let rank = self.session_rank(session);
        let fixed = current.iter().take_while(|g| self.position(**g) <= rank).count();
        if order[..fixed] != current[..fixed] {
            return Err(err(ErrorCode::Forbidden, "roles at or above your rank keep their places"));
        }
        if order == current {
            return Ok(Response::Empty {});
        }
        self.store.reorder_groups(&order).map_err(storage_error)?;
        for (index, id) in order.iter().enumerate() {
            if let Some(group) = self.groups.get_mut(id) {
                group.position = index as u32;
            }
        }
        if let Some(member) = self.groups.get_mut(&MEMBER_GROUP) {
            member.position = order.len() as u32;
        }
        self.broadcast_order();
        Ok(Response::Empty {})
    }

    /// Tells everyone the roles' order (positions follow it; Member is last).
    pub(super) fn broadcast_order(&mut self) {
        let mut order: Vec<&Group> = self.groups.values().filter(|g| g.id != MEMBER_GROUP).collect();
        order.sort_by_key(|g| g.position);
        let groups = order.into_iter().map(|g| g.id).collect();
        self.broadcast(Event::GroupsReordered { groups }, |_| true);
    }

    // ---------------------------------------------------------------- groups

    pub(super) fn group_create(&mut self, session: SessionId, c: GroupCreate) -> Reply {
        self.require(session, Permission::ManageRoles)?;
        // A new role goes right above Member, so it must still be below the creator.
        if self.session_rank(session) >= self.position(MEMBER_GROUP) {
            return Err(err(ErrorCode::Forbidden, "only a role above Member can create roles"));
        }
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
        let id = self
            .store
            .insert_group(&name, &permissions, color.as_deref(), c.hoist, c.mentionable)
            .map_err(storage_error)?;
        // A new role goes last above Member.
        let position = self.groups.values().filter(|g| g.id != MEMBER_GROUP).count() as u32;
        let group = Group { id, name, permissions, color, position, hoist: c.hoist, mentionable: c.mentionable };
        self.groups.insert(id, group.clone());
        if let Some(member) = self.groups.get_mut(&MEMBER_GROUP) {
            member.position = position + 1;
        }
        self.broadcast(Event::GroupCreated(group.clone()), |_| true);
        self.broadcast_order();
        Ok(Response::Group { group })
    }

    pub(super) fn group_update(&mut self, session: SessionId, u: GroupUpdate) -> Reply {
        self.require(session, Permission::ManageRoles)?;
        let Some(mut group) = self.groups.get(&u.group).cloned() else {
            return Err(err(ErrorCode::NotFound, "no such group"));
        };
        if !self.below(session, group.id) {
            return Err(err(ErrorCode::Forbidden, "this role is not below your rank"));
        }
        if let Some(name) = &u.name {
            group.name = clean(name, MAX_GROUP_NAME, "group name")?;
        }
        if let Some(permissions) = u.permissions {
            if group.id == ADMIN_GROUP {
                return Err(err(ErrorCode::BadRequest, "the Admin group always has every permission"));
            }
            let permissions: BTreeSet<_> = permissions.into_iter().collect();
            let before: BTreeSet<_> = group.permissions.iter().copied().collect();
            // Adding or taking away a permission needs it.
            if !self.covers(session, &permissions.symmetric_difference(&before).copied().collect()) {
                return Err(err(ErrorCode::Forbidden, "cannot change permissions you do not have"));
            }
            group.permissions = permissions.into_iter().collect();
        }
        match u.color.as_deref() {
            Some("") => group.color = None,
            Some(c) if !valid_color(c) => return Err(err(ErrorCode::BadRequest, "color must look like #a1b2c3")),
            Some(c) => group.color = Some(c.to_owned()),
            None => {}
        }
        if let Some(hoist) = u.hoist {
            group.hoist = hoist;
        }
        if let Some(mentionable) = u.mentionable {
            group.mentionable = mentionable;
        }
        // `administrator` admits to private channels: what changes it changes admissions.
        let before = self.admissions();
        self.store.update_group(&group).map_err(storage_error)?;
        self.groups.insert(group.id, group.clone());
        self.broadcast(Event::GroupUpdated(group.clone()), |_| true);
        self.reconcile_admissions(before);
        self.enforce_connect_everywhere();
        Ok(Response::Group { group })
    }

    pub(super) fn group_delete(&mut self, session: SessionId, id: GroupId) -> Reply {
        self.require(session, Permission::ManageRoles)?;
        let Some(group) = self.groups.get(&id) else {
            return Err(err(ErrorCode::NotFound, "no such group"));
        };
        if id == ADMIN_GROUP || id == MEMBER_GROUP {
            return Err(err(ErrorCode::BadRequest, "the built-in groups cannot be deleted"));
        }
        let _ = group;
        if !self.below(session, id) {
            return Err(err(ErrorCode::Forbidden, "this role is not below your rank"));
        }
        let before = self.admissions();
        self.store.delete_group(id).map_err(storage_error)?;
        self.groups.remove(&id);
        self.forget_role_overrides(id);
        self.renumber_groups();
        self.broadcast_order();
        let affected: Vec<_> = self.sessions.values().filter(|s| s.groups.contains(&id)).map(|s| s.id).collect();
        for s in affected {
            if let Some(session) = self.sessions.get_mut(&s) {
                session.groups.retain(|g| *g != id);
            }
            self.client_updated(s);
        }
        // Private channels stop listing the deleted role.
        let listing: Vec<_> = self
            .channels
            .values()
            .filter(|c| c.access.as_ref().is_some_and(|a| a.roles.contains(&id)))
            .map(|c| c.id)
            .collect();
        for channel in listing {
            let mut row = self.channels[&channel].clone();
            if let Some(access) = row.access.as_mut() {
                access.roles.retain(|r| *r != id);
            }
            self.store.update_channel(&row).map_err(storage_error)?;
            let view = super::channel_view(&row);
            self.channels.insert(channel, row);
            self.broadcast_channel(Event::ChannelUpdated(view));
        }
        self.broadcast(Event::GroupDeleted { group: id }, |_| true);
        self.reconcile_admissions(before);
        self.enforce_connect_everywhere();
        Ok(Response::Empty {})
    }

    pub(super) fn member_groups(&mut self, session: SessionId, uid: Uid, groups: Vec<GroupId>) -> Reply {
        self.require(session, Permission::ManageRoles)?;
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
        if me.uid != uid && !self.outranks(session, &uid, &member.groups) {
            return Err(err(ErrorCode::Forbidden, "this member is not below your rank"));
        }
        // Every role added or taken away must be below one's rank (Member is everyone's).
        let changed: Vec<GroupId> = wanted
            .iter()
            .filter(|g| !member.groups.contains(g))
            .chain(member.groups.iter().filter(|g| !wanted.contains(g)))
            .copied()
            .filter(|g| *g != MEMBER_GROUP)
            .collect();
        if me.uid == uid && member.groups.contains(&ADMIN_GROUP) && !wanted.contains(&ADMIN_GROUP) {
            return Err(err(ErrorCode::BadRequest, "you cannot remove your own Admin group"));
        }
        if let Some(role) = changed.iter().find(|g| !self.below(session, **g)) {
            let name = self.groups.get(role).map_or("this role", |g| g.name.as_str());
            return Err(err(ErrorCode::Forbidden, &format!("{name} is not below your rank")));
        }
        self.store.set_user_groups(user_id, &wanted).map_err(storage_error)?;
        member.groups = wanted.clone();
        let before = self.admissions();
        let online: Vec<_> = self.sessions.values().filter(|s| s.user_id == user_id).map(|s| s.id).collect();
        for s in online {
            if let Some(session) = self.sessions.get_mut(&s) {
                session.groups = wanted.clone();
            }
            self.client_updated(s);
        }
        self.broadcast(Event::MemberUpdated(member), |_| true);
        self.reconcile_admissions(before);
        self.enforce_connect_everywhere();
        Ok(Response::Groups { groups: wanted })
    }

    // --------------------------------------------------------------- removal

    pub(super) fn member_remove(&mut self, session: SessionId, uid: Uid, delete_messages: bool) -> Reply {
        self.require(session, Permission::ManageMembers)?;
        if self.sessions[&session].uid == uid {
            return Err(err(ErrorCode::BadRequest, "you cannot remove yourself"));
        }
        let Some((user_id, member)) = self.store.user_by_uid(&uid).map_err(storage_error)? else {
            return Err(err(ErrorCode::NotFound, "no such member"));
        };
        if !self.outranks(session, &uid, &member.groups) {
            return Err(err(ErrorCode::Forbidden, "this member is not below your rank"));
        }
        let by = self.sessions[&session].nickname.clone();
        self.erase_member(&by, user_id, &member, delete_messages)?;
        Ok(Response::Empty {})
    }

    /// Removes members not seen for `inactive_days`, a batch per call. Online members, the
    /// caller and, with `without_groups_only`, anyone holding a role are always spared, as is
    /// anyone not ranked below the caller. Core work is bounded by [`PRUNE_BATCH`]:
    /// the caller repeats the request while `count` exceeds the uids returned.
    pub(super) fn member_prune(&mut self, session: SessionId, p: MemberPrune) -> Reply {
        self.require(session, Permission::ManageMembers)?;
        if p.inactive_days == 0 || p.inactive_days > MAX_INACTIVE_DAYS {
            return Err(err(ErrorCode::BadRequest, "inactive days must be 1–3650"));
        }
        let cutoff = now_ms() - i64::from(p.inactive_days) * DAY_MS;
        let me = &self.sessions[&session];
        let (my_uid, by) = (me.uid.clone(), me.nickname.clone());
        let online: BTreeSet<&str> = self.sessions.values().map(|s| s.member.as_str()).collect();
        let mut doomed: Vec<_> = self
            .store
            .member_rows(Some(cutoff))
            .map_err(storage_error)?
            .into_iter()
            .filter(|m| m.member.uid != my_uid && !online.contains(m.member.uid.as_str()))
            .filter(|m| !(p.without_groups_only && m.has_roles()))
            .filter(|m| self.outranks(session, &m.member.uid, &m.member.groups))
            .collect();
        let count = doomed.len() as u32;
        if p.dry_run {
            doomed.truncate(PRUNE_PREVIEW);
            let members: Vec<_> = doomed.into_iter().map(|m| m.member).collect();
            return Ok(Response::Pruned { uids: members.iter().map(|m| m.uid.clone()).collect(), count, members });
        }
        doomed.truncate(PRUNE_BATCH);
        let mut members = Vec::with_capacity(doomed.len());
        for row in doomed {
            self.erase_member(&by, row.id, &row.member, p.delete_messages)?;
            members.push(row.member);
        }
        Ok(Response::Pruned { uids: members.iter().map(|m| m.uid.clone()).collect(), count, members })
    }

    /// Disconnects the member's sessions, deletes their record (and their messages
    /// if asked) and tells everyone. The caller has checked that this is allowed.
    fn erase_member(
        &mut self,
        by: &str,
        user_id: i64,
        member: &Member,
        delete_messages: bool,
    ) -> Result<(), ErrorBody> {
        // Linked TeamSpeak identities act as this member, so they leave too (their links go with the record).
        let online: Vec<_> = self.sessions.values().filter(|s| s.member == member.uid).map(|s| s.id).collect();
        for s in online {
            self.remove(s, LeaveReason::Removed { by: by.to_owned() });
        }
        let tags = self.store.member_tags().map_err(storage_error)?;
        self.connect_pending.remove(&member.uid);
        let removal = self.store.remove_member(user_id, &member.uid, delete_messages).map_err(storage_error)?;
        // Their overrides went with them (in the same transaction): one who comes back starts afresh.
        for &channel in &removal.overrides {
            if let Some(row) = self.channels.get_mut(&channel) {
                row.overrides.retain(|o| o.user.as_deref() != Some(member.uid.as_str()));
                let view = super::channel_view(row);
                self.broadcast_channel(Event::ChannelUpdated(view));
            }
        }
        for (conversation, ids) in removal.sealed {
            let readers = self.mls_readers(&conversation);
            self.broadcast(Event::ChatSealedRemoved { conversation, ids }, |s| readers.contains(&s.id));
        }
        self.mls_reconcile().map_err(storage_error)?;
        if !removal.files.is_empty() {
            self.remove_files();
        }
        let mut readers: BTreeMap<ChannelId, BTreeSet<SessionId>> = BTreeMap::new();
        for (message, channel) in removal.messages {
            let readers = readers.entry(channel).or_insert_with(|| self.readers(channel));
            self.broadcast(Event::ChatDeleted { channel, message }, |s| readers.contains(&s.id));
        }
        self.broadcast(Event::MemberRemoved { uid: member.uid.clone() }, |_| true);
        self.announce_tags(tags);
        Ok(())
    }

    /// Checks an invite at connect: `Some(group)` if valid. It is only used
    /// up when it does something: admits a newcomer or grants a new group.
    pub(super) fn admit_by_invite(
        &self,
        code: &str,
        known: Option<&gwar_proto::Member>,
    ) -> anyhow::Result<Option<Option<GroupId>>> {
        let now = now_ms();
        let Some((group, creator)) = self.store.peek_invite(code, now)? else { return Ok(None) };
        if !self.could_grant(&creator, Permission::CreateInvites, group)? {
            return Ok(None);
        }
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
        self.store.bans(now_ms()).ok()?.into_iter().find(|b| {
            let named = uid.is_some() && b.uid.as_deref() == uid;
            named || (bans(b, None, ip) && self.ip_ban_reaches(b, uid))
        })
    }

    /// An IP ban keeps out others behind the banned address only if they rank below its issuer
    /// (newcomers always do). A ban without a recorded issuer spares only the owner.
    fn ip_ban_reaches(&self, ban: &Ban, uid: Option<&str>) -> bool {
        let Some(uid) = uid else { return true };
        let Ok(Some(groups)) = self.store.groups_of(uid) else { return true };
        let target = self.rank_of(uid, &groups);
        match self.store.ban_issuer(ban.id).ok().flatten() {
            Some(issuer) => {
                let theirs = self.store.groups_of(&issuer).ok().flatten().unwrap_or_default();
                self.rank_of(&issuer, &theirs) < target
            }
            None => target >= 0,
        }
    }

    pub(super) fn ban_create(&mut self, session: SessionId, b: BanCreate) -> Reply {
        self.require(session, Permission::BanMembers)?;
        let reason = b.reason.map(|r| r.trim().chars().take(MAX_REASON).collect::<String>()).filter(|r| !r.is_empty());
        if b.duration.is_some_and(|d| d == 0 || d > MAX_DURATION_SECS) {
            return Err(err(ErrorCode::BadRequest, "duration must be 1 second to 10 years"));
        }
        // Who: an online client, or a known member by uid.
        let (uid, nickname, ip, groups) = match (b.client, b.uid) {
            (Some(client), uid) => {
                // A reused session id is someone else.
                let Some(s) = self.sessions.get(&client).filter(|s| uid.as_ref().is_none_or(|u| *u == s.uid)) else {
                    return Err(err(ErrorCode::NotFound, "no such client"));
                };
                // A linked TeamSpeak user is the member they act as.
                (s.member.clone(), s.nickname.clone(), s.ip.filter(|_| b.ip), s.groups.clone())
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
        if !self.outranks(session, &uid, &groups) {
            return Err(err(ErrorCode::Forbidden, "this member is not below your rank"));
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
        ban.id = self.store.insert_ban(&ban, Some(&me.uid)).map_err(storage_error)?;
        // Everyone the ban covers leaves now: behind the address, only those ranked below the issuer.
        let range = ip.map(subscriber);
        let banned: Vec<_> = self
            .sessions
            .values()
            .filter(|s| {
                s.member == uid
                    || (range.is_some()
                        && s.ip.map(subscriber) == range
                        && self.outranks(session, &s.member, &s.groups))
            })
            .map(|s| s.id)
            .collect();
        let reason = LeaveReason::Banned { by: ban.by.clone(), reason: ban.reason.clone(), until: ban.expires_at };
        for s in banned {
            // The bridge bans a TeamSpeak session it removes; a linked identity's ban there is lifted with this one.
            if let Some(teamspeak) =
                self.sessions.get(&s).filter(|s| s.out.is_none() && s.uid != uid).map(|s| s.uid.clone())
            {
                self.store.add_ban_teamspeak(ban.id, &teamspeak).map_err(storage_error)?;
            }
            self.remove(s, reason.clone());
        }
        self.mls_reconcile().map_err(storage_error)?;
        Ok(Response::Ban { ban })
    }

    pub(super) fn ban_list(&mut self, session: SessionId) -> Reply {
        self.require(session, Permission::BanMembers)?;
        Ok(Response::Bans { bans: self.store.bans(now_ms()).map_err(storage_error)? })
    }

    pub(super) fn ban_delete(&mut self, session: SessionId, id: BanId) -> Reply {
        self.require(session, Permission::BanMembers)?;
        let Some(ban) = self.store.delete_ban(id, now_ms()).map_err(storage_error)? else {
            return Err(err(ErrorCode::NotFound, "no such ban"));
        };
        // A TeamSpeak user's ban also lives on the TeamSpeak server.
        if let Some(uid) = ban.uid.filter(|uid| uid.starts_with("ts:")) {
            self.notify_bridge(|| BridgeNote::Unban { uid });
        }
        // Banning a member banned the TeamSpeak sessions of their linked identities there too.
        for teamspeak in self.store.take_ban_teamspeak(id).map_err(storage_error)? {
            self.notify_bridge(|| BridgeNote::Unban { uid: teamspeak });
        }
        Ok(Response::Empty {})
    }

    // --------------------------------------------------------------- invites

    pub(super) fn invite_create(&mut self, session: SessionId, i: InviteCreate) -> Reply {
        self.require(session, Permission::CreateInvites)?;
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
            let _ = g;
            if !self.below(session, group) {
                return Err(err(ErrorCode::Forbidden, "this role is not below your rank"));
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
        self.store.insert_invite(&invite, &self.sessions[&session].uid).map_err(storage_error)?;
        Ok(Response::Invite { invite })
    }

    /// Everyone with `invite_create` sees the invites they made; managers see all.
    /// Ownership is the creator's identity, never their (changeable) nickname.
    pub(super) fn invite_list(&mut self, session: SessionId) -> Reply {
        self.require(session, Permission::CreateInvites)?;
        let uid = self.sessions[&session].uid.clone();
        let creator = (!self.has(session, Permission::ManageServer)).then_some(uid.as_str());
        Ok(Response::Invites { invites: self.store.invites(creator).map_err(storage_error)? })
    }

    pub(super) fn invite_delete(&mut self, session: SessionId, code: &str) -> Reply {
        self.require(session, Permission::CreateInvites)?;
        let uid = self.sessions[&session].uid.clone();
        let creator = (!self.has(session, Permission::ManageServer)).then_some(uid.as_str());
        if self.store.delete_invite(code, creator).map_err(storage_error)? {
            return Ok(Response::Empty {});
        }
        if creator.is_some() && self.store.invites(None).map_err(storage_error)?.iter().any(|i| i.code == code) {
            return Err(err(ErrorCode::Forbidden, "you can only delete your own invites"));
        }
        Err(err(ErrorCode::NotFound, "no such invite"))
    }

    // -------------------------------------------------------------- settings

    pub(super) fn server_update(&mut self, session: SessionId, id: u32, u: ServerUpdate) -> Option<Reply> {
        let checked = (|| {
            self.require(session, Permission::ManageServer)?;
            let name = u.name.as_deref().map(|n| clean(n, 64, "server name")).transpose()?;
            if u.welcome.as_ref().is_some_and(|w| w.chars().count() > 1000) {
                return Err(err(ErrorCode::BadRequest, "welcome message too long"));
            }
            if u.default_channel.is_some_and(|c| !self.channels.contains_key(&c)) {
                return Err(err(ErrorCode::NotFound, "no such channel"));
            }
            if u.default_channel.is_some_and(|c| self.channels.get(&c).is_some_and(|c| c.private)) {
                return Err(err(ErrorCode::BadRequest, "a private channel cannot be the default channel"));
            }
            if self.info.teamspeak && u.default_channel.is_some_and(|c| self.is_encrypted(c)) {
                return Err(err(
                    ErrorCode::BadRequest,
                    "with TeamSpeak compatibility the default channel must be plaintext",
                ));
            }
            if u.max_clients.is_some_and(|m| m == 0 || m > 100_000) {
                return Err(err(ErrorCode::BadRequest, "max clients must be 1–100000"));
            }
            if u.password.as_ref().is_some_and(|p| p.len() > 128) {
                return Err(err(ErrorCode::BadRequest, "password too long"));
            }
            if let Some(limits) = &u.screen {
                super::egress::check_limits(limits)?;
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
            if let Some(limits) = u.screen {
                core.set_screen_limits(limits).map_err(storage_error)?;
            }
            if let Some(access) = u.file_access {
                let value = match access {
                    FileAccess::Link => "link",
                    FileAccess::Members => "members",
                };
                core.store.set_meta("file_access", value).map_err(storage_error)?;
                core.info.file_access = access;
            }
            if let Some(on) = u.screen_guest_links {
                core.store.set_meta("screen_guest_links", if on { "on" } else { "off" }).map_err(storage_error)?;
                core.info.screen_guest_links = on;
            }
            // Members-only files or no watch links: the links that work now stop.
            core.recheck_watch_links();
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
                let typed = password.clone();
                self.defer(
                    move || (!password.is_empty()).then(|| passwords::hash_secret(&password, false)),
                    move |core, hash| {
                        if !core.sessions.contains_key(&session) {
                            return;
                        }
                        let result = apply(core, Some(hash.clone()));
                        if result.is_ok() {
                            // TeamSpeak takes the same password: it waits for the bridge (which may be
                            // restarting) like one typed at an import, and replaces any such.
                            let queued = match (&hash, core.info.teamspeak) {
                                (Some(hash), true) => core.store.queue_bridge_server_password(hash, &typed),
                                _ => core.store.forget_bridge_server_password(None),
                            };
                            if let Err(e) = queued {
                                tracing::warn!("store: {e:#}");
                            }
                            let note =
                                super::bridge::ServerPassword { plaintext: hash.is_some().then_some(typed), hash };
                            core.notify_bridge(|| BridgeNote::ServerPassword(note));
                        }
                        core.reply(session, id, result);
                    },
                )
            }
        }
    }

    /// Stores revocations from Gwar Connect and signs those devices out.
    pub(super) fn revoked(&mut self, devices: Vec<super::RevokedDevice>, seq: i64) {
        let current = self.store.meta("connect_seq").ok().flatten().and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
        let devices: Vec<_> = devices
            .into_iter()
            .filter(|d| d.seq > current && d.seq <= seq)
            .filter(|d| {
                let statement = crate::identity::revoke_statement(&d.account_key, &d.device_key, d.revoked_at);
                crate::identity::signed_by(&d.account_key, &statement, &d.signature)
            })
            .collect();
        match self.store.apply_revocations(&devices, seq) {
            Ok(true) => {
                self.voice_dirty = true;
            }
            Ok(false) => return,
            Err(e) => {
                tracing::warn!("store: {e:#}");
                return;
            }
        }
        let gone: Vec<_> = self
            .sessions
            .values()
            .filter(|s| {
                s.device
                    .as_ref()
                    .is_some_and(|d| devices.iter().any(|r| r.account_key == s.account_key && &r.device_key == d))
            })
            .map(|s| s.id)
            .collect();
        let reason =
            LeaveReason::Kicked { by: "Gwar Connect".into(), reason: Some("this device was signed out".into()) };
        for session in gone {
            self.remove(session, reason.clone());
        }
        if let Err(e) = self.mls_reconcile() {
            tracing::warn!("MLS reconciliation: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ip_bans_cover_the_banned_ipv6_64() {
        let ban = |ip: &str| Ban {
            id: 1,
            uid: Some("u".into()),
            ip: Some(ip.into()),
            nickname: String::new(),
            reason: None,
            by: String::new(),
            created_at: 0,
            expires_at: None,
        };
        let v6 = ban("2001:db8:1:2::5");
        assert!(bans(&v6, None, Some("2001:db8:1:2:ffff::1".parse().unwrap())));
        assert!(!bans(&v6, None, Some("2001:db8:1:3::5".parse().unwrap())));
        assert!(bans(&v6, Some("u"), None));
        assert!(!bans(&v6, Some("other"), None));
        let v4 = ban("192.0.2.1");
        assert!(bans(&v4, None, Some("::ffff:192.0.2.1".parse().unwrap())));
        assert!(!bans(&v4, None, Some("192.0.2.2".parse().unwrap())));
        assert!(!bans(&Ban { ip: None, ..v4 }, None, Some("192.0.2.1".parse().unwrap())));
    }
}
