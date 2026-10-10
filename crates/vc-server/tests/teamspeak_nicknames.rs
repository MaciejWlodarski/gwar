#![cfg(feature = "teamspeak")]
//! Real puppet nickname behavior. Run with VC_TS3_DIR and --ignored --test-threads=1.

mod common;

use std::{path::PathBuf, time::Duration};

use common::*;
use futures::StreamExt;
use serde_json::json;
use vc_server::{
    TeamSpeakConfig,
    teamspeak::query::{Cmd, Query},
};

fn free_port(udp: bool) -> u16 {
    if udp {
        std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    } else {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }
}

async fn names(query: &Query, expected: &[String]) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let clients = query.call(Cmd::new("clientlist")).await.unwrap();
            let present: Vec<_> = clients.iter().filter_map(|c| c.get("client_nickname")).collect();
            if expected.iter().all(|name| present.contains(&name)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("puppet names on the official TeamSpeak server");
}

#[tokio::test]
#[ignore]
async fn puppet_nicknames_follow_members_and_resolve_collisions() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "vc_server::teamspeak=debug,teamspeak::server=warn".into()),
        )
        .try_init();
    let root = PathBuf::from(std::env::var_os("VC_TS3_DIR").expect("VC_TS3_DIR"));
    let voice = format!("127.0.0.1:{}", free_port(true)).parse().unwrap();
    let query_port = free_port(false);
    let server = TestServer::start_with(|c| {
        c.teamspeak = Some(TeamSpeakConfig {
            dir: root.clone(),
            voice,
            query_port,
            filetransfer: format!("127.0.0.1:{}", free_port(false)).parse().unwrap(),
            public_url: None,
        })
    })
    .await;
    let mut ready = server.running.teamspeak_ready.clone().unwrap();
    tokio::time::timeout(Duration::from_secs(120), ready.wait_for(|ready| *ready)).await.unwrap().unwrap();
    let password = std::fs::read_to_string(root.join("query-password")).unwrap();
    let (query, _notes) =
        Query::connect(format!("127.0.0.1:{query_port}").parse().unwrap(), "serveradmin", password.trim(), 1)
            .await
            .unwrap();

    // A native TS user occupies the requested 30-character base name.
    let base = "n".repeat(30);
    let mut native = tsclientlib::Connection::build(voice.to_string()).name(base.clone()).connect().unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut stream = native.events();
        while let Some(item) = stream.next().await {
            if matches!(item.unwrap(), tsclientlib::StreamItem::BookEvents(_)) {
                break;
            }
        }
    })
    .await
    .unwrap();
    let native_task = tokio::spawn(async move {
        native.events().for_each(|_| async {}).await;
    });

    let key = new_key();
    let mut first = Client::connect_as(&server, key.clone(), &"n".repeat(32), None).await.unwrap();
    first.ok("channel.join", json!({"channel": first.default_channel()})).await;
    let mut second = Client::connect(&server, &"n".repeat(32)).await;
    second.ok("channel.join", json!({"channel": second.default_channel()})).await;
    names(&query, &[base.clone(), format!("{} (2)", "n".repeat(26)), format!("{} (3)", "n".repeat(26))]).await;

    // A second device of the first member gets the same stored name and its own puppet.
    let mut other_device = Client::connect_as(&server, key, "Ignored", None).await.unwrap();
    other_device.ok("channel.join", json!({"channel": other_device.default_channel()})).await;
    names(&query, &[format!("{} (4)", "n".repeat(26))]).await;
    first.ok("member.nickname", json!({"uid": first.welcome.uid, "nickname": "Renamed"})).await;
    names(&query, &[base.clone(), "Renamed".into(), "Renamed (2)".into()]).await;
    // Renaming to the native user's occupied base also retries, for every session.
    first.ok("member.nickname", json!({"uid": first.welcome.uid, "nickname": "n".repeat(32)})).await;
    names(
        &query,
        &[
            base,
            format!("{} (2)", "n".repeat(26)),
            format!("{} (3)", "n".repeat(26)),
            format!("{} (4)", "n".repeat(26)),
        ],
    )
    .await;
    native_task.abort();
}
