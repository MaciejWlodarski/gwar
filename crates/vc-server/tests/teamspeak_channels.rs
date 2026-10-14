#![cfg(feature = "teamspeak")]
//! Mirrored channels on the official TeamSpeak server. Run with VC_TS3_DIR and --ignored --test-threads=1.

mod common;

use std::{path::PathBuf, time::Duration};

use common::*;
use futures::StreamExt;
use serde_json::json;
use tsclientlib::{OutCommandExt, StreamItem};
use tsproto_packets::packets::{Direction, Flags, OutCommand, PacketType};
use vc_server::{
    TeamSpeakConfig,
    teamspeak::{
        install,
        query::{Cmd, Query},
    },
};

fn free_port(udp: bool) -> u16 {
    if udp {
        std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    } else {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn channel_permissions(query: &Query, cid: &str) -> Vec<(String, String)> {
    let perms = query.call(Cmd::new("channelpermlist").arg("cid", cid).flag("permsid")).await.unwrap();
    perms.iter().filter_map(|p| Some((p.get("permsid")?.to_owned(), p.get("permvalue")?.to_owned()))).collect()
}

async fn find_channel(query: &Query, name: &str) -> String {
    within("mirrored channel", async {
        loop {
            let channels = query.call(Cmd::new("channellist")).await.unwrap();
            let mirrored = channels.iter().find(|c| c.get("channel_name").is_some_and(|n| n == name));
            if let Some(cid) = mirrored.and_then(|c| c.get("cid")) {
                return cid.to_owned();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
}

async fn wait_for_permission(query: &Query, cid: &str, perm: (&str, &str)) -> Vec<(String, String)> {
    within("the channel permission", async {
        loop {
            let perms = channel_permissions(query, cid).await;
            if perms.contains(&(perm.0.into(), perm.1.into())) {
                return perms;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
}

/// A TeamSpeak client's `channeladdperm` of the channel's needed join power.
async fn set_join_power(ts: &mut tsclientlib::Connection, cid: &str, power: &str) -> Result<(), tsclientlib::CommandError> {
    let mut command = OutCommand::new(Direction::C2S, Flags::empty(), PacketType::Command, "channeladdperm");
    command.write_arg("cid", &cid);
    command.write_arg("permsid", &"i_channel_needed_join_power");
    command.write_arg("permvalue", &power);
    let handle = command.send_with_result(ts).unwrap();
    within("channeladdperm answer", async {
        let mut stream = ts.events();
        while let Some(item) = stream.next().await {
            if let StreamItem::MessageResult(h, result) = item.unwrap()
                && h == handle
            {
                return result;
            }
        }
        panic!("TeamSpeak connection closed");
    })
    .await
}

#[tokio::test]
#[ignore]
async fn teamspeak_server_admins_manage_permissions_of_mirrored_channels() {
    let root = PathBuf::from(std::env::var_os("VC_TS3_DIR").expect("VC_TS3_DIR"));
    // One shared installation, a fresh server state.
    let installed = install::ensure_installed(&root).await.unwrap();
    let dir = TempDir(std::env::temp_dir().join(format!("vc-ts-channels-{}", free_port(false))));
    std::fs::create_dir_all(&dir.0).unwrap();
    std::os::unix::fs::symlink(&installed.dir, dir.0.join(installed.dir.file_name().unwrap())).unwrap();
    let voice = format!("127.0.0.1:{}", free_port(true)).parse().unwrap();
    let query_port = free_port(false);
    let server = TestServer::start_with(|c| {
        c.teamspeak = Some(TeamSpeakConfig {
            dir: dir.0.clone(),
            voice,
            query_port,
            filetransfer: format!("127.0.0.1:{}", free_port(false)).parse().unwrap(),
            public_url: None,
        })
    })
    .await;
    let mut ready = server.running.teamspeak_ready.clone().unwrap();
    tokio::time::timeout(Duration::from_secs(120), ready.wait_for(|ready| *ready)).await.unwrap().unwrap();
    let password = std::fs::read_to_string(dir.0.join("query-password")).unwrap();
    let (query, _notes) =
        Query::connect(format!("127.0.0.1:{query_port}").parse().unwrap(), "serveradmin", password.trim(), 1)
            .await
            .unwrap();

    let mut admin = admin(&server, "root").await;
    admin.create_channel(json!({"name": "Mirrored"})).await;
    admin.create_channel(json!({"name": "Sealed", "encrypted": true})).await;
    let cid = find_channel(&query, "Mirrored").await;
    // The bridge sets the power right after creating the channel.
    let perms = wait_for_permission(&query, &cid, ("i_channel_needed_permission_modify_power", "75")).await;
    // Deleting stays out of reach: the bridge would only bring the channel back.
    assert!(perms.contains(&("i_channel_needed_delete_power".into(), "100".into())), "{perms:?}");

    // A TeamSpeak user in the stock Server Admin group sets a channel permission.
    let mut ts = tsclientlib::Connection::build(voice.to_string()).name("ts-admin".to_string()).connect().unwrap();
    within("TeamSpeak client", async {
        let mut stream = ts.events();
        while let Some(item) = stream.next().await {
            if matches!(item.unwrap(), StreamItem::BookEvents(_)) {
                break;
            }
        }
    })
    .await;
    let clients = query.call(Cmd::new("clientlist")).await.unwrap();
    let me = clients.iter().find(|c| c.get("client_nickname").is_some_and(|n| n == "ts-admin")).unwrap();
    let cldbid = me.get("client_database_id").unwrap().to_owned();
    query.call(Cmd::new("servergroupaddclient").arg("sgid", 6).arg("cldbid", &cldbid)).await.unwrap();

    let result = set_join_power(&mut ts, &cid, "10").await;
    assert!(result.is_ok(), "Server Admin was refused: {result:?}");
    let perms = channel_permissions(&query, &cid).await;
    assert!(perms.contains(&("i_channel_needed_join_power".into(), "10".into())), "{perms:?}");

    // An encrypted channel is locked, and only ServerQuery could open it.
    let sealed = find_channel(&query, "Sealed").await;
    wait_for_permission(&query, &sealed, ("i_channel_needed_join_power", "1000000")).await;
    let perms = wait_for_permission(&query, &sealed, ("i_channel_needed_permission_modify_power", "100")).await;
    assert!(perms.contains(&("i_channel_needed_join_power".into(), "1000000".into())), "{perms:?}");
    let result = set_join_power(&mut ts, &sealed, "10").await;
    assert!(result.is_err(), "Server Admin lowered the lock");
    let perms = channel_permissions(&query, &sealed).await;
    assert!(perms.contains(&("i_channel_needed_join_power".into(), "1000000".into())), "{perms:?}");
}
