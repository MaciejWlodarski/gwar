use std::collections::BTreeMap;

use vc_proto::Permission;

use super::{
    apply::apply,
    parse_passwords,
    plan::{Options, Password, Target, map_permissions, plan},
    source::{Perms, Source},
};
use crate::{
    store::{ADMIN_GROUP, MEMBER_GROUP, Store},
    teamspeak::query::Record,
};

fn r(pairs: &[(&str, &str)]) -> Record {
    pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
}

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
            ("uid", "BadUid="),
            ("lastnickname", "Troll"),
            ("created", "1700000000"),
            ("duration", "0"),
            ("reason", "spam"),
        ]),
        r(&[("ip", "203.0.113.9"), ("created", "1700000000"), ("duration", "0")]),
        r(&[("name", "troll.*"), ("created", "1700000000"), ("duration", "0")]),
        r(&[("uid", "OldUid="), ("created", "1000"), ("duration", "60")]),
    ];
    s
}

#[test]
fn groups_map_by_their_own_assignments_and_the_default_groups_needed_powers() {
    let s = source();
    let admin = map_permissions(&s.group_perms[&6], &s.group_perms[&8]);
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
        map_permissions(&s.group_perms[&9], &s.group_perms[&8]).into_iter().map(|(p, _)| p).collect();
    assert_eq!(moderator, [Permission::KickMembers]);
    // Negated assignments do not count.
    let negated = Perms([("b_client_ban_create".to_owned(), (1, true))].into_iter().collect());
    assert!(map_permissions(&negated, &Perms::default()).is_empty());
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
    let plan = plan(&source(), &Options::default());
    let secret = plan.channels.iter().find(|c| c.name == "Secret").unwrap();
    assert_eq!(secret.password, Password::Locked);
    assert!(plan.notes.iter().any(|n| n.contains("Secret") && n.contains("locked")));
    let dropped = super::plan::plan(&source(), &Options { drop_passwords: true, ..Default::default() });
    assert_eq!(dropped.channels.iter().find(|c| c.name == "Secret").unwrap().password, Password::Dropped);
}

#[test]
fn applying_twice_updates_instead_of_adding_copies() {
    let store = Store::in_memory().unwrap();
    let mut options = Options::default();
    options.passwords.insert("Secret".into(), "hunter2".into());
    let first = apply(&store, &plan(&source(), &options), "src", false).unwrap();
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
    assert_eq!(store.take_bridge_passwords().unwrap(), vec![(secret.id, "hunter2".to_owned())]);
    let (_, boss) = store.user_by_uid("ts:AdminUid=").unwrap().unwrap();
    assert!(boss.groups.contains(&groups[1].id) && boss.groups.contains(&MEMBER_GROUP));

    let second = apply(&store, &plan(&source(), &options), "src", false).unwrap();
    assert_eq!((second.roles_created, second.channels_created, second.members_created, second.bans), (0, 0, 0, 0));
    assert_eq!((second.roles_updated, second.channels_updated, second.members_updated), (2, 5, 3));
    assert_eq!(store.channels().unwrap().len(), channels.len());
}

#[test]
fn replacing_channels_clears_the_servers_own_and_moves_the_default() {
    let store = Store::in_memory().unwrap();
    apply(&store, &plan(&source(), &Options::default()), "src", true).unwrap();
    let names: BTreeMap<_, _> = store.channels().unwrap().into_iter().map(|c| (c.name.clone(), c.id)).collect();
    assert!(names.contains_key("Lobby") && !names.contains_key("General") && !names.contains_key("Lobby (2)"));
    assert_eq!(store.meta("default_channel").unwrap(), Some(names["Lobby"].to_string()));
}

#[test]
fn the_passwords_file_takes_paths_names_and_quotes() {
    let parsed = parse_passwords("# comment\nserver = top\n\"Clan/Room\" = \"a b\"\nSecret=x=y\n").unwrap();
    assert_eq!(parsed["server"], "top");
    assert_eq!(parsed["Clan/Room"], "a b");
    assert_eq!(parsed["Secret"], "x=y");
    assert!(parse_passwords("no equals sign").is_err());
}
