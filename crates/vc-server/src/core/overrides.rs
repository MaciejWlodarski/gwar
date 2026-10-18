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
        self.admitted(Some(s), channel) && self.channel_permissions(&s.uid, &s.groups, channel).contains(&permission)
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
        match (&entry.role, &entry.user) {
            (Some(role), None) => {
                if !self.groups.contains_key(role) {
                    return Err(err(ErrorCode::NotFound, "no such role"));
                }
                if !self.below(session, *role) {
                    return Err(err(ErrorCode::Forbidden, "this role is not below your rank"));
                }
            }
            (None, Some(uid)) => {
                let Some((_, member)) = self.store.user_by_uid(uid).map_err(storage_error)? else {
                    return Err(err(ErrorCode::NotFound, "no such member"));
                };
                if !self.outranks(session, uid, &member.groups) {
                    return Err(err(ErrorCode::Forbidden, "this member is not below your rank"));
                }
            }
            _ => return Err(err(ErrorCode::BadRequest, "name a role or a member")),
        }
        let mut row = self.channels[&channel].clone();
        // Editing a synced channel starts it off with its parent's overrides.
        if row.synced {
            row.overrides = self.effective_overrides(channel).to_vec();
            row.synced = false;
        }
        let same = |o: &ChannelOverride| o.role == entry.role && o.user == entry.user;
        let before = row.overrides.iter().find(|o| same(o)).cloned().unwrap_or_default();
        // Allowing or denying a permission, or lifting either, needs it here.
        let mine = {
            let s = &self.sessions[&session];
            self.channel_permissions(&s.uid, &s.groups, channel)
        };
        let changed = |a: &[Permission], b: &[Permission]| {
            a.iter().filter(|p| !b.contains(p)).chain(b.iter().filter(|p| !a.contains(p))).copied().collect::<Vec<_>>()
        };
        let touched: Vec<_> =
            changed(&entry.allow, &before.allow).into_iter().chain(changed(&entry.deny, &before.deny)).collect();
        if touched.iter().any(|p| !mine.contains(p)) {
            return Err(err(ErrorCode::Forbidden, "you cannot change a permission you do not have here"));
        }
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

    /// Takes out of voice, in `channel` and every channel under it, whoever may no longer join
    /// there. TeamSpeak users follow their own server.
    pub(super) fn enforce_connect(&mut self, channel: ChannelId) {
        self.enforce_connect_where(|core, c| core.is_descendant(c, channel));
    }

    /// The same everywhere, after roles or their permissions changed.
    pub(super) fn enforce_connect_everywhere(&mut self) {
        self.enforce_connect_where(|_, _| true);
    }

    fn enforce_connect_where(&mut self, within: impl Fn(&Self, ChannelId) -> bool) {
        let affected: Vec<SessionId> = self
            .sessions
            .values()
            .filter(|s| s.out.is_some())
            .filter(|s| s.channel.is_some_and(|c| within(self, c)))
            .filter(|s| {
                let c = s.channel.expect("in a channel");
                !self.channel_permissions(&s.uid, &s.groups, c).contains(&Permission::Connect)
            })
            .map(|s| s.id)
            .collect();
        for session in affected {
            let _ = self.channel_leave(session);
        }
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
