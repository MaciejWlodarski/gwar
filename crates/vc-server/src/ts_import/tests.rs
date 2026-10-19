use std::collections::BTreeMap;

use vc_proto::Permission;

use super::{
    apply::{ApplyOptions, apply},
    parse_passwords,
    plan::{Options, Password, Plan, Target, map_permissions},
    source::{Perms, Source},
};
use crate::{
    store::{ADMIN_GROUP, MEMBER_GROUP, Store},
    teamspeak::query::Record,
};

fn r(pairs: &[(&str, &str)]) -> Record {
    pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
}

fn plan(source: &Source, options: &Options) -> Plan {
    super::plan::plan(source, options).unwrap()
}

const BRIDGE: ApplyOptions = ApplyOptions { replace_channels: false, force: false, for_bridge: true };

fn perms(pairs: &[(&str, i64)]) -> Perms {
    Perms(pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v, false))).collect())
}

/// A small TeamSpeak server: Guest (8, default), Server Admin (6), Moderator (9), a template group;
/// Lobby (default), Clan with a members-only Room under it, a moderated Stage, a spacer, a temporary
/// channel, and a password channel.
fn source() -> Source {
    let mut s = Source {
        server: r(&[
            ("virtualserver_name", "Old TS"),
            ("virtualserver_welcomemessage", "Hi"),
            ("virtualserver_maxclients", "32"),
            ("virtualserver_default_server_group", "8"),
            ("virtualserver_default_channel_group", "8"),
        ]),
        ..Default::default()
    };
    let ch = |cid: &str, pid: &str, order: &str, name: &str, extra: &[(&str, &str)]| {
        let mut c = r(&[
            ("cid", cid),
            ("pid", pid),
            ("channel_order", order),
            ("channel_name", name),
            ("channel_topic", ""),
            ("channel_flag_permanent", "1"),
            ("channel_maxclients", "-1"),
        ]);
        for (k, v) in extra {
            c.insert((*k).into(), (*v).into());
        }
        c
    };
    s.channels = vec![
        ch("1", "0", "0", "Lobby", &[("channel_flag_default", "1"), ("channel_topic", "Welcome")]),
        ch("2", "0", "1", "Clan", &[("channel_description", "Our clan")]),
        ch("3", "2", "0", "Room", &[]),
        ch("4", "0", "2", "Stage", &[("channel_needed_talk_power", "50")]),
        ch("5", "0", "4", "[cspacer]----", &[]),
        ch("6", "0", "5", "Temp", &[("channel_flag_permanent", "0")]),
        ch("7", "0", "6", "Secret", &[("channel_flag_password", "1"), ("channel_maxclients", "5")]),
    ];
    s.channel_perms.insert(3, perms(&[("i_channel_needed_join_power", 40)]));
    s.groups = vec![
        r(&[("sgid", "6"), ("name", "Server Admin"), ("type", "1"), ("sortid", "0")]),
        r(&[("sgid", "8"), ("name", "Guest"), ("type", "1"), ("sortid", "0")]),
        r(&[("sgid", "9"), ("name", "Moderator"), ("type", "1"), ("sortid", "5")]),
        r(&[("sgid", "1"), ("name", "Guest Server Query"), ("type", "2"), ("sortid", "0")]),
    ];
    s.group_perms.insert(
        8,
        perms(&[
            ("i_channel_join_power", 1),
            ("i_client_talk_power", 0),
            ("i_client_needed_kick_from_server_power", 25),
            ("i_client_needed_move_power", 25),
            ("i_ft_file_upload_power", 0),
        ]),
    );
    s.group_perms.insert(
        6,
        perms(&[
            ("b_virtualserver_modify_name", 1),
            ("b_virtualserver_servergroup_create", 1),
            ("b_client_ban_create", 1),
            ("b_virtualserver_token_add", 1),
            ("b_channel_create_permanent", 1),
            ("b_channel_join_ignore_password", 1),
            ("i_client_kick_from_server_power", 75),
            ("i_client_move_power", 75),
            ("i_channel_join_power", 75),
            ("i_client_talk_power", 75),
        ]),
    );
    s.group_perms.insert(
        9,
        perms(&[
            ("i_client_kick_from_server_power", 25),
            ("i_client_move_power", 10),
            ("i_channel_join_power", 50),
            ("i_client_talk_power", 50),
        ]),
    );
    s.group_members.insert(6, vec![100]);
    s.group_members.insert(9, vec![101]);
    s.group_members.insert(8, vec![100, 101, 102, 103]);
    s.channel_group_perms.insert(5, perms(&[("b_channel_modify_name", 1), ("i_client_move_power", 30)]));
    s.channel_group_members = vec![
        r(&[("cid", "2"), ("cldbid", "102"), ("cgid", "5")]),
        r(&[("cid", "1"), ("cldbid", "103"), ("cgid", "8")]),
    ];
    let client = |id: &str, uid: &str, nick: &str| {
        r(&[
            ("cldbid", id),
            ("client_unique_identifier", uid),
            ("client_nickname", nick),
            ("client_lastconnected", "1700000000"),
        ])
    };
    s.clients = vec![
        client("100", "AdminUid=", "Boss"),
        client("101", "ModUid=", "Mod"),
        client("102", "ClanAdminUid=", "Leader"),
        client("103", "GuestUid=", "Visitor"),
        client("104", "serveradmin", "serveradmin"),
    ];
    s.bans = vec![
        r(&[
            ("banid", "1"),
            ("uid", "BadUid="),
            ("lastnickname", "Troll"),
            ("created", "1700000000"),
            ("duration", "0"),
            ("reason", "spam"),
        ]),
        r(&[("banid", "2"), ("ip", "203.0.113.9"), ("created", "1700000000"), ("duration", "0")]),
        r(&[("banid", "3"), ("name", "troll.*"), ("created", "1700000000"), ("duration", "0")]),
        r(&[("banid", "4"), ("uid", "OldUid="), ("created", "1000"), ("duration", "60")]),
    ];
    s
}

#[test]
fn groups_map_by_their_own_assignments_and_the_default_groups_needed_powers() {
    let s = source();
    let admin = map_permissions(&s.group_perms[&6], &s.group_perms[&8], 0);
    let given: Vec<_> = admin.iter().map(|(p, _)| *p).collect();
    for p in [
        Permission::ManageServer,
        Permission::ManageRoles,
        Permission::BanMembers,
        Permission::KickMembers,
        Permission::CreateInvites,
    ] {
        assert!(given.contains(&p), "{p:?}");
    }
    assert!(admin.iter().any(|(p, why)| *p == Permission::KickMembers && why.contains("75")));
    // The moderator's move power (10) is below what moving an ordinary member needs (25).
    let moderator: Vec<_> =
        map_permissions(&s.group_perms[&9], &s.group_perms[&8], 0).into_iter().map(|(p, _)| p).collect();
    assert_eq!(moderator, [Permission::KickMembers]);
    // Negated assignments do not count.
    let negated = Perms([("b_client_ban_create".to_owned(), (1, true))].into_iter().collect());
    assert!(map_permissions(&negated, &Perms::default(), 0).is_empty());
}

#[test]
fn the_plan_follows_the_tree_and_reports_what_does_not_move() {
    let mut options = Options::default();
    options.passwords.insert("Secret".into(), "hunter2".into());
    let plan = plan(&source(), &options);
    assert_eq!(plan.server_name.as_deref(), Some("Old TS"));
    // Roles by sort id, query and template groups skipped, the default group is Member.
    assert_eq!(plan.roles.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["Server Admin", "Moderator"]);
    assert!(plan.member_permissions.contains(&Permission::Connect));
    // Channels: parents first, siblings in TeamSpeak's order, no spacer and no temporary channel.
    let names: Vec<_> = plan.channels.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["Lobby", "Clan", "Stage", "Secret", "Room"]);
    let room = plan.channels.iter().find(|c| c.name == "Room").unwrap();
    assert_eq!(room.parent, Some(2));
    // Join power 40: Member may not join; Server Admin (75) and Moderator (50) may.
    assert!(room.overrides.iter().any(|o| o.target == Target::Member && o.deny == [Permission::Connect]));
    assert!(room.overrides.iter().any(|o| o.target == Target::Role(6) && o.allow == [Permission::Connect]));
    assert!(room.overrides.iter().any(|o| o.target == Target::Role(9) && o.allow == [Permission::Connect]));
    // Talk power 50: only those with enough speak.
    let stage = plan.channels.iter().find(|c| c.name == "Stage").unwrap();
    assert!(stage.overrides.iter().any(|o| o.target == Target::Member && o.deny == [Permission::Speak]));
    assert!(stage.overrides.iter().any(|o| o.target == Target::Role(9) && o.allow == [Permission::Speak]));
    // A channel admin in Clan gets that channel's rights by name.
    let clan = plan.channels.iter().find(|c| c.name == "Clan").unwrap();
    assert!(
        clan.overrides
            .iter()
            .any(|o| o.target == Target::User("ts:ClanAdminUid=".into())
                && o.allow.contains(&Permission::ManageChannels))
    );
    let secret = plan.channels.iter().find(|c| c.name == "Secret").unwrap();
    assert_eq!((secret.password.clone(), secret.max_clients), (Password::Kept("hunter2".into()), Some(5)));
    assert!(plan.channels.iter().find(|c| c.name == "Lobby").unwrap().default);
    // Members: those with a role or channel rights, never ServerQuery.
    let mut uids: Vec<_> = plan.members.iter().map(|m| m.uid.as_str()).collect();
    uids.sort();
    assert_eq!(uids, ["ts:AdminUid=", "ts:ClanAdminUid=", "ts:ModUid="]);
    // Bans: uid and plain IP; a pattern and an expired one are not imported.
    assert_eq!(plan.bans.len(), 2);
    for expected in ["Spacer", "Temporary", "description", "pattern", "without a role", "query groups"] {
        assert!(plan.notes.iter().any(|n| n.contains(expected)), "no note about {expected}: {:?}", plan.notes);
    }
}

#[test]
fn a_password_not_given_locks_the_channel_and_says_so() {
    let locked = plan(&source(), &Options::default());
    let secret = locked.channels.iter().find(|c| c.name == "Secret").unwrap();
    assert_eq!(secret.password, Password::Locked);
    assert!(locked.notes.iter().any(|n| n.contains("Secret") && n.contains("locked")));
    let dropped = plan(&source(), &Options { drop_passwords: true, ..Default::default() });
    assert_eq!(dropped.channels.iter().find(|c| c.name == "Secret").unwrap().password, Password::Dropped);
}

#[test]
fn applying_twice_updates_instead_of_adding_copies() {
    let store = Store::in_memory().unwrap();
    let mut options = Options::default();
    options.passwords.insert("Secret".into(), "hunter2".into());
    let first = apply(&store, &plan(&source(), &options), "src", BRIDGE).unwrap();
    assert_eq!((first.roles_created, first.channels_created, first.members_created, first.bans), (2, 5, 3, 2));
    let groups = store.groups().unwrap();
    let names: Vec<_> = groups.iter().map(|g| g.name.as_str()).collect();
    assert_eq!(names, ["Admin", "Server Admin", "Moderator", "Member"], "Admin first, imported in order, Member last");
    assert_eq!(groups[0].id, ADMIN_GROUP);
    let channels = store.channels().unwrap();
    let room = channels.iter().find(|c| c.name == "Room").unwrap();
    let clan = channels.iter().find(|c| c.name == "Clan").unwrap();
    assert_eq!(room.parent, Some(clan.id));
    assert!(room.overrides.iter().any(|o| o.role == Some(MEMBER_GROUP) && o.deny == [Permission::Connect]));
    // The seeded Lobby keeps its name; the imported one takes "Lobby (2)".
    assert!(channels.iter().any(|c| c.name == "Lobby (2)"));
    let secret = channels.iter().find(|c| c.name == "Secret").unwrap();
    assert!(crate::core::passwords::verify_secret("hunter2", secret.password_hash.as_deref().unwrap(), false));
    assert_eq!(store.bridge_passwords().unwrap(), vec![(secret.id, "hunter2".to_owned())]);
    store.forget_bridge_password(secret.id).unwrap();
    let (_, boss) = store.user_by_uid("ts:AdminUid=").unwrap().unwrap();
    assert!(boss.groups.contains(&groups[1].id) && boss.groups.contains(&MEMBER_GROUP));

    let second = apply(&store, &plan(&source(), &options), "src", BRIDGE).unwrap();
    assert_eq!((second.roles_created, second.channels_created, second.members_created, second.bans), (0, 0, 0, 0));
    assert_eq!((second.roles_updated, second.channels_updated, second.members_updated), (2, 5, 3));
    assert!(second.notes.is_empty(), "{:?}", second.notes);
    let dump = |rows: Vec<crate::store::ChannelRow>| format!("{rows:?}");
    assert_eq!(dump(store.channels().unwrap()), dump(channels), "an unchanged source changes nothing");
    // Sent again: the bridge confirms each (so a first run without --for-bridge is made up for).
    assert_eq!(store.bridge_passwords().unwrap().len(), 1);
}

#[test]
fn replacing_channels_clears_the_servers_own_and_moves_the_default() {
    let store = Store::in_memory().unwrap();
    let replace = ApplyOptions { replace_channels: true, ..Default::default() };
    let applied = apply(&store, &plan(&source(), &Options::default()), "src", replace).unwrap();
    assert!(applied.deleted.contains(&"General".to_owned()));
    let names: BTreeMap<_, _> = store.channels().unwrap().into_iter().map(|c| (c.name.clone(), c.id)).collect();
    assert!(names.contains_key("Lobby") && !names.contains_key("General") && !names.contains_key("Lobby (2)"));
    assert_eq!(store.meta("default_channel").unwrap(), Some(names["Lobby"].to_string()));
}

#[test]
fn the_passwords_file_takes_paths_names_and_quotes() {
    let parsed = parse_passwords("# comment\n@server = top\n\"Clan/Room\" = \"a b\"\nSecret=x=y\n").unwrap();
    assert_eq!(parsed["@server"], "top");
    assert_eq!(parsed["Clan/Room"], "a b");
    assert_eq!(parsed["Secret"], "x=y");
    assert!(parse_passwords("no equals sign").is_err());
}

#[test]
fn a_second_import_keeps_what_was_changed_or_removed_in_gwar() {
    let store = Store::in_memory().unwrap();
    let options = Options { passwords: [("Secret".into(), "hunter2".into())].into(), ..Default::default() };
    apply(&store, &plan(&source(), &options), "src", BRIDGE).unwrap();
    let channels = store.channels().unwrap();
    let by_name = |name: &str| channels.iter().find(|c| c.name == name).unwrap().clone();
    let groups = store.groups().unwrap();
    let moderator = groups.iter().find(|g| g.name == "Moderator").unwrap().clone();

    // The operator, between the two runs: a password on Stage, Clan closed to Member, Room
    // deleted, Moderator made administrator and moved to the top, Mod demoted, the bans lifted.
    let mut stage = by_name("Stage");
    stage.password_hash = Some(crate::core::passwords::hash_secret("staff", false));
    store.update_channel(&stage).unwrap();
    let mut clan = by_name("Clan");
    clan.overrides.push(vc_proto::ChannelOverride {
        role: Some(MEMBER_GROUP),
        deny: vec![Permission::Connect],
        ..Default::default()
    });
    store.update_channel(&clan).unwrap();
    store.delete_channel(by_name("Room").id).unwrap();
    let mut promoted = moderator.clone();
    promoted.permissions.push(Permission::Administrator);
    store.update_group(&promoted).unwrap();
    let order: Vec<_> =
        std::iter::once(moderator.id).chain(groups.iter().map(|g| g.id).filter(|id| *id != moderator.id)).collect();
    store.reorder_groups(&order).unwrap();
    let (mod_id, _) = store.user_by_uid("ts:ModUid=").unwrap().unwrap();
    store.set_user_groups(mod_id, &[MEMBER_GROUP]).unwrap();
    for ban in store.bans(0).unwrap() {
        store.delete_ban(ban.id, 0).unwrap();
    }

    let second = apply(&store, &plan(&source(), &options), "src", BRIDGE).unwrap();
    let channels = store.channels().unwrap();
    let by_name = |name: &str| channels.iter().find(|c| c.name == name).cloned();
    assert!(crate::core::passwords::verify_secret(
        "staff",
        by_name("Stage").unwrap().password_hash.as_deref().unwrap(),
        false
    ));
    assert!(
        by_name("Clan")
            .unwrap()
            .overrides
            .iter()
            .any(|o| o.role == Some(MEMBER_GROUP) && o.deny == [Permission::Connect])
    );
    assert!(by_name("Room").is_none(), "a channel deleted in Gwar is not made again");
    let groups = store.groups().unwrap();
    assert_eq!(groups[0].id, moderator.id, "the operator's order stays");
    assert!(groups.iter().find(|g| g.id == moderator.id).unwrap().permissions.contains(&Permission::Administrator));
    assert_eq!(store.user_by_uid("ts:ModUid=").unwrap().unwrap().1.groups, vec![MEMBER_GROUP]);
    assert!(store.bans(0).unwrap().is_empty(), "lifted bans stay lifted");
    assert_eq!(second.bans, 0);
    for expected in ["Stage", "Clan", "Room", "Moderator", "role order"] {
        assert!(second.notes.iter().any(|n| n.contains(expected)), "no note about {expected}: {:?}", second.notes);
    }
}

#[test]
fn a_group_new_on_teamspeak_is_given_and_one_taken_there_goes() {
    let store = Store::in_memory().unwrap();
    apply(&store, &plan(&source(), &Options::default()), "src", BRIDGE).unwrap();
    let mut s = source();
    // Mod moves from Moderator to Server Admin on TeamSpeak.
    s.group_members.insert(6, vec![100, 101]);
    s.group_members.insert(9, vec![]);
    apply(&store, &plan(&s, &Options::default()), "src", BRIDGE).unwrap();
    let groups = store.groups().unwrap();
    let id = |name: &str| groups.iter().find(|g| g.name == name).unwrap().id;
    let (_, member) = store.user_by_uid("ts:ModUid=").unwrap().unwrap();
    assert!(member.groups.contains(&id("Server Admin")) && !member.groups.contains(&id("Moderator")));
}

#[test]
fn roles_rank_by_what_they_may_do_not_by_the_display_order() {
    let mut s = source();
    // Moderator listed first, adding members to Moderator only (Server Admin needs 75).
    s.groups[2].insert("sortid".into(), "-1".into());
    s.group_perms.get_mut(&9).unwrap().0.insert("i_group_member_add_power".into(), (50, false));
    s.group_perms.get_mut(&9).unwrap().0.insert("i_group_needed_member_add_power".into(), (50, false));
    s.group_perms.get_mut(&6).unwrap().0.insert("i_group_needed_member_add_power".into(), (75, false));
    let planned = plan(&s, &Options::default());
    let names: Vec<_> = planned.roles.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["Server Admin", "Moderator"]);
    let moderator = planned.roles.iter().find(|r| r.name == "Moderator").unwrap();
    assert!(!moderator.permissions.contains(&Permission::ManageRoles));
    assert!(planned.notes.iter().any(|n| n.contains("Moderator") && n.contains("manage_roles")));
    assert!(planned.notes.iter().any(|n| n.contains("ranked by their TeamSpeak powers")));
}

#[test]
fn a_password_goes_only_where_the_file_says_clearly() {
    let mut s = source();
    // A second "Room", under Lobby, with a password too; and Clan/Room gets one.
    let mut other = s.channels[2].clone();
    other.insert("cid".into(), "8".into());
    other.insert("pid".into(), "1".into());
    other.insert("channel_flag_password".into(), "1".into());
    s.channels.push(other);
    s.channels[2].insert("channel_flag_password".into(), "1".into());
    let with = |pairs: &[(&str, &str)]| Options {
        passwords: pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect(),
        ..Default::default()
    };
    let error = super::plan::plan(&s, &with(&[("Room", "x")])).unwrap_err().to_string();
    assert!(error.contains("several channels"), "{error}");
    let planned = plan(&s, &with(&[("Clan/Room", "x")]));
    let room = |parent: u64| planned.channels.iter().find(|c| c.name == "Room" && c.parent == Some(parent)).unwrap();
    assert_eq!(room(2).password, Password::Kept("x".into()));
    assert_eq!(room(1).password, Password::Locked);
    // A key that matches nothing is a mistake, not silence.
    let error = super::plan::plan(&s, &with(&[("Clan/Rom", "x")])).unwrap_err().to_string();
    assert!(error.contains("no imported password channel"), "{error}");
    // A channel called like the server's key does not take its password.
    s.channels[6].insert("channel_name".into(), "@server".into());
    s.server.insert("virtualserver_flag_password".into(), "1".into());
    let planned = plan(&s, &with(&[("@server", "top")]));
    assert_eq!(planned.server_password, Password::Kept("top".into()));
    assert_eq!(planned.channels.iter().find(|c| c.name == "@server").unwrap().password, Password::Locked);
}

#[test]
fn replacing_channels_spares_those_in_use_unless_forced() {
    let store = Store::in_memory().unwrap();
    let general = store.channels().unwrap().into_iter().find(|c| c.name == "General").unwrap().id;
    store.add_message(general, "u", "u", "hello", 1).unwrap();
    let replace = ApplyOptions { replace_channels: true, ..Default::default() };
    let error = apply(&store, &plan(&source(), &Options::default()), "src", replace).unwrap_err().to_string();
    assert!(error.contains("General") && error.contains("--force"), "{error}");
    let forced = ApplyOptions { force: true, ..replace };
    apply(&store, &plan(&source(), &Options::default()), "src2", forced).unwrap();
    assert!(store.channels().unwrap().iter().all(|c| c.name != "General"));
}

#[test]
fn a_channel_groups_talk_power_lets_its_members_speak() {
    let mut s = source();
    s.channel_group_perms.insert(5, perms(&[("i_client_talk_power", 60)]));
    s.channel_group_members.push(r(&[("cid", "4"), ("cldbid", "102"), ("cgid", "5")]));
    let planned = plan(&s, &Options::default());
    let stage = planned.channels.iter().find(|c| c.name == "Stage").unwrap();
    assert!(
        stage
            .overrides
            .iter()
            .any(|o| o.target == Target::User("ts:ClanAdminUid=".into()) && o.allow == [Permission::Speak])
    );
}

#[test]
fn passwords_wait_for_the_bridge_only_with_one() {
    let store = Store::in_memory().unwrap();
    let options = Options { passwords: [("Secret".into(), "hunter2".into())].into(), ..Default::default() };
    apply(&store, &plan(&source(), &options), "src", ApplyOptions::default()).unwrap();
    assert!(store.bridge_passwords().unwrap().is_empty());
}

#[test]
fn an_import_is_all_or_nothing_and_a_dry_run_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("vc.sqlite3");
    {
        let store = Store::open(&database).unwrap();
        store.seed_channels().unwrap();
        let general = store.channels().unwrap().into_iter().find(|c| c.name == "General").unwrap().id;
        store.add_message(general, "u", "u", "hello", 1).unwrap();
    }
    let count = || Store::open(&database).unwrap().channels().unwrap().len();
    let before = count();
    let planned = plan(&source(), &Options::default());
    // Fails half way (a busy channel to replace): nothing changed, nothing left behind.
    let replace = ApplyOptions { replace_channels: true, ..Default::default() };
    assert!(super::import(&database, &planned, "src", replace, true).is_err());
    let dry = super::import(&database, &planned, "src", ApplyOptions::default(), false).unwrap();
    assert_eq!(dry.channels_created, 5);
    assert_eq!(count(), before);
    super::import(&database, &planned, "src", ApplyOptions::default(), true).unwrap();
    assert_eq!(count(), before + 5);
    let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert!(left.iter().all(|f| !f.to_string_lossy().contains("importing")), "{left:?}");
}

#[test]
fn what_gwar_itself_drops_or_merges_is_no_edit_and_a_default_chosen_in_gwar_stays() {
    let store = Store::in_memory().unwrap();
    apply(&store, &plan(&source(), &Options::default()), "src", BRIDGE).unwrap();
    // A role deleted (its overrides go with it) and the channel admin merged into a Gwar identity.
    let moderator = store.groups().unwrap().into_iter().find(|g| g.name == "Moderator").unwrap().id;
    store.delete_group(moderator).unwrap();
    store.touch_user("native", "key", "Leader", 1).unwrap();
    store.merge_members("ts:ClanAdminUid=", "native", false).unwrap();
    let general = store.channels().unwrap().into_iter().find(|c| c.name == "General").unwrap().id;
    store.set_meta("default_channel", &general.to_string()).unwrap();
    let second = apply(&store, &plan(&source(), &Options::default()), "src", BRIDGE).unwrap();
    assert!(second.notes.iter().all(|n| !n.contains("Channel")), "{:?}", second.notes);
    assert_eq!(store.meta("default_channel").unwrap(), Some(general.to_string()));
    assert!(second.notes.iter().any(|n| n.contains("default channel")));
}

#[test]
fn a_subchannel_called_like_the_servers_key_does_not_take_its_password() {
    let mut s = source();
    s.channels[2].insert("channel_name".into(), "@server".into());
    s.channels[2].insert("channel_flag_password".into(), "1".into());
    s.server.insert("virtualserver_flag_password".into(), "1".into());
    let options = Options { passwords: [("@server".into(), "top".into())].into(), ..Default::default() };
    let planned = plan(&s, &options);
    assert_eq!(planned.channels.iter().find(|c| c.name == "@server").unwrap().password, Password::Locked);
}

#[test]
fn a_server_that_never_ran_is_imported_into_a_new_database() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("vc.sqlite3");
    let planned = plan(&source(), &Options::default());
    super::import(&database, &planned, "src", ApplyOptions::default(), false).unwrap();
    assert!(!database.exists(), "a dry run leaves nothing");
    let applied = super::import(&database, &planned, "src", ApplyOptions::default(), true).unwrap();
    assert_eq!(applied.channels_created, 5);
    let store = Store::open(&database).unwrap();
    let default: u32 = store.meta("default_channel").unwrap().unwrap().parse().unwrap();
    assert_eq!(store.channels().unwrap().iter().find(|c| c.id == default).unwrap().name, "Lobby");
}
