#![cfg(all(feature = "teamspeak", feature = "test-hooks"))]
//! Reading a real TeamSpeak server for an import, and importing into a running Gwar server
//! (docs/teamspeak-import.md, docs/teamspeak-move.md). Run with GWAR_TS3_DIR and
//! --ignored --test-threads=1: the official server's free license allows one instance per host.

mod common;

use std::{path::PathBuf, time::Duration};

use common::*;
use gwar_proto::Permission;
use gwar_server::{
    TeamSpeakConfig,
    store::{MEMBER_GROUP, Store},
    teamspeak::{
        install,
        query::{Cmd, Query},
    },
    ts_import::{
        apply::{ApplyOptions, apply},
        plan, source,
    },
};
use serde_json::json;

fn free_port(udp: bool) -> u16 {
    if udp {
        std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    } else {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }
}

async fn cid(query: &Query, name: &str) -> String {
    within("mirrored channel", async {
        loop {
            let channels = query.call(Cmd::new("channellist")).await.unwrap();
            if let Some(cid) =
                channels.iter().find(|c| c.get("channel_name").is_some_and(|n| n == name)).and_then(|c| c.get("cid"))
            {
                return cid.to_owned();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
}

#[tokio::test]
#[ignore]
async fn a_real_teamspeak_server_reads_and_imports() {
    // A TeamSpeak server to read: the one Gwar runs, with its channels mirrored there.
    let root_dir = PathBuf::from(std::env::var_os("GWAR_TS3_DIR").expect("GWAR_TS3_DIR"));
    let installed = install::ensure_installed(&root_dir).await.unwrap();
    let dir = std::env::temp_dir().join(format!("gwar-ts-import-{}", free_port(false)));
    std::fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(&installed.dir, dir.join(installed.dir.file_name().unwrap())).unwrap();
    let voice = format!("127.0.0.1:{}", free_port(true)).parse().unwrap();
    let query_port = free_port(false);
    let server = TestServer::start_with(|c| {
        // Also where the control socket (admin.sock) goes, for the live import below.
        c.data_dir = Some(dir.clone());
        c.teamspeak = Some(TeamSpeakConfig {
            dir: dir.clone(),
            voice,
            query_port,
            filetransfer: format!("127.0.0.1:{}", free_port(false)).parse().unwrap(),
            public_url: None,
        })
    })
    .await;
    let mut ready = server.running.teamspeak_ready.clone().unwrap();
    tokio::time::timeout(Duration::from_secs(120), ready.wait_for(|ready| *ready)).await.unwrap().unwrap();
    let password = std::fs::read_to_string(dir.join("query-password")).unwrap();
    let addr = format!("127.0.0.1:{query_port}").parse().unwrap();
    let (query, _notes) = Query::connect(addr, "serveradmin", password.trim(), 1).await.unwrap();

    let mut admin = admin(&server, "root").await;
    admin.create_channel(json!({"name": "Clan"})).await;
    admin.create_channel(json!({"name": "Secret", "password": "hunter2"})).await;
    let clan = cid(&query, "Clan").await;
    cid(&query, "Secret").await;
    // A moderator group with powers, and a members-only Clan, set as a TeamSpeak admin would.
    let created = query.call(Cmd::new("servergroupadd").arg("name", "Moderator")).await.unwrap();
    let sgid = created[0]["sgid"].clone();
    for (perm, value) in [("i_client_kick_from_server_power", "50"), ("i_channel_join_power", "60")] {
        query
            .call(
                Cmd::new("servergroupaddperm")
                    .arg("sgid", &sgid)
                    .arg("permsid", perm)
                    .arg("permvalue", value)
                    .arg("permnegated", 0)
                    .arg("permskip", 0),
            )
            .await
            .unwrap();
    }
    query
        .call(
            Cmd::new("channeladdperm")
                .arg("cid", &clan)
                .arg("permsid", "i_channel_needed_join_power")
                .arg("permvalue", 55),
        )
        .await
        .unwrap();

    let read = source::read(&query).await.unwrap();
    let mut options = plan::Options::default();
    options.passwords.insert("Secret".into(), "hunter2".into());
    let plan = plan::plan(&read, &options).unwrap();
    eprintln!("{}", plan::report(&plan));
    let moderator = plan.roles.iter().find(|r| r.name == "Moderator").expect("the Moderator group");
    assert!(moderator.permissions.contains(&Permission::KickMembers), "{:?}", moderator.reasons);
    let planned = plan.channels.iter().find(|c| c.name == "Clan").unwrap();
    // The bridge's channels also need its talk power, so Member may not speak there either.
    assert!(
        planned.overrides.iter().any(|o| o.target == plan::Target::Member && o.deny.contains(&Permission::Connect))
    );
    assert!(
        planned
            .overrides
            .iter()
            .any(|o| o.target == plan::Target::Role(sgid.parse().unwrap()) && o.allow == [Permission::Connect])
    );
    assert_eq!(
        plan.channels.iter().find(|c| c.name == "Secret").unwrap().password,
        plan::Password::Kept("hunter2".into())
    );

    // Into a fresh Gwar server.
    let store = Store::in_memory().unwrap();
    let replace = ApplyOptions { replace_channels: true, ..Default::default() };
    let applied = apply(&store, &plan, "real", replace).unwrap();
    assert!(applied.channels_created >= 2 && applied.roles_created >= 1, "{applied:?}");
    let channels = store.channels().unwrap();
    let imported = channels.iter().find(|c| c.name == "Clan").unwrap();
    assert!(imported.overrides.iter().any(|o| o.role == Some(MEMBER_GROUP) && o.deny.contains(&Permission::Connect)));

    // Live, into this Gwar server while it runs (what `import-teamspeak` does when the server holds
    // its data directory): applied in one go, everyone reconnects, and the bridge carries the new
    // channels to TeamSpeak with the typed password.
    assert!(gwar_server::control::available(&dir));
    let mut watcher = Client::connect(&server, "watcher").await;
    let for_bridge = ApplyOptions { for_bridge: true, ..Default::default() };
    let applied = gwar_server::control::import(&dir, plan.clone(), "real-live".into(), for_bridge).await.unwrap();
    assert!(applied.channels_created >= 2 && applied.roles_created >= 1, "{applied:?}");
    within("the server closes connections after a live import", watcher.closed()).await;
    let after = Client::connect(&server, "after").await;
    let names: Vec<_> = after.welcome.channels.iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains(&"Clan (2)") && names.contains(&"Secret (2)"), "{names:?}");
    let secret = cid(&query, "Secret (2)").await;
    within("the bridge sets the kept password on TeamSpeak", async {
        loop {
            let info = query.call(Cmd::new("channelinfo").arg("cid", &secret)).await.unwrap();
            if info.first().and_then(|c| c.get("channel_flag_password")).is_some_and(|f| f == "1") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    let _ = std::fs::remove_dir_all(&dir);
}
