//! What an import does, worked out from what was read (pure, so it is tested without a server).
//! The mapping is docs/teamspeak-import.md, "Roles and permissions", onto docs/roles.md.

use std::collections::{BTreeMap, BTreeSet};

use vc_proto::{Ban, Permission};

use super::source::{Perms, Source};

/// Choices made on the command line.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Import every client in the database, not only those holding a role.
    pub all_clients: bool,
    /// Keep spacer channels (`[spacer]`, `[cspacer]`, …) as plain channels.
    pub spacers: bool,
    /// Channel passwords to keep, by channel path ("Parent/Child") or name; `server` for the server's.
    pub passwords: BTreeMap<String, String>,
    /// Import password channels without a password instead of locking them.
    pub drop_passwords: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlannedRole {
    pub ts_id: u64,
    pub name: String,
    pub permissions: Vec<Permission>,
    /// Why each permission was given: the TeamSpeak permissions behind it.
    pub reasons: Vec<(Permission, String)>,
}

/// One override in a planned channel: for an imported role (by TeamSpeak id), Member, or a member.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Member,
    Role(u64),
    User(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlannedOverride {
    pub target: Target,
    pub allow: Vec<Permission>,
    pub deny: Vec<Permission>,
}

#[derive(Debug, Clone, PartialEq, Default)]
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

#[derive(Debug, Clone, PartialEq)]
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

#[derive(Debug, Clone, PartialEq)]
pub struct PlannedMember {
    /// `ts:<unique id>`.
    pub uid: String,
    pub nickname: String,
    /// Unix ms.
    pub last_seen: i64,
    pub roles: Vec<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub server_name: Option<String>,
    pub welcome: Option<String>,
    pub max_clients: Option<u32>,
    pub server_password: Password,
    /// The default group's mapping: Member's permissions.
    pub member_permissions: Vec<Permission>,
    /// Imported roles, from the top (TeamSpeak's sort order).
    pub roles: Vec<PlannedRole>,
    /// Parents before children, siblings in order.
    pub channels: Vec<PlannedChannel>,
    pub members: Vec<PlannedMember>,
    pub bans: Vec<Ban>,
    /// What did not move, or moved differently, by name: the report.
    pub notes: Vec<String>,
}

/// Group types: 1 is a regular group (0 template, 2 query).
const REGULAR: &str = "1";

/// The Gwar permissions a server group's own assignments grant, and why. `needed` holds the
/// default group's "needed" powers: a power counts when it reaches the one an ordinary member needs.
pub fn map_permissions(perms: &Perms, needed: &Perms) -> Vec<(Permission, String)> {
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
    give(
        Permission::ManageRoles,
        any(&["b_virtualserver_servergroup_create"])
            .or_else(|| power("i_group_member_add_power", "i_group_needed_member_add_power")),
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

fn is_spacer(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with('[') && lower.split(']').next().is_some_and(|tag| tag.contains("spacer"))
}

/// Works out the import. `source` is what ServerQuery returned.
pub fn plan(source: &Source, options: &Options) -> Plan {
    let mut plan = Plan::default();
    let server = &source.server;
    plan.server_name = server.get("virtualserver_name").filter(|n| !n.is_empty()).cloned();
    plan.welcome = server.get("virtualserver_welcomemessage").cloned();
    plan.max_clients = server.get("virtualserver_maxclients").and_then(|m| m.parse().ok());
    if server.get("virtualserver_flag_password").is_some_and(|f| f == "1") {
        plan.server_password = password_for(options, "server", &mut plan.notes, "the server");
    }

    // --------------------------------------------------------------- roles
    let default_group: u64 = server.get("virtualserver_default_server_group").and_then(|g| g.parse().ok()).unwrap_or(0);
    let empty = Perms::default();
    let default_perms = source.group_perms.get(&default_group).unwrap_or(&empty);
    plan.member_permissions = map_permissions(default_perms, default_perms).into_iter().map(|(p, _)| p).collect();
    // Members of TeamSpeak's default group may join, talk and write unless a channel says otherwise.
    for p in [Permission::Connect, Permission::Speak, Permission::SendMessages] {
        if !plan.member_permissions.contains(&p) {
            plan.member_permissions.push(p);
        }
    }
    let mut groups: Vec<_> = source
        .groups
        .iter()
        .filter(|g| g.get("type").map(String::as_str) == Some(REGULAR))
        .filter_map(|g| Some((g.get("sgid")?.parse::<u64>().ok()?, g)))
        .filter(|(id, _)| *id != default_group)
        .collect();
    // TeamSpeak lists groups by sort id: that is the order roles take, the lowest on top.
    groups.sort_by_key(|(id, g)| (g.get("sortid").and_then(|s| s.parse::<i64>().ok()).unwrap_or(0), *id));
    for (id, g) in &groups {
        let perms = source.group_perms.get(id).unwrap_or(&empty);
        let reasons = map_permissions(perms, default_perms);
        plan.roles.push(PlannedRole {
            ts_id: *id,
            name: g.get("name").cloned().unwrap_or_else(|| format!("Group {id}")),
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
        let name = c.get("channel_name").cloned().unwrap_or_default();
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
    let default_join = default_perms.int("i_channel_join_power");
    let default_talk = default_perms.int("i_client_talk_power");
    let mut queue: Vec<Option<u64>> = vec![None];
    while let Some(parent) = queue.pop() {
        let Some(ids) = children.get(&parent) else { continue };
        for (position, cid) in sorted(ids).into_iter().enumerate() {
            let c = kept[&cid];
            let name = c.get("channel_name").cloned().unwrap_or_default();
            let path = channel_path(&kept, cid);
            let mut overrides = Vec::new();
            // Join power above the default group's: only roles with enough may join.
            let needed_join = source.channel_perms.get(&cid).map_or(0, |p| p.int("i_channel_needed_join_power"));
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
            let needed_talk: i64 = c.get("channel_needed_talk_power").and_then(|t| t.parse().ok()).unwrap_or(0);
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
                    password_for(options, &path, &mut plan.notes, &format!("\"{path}\""))
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
                topic: c.get("channel_topic").cloned().unwrap_or_default(),
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
    // Channel groups: each channel's rights for its members, as member overrides.
    let mut channel_rights: BTreeSet<u64> = BTreeSet::new();
    let default_channel_group: u64 =
        server.get("virtualserver_default_channel_group").and_then(|g| g.parse().ok()).unwrap_or(0);
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
        let allow: Vec<Permission> =
            map_permissions(source.channel_group_perms.get(&cgid).unwrap_or(&empty), default_perms)
                .into_iter()
                .map(|(p, _)| p)
                .filter(|p| p.is_channel())
                .collect();
        if allow.is_empty() {
            continue;
        }
        if let Some(channel) = plan.channels.iter_mut().find(|c| c.ts_id == cid) {
            channel.overrides.push(PlannedOverride { target: Target::User(format!("ts:{uid}")), allow, deny: vec![] });
            channel_rights.insert(cldbid);
        }
    }
    for (&cldbid, c) in &clients {
        let roles = roles_of.remove(&cldbid).unwrap_or_default();
        if roles.is_empty() && !options.all_clients && !channel_rights.contains(&cldbid) {
            continue;
        }
        let Some(uid) = c.get("client_unique_identifier").filter(|u| !u.is_empty() && *u != "ServerQuery") else {
            continue;
        };
        plan.members.push(PlannedMember {
            uid: format!("ts:{uid}"),
            nickname: c.get("client_nickname").cloned().unwrap_or_else(|| uid.clone()),
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
        plan.bans.push(Ban {
            id: 0,
            uid,
            ip,
            nickname: b.get("lastnickname").cloned().unwrap_or_default(),
            reason,
            by: b.get("invokername").cloned().unwrap_or_else(|| "TeamSpeak".into()),
            created_at: created,
            expires_at,
        });
    }
    plan
}

/// "Parent/Child" for passwords and the report.
fn channel_path(kept: &BTreeMap<u64, &BTreeMap<String, String>>, cid: u64) -> String {
    let mut names = Vec::new();
    let mut id = Some(cid);
    while let Some(c) = id.and_then(|c| kept.get(&c)) {
        names.push(c.get("channel_name").cloned().unwrap_or_default());
        id = c.get("pid").and_then(|p| p.parse().ok()).filter(|p| kept.contains_key(p));
        if names.len() > 64 {
            break;
        }
    }
    names.reverse();
    names.join("/")
}

fn password_for(options: &Options, key: &str, notes: &mut Vec<String>, what: &str) -> Password {
    let name = key.rsplit('/').next().unwrap_or(key);
    match options.passwords.get(key).or_else(|| options.passwords.get(name)) {
        Some(p) => Password::Kept(p.clone()),
        None => {
            notes.push(format!(
                "{what}: password not given, imported locked with a random one (set a new one in Gwar)."
            ));
            Password::Locked
        }
    }
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
