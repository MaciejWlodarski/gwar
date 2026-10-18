#![cfg(all(feature = "teamspeak", feature = "test-hooks"))]
//! SPIKE (not for main): what the official TeamSpeak server stores for a channel password, and whether
//! ServerQuery can set it from the stored hash. Prints its findings; run with --ignored --nocapture.

mod common;

use std::{path::PathBuf, time::Duration};

use base64::{Engine, engine::general_purpose::STANDARD};
use common::*;
use futures::StreamExt;
use serde_json::json;
use sha1::{Digest, Sha1};
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

fn wire(password: &str) -> String {
    STANDARD.encode(Sha1::digest(password.as_bytes()))
}

async fn find_channel(query: &Query, name: &str) -> String {
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

async fn stored(query: &Query, cid: &str) -> String {
    let info = query.call(Cmd::new("channelinfo").arg("cid", cid)).await.unwrap();
    info.first().and_then(|r| r.get("channel_password")).cloned().unwrap_or_default()
}

/// A raw `clientmove` from a TeamSpeak client with `cpw` exactly as given (clients send the wire form).
async fn join(ts: &mut tsclientlib::Connection, cid: &str, cpw: &str) -> Result<(), tsclientlib::CommandError> {
    let me = ts.get_state().unwrap().own_client.0;
    let mut command = OutCommand::new(Direction::C2S, Flags::empty(), PacketType::Command, "clientmove");
    command.write_arg("clid", &me);
    command.write_arg("cid", &cid);
    command.write_arg("cpw", &cpw);
    let handle = command.send_with_result(ts).unwrap();
    within("clientmove answer", async {
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
async fn spike_a_channel_password_hash() {
    run("first").await;
}

#[tokio::test]
#[ignore]
async fn spike_b_fresh_instance() {
    run("second").await;
}

fn find_db(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if entry.file_type().ok()?.is_symlink() {
            continue;
        }
        if path.is_dir() {
            if let Some(found) = find_db(&path) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|n| n == "ts3server.sqlitedb") {
            return Some(path);
        }
    }
    None
}

fn dump_db(dir: &std::path::Path, label: &str) {
    let Some(db) = find_db(dir) else {
        eprintln!("SPIKE [{label}] no ts3server.sqlitedb under {}", dir.display());
        return;
    };
    let conn = rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut tables = conn.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name").unwrap();
    let names: Vec<String> = tables.query_map([], |r| r.get(0)).unwrap().flatten().collect();
    eprintln!("SPIKE [{label}] tables: {names:?}");
    for table in ["channel_properties", "server_properties", "instance_properties"] {
        let Ok(mut q) = conn.prepare(&format!("SELECT * FROM {table}")) else { continue };
        let cols: Vec<String> = q.column_names().iter().map(|c| c.to_string()).collect();
        let n = cols.len();
        let rows = q
            .query_map([], |r| {
                Ok((0..n)
                    .map(|i| r.get::<_, rusqlite::types::Value>(i).map(|v| format!("{v:?}")).unwrap_or_default())
                    .collect::<Vec<_>>())
            })
            .unwrap();
        for row in rows.flatten() {
            let line = row.join(" | ");
            if table != "channel_properties" || line.to_lowercase().contains("password") {
                eprintln!("SPIKE [{label}] {table} {cols:?}: {line}");
            }
        }
    }
}

async fn run(label: &str) {
    let root = PathBuf::from(std::env::var_os("VC_TS3_DIR").expect("VC_TS3_DIR"));
    let installed = install::ensure_installed(&root).await.unwrap();
    let dir = std::env::temp_dir().join(format!("vc-ts-pw-spike-{}", free_port(false)));
    std::fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(&installed.dir, dir.join(installed.dir.file_name().unwrap())).unwrap();
    let voice = format!("127.0.0.1:{}", free_port(true)).parse().unwrap();
    let query_port = free_port(false);
    let server = TestServer::start_with(|c| {
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
    let qpw = std::fs::read_to_string(dir.join("query-password")).unwrap();
    let (query, _notes) =
        Query::connect(format!("127.0.0.1:{query_port}").parse().unwrap(), "serveradmin", qpw.trim(), 1).await.unwrap();

    let mut admin = admin(&server, "root").await;
    admin.create_channel(json!({"name": "Pw", "password": "sekret"})).await;
    let cid = find_channel(&query, "Pw").await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let h = wire("sekret");
    let a = stored(&query, &cid).await;
    eprintln!("SPIKE wire(sekret) = {h}");
    eprintln!("SPIKE [{label}] A channelinfo after plaintext set = {a:?} (equals wire: {})", a == h);
    dump_db(&dir, label);

    let mut ts = tsclientlib::Connection::build(voice.to_string()).name("ts-guest".to_string()).connect().unwrap();
    within("TeamSpeak client", async {
        let mut stream = ts.events();
        while let Some(item) = stream.next().await {
            if matches!(item.unwrap(), StreamItem::BookEvents(_)) {
                break;
            }
        }
    })
    .await;
    eprintln!("SPIKE B join with cpw=wire(sekret): {:?}", join(&mut ts, &cid, &h).await);
    eprintln!("SPIKE B2 join with cpw=sekret (plain): {:?}", join(&mut ts, &cid, "sekret").await);

    // Can ServerQuery set the password from the hash alone?
    for (label, value) in [("C channel_password=<hash>", h.clone()), ("D channel_password=<hash> again", h.clone())] {
        let r = query.call(Cmd::new("channeledit").arg("cid", &cid).arg("channel_password", &value)).await;
        let now = stored(&query, &cid).await;
        eprintln!(
            "SPIKE {label}: edit {:?}; channelinfo = {now:?}; equals wire(sekret): {}; equals wire(hash): {}",
            r.map(|_| ()),
            now == h,
            now == wire(&h)
        );
    }
    let other = tsclientlib::Connection::build(voice.to_string()).name("ts-guest-2".to_string()).connect();
    if let Ok(mut other) = other {
        within("second TeamSpeak client", async {
            let mut stream = other.events();
            while let Some(item) = stream.next().await {
                if matches!(item.unwrap(), StreamItem::BookEvents(_)) {
                    break;
                }
            }
        })
        .await;
        eprintln!("SPIKE E after edit, join with cpw=wire(sekret): {:?}", join(&mut other, &cid, &h).await);
        eprintln!("SPIKE E2 after edit, join with cpw=wire(hash): {:?}", join(&mut other, &cid, &wire(&h)).await);
    }

    // Server password, for completeness.
    let info = query.call(Cmd::new("serverinfo")).await.unwrap();
    eprintln!(
        "SPIKE F serverinfo virtualserver_password before = {:?}",
        info.first().and_then(|r| r.get("virtualserver_password"))
    );
    let r = query.call(Cmd::new("serveredit").arg("virtualserver_password", "sekret")).await;
    let info = query.call(Cmd::new("serverinfo")).await.unwrap();
    eprintln!(
        "SPIKE F2 serveredit plain: {:?}; virtualserver_password = {:?}",
        r.map(|_| ()),
        info.first().and_then(|r| r.get("virtualserver_password"))
    );
    let _ = query.call(Cmd::new("serveredit").arg("virtualserver_password", "")).await;
    drop(server);
    tokio::time::sleep(Duration::from_secs(3)).await;
    let _ = std::fs::remove_dir_all(&dir);
}
