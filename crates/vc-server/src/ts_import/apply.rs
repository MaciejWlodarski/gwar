//! Writes a plan into a Gwar server's database (stopped: the data directory is locked by the
//! caller). A second import of the same source updates what the first made (`ts_imports`).

use std::collections::BTreeSet;

use anyhow::Result;
use vc_proto::{ChannelOverride, Group, GroupId};

use super::plan::{Password, Plan, Target};
use crate::{
    core::{now_ms, passwords},
    store::{ADMIN_GROUP, ChannelRow, MEMBER_GROUP, Store},
};

#[derive(Debug, Default, PartialEq)]
pub struct Applied {
    pub roles_created: usize,
    pub roles_updated: usize,
    pub channels_created: usize,
    pub channels_updated: usize,
    pub members_created: usize,
    pub members_updated: usize,
    pub bans: usize,
}

fn random_password() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 18];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Applies `plan`, read from the source server `source` (its unique id).
pub fn apply(store: &Store, plan: &Plan, source: &str, replace_channels: bool) -> Result<Applied> {
    let mut applied = Applied::default();

    // ---------------------------------------------------------------- server
    if let Some(name) = &plan.server_name {
        store.set_meta("name", name)?;
    }
    if let Some(welcome) = &plan.welcome {
        store.set_meta("welcome", welcome)?;
    }
    if let Some(max) = plan.max_clients {
        store.set_meta("max_clients", &max.to_string())?;
    }
    match &plan.server_password {
        Password::Kept(p) => store.set_meta("password_hash", &passwords::hash_secret(p, false))?,
        Password::Locked => store.set_meta("password_hash", &passwords::hash_secret(&random_password(), false))?,
        Password::Dropped => store.set_meta("password_hash", "")?,
        Password::None => {}
    }

    // ----------------------------------------------------------------- roles
    let groups = store.groups()?;
    if let Some(member) = groups.iter().find(|g| g.id == MEMBER_GROUP) {
        store.update_group(&Group { permissions: plan.member_permissions.clone(), ..member.clone() })?;
    }
    let mut imported_roles: Vec<GroupId> = Vec::new();
    for role in &plan.roles {
        let existing = store
            .ts_import_target(source, "role", role.ts_id)?
            .and_then(|id| groups.iter().find(|g| i64::from(g.id) == id).cloned());
        let id = match existing {
            Some(g) => {
                store.update_group(&Group {
                    name: role.name.clone(),
                    permissions: role.permissions.clone(),
                    ..g.clone()
                })?;
                applied.roles_updated += 1;
                g.id
            }
            None => {
                let id = store.insert_group(&role.name, &role.permissions, None, false, false)?;
                store.set_ts_import_target(source, "role", role.ts_id, i64::from(id))?;
                applied.roles_created += 1;
                id
            }
        };
        imported_roles.push(id);
    }
    // Admin first, then the imported roles in TeamSpeak's order, then the server's others; Member last.
    let others: Vec<GroupId> = store
        .groups()?
        .into_iter()
        .map(|g| g.id)
        .filter(|id| *id != ADMIN_GROUP && *id != MEMBER_GROUP && !imported_roles.contains(id))
        .collect();
    let order: Vec<GroupId> =
        std::iter::once(ADMIN_GROUP).chain(imported_roles.iter().copied()).chain(others).collect();
    store.reorder_groups(&order)?;
    let role_of = |ts: u64| -> Result<Option<GroupId>> {
        Ok(store.ts_import_target(source, "role", ts)?.and_then(|id| GroupId::try_from(id).ok()))
    };

    // -------------------------------------------------------------- channels
    let mut existing = store.channels()?;
    let mapped: BTreeSet<i64> = plan
        .channels
        .iter()
        .filter_map(|c| store.ts_import_target(source, "channel", c.ts_id).ok().flatten())
        .collect();
    if replace_channels {
        // The server's own channels make room; the default one stays unless TeamSpeak's replaces it.
        let default: Option<u32> = store.meta("default_channel")?.and_then(|d| d.parse().ok());
        let replaced = plan.channels.iter().any(|c| c.default);
        for row in existing.iter().filter(|r| r.parent.is_none() && !mapped.contains(&i64::from(r.id))) {
            if replaced || Some(row.id) != default {
                store.delete_channel(row.id)?;
            }
        }
        existing = store.channels()?;
    }
    for c in &plan.channels {
        let parent = match c.parent {
            Some(p) => store.ts_import_target(source, "channel", p)?.and_then(|id| u32::try_from(id).ok()),
            None => None,
        };
        let mut overrides = Vec::new();
        for o in &c.overrides {
            let (role, user) = match &o.target {
                Target::Member => (Some(MEMBER_GROUP), None),
                Target::Role(ts) => match role_of(*ts)? {
                    Some(id) => (Some(id), None),
                    None => continue,
                },
                Target::User(uid) => (None, Some(uid.clone())),
            };
            overrides.push(ChannelOverride { role, user, allow: o.allow.clone(), deny: o.deny.clone() });
        }
        let (password_hash, plaintext) = match &c.password {
            Password::Kept(p) => (Some(passwords::hash_secret(p, false)), Some(p.clone())),
            Password::Locked => (Some(passwords::hash_secret(&random_password(), false)), None),
            Password::None | Password::Dropped => (None, None),
        };
        let current = store
            .ts_import_target(source, "channel", c.ts_id)?
            .and_then(|id| existing.iter().find(|r| i64::from(r.id) == id).cloned());
        // Siblings keep distinct names: one of the server's own takes precedence.
        let taken = |name: &str| {
            existing
                .iter()
                .any(|r| r.parent == parent && r.name == name && current.as_ref().is_none_or(|cur| cur.id != r.id))
        };
        let mut name = c.name.clone();
        for n in 2.. {
            if !taken(&name) {
                break;
            }
            name = format!("{} ({n})", c.name);
        }
        let row = ChannelRow {
            id: current.as_ref().map_or(0, |r| r.id),
            parent,
            name,
            topic: c.topic.clone(),
            position: c.position,
            password_hash,
            max_clients: c.max_clients,
            overrides,
            synced: false,
            ..Default::default()
        };
        let id = match &current {
            Some(_) => {
                store.update_channel(&row)?;
                applied.channels_updated += 1;
                row.id
            }
            None => {
                let id = store.create_channel(&row)?;
                store.set_ts_import_target(source, "channel", c.ts_id, i64::from(id))?;
                applied.channels_created += 1;
                id
            }
        };
        if let Some(p) = plaintext {
            store.queue_bridge_password(id, &p)?;
        }
        if c.default {
            store.set_meta("default_channel", &id.to_string())?;
        }
        existing = store.channels()?;
    }

    // --------------------------------------------------------------- members
    for m in &plan.members {
        let roles: Vec<GroupId> = m.roles.iter().filter_map(|r| role_of(*r).ok().flatten()).collect();
        match store.user_by_uid(&m.uid)? {
            Some((id, member)) => {
                let mut groups: Vec<GroupId> = member.groups.iter().copied().chain(roles).collect();
                groups.sort_unstable();
                groups.dedup();
                store.set_user_groups(id, &groups)?;
                applied.members_updated += 1;
            }
            None => {
                // A TeamSpeak user has no Gwar key; their uid stands in for it.
                let row = store.touch_user(&m.uid, &m.uid, &m.nickname, m.last_seen)?;
                let groups: Vec<GroupId> = std::iter::once(MEMBER_GROUP).chain(roles).collect();
                store.set_user_groups(row.id, &groups)?;
                applied.members_created += 1;
            }
        }
    }

    // ------------------------------------------------------------------ bans
    let now = now_ms();
    let current = store.bans(now)?;
    for ban in &plan.bans {
        let duplicate = current.iter().any(|b| b.uid == ban.uid && b.ip == ban.ip);
        if !duplicate {
            store.insert_ban(ban, None)?;
            applied.bans += 1;
        }
    }
    Ok(applied)
}
