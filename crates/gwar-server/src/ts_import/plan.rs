//! What an import does, worked out from what was read (pure, so it is tested without a server).
//! The mapping is docs/teamspeak-import.md, "Roles and permissions", onto docs/roles.md.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use gwar_proto::{Ban, Permission};

use super::source::{Perms, Source};

/// Choices made on the command line.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Import every client in the database, not only those holding a role.
    pub all_clients: bool,
    /// Keep spacer channels (`[spacer]`, `[cspacer]`, …) as plain channels.
    pub spacers: bool,
    /// Passwords to keep, by channel path ("Parent/Child", a `/` in a name written `\/`), by a
    /// name no other kept channel has, or [`SERVER_PASSWORD`] for the server's.
    pub passwords: BTreeMap<String, String>,
    /// Import the server and its channels without a password instead of locking them.
    pub drop_passwords: bool,
}

/// The passwords file's key for the server's password; never a channel's, even one so named.
pub const SERVER_PASSWORD: &str = "@server";

/// Gwar's limits (core): longer TeamSpeak text is cut, control characters go.
const MAX_CHANNEL_NAME: usize = 64;
const MAX_TOPIC: usize = 255;
const MAX_GROUP_NAME: usize = 32;
const MAX_SERVER_NAME: usize = 64;
const MAX_WELCOME: usize = 1000;
const MAX_OVERRIDES: usize = 100;

/// `text` as Gwar takes it: no control characters (but line breaks where `lines`), trimmed, at
/// most `max` characters.
fn fit(text: &str, max: usize, lines: bool) -> String {
    let kept: String = text.chars().filter(|c| !c.is_control() || (lines && *c == '\n')).collect();
    kept.trim().chars().take(max).collect::<String>().trim_end().to_owned()
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlannedRole {
    pub ts_id: u64,
    pub name: String,
    pub permissions: Vec<Permission>,
    /// Why each permission was given: the TeamSpeak permissions behind it.
    pub reasons: Vec<(Permission, String)>,
}

/// One override in a planned channel: for an imported role (by TeamSpeak id), Member, or a member.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Target {
    Member,
    Role(u64),
    User(String),
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlannedOverride {
    pub target: Target,
    pub allow: Vec<Permission>,
    pub deny: Vec<Permission>,
}

#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub enum Password {
    #[default]
    None,
    /// The password typed for it: kept as it was.
    Kept(String),
    /// No password given: imported with a random one, so it stays closed.
    Locked,
    /// `--drop-passwords`.
    Dropped,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlannedChannel {
    pub ts_id: u64,
    pub parent: Option<u64>,
    pub name: String,
    pub topic: String,
    /// Order among its siblings.
    pub position: i32,
    pub max_clients: Option<u32>,
    pub password: Password,
    pub default: bool,
    pub overrides: Vec<PlannedOverride>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlannedBan {
    /// TeamSpeak's ban id: each is imported once, so one lifted in Gwar stays lifted.
    pub ts_id: u64,
    pub ban: Ban,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlannedMember {
    /// `ts:<unique id>`.
    pub uid: String,
    pub nickname: String,
    /// Unix ms.
    pub last_seen: i64,
    pub roles: Vec<u64>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Plan {
    pub server_name: Option<String>,
    pub welcome: Option<String>,
    pub max_clients: Option<u32>,
    pub server_password: Password,
    /// The default group's mapping: Member's permissions.
    pub member_permissions: Vec<Permission>,
    /// Imported roles, from the top (by their TeamSpeak powers, see [`rank_power`]).
    pub roles: Vec<PlannedRole>,
    /// Parents before children, siblings in order.
    pub channels: Vec<PlannedChannel>,
    pub members: Vec<PlannedMember>,
    pub bans: Vec<PlannedBan>,
    /// What did not move, or moved differently, by name: the report.
    pub notes: Vec<String>,
}

/// Group types: 1 is a regular group (0 template, 2 query).
const REGULAR: &str = "1";

/// The Gwar permissions a server group's own assignments grant, and why. `needed` holds the
/// default group's "needed" powers: a power counts when it reaches the one an ordinary member needs.
/// `add_needed`: the highest power any imported group needs to add members to it; adding to every
/// group comes close to `manage_roles` (giving every role below), adding to some does not.
pub fn map_permissions(perms: &Perms, needed: &Perms, add_needed: i64) -> Vec<(Permission, String)> {
    let granted = |name: &str| perms.bool(name);
    let power = |name: &str, needed_name: &str| {
        let have = perms.int(name);
        (have > 0 && have >= needed.int(needed_name)).then(|| format!("{name} {have}"))
    };
    let any = |names: &[&str]| names.iter().find(|n| granted(n)).map(|n| (*n).to_owned());
    let mut out = Vec::new();
    let mut give = |p: Permission, why: Option<String>| {
        if let Some(why) = why {
            out.push((p, why));
        }
    };
    give(
        Permission::ManageServer,
        any(&[
            "b_virtualserver_modify_name",
            "b_virtualserver_modify_welcomemessage",
            "b_virtualserver_modify_password",
        ]),
    );
    let add = perms.int("i_group_member_add_power");
    give(
        Permission::ManageRoles,
        any(&["b_virtualserver_servergroup_create"]).or_else(|| {
            (add > 0 && add >= add_needed).then(|| format!("i_group_member_add_power {add} (every group)"))
        }),
    );
    give(Permission::ManageMembers, any(&["b_client_modify_dbproperties", "b_client_delete_dbproperties"]));
    give(Permission::KickMembers, power("i_client_kick_from_server_power", "i_client_needed_kick_from_server_power"));
    give(Permission::BanMembers, any(&["b_client_ban_create"]));
    // TeamSpeak keeps no chat to moderate: deleting others' messages goes with banning.
    give(
        Permission::ManageMessages,
        any(&["b_client_ban_create"]).map(|why| format!("{why} (no TeamSpeak counterpart)")),
    );
    give(Permission::CreateInvites, any(&["b_virtualserver_token_add"]));
    give(Permission::BypassPassword, any(&["b_channel_join_ignore_password"]));
    give(Permission::AttachFiles, power("i_ft_file_upload_power", "i_ft_needed_file_upload_power"));
    give(Permission::MoveMembers, power("i_client_move_power", "i_client_needed_move_power"));
    // Muting someone for everyone is closest to keeping them from talking in a channel.
    give(Permission::MuteMembers, power("i_client_kick_from_channel_power", "i_client_needed_kick_from_channel_power"));
    give(Permission::ManageChannels, any(&["b_channel_create_permanent", "b_channel_modify_name"]));
    out
}

/// How far a group reaches over others on TeamSpeak, which decides who may act on whom there (its
/// sort id only orders the list): the rank its role takes.
pub fn rank_power(perms: &Perms) -> i64 {
    [
        "i_client_kick_from_server_power",
        "i_client_ban_power",
        "i_group_member_add_power",
        "i_group_member_remove_power",
        "i_group_modify_power",
        "i_client_move_power",
        "i_client_kick_from_channel_power",
    ]
    .iter()
    .map(|name| perms.int(name))
    .max()
    .unwrap_or(0)
}

fn is_spacer(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with('[') && lower.split(']').next().is_some_and(|tag| tag.contains("spacer"))
}

/// Works out the import. `source` is what ServerQuery returned. Fails on a passwords file that
/// does not say clearly which channel each password is for.
pub fn plan(source: &Source, options: &Options) -> Result<Plan> {
    let mut plan = Plan::default();
    let server = &source.server;
    plan.server_name =
        server.get("virtualserver_name").map(|n| fit(n, MAX_SERVER_NAME, false)).filter(|n| !n.is_empty());
    plan.welcome = server.get("virtualserver_welcomemessage").map(|w| fit(w, MAX_WELCOME, true));
    plan.max_clients = server.get("virtualserver_maxclients").and_then(|m| m.parse().ok());
    let mut used_passwords: BTreeSet<&str> = BTreeSet::new();
    if server.get("virtualserver_flag_password").is_some_and(|f| f == "1") {
        plan.server_password = if options.drop_passwords {
            plan.notes.push("The server: password dropped (--drop-passwords).".into());
            Password::Dropped
        } else {
            match options.passwords.get_key_value(SERVER_PASSWORD) {
                Some((key, p)) => {
                    used_passwords.insert(key);
                    Password::Kept(p.clone())
                }
                None => {
                    plan.notes.push(locked_note("The server"));
                    Password::Locked
                }
            }
        };
    }

    // --------------------------------------------------------------- roles
    let default_group: u64 = server.get("virtualserver_default_server_group").and_then(|g| g.parse().ok()).unwrap_or(0);
    let empty = Perms::default();
    let default_perms = source.group_perms.get(&default_group).unwrap_or(&empty);
    let regular: Vec<(u64, &BTreeMap<String, String>)> = source
        .groups
        .iter()
        .filter(|g| g.get("type").map(String::as_str) == Some(REGULAR))
        .filter_map(|g| Some((g.get("sgid")?.parse::<u64>().ok()?, g)))
        .filter(|(id, _)| *id != default_group)
        .collect();
    let add_needed = regular
        .iter()
        .map(|(id, _)| source.group_perms.get(id).map_or(0, |p| p.int("i_group_needed_member_add_power")))
        .max()
        .unwrap_or(0);
    plan.member_permissions =
        map_permissions(default_perms, default_perms, add_needed).into_iter().map(|(p, _)| p).collect();
    // Members of TeamSpeak's default group may join, talk and write unless a channel says otherwise.
    for p in [Permission::Connect, Permission::Speak, Permission::SendMessages] {
        if !plan.member_permissions.contains(&p) {
            plan.member_permissions.push(p);
        }
    }
    // Ranked by what each group may do to others (the sort id breaks ties): a display order
    // that puts moderators above admins must not let them act on the admins.
    let sort_id = |g: &BTreeMap<String, String>| g.get("sortid").and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    let power_of = |id: &u64| source.group_perms.get(id).map_or(0, rank_power);
    let mut groups = regular.clone();
    groups.sort_by_key(|(id, g)| (std::cmp::Reverse(power_of(id)), sort_id(g), *id));
    let mut displayed = regular;
    displayed.sort_by_key(|(id, g)| (sort_id(g), *id));
    if groups.iter().map(|(id, _)| id).ne(displayed.iter().map(|(id, _)| id)) {
        plan.notes.push(
            "Roles are ranked by their TeamSpeak powers (kick, ban, move, group powers), not TeamSpeak's display order."
                .into(),
        );
    }
    for (id, g) in &groups {
        let perms = source.group_perms.get(id).unwrap_or(&empty);
        let reasons = map_permissions(perms, default_perms, add_needed);
        let name = g.get("name").map(|n| fit(n, MAX_GROUP_NAME, false)).filter(|n| !n.is_empty());
        let name = name.unwrap_or_else(|| format!("Group {id}"));
        let add = perms.int("i_group_member_add_power");
        if add > 0 && !reasons.iter().any(|(p, _)| *p == Permission::ManageRoles) {
            plan.notes.push(format!(
                "\"{name}\": adds members to some groups on TeamSpeak (power {add}); not manage_roles, which gives every role below."
            ));
        }
        if reasons.is_empty() && !perms.0.is_empty() {
            plan.notes.push(format!(
                "\"{name}\": nothing it grants maps to Gwar; TeamSpeak restrictions (negated or low powers) are not imported."
            ));
        }
        plan.roles.push(PlannedRole {
            ts_id: *id,
            name,
            permissions: reasons.iter().map(|(p, _)| *p).collect(),
            reasons,
        });
    }
    let skipped: Vec<_> = source
        .groups
        .iter()
        .filter(|g| g.get("type").map(String::as_str) != Some(REGULAR))
        .filter_map(|g| g.get("name").cloned())
        .collect();
    if !skipped.is_empty() {
        plan.notes.push(format!("Template and query groups not imported: {}.", skipped.join(", ")));
    }
    let imported: BTreeSet<u64> = plan.roles.iter().map(|r| r.ts_id).collect();

    // ------------------------------------------------------------ channels
    let by_id: BTreeMap<u64, &BTreeMap<String, String>> =
        source.channels.iter().filter_map(|c| Some((c.get("cid")?.parse().ok()?, c))).collect();
    let mut kept: BTreeMap<u64, &BTreeMap<String, String>> = BTreeMap::new();
    for (&cid, c) in &by_id {
        let name = fit(c.get("channel_name").map_or("", String::as_str), MAX_CHANNEL_NAME, false);
        let permanent = c.get("channel_flag_permanent").is_some_and(|f| f == "1")
            || c.get("channel_flag_semi_permanent").is_some_and(|f| f == "1")
            || c.get("channel_flag_default").is_some_and(|f| f == "1");
        if !permanent {
            plan.notes.push(format!("Temporary channel \"{name}\" not imported."));
            continue;
        }
        if is_spacer(&name) && !options.spacers {
            plan.notes.push(format!("Spacer \"{name}\" not imported (--spacers keeps spacers)."));
            continue;
        }
        kept.insert(cid, c);
    }
    // A channel whose parent was not imported goes to the top.
    let parent_of = |c: &BTreeMap<String, String>| {
        c.get("pid").and_then(|p| p.parse::<u64>().ok()).filter(|p| kept.contains_key(p))
    };
    // channel_order is the id of the sibling above (0: first).
    let mut children: BTreeMap<Option<u64>, Vec<u64>> = BTreeMap::new();
    for (&cid, c) in &kept {
        children.entry(parent_of(c)).or_default().push(cid);
    }
    let sorted = |ids: &[u64]| -> Vec<u64> {
        let above: BTreeMap<u64, u64> = ids
            .iter()
            .map(|id| (*id, kept[id].get("channel_order").and_then(|o| o.parse().ok()).unwrap_or(0)))
            .collect();
        let mut out = Vec::new();
        let mut previous = 0;
        let mut left: BTreeSet<u64> = ids.iter().copied().collect();
        while !left.is_empty() {
            let next =
                left.iter().copied().find(|id| above[id] == previous).unwrap_or_else(|| *left.iter().next().unwrap());
            left.remove(&next);
            out.push(next);
            previous = next;
        }
        out
    };
    // What anyone has by default: the default server group's power or the default channel group's.
    let default_channel_group: u64 =
        server.get("virtualserver_default_channel_group").and_then(|g| g.parse().ok()).unwrap_or(0);
    let default_channel_perms = source.channel_group_perms.get(&default_channel_group).unwrap_or(&empty);
    let default_join = default_perms.int("i_channel_join_power").max(default_channel_perms.int("i_channel_join_power"));
    let default_talk = default_perms.int("i_client_talk_power").max(default_channel_perms.int("i_client_talk_power"));
    // Per channel, the join and talk power it needs, where above the default.
    let mut needs: BTreeMap<u64, (i64, i64)> = BTreeMap::new();
    // Each password key matches either one channel path or one name unique among kept channels.
    let paths: BTreeMap<u64, String> = kept.keys().map(|cid| (*cid, channel_path(&kept, *cid))).collect();
    let mut name_count: BTreeMap<String, usize> = BTreeMap::new();
    for cid in kept.keys() {
        *name_count.entry(channel_name(&kept, *cid)).or_default() += 1;
    }
    for key in options.passwords.keys().filter(|k| k.as_str() != SERVER_PASSWORD) {
        if !paths.values().any(|p| p == key) && name_count.get(key).is_some_and(|n| *n > 1) {
            bail!("the passwords file names \"{key}\", which several channels are called: write the channel's path");
        }
    }
    let mut queue: Vec<Option<u64>> = vec![None];
    while let Some(parent) = queue.pop() {
        let Some(ids) = children.get(&parent) else { continue };
        for (position, cid) in sorted(ids).into_iter().enumerate() {
            let c = kept[&cid];
            let name = channel_name(&kept, cid);
            let path = paths[&cid].clone();
            let mut overrides = Vec::new();
            // Join power above the default group's: only roles with enough may join.
            let needed_join = source.channel_perms.get(&cid).map_or(0, |p| p.int("i_channel_needed_join_power"));
            let needed_talk: i64 = c.get("channel_needed_talk_power").and_then(|t| t.parse().ok()).unwrap_or(0);
            needs.insert(cid, (needed_join, needed_talk));
            if needed_join > default_join {
                overrides.push(PlannedOverride {
                    target: Target::Member,
                    allow: vec![],
                    deny: vec![Permission::Connect],
                });
                for role in &plan.roles {
                    let power = source.group_perms.get(&role.ts_id).map_or(0, |p| p.int("i_channel_join_power"));
                    if power >= needed_join {
                        overrides.push(PlannedOverride {
                            target: Target::Role(role.ts_id),
                            allow: vec![Permission::Connect],
                            deny: vec![],
                        });
                    }
                }
                plan.notes.push(format!("\"{path}\": join power {needed_join} became who may join (connect)."));
            }
            // A moderated channel: only roles with enough talk power speak.
            if needed_talk > default_talk {
                overrides.push(PlannedOverride {
                    target: Target::Member,
                    allow: vec![],
                    deny: vec![Permission::Speak],
                });
                for role in &plan.roles {
                    let power = source.group_perms.get(&role.ts_id).map_or(0, |p| p.int("i_client_talk_power"));
                    if power >= needed_talk {
                        overrides.push(PlannedOverride {
                            target: Target::Role(role.ts_id),
                            allow: vec![Permission::Speak],
                            deny: vec![],
                        });
                    }
                }
                plan.notes.push(format!("\"{path}\": talk power {needed_talk} became who may speak."));
            }
            let overrides = merge_overrides(overrides);
            let password = if c.get("channel_flag_password").is_some_and(|f| f == "1") {
                if options.drop_passwords {
                    plan.notes.push(format!("\"{path}\": password dropped (--drop-passwords)."));
                    Password::Dropped
                } else {
                    let key = if path == SERVER_PASSWORD {
                        None
                    } else if options.passwords.contains_key(&path) {
                        Some(path.as_str())
                    } else {
                        Some(name.as_str()).filter(|n| {
                            *n != SERVER_PASSWORD && name_count[*n] == 1 && options.passwords.contains_key(*n)
                        })
                    };
                    match key.and_then(|k| options.passwords.get_key_value(k)) {
                        Some((key, p)) => {
                            used_passwords.insert(key);
                            Password::Kept(p.clone())
                        }
                        None => {
                            plan.notes.push(locked_note(&format!("\"{path}\"")));
                            Password::Locked
                        }
                    }
                }
            } else {
                Password::None
            };
            if c.get("channel_description").is_some_and(|d| !d.is_empty()) {
                plan.notes.push(format!("\"{path}\": description not imported (Gwar has a topic only)."));
            }
            plan.channels.push(PlannedChannel {
                ts_id: cid,
                parent,
                name,
                topic: fit(c.get("channel_topic").map_or("", String::as_str), MAX_TOPIC, false),
                position: position as i32,
                max_clients: c
                    .get("channel_maxclients")
                    .and_then(|m| m.parse::<i64>().ok())
                    .filter(|m| *m >= 0 && c.get("channel_flag_maxclients_unlimited").is_none_or(|f| f != "1"))
                    .and_then(|m| u32::try_from(m).ok()),
                password,
                default: c.get("channel_flag_default").is_some_and(|f| f == "1"),
                overrides,
            });
            queue.push(Some(cid));
        }
    }
    // Parents before children for creation.
    plan.channels = parents_first(std::mem::take(&mut plan.channels));

    // ------------------------------------------------------------- members
    let clients: BTreeMap<u64, &BTreeMap<String, String>> =
        source.clients.iter().filter_map(|c| Some((c.get("cldbid")?.parse().ok()?, c))).collect();
    let mut roles_of: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    for (sgid, cldbids) in &source.group_members {
        if imported.contains(sgid) {
            for cldbid in cldbids {
                roles_of.entry(*cldbid).or_default().push(*sgid);
            }
        }
    }
    if let Some(unused) = options.passwords.keys().find(|k| !used_passwords.contains(k.as_str())) {
        bail!("the passwords file names \"{unused}\", which is no imported password channel (nor {SERVER_PASSWORD})");
    }
    // Channel groups: each channel's rights for its members, as member overrides.
    let mut channel_rights: BTreeSet<u64> = BTreeSet::new();
    for entry in &source.channel_group_members {
        let (Some(cid), Some(cldbid), Some(cgid)) = (
            entry.get("cid").and_then(|v| v.parse::<u64>().ok()),
            entry.get("cldbid").and_then(|v| v.parse::<u64>().ok()),
            entry.get("cgid").and_then(|v| v.parse::<u64>().ok()),
        ) else {
            continue;
        };
        if cgid == default_channel_group {
            continue;
        }
        let Some(uid) = clients.get(&cldbid).and_then(|c| c.get("client_unique_identifier")) else { continue };
        let group_perms = source.channel_group_perms.get(&cgid).unwrap_or(&empty);
        let mut allow: Vec<Permission> = map_permissions(group_perms, default_perms, add_needed)
            .into_iter()
            .map(|(p, _)| p)
            .filter(|p| p.is_channel())
            .collect();
        // A channel group's own join and talk power (a channel operator who may talk in a moderated room).
        let (needed_join, needed_talk) = needs.get(&cid).copied().unwrap_or_default();
        if needed_join > default_join && group_perms.int("i_channel_join_power") >= needed_join {
            allow.push(Permission::Connect);
        }
        if needed_talk > default_talk && group_perms.int("i_client_talk_power") >= needed_talk {
            allow.push(Permission::Speak);
        }
        if allow.is_empty() {
            continue;
        }
        if let Some(channel) = plan.channels.iter_mut().find(|c| c.ts_id == cid) {
            if channel.overrides.len() >= MAX_OVERRIDES {
                plan.notes
                    .push(format!("\"{}\": more than {MAX_OVERRIDES} overrides, the rest not imported.", channel.name));
                continue;
            }
            channel.overrides.push(PlannedOverride { target: Target::User(format!("ts:{uid}")), allow, deny: vec![] });
            channel.overrides = merge_overrides(std::mem::take(&mut channel.overrides));
            channel_rights.insert(cldbid);
        }
    }
    for (&cldbid, c) in &clients {
        let roles = roles_of.remove(&cldbid).unwrap_or_default();
        if roles.is_empty() && !options.all_clients && !channel_rights.contains(&cldbid) {
            continue;
        }
        // Query logins are no people.
        let Some(uid) = c
            .get("client_unique_identifier")
            .filter(|u| !u.is_empty() && !matches!(u.as_str(), "ServerQuery" | "serveradmin"))
        else {
            continue;
        };
        plan.members.push(PlannedMember {
            uid: format!("ts:{uid}"),
            nickname: c
                .get("client_nickname")
                .map(|n| fit(n, MAX_CHANNEL_NAME, false))
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| uid.clone()),
            last_seen: c.get("client_lastconnected").and_then(|t| t.parse::<i64>().ok()).unwrap_or(0) * 1000,
            roles,
        });
    }
    let skipped_clients = clients.len() - plan.members.len();
    if skipped_clients > 0 && !options.all_clients {
        plan.notes.push(format!("{skipped_clients} clients without a role not imported (--all-clients imports them)."));
    }

    // ---------------------------------------------------------------- bans
    let now = crate::core::now_ms();
    for b in &source.bans {
        let created = b.get("created").and_then(|t| t.parse::<i64>().ok()).unwrap_or(0) * 1000;
        let duration = b.get("duration").and_then(|t| t.parse::<i64>().ok()).unwrap_or(0);
        let expires_at = (duration > 0).then(|| created + duration * 1000);
        if expires_at.is_some_and(|t| t <= now) {
            continue;
        }
        let uid = b.get("uid").filter(|u| !u.is_empty()).map(|u| format!("ts:{u}"));
        let ip = b.get("ip").filter(|ip| ip.parse::<std::net::IpAddr>().is_ok()).cloned();
        let reason = b.get("reason").filter(|r| !r.is_empty()).cloned();
        if uid.is_none() && ip.is_none() {
            let what = b.get("name").or(b.get("ip")).or(b.get("mytsid")).cloned().unwrap_or_default();
            plan.notes.push(format!("Ban by name, pattern or myTeamSpeak id \"{what}\" not imported."));
            continue;
        }
        let Some(ts_id) = b.get("banid").and_then(|i| i.parse().ok()) else { continue };
        plan.bans.push(PlannedBan {
            ts_id,
            ban: Ban {
                id: 0,
                uid,
                ip,
                nickname: fit(b.get("lastnickname").map_or("", String::as_str), MAX_CHANNEL_NAME, false),
                reason: reason.map(|r| fit(&r, 200, false)),
                by: b.get("invokername").map(|n| fit(n, MAX_CHANNEL_NAME, false)).unwrap_or_else(|| "TeamSpeak".into()),
                created_at: created,
                expires_at,
            },
        });
    }
    Ok(plan)
}

fn locked_note(what: &str) -> String {
    format!("{what}: password not given, imported locked with a random one (set a new one in Gwar).")
}

fn channel_name(kept: &BTreeMap<u64, &BTreeMap<String, String>>, cid: u64) -> String {
    let name = fit(kept[&cid].get("channel_name").map_or("", String::as_str), MAX_CHANNEL_NAME, false);
    if name.is_empty() { format!("Channel {cid}") } else { name }
}

/// "Parent/Child" for passwords and the report; a `/` or `\` in a name is written `\/`, `\\`.
fn channel_path(kept: &BTreeMap<u64, &BTreeMap<String, String>>, cid: u64) -> String {
    let mut names = Vec::new();
    let mut id = Some(cid);
    while let Some(at) = id {
        names.push(channel_name(kept, at).replace('\\', "\\\\").replace('/', "\\/"));
        id = kept[&at].get("pid").and_then(|p| p.parse().ok()).filter(|p| kept.contains_key(p));
        if names.len() > 64 {
            break;
        }
    }
    names.reverse();
    names.join("/")
}

/// One override per target (Member's first), deny and allow merged.
fn merge_overrides(list: Vec<PlannedOverride>) -> Vec<PlannedOverride> {
    let mut out: Vec<PlannedOverride> = Vec::new();
    for o in list {
        match out.iter_mut().find(|x| x.target == o.target) {
            Some(x) => {
                x.allow.extend(o.allow);
                x.deny.extend(o.deny);
            }
            None => out.push(o),
        }
    }
    for o in &mut out {
        o.allow.sort_unstable();
        o.allow.dedup();
        o.deny.sort_unstable();
        o.deny.dedup();
    }
    out
}

fn parents_first(channels: Vec<PlannedChannel>) -> Vec<PlannedChannel> {
    let mut out: Vec<PlannedChannel> = Vec::with_capacity(channels.len());
    let mut left = channels;
    while !left.is_empty() {
        let before = left.len();
        let (ready, rest): (Vec<_>, Vec<_>) =
            left.into_iter().partition(|c| c.parent.is_none_or(|p| out.iter().any(|o| o.ts_id == p)));
        out.extend(ready);
        left = rest;
        if left.len() == before {
            // A cycle cannot come from TeamSpeak; whatever is left goes to the top.
            out.extend(left.into_iter().map(|c| PlannedChannel { parent: None, ..c }));
            break;
        }
    }
    out
}

/// The dry run's and the final report.
pub fn report(plan: &Plan) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "Server: {}", plan.server_name.as_deref().unwrap_or("(unchanged)"));
    let _ = writeln!(out, "\nMember (TeamSpeak's default group): {}", names(&plan.member_permissions));
    let _ = writeln!(out, "\nRoles, from the top ({}):", plan.roles.len());
    for r in &plan.roles {
        let _ = writeln!(
            out,
            "  {} — {}",
            r.name,
            if r.permissions.is_empty() { "no permissions".into() } else { names(&r.permissions) }
        );
        for (p, why) in &r.reasons {
            let _ = writeln!(out, "      {}: {why}", name(*p));
        }
    }
    let _ = writeln!(out, "\nChannels ({}):", plan.channels.len());
    for c in &plan.channels {
        let depth = depth_of(plan, c);
        let lock = match &c.password {
            Password::None => "",
            Password::Kept(_) => " [password kept]",
            Password::Locked => " [password: LOCKED, set a new one]",
            Password::Dropped => " [password dropped]",
        };
        let rules =
            if c.overrides.is_empty() { String::new() } else { format!(" [{} override(s)]", c.overrides.len()) };
        let _ = writeln!(
            out,
            "  {}{}{}{}{}",
            "  ".repeat(depth),
            c.name,
            if c.default { " (default)" } else { "" },
            lock,
            rules
        );
    }
    let _ = writeln!(out, "\nMembers: {}   Bans: {}", plan.members.len(), plan.bans.len());
    if !plan.notes.is_empty() {
        let _ = writeln!(out, "\nNot imported or changed:");
        for n in &plan.notes {
            let _ = writeln!(out, "  - {n}");
        }
    }
    out
}

fn depth_of(plan: &Plan, c: &PlannedChannel) -> usize {
    let mut depth = 0;
    let mut parent = c.parent;
    while let Some(p) = parent {
        depth += 1;
        parent = plan.channels.iter().find(|x| x.ts_id == p).and_then(|x| x.parent);
        if depth > 64 {
            break;
        }
    }
    depth
}

fn name(p: Permission) -> String {
    serde_json::to_value(p).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
}

fn names(ps: &[Permission]) -> String {
    ps.iter().map(|p| name(*p)).collect::<Vec<_>>().join(", ")
}
