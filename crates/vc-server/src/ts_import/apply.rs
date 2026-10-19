//! Writes a plan into a Gwar server's database: a copy of it, which replaces it once all went
//! well (see [`super::import`]). A second import of the same source updates what the first made
//! (`ts_imports`), but only what nobody changed in Gwar since: each record keeps what the import
//! last wrote there.

use std::collections::BTreeSet;

use anyhow::{Result, bail};
use serde_json::json;
use vc_proto::{ChannelOverride, Group, GroupId};

use super::plan::{Password, Plan, Target};
use crate::{
    core::{now_ms, passwords},
    store::{ADMIN_GROUP, ChannelRow, MEMBER_GROUP, Store},
};

#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct ApplyOptions {
    /// Remove this server's own top-level channels (not imported from this source) first.
    pub replace_channels: bool,
    /// With `replace_channels`, also those holding messages, subchannels or encrypted history.
    pub force: bool,
    /// The server runs the TeamSpeak bridge: typed passwords wait for it in plain text (it sets
    /// them on TeamSpeak once), see docs/teamspeak-import.md.
    pub for_bridge: bool,
}

#[derive(Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Applied {
    pub roles_created: usize,
    pub roles_updated: usize,
    pub channels_created: usize,
    pub channels_updated: usize,
    pub members_created: usize,
    pub members_updated: usize,
    pub bans: usize,
    /// Channels `replace_channels` removed.
    pub deleted: Vec<String>,
    /// Left as they are: changed or removed in Gwar since the last import, and the like.
    pub notes: Vec<String>,
}

fn random_password() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 18];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The hash to store for `password`, keeping `current` where it still fits (a re-run changes
/// nothing): the typed password if it verifies, any password for a locked one.
fn password_hash(password: &Password, current: Option<&str>) -> Option<String> {
    match password {
        Password::Kept(p) => match current {
            Some(hash) if passwords::verify_secret(p, hash, false) => Some(hash.to_owned()),
            _ => Some(passwords::hash_secret(p, false)),
        },
        Password::Locked => {
            Some(current.map_or_else(|| passwords::hash_secret(&random_password(), false), str::to_owned))
        }
        Password::None | Password::Dropped => None,
    }
}

// What an item looks like, for telling whether it changed since the import wrote it.

fn channel_print(row: &ChannelRow) -> String {
    let mut overrides = row.overrides.clone();
    for o in &mut overrides {
        o.allow.sort_unstable();
        o.deny.sort_unstable();
    }
    overrides.sort_by(|a, b| (a.role, &a.user).cmp(&(b.role, &b.user)));
    // Not the position: moving channels around in Gwar is no reason to stop updating them.
    json!({
        "parent": row.parent,
        "name": row.name,
        "topic": row.topic,
        "password_hash": row.password_hash,
        "max_clients": row.max_clients,
        "overrides": overrides,
        "synced": row.synced,
    })
    .to_string()
}

/// A channel's print as compared: overrides Gwar itself dropped since (a deleted role, a removed
/// member) are no change by the operator. `None` for a print that does not parse.
fn comparable(print: &str, keep: &impl Fn(&ChannelOverride) -> bool) -> Option<serde_json::Value> {
    let mut value: serde_json::Value = serde_json::from_str(print).ok()?;
    let overrides: Vec<ChannelOverride> = serde_json::from_value(value.get_mut("overrides")?.take()).ok()?;
    let mut kept: Vec<_> = overrides.into_iter().filter(|o| keep(o)).collect();
    kept.sort_by(|a, b| (a.role, &a.user).cmp(&(b.role, &b.user)));
    value["overrides"] = json!(kept);
    Some(value)
}

fn role_print(group: &Group) -> String {
    let mut permissions = group.permissions.clone();
    permissions.sort_unstable();
    json!({ "name": group.name, "permissions": permissions }).to_string()
}

fn server_print(store: &Store) -> Result<String> {
    let meta = |key| store.meta(key);
    Ok(json!({
        "name": meta("name")?,
        "welcome": meta("welcome")?,
        "max_clients": meta("max_clients")?,
        "password_hash": meta("password_hash")?,
    })
    .to_string())
}

/// A role as stored now (read back, so the print matches the next run's).
fn stored_role_print(store: &Store, id: GroupId) -> Result<Option<String>> {
    Ok(store.groups()?.iter().find(|g| g.id == id).map(role_print))
}

fn order_print(store: &Store) -> Result<String> {
    Ok(json!(store.groups()?.iter().map(|g| g.id).collect::<Vec<_>>()).to_string())
}

/// Whether what the import last wrote (`written`) is still what is there (`now`); nothing written
/// yet counts as unchanged.
fn untouched(record: &Option<(i64, Option<String>)>, now: &str) -> bool {
    record.as_ref().is_none_or(|(_, written)| written.as_deref() == Some(now))
}

/// Applies `plan`, read from the source server `source` (its unique id).
pub fn apply(store: &Store, plan: &Plan, source: &str, options: ApplyOptions) -> Result<Applied> {
    let mut applied = Applied::default();

    // ---------------------------------------------------------------- server
    let record = store.ts_import_record(source, "server", 0)?;
    if untouched(&record, &server_print(store)?) {
        if let Some(name) = &plan.server_name {
            store.set_meta("name", name)?;
        }
        if let Some(welcome) = &plan.welcome {
            store.set_meta("welcome", welcome)?;
        }
        if let Some(max) = plan.max_clients {
            store.set_meta("max_clients", &max.to_string())?;
        }
        let current = store.meta("password_hash")?.filter(|h| !h.is_empty());
        if plan.server_password != Password::None {
            let hash = password_hash(&plan.server_password, current.as_deref());
            store.set_meta("password_hash", hash.as_deref().unwrap_or(""))?;
        }
        store.set_ts_import_record(source, "server", 0, 0, Some(&server_print(store)?))?;
    } else {
        applied
            .notes
            .push("The server's name, welcome, size or password changed in Gwar since the last import: kept.".into());
    }

    // ----------------------------------------------------------------- roles
    let groups = store.groups()?;
    if let Some(member) = groups.iter().find(|g| g.id == MEMBER_GROUP) {
        let record = store.ts_import_record(source, "member_role", 0)?;
        if untouched(&record, &role_print(member)) {
            let lost: Vec<_> = member.permissions.iter().filter(|p| !plan.member_permissions.contains(p)).collect();
            if record.is_none() && !lost.is_empty() {
                applied.notes.push(format!("Member had {lost:?} before; TeamSpeak's default group has not."));
            }
            let updated = Group { permissions: plan.member_permissions.clone(), ..member.clone() };
            store.update_group(&updated)?;
            let written = stored_role_print(store, MEMBER_GROUP)?;
            store.set_ts_import_record(source, "member_role", 0, i64::from(MEMBER_GROUP), written.as_deref())?;
        } else {
            applied.notes.push("Member's permissions changed in Gwar since the last import: kept.".into());
        }
    }
    let order_record = store.ts_import_record(source, "order", 0)?;
    let order_untouched = untouched(&order_record, &order_print(store)?);
    let mut imported_roles: Vec<GroupId> = Vec::new();
    for role in &plan.roles {
        let record = store.ts_import_record(source, "role", role.ts_id)?;
        let id = match &record {
            Some((id, _)) => match groups.iter().find(|g| i64::from(g.id) == *id) {
                None => {
                    applied.notes.push(format!("Role \"{}\" was deleted in Gwar: not made again.", role.name));
                    continue;
                }
                Some(g) if untouched(&record, &role_print(g)) => {
                    let updated = Group { name: role.name.clone(), permissions: role.permissions.clone(), ..g.clone() };
                    store.update_group(&updated)?;
                    let written = stored_role_print(store, g.id)?;
                    store.set_ts_import_record(source, "role", role.ts_id, *id, written.as_deref())?;
                    applied.roles_updated += 1;
                    g.id
                }
                Some(g) => {
                    applied.notes.push(format!("Role \"{}\" changed in Gwar since the last import: kept.", g.name));
                    g.id
                }
            },
            None => {
                let id = store.insert_group(&role.name, &role.permissions, None, false, false)?;
                let written = stored_role_print(store, id)?;
                store.set_ts_import_record(source, "role", role.ts_id, i64::from(id), written.as_deref())?;
                applied.roles_created += 1;
                id
            }
        };
        imported_roles.push(id);
    }
    if order_untouched {
        // Admin first, then the imported roles in their rank, then the server's others; Member last.
        let others: Vec<GroupId> = store
            .groups()?
            .into_iter()
            .map(|g| g.id)
            .filter(|id| *id != ADMIN_GROUP && *id != MEMBER_GROUP && !imported_roles.contains(id))
            .collect();
        let order: Vec<GroupId> =
            std::iter::once(ADMIN_GROUP).chain(imported_roles.iter().copied()).chain(others).collect();
        store.reorder_groups(&order)?;
        store.set_ts_import_record(source, "order", 0, 0, Some(&order_print(store)?))?;
    } else {
        applied.notes.push(
            "The role order changed in Gwar since the last import: kept (new roles are just above Member).".into(),
        );
    }
    let live: BTreeSet<GroupId> = store.groups()?.iter().map(|g| g.id).collect();
    let role_of = |ts: u64| -> Result<Option<GroupId>> {
        Ok(store
            .ts_import_target(source, "role", ts)?
            .and_then(|id| GroupId::try_from(id).ok())
            .filter(|id| live.contains(id)))
    };

    // -------------------------------------------------------------- channels
    let imported: BTreeSet<i64> = store.ts_import_targets(source, "channel")?.into_iter().collect();
    let mut existing = store.channels()?;
    if options.replace_channels {
        // The server's own channels make room; the default one stays unless TeamSpeak's replaces it.
        let default: Option<u32> = store.meta("default_channel")?.and_then(|d| d.parse().ok());
        let replaced = plan.channels.iter().any(|c| c.default);
        let doomed: Vec<&ChannelRow> = existing
            .iter()
            .filter(|r| r.parent.is_none() && !imported.contains(&i64::from(r.id)))
            .filter(|r| replaced || Some(r.id) != default)
            .collect();
        let mut busy = Vec::new();
        for row in &doomed {
            if store.channel_in_use(row.id)? {
                busy.push(row.name.clone());
            }
        }
        if !busy.is_empty() && !options.force {
            bail!(
                "--replace-channels would delete channels with messages, subchannels or encrypted history: {} (--force deletes them too)",
                busy.join(", ")
            );
        }
        for row in doomed {
            store.delete_channel(row.id)?;
            applied.deleted.push(row.name.clone());
        }
        existing = store.channels()?;
    }
    // New top-level channels go after the server's own.
    let offset = existing
        .iter()
        .filter(|r| r.parent.is_none() && !imported.contains(&i64::from(r.id)))
        .map(|r| r.position + 1)
        .max()
        .unwrap_or(0);
    // TeamSpeak channels left out: deleted in Gwar, or under one that was.
    let mut skipped: BTreeSet<u64> = BTreeSet::new();
    // The default channel follows TeamSpeak's unless chosen in Gwar since the last import.
    let default_record = store.ts_import_record(source, "default", 0)?;
    let default_untouched = untouched(&default_record, &store.meta("default_channel")?.unwrap_or_default());
    if !default_untouched {
        applied.notes.push("The default channel was chosen in Gwar since the last import: kept.".into());
    }
    let exists = |o: &ChannelOverride| {
        o.role.is_none_or(|r| live.contains(&r))
            && o.user.as_ref().is_none_or(|u| store.user_by_uid(u).ok().flatten().is_some())
    };
    for c in &plan.channels {
        if c.parent.is_some_and(|p| skipped.contains(&p)) {
            skipped.insert(c.ts_id);
            continue;
        }
        let parent = match c.parent {
            Some(p) => store
                .ts_import_target(source, "channel", p)?
                .and_then(|id| u32::try_from(id).ok())
                .filter(|id| existing.iter().any(|r| r.id == *id)),
            None => None,
        };
        let record = store.ts_import_record(source, "channel", c.ts_id)?;
        let current = record.as_ref().and_then(|(id, _)| existing.iter().find(|r| i64::from(r.id) == *id).cloned());
        if record.is_some() && current.is_none() {
            applied.notes.push(format!("Channel \"{}\" was deleted in Gwar: not made again.", c.name));
            skipped.insert(c.ts_id);
            continue;
        }
        if let (Some(cur), Some((_, written))) = (&current, &record)
            && written.as_deref().and_then(|w| comparable(w, &exists)) != comparable(&channel_print(cur), &|_| true)
        {
            applied.notes.push(format!("Channel \"{}\" changed in Gwar since the last import: kept.", cur.name));
            if c.default && default_untouched {
                store.set_meta("default_channel", &cur.id.to_string())?;
            }
            continue;
        }
        let mut overrides = Vec::new();
        for o in &c.overrides {
            let (role, user) = match &o.target {
                Target::Member => (Some(MEMBER_GROUP), None),
                Target::Role(ts) => match role_of(*ts)? {
                    Some(id) => (Some(id), None),
                    None => continue,
                },
                // A linked identity's override is its member's; two identities of one member keep the first.
                Target::User(uid) => (None, Some(store.teamspeak_member(uid)?.unwrap_or_else(|| uid.clone()))),
            };
            if user.is_some() && overrides.iter().any(|o: &ChannelOverride| o.user == user) {
                continue;
            }
            overrides.push(ChannelOverride { role, user, allow: o.allow.clone(), deny: o.deny.clone() });
        }
        let current_hash = current.as_ref().and_then(|r| r.password_hash.clone());
        let hash = password_hash(&c.password, current_hash.as_deref());
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
        let position = match &current {
            Some(cur) => cur.position,
            None if parent.is_none() => c.position + offset,
            None => c.position,
        };
        let row = ChannelRow {
            id: current.as_ref().map_or(0, |r| r.id),
            parent,
            name,
            topic: c.topic.clone(),
            position,
            password_hash: hash.clone(),
            max_clients: c.max_clients,
            overrides,
            synced: false,
            ..current.clone().unwrap_or_default()
        };
        let id = match &current {
            Some(_) => {
                store.update_channel(&row)?;
                applied.channels_updated += 1;
                row.id
            }
            None => {
                let id = store.create_channel(&row)?;
                applied.channels_created += 1;
                id
            }
        };
        existing = store.channels()?;
        let written = existing.iter().find(|r| r.id == id).map(channel_print);
        store.set_ts_import_record(source, "channel", c.ts_id, i64::from(id), written.as_deref())?;
        // For the bridge to set on TeamSpeak; it confirms each, so sending one again does no harm
        // (and covers a first run without --for-bridge).
        if let Password::Kept(p) = &c.password
            && options.for_bridge
        {
            store.queue_bridge_password(id, p)?;
        }
        if c.default && default_untouched {
            store.set_meta("default_channel", &id.to_string())?;
        }
    }
    // The default channel always exists.
    let default: Option<u32> = store.meta("default_channel")?.and_then(|d| d.parse().ok());
    if !default.is_some_and(|d| existing.iter().any(|r| r.id == d))
        && let Some(first) = existing.iter().find(|r| r.parent.is_none() && !r.private)
    {
        store.set_meta("default_channel", &first.id.to_string())?;
    }
    let default_now = store.meta("default_channel")?.unwrap_or_default();
    store.set_ts_import_record(source, "default", 0, 0, Some(&default_now))?;

    // --------------------------------------------------------------- members
    for m in &plan.members {
        let roles: BTreeSet<GroupId> = m.roles.iter().filter_map(|r| role_of(*r).ok().flatten()).collect();
        let record = store.ts_import_record(source, "member", &m.uid)?;
        // The roles the last import gave: one taken away in Gwar since is not given again.
        let before: BTreeSet<GroupId> =
            record.as_ref().and_then(|(_, written)| serde_json::from_str(written.as_deref()?).ok()).unwrap_or_default();
        // A TeamSpeak identity linked to a member is that member, whose roles are managed in Gwar.
        if let Some(linked) = store.teamspeak_member(&m.uid)? {
            applied.notes.push(format!(
                "Member \"{}\" is linked to the Gwar member {linked}: roles left as they are.",
                m.nickname
            ));
            continue;
        }
        let id = match store.user_by_uid(&m.uid)? {
            Some((id, member)) => {
                let mut groups: BTreeSet<GroupId> = member.groups.iter().copied().collect();
                groups.extend(roles.difference(&before));
                for gone in before.difference(&roles) {
                    groups.remove(gone);
                }
                store.set_user_groups(id, &groups.into_iter().collect::<Vec<_>>())?;
                applied.members_updated += 1;
                id
            }
            None if record.is_some() => {
                applied.notes.push(format!("Member \"{}\" was removed in Gwar: not added again.", m.nickname));
                continue;
            }
            None => {
                // A TeamSpeak user has no Gwar key; their uid stands in for it.
                let row = store.touch_user(&m.uid, &m.uid, &m.nickname, m.last_seen)?;
                let groups: Vec<GroupId> = std::iter::once(MEMBER_GROUP).chain(roles.iter().copied()).collect();
                store.set_user_groups(row.id, &groups)?;
                applied.members_created += 1;
                row.id
            }
        };
        store.set_ts_import_record(source, "member", &m.uid, id, Some(&json!(roles).to_string()))?;
    }

    // ------------------------------------------------------------------ bans
    let now = now_ms();
    let mut current = store.bans(now)?;
    for planned in &plan.bans {
        // Each TeamSpeak ban once: one lifted in Gwar stays lifted.
        if store.ts_import_record(source, "ban", planned.ts_id)?.is_some() {
            continue;
        }
        let ban = &planned.ban;
        let id = match current.iter().find(|b| b.uid == ban.uid && b.ip == ban.ip) {
            Some(same) => same.id,
            None => {
                let id = store.insert_ban(ban, None)?;
                current.push(vc_proto::Ban { id, ..ban.clone() });
                applied.bans += 1;
                id
            }
        };
        store.set_ts_import_record(source, "ban", planned.ts_id, i64::from(id), None)?;
    }
    Ok(applied)
}
