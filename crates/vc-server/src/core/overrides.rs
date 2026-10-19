//! Per-channel overrides (docs/roles.md, "Overrides" and "Resolving a permission").
//!
//! A channel's overrides allow or deny channel permissions for a role or one member. A synced
//! subchannel uses its nearest unsynced ancestor's. Resolution follows Discord's order: the
//! member's roles, then the Member role's override, then their other roles' overrides together
//! (deny, then allow), then their own. `administrator` and the owner skip overrides; seeing the
//! channel (public, or admitted to a private one) is the condition for all of it.

use std::collections::BTreeSet;

use vc_proto::{ChannelId, ChannelOverride, ErrorCode, Event, GroupId, Permission, Response, SessionId};

use super::{Core, ErrorBody, Reply, channel_view, err, storage_error};
use crate::store::MEMBER_GROUP;

/// Most overrides one channel keeps.
const MAX_OVERRIDES: usize = 100;
/// Synced chains longer than this stop (the tree is far shallower).
const MAX_DEPTH: usize = 64;

impl Core {
    /// The channel whose overrides apply in `channel`: itself, or its nearest unsynced ancestor.
    fn override_source(&self, channel: ChannelId) -> Option<ChannelId> {
        let mut id = channel;
        for _ in 0..MAX_DEPTH {
            let row = self.channels.get(&id)?;
            match row.parent {
                Some(parent) if row.synced => id = parent,
                _ => return Some(id),
            }
        }
        Some(id)
    }

    pub(super) fn effective_overrides(&self, channel: ChannelId) -> &[ChannelOverride] {
        self.override_source(channel).and_then(|c| self.channels.get(&c)).map_or(&[], |row| &row.overrides)
    }

    /// What a member (`uid`, holding `groups`) may do in `channel`, admission aside.
    pub(super) fn channel_permissions(
        &self,
        uid: &str,
        groups: &[GroupId],
        channel: ChannelId,
    ) -> BTreeSet<Permission> {
        let mut held = self.expand(self.group_permissions(groups), uid);
        if held.contains(&Permission::Administrator) {
            return held;
        }
        let overrides = self.effective_overrides(channel);
        let apply = |held: &mut BTreeSet<Permission>, o: &ChannelOverride| {
            for p in &o.deny {
                held.remove(p);
            }
            held.extend(o.allow.iter().copied());
        };
        // Member is everyone's role.
        if let Some(o) = overrides.iter().find(|o| o.role == Some(MEMBER_GROUP)) {
            apply(&mut held, o);
        }
        let mine = overrides.iter().filter(|o| o.role.is_some_and(|r| r != MEMBER_GROUP && groups.contains(&r)));
        let (mut allow, mut deny) = (BTreeSet::new(), BTreeSet::new());
        for o in mine {
            allow.extend(o.allow.iter().copied());
            deny.extend(o.deny.iter().copied());
        }
        for p in &deny {
            held.remove(p);
        }
        held.extend(allow);
        if let Some(o) = overrides.iter().find(|o| o.user.as_deref() == Some(uid)) {
            apply(&mut held, o);
        }
        if !held.contains(&Permission::Connect) {
            held.remove(&Permission::Speak);
        }
        held
    }

    /// Whether `session` sees `channel` and may do `permission` there.
    pub(super) fn can_in(&self, session: SessionId, channel: ChannelId, permission: Permission) -> bool {
        let s = &self.sessions[&session];
        self.admitted(Some(s), channel) && self.channel_permissions(&s.member, &s.groups, channel).contains(&permission)
    }

    pub(super) fn require_in(
        &self,
        session: SessionId,
        channel: ChannelId,
        permission: Permission,
    ) -> Result<(), ErrorBody> {
        if self.can_in(session, channel, permission) {
            Ok(())
        } else {
            Err(err(ErrorCode::Forbidden, "missing permission in this channel"))
        }
    }

    /// Whether `session` holds `permission` in any channel it sees (for what is not tied to one yet).
    pub(super) fn can_anywhere(&self, session: SessionId, permission: Permission) -> bool {
        self.has(session, permission) || self.channels.keys().any(|&c| self.can_in(session, c, permission))
    }

    /// Sets (or with nothing in it, removes) one role's or member's override in `channel`.
    pub(super) fn channel_override_set(
        &mut self,
        session: SessionId,
        channel: ChannelId,
        entry: ChannelOverride,
    ) -> Reply {
        self.require_in(session, channel, Permission::ManageChannels)?;
        let mut entry = entry;
        entry.allow.sort_unstable();
        entry.allow.dedup();
        entry.deny.sort_unstable();
        entry.deny.dedup();
        if entry.allow.iter().chain(&entry.deny).any(|p| !p.is_channel()) {
            return Err(err(ErrorCode::BadRequest, "only channel permissions can be overridden"));
        }
        if entry.allow.iter().any(|p| entry.deny.contains(p)) {
            return Err(err(ErrorCode::BadRequest, "a permission is either allowed or denied"));
        }
        if entry.role.is_some() == entry.user.is_some() {
            return Err(err(ErrorCode::BadRequest, "name a role or a member"));
        }
        if entry.role.is_some_and(|r| !self.groups.contains_key(&r)) {
            return Err(err(ErrorCode::NotFound, "no such role"));
        }
        // A new or changed override names a member; lifting one may name someone who left.
        let lifting = entry.allow.is_empty() && entry.deny.is_empty();
        if let Some(uid) = entry.user.as_deref().filter(|_| !lifting)
            && self.store.user_by_uid(uid).map_err(storage_error)?.is_none()
        {
            return Err(err(ErrorCode::NotFound, "no such member"));
        }
        let Some(mut row) = self.channels.get(&channel).cloned() else {
            return Err(err(ErrorCode::NotFound, "no such channel"));
        };
        // Editing a synced channel starts it off with its parent's overrides.
        if row.synced {
            row.overrides = self.effective_overrides(channel).to_vec();
            row.synced = false;
        }
        let same = |o: &ChannelOverride| o.role == entry.role && o.user == entry.user;
        let before = row.overrides.iter().find(|o| same(o)).cloned();
        let mine = self.mine_in(session, channel);
        self.may_change_entry(session, &mine, before.as_ref(), Some(&entry))?;
        row.overrides.retain(|o| !same(o));
        if !entry.allow.is_empty() || !entry.deny.is_empty() {
            if row.overrides.len() >= MAX_OVERRIDES {
                return Err(err(ErrorCode::BadRequest, "a channel has at most 100 overrides"));
            }
            row.overrides.push(entry);
        }
        self.store.update_channel(&row).map_err(storage_error)?;
        let view = channel_view(&row);
        self.channels.insert(channel, row);
        self.broadcast_channel(Event::ChannelUpdated(view.clone()));
        self.enforce_connect(channel);
        Ok(Response::Channel(view))
    }

    /// What `session` may do in `channel` now.
    fn mine_in(&self, session: SessionId, channel: ChannelId) -> BTreeSet<Permission> {
        let s = &self.sessions[&session];
        self.channel_permissions(&s.member, &s.groups, channel)
    }

    /// Whether `session` may turn one target's override from `before` into `after` (either may be
    /// absent): the target must be a role below their rank or a member ranked below them, and
    /// every permission allowed, denied or lifted must be one they hold there (`mine`).
    fn may_change_entry(
        &self,
        session: SessionId,
        mine: &BTreeSet<Permission>,
        before: Option<&ChannelOverride>,
        after: Option<&ChannelOverride>,
    ) -> Result<(), ErrorBody> {
        let Some(target) = after.or(before) else { return Ok(()) };
        let empty = ChannelOverride::default();
        let (b, a) = (before.unwrap_or(&empty), after.unwrap_or(&empty));
        if b.allow == a.allow && b.deny == a.deny {
            return Ok(());
        }
        if let Some(role) = target.role {
            if !self.below(session, role) {
                return Err(err(ErrorCode::Forbidden, "this role is not below your rank"));
            }
        } else if let Some(uid) = &target.user {
            // Someone no longer a member ranks as Member: their override can still be lifted.
            let groups = self.store.user_by_uid(uid).map_err(storage_error)?.map(|(_, m)| m.groups).unwrap_or_default();
            if !self.outranks(session, uid, &groups) {
                return Err(err(ErrorCode::Forbidden, "this member is not below your rank"));
            }
        }
        let changed = |x: &[Permission], y: &[Permission]| {
            x.iter().filter(|p| !y.contains(p)).chain(y.iter().filter(|p| !x.contains(p))).copied().collect::<Vec<_>>()
        };
        let touched = changed(&a.allow, &b.allow).into_iter().chain(changed(&a.deny, &b.deny));
        if touched.into_iter().any(|p| !mine.contains(&p)) {
            return Err(err(ErrorCode::Forbidden, "you cannot change a permission you do not have here"));
        }
        Ok(())
    }

    /// Whether `session` may replace `channel`'s effective overrides `old` with `new` (syncing it,
    /// or moving a synced channel): every entry that changes follows the rules of `channel.override`.
    pub(super) fn may_replace_overrides(
        &self,
        session: SessionId,
        channel: ChannelId,
        old: &[ChannelOverride],
        new: &[ChannelOverride],
    ) -> Result<(), ErrorBody> {
        let mine = self.mine_in(session, channel);
        let same = |x: &ChannelOverride, y: &ChannelOverride| x.role == y.role && x.user == y.user;
        for o in old.iter().chain(new) {
            let before = old.iter().find(|x| same(x, o));
            let after = new.iter().find(|x| same(x, o));
            self.may_change_entry(session, &mine, before, after)?;
        }
        Ok(())
    }

    /// Takes out of voice, in `channel` and every channel under it, whoever may no longer join
    /// there. TeamSpeak users, always in some channel, go to the default one (and the bridge
    /// moves them there on TeamSpeak).
    pub(super) fn enforce_connect(&mut self, channel: ChannelId) {
        self.enforce_connect_where(|core, c| core.is_descendant(c, channel));
    }

    /// The same everywhere, after roles or their permissions changed.
    pub(super) fn enforce_connect_everywhere(&mut self) {
        self.enforce_connect_where(|_, _| true);
    }

    fn enforce_connect_where(&mut self, within: impl Fn(&Self, ChannelId) -> bool) {
        let fallback = self.info.default_channel;
        let affected: Vec<(SessionId, bool)> = self
            .sessions
            .values()
            .filter(|s| s.channel.is_some_and(|c| within(self, c) && (s.out.is_some() || c != fallback)))
            .filter(|s| {
                let c = s.channel.expect("in a channel");
                !self.channel_permissions(&s.member, &s.groups, c).contains(&Permission::Connect)
            })
            .map(|s| (s.id, s.out.is_none()))
            .collect();
        for (session, remote) in affected {
            if remote {
                // A move the bridge did not ask for reaches it as usual; during a bridge message
                // it is told afterwards.
                let _ = self.move_client(session, Some(fallback));
                if self.applying_remote {
                    self.bridge_corrections.push(session);
                }
            } else {
                let _ = self.channel_leave(session);
            }
        }
        // Who may speak can have changed as well: the SFU follows.
        self.publish_routing();
    }

    /// Mutes or deafens someone for everyone (docs/roles.md, `mute_members`): needs the permission in
    /// their channel and a rank above theirs. Lasts for their connection.
    pub(super) fn client_voice(
        &mut self,
        session: SessionId,
        client: SessionId,
        muted: Option<bool>,
        deafened: Option<bool>,
    ) -> Reply {
        let Some(target) = self.sessions.get(&client) else {
            return Err(err(ErrorCode::NotFound, "no such client"));
        };
        let Some(channel) = target.channel else {
            return Err(err(ErrorCode::BadRequest, "this person is not in voice"));
        };
        if client == session {
            return Err(err(ErrorCode::BadRequest, "mute or deafen yourself instead"));
        }
        // A TeamSpeak user hears through the bridge's puppets, which a deafen does not reach.
        if deafened.is_some() && target.out.is_none() {
            return Err(err(ErrorCode::BadRequest, "TeamSpeak users cannot be deafened from here"));
        }
        self.require_in(session, channel, Permission::MuteMembers)?;
        if !self.outranks(session, &target.member, &target.groups) {
            return Err(err(ErrorCode::Forbidden, "this member is not below your rank"));
        }
        let s = self.sessions.get_mut(&client).expect("checked above");
        if let Some(muted) = muted {
            s.server_muted = muted;
        }
        if let Some(deafened) = deafened {
            s.server_deafened = deafened;
        }
        self.client_updated(client);
        self.publish_routing();
        Ok(Response::Empty {})
    }

    /// Whether a remote (TeamSpeak) session may be in `channel`'s voice; if not, the bridge
    /// is told to put it back (see `bridge_corrections`).
    pub(super) fn remote_may_join(&self, uid: &str, groups: &[GroupId], channel: ChannelId) -> bool {
        self.channel_permissions(uid, groups, channel).contains(&Permission::Connect)
    }

    /// Drops a deleted role's overrides (the database already did) and tells those who see them.
    pub(super) fn forget_role_overrides(&mut self, role: GroupId) {
        let touched: Vec<ChannelId> =
            self.channels.values().filter(|c| c.overrides.iter().any(|o| o.role == Some(role))).map(|c| c.id).collect();
        for channel in touched {
            let Some(row) = self.channels.get_mut(&channel) else { continue };
            row.overrides.retain(|o| o.role != Some(role));
            let view = channel_view(row);
            self.broadcast_channel(Event::ChannelUpdated(view));
        }
    }
}
