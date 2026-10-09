#![cfg(feature = "teamspeak")]
//! Tests against the real, official TeamSpeak 3 server. Ignored by default; see the env vars per test.

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use tokio::{net::TcpStream, time::sleep};
use vc_server::teamspeak::{
    install::ensure_installed,
    process::{ProcessConfig, TsProcess},
    query::{Cmd, Query},
};

/// `VC_TS3_QUERY=127.0.0.1:10011 VC_TS3_QUERY_PASSWORD=...` points at a running official server.
#[tokio::test]
#[ignore]
async fn query_against_running_server() {
    let addr: SocketAddr = std::env::var("VC_TS3_QUERY").expect("VC_TS3_QUERY").parse().unwrap();
    let password = std::env::var("VC_TS3_QUERY_PASSWORD").expect("VC_TS3_QUERY_PASSWORD");
    let (query, mut notes) = Query::connect(addr, "serveradmin", &password, 1).await.unwrap();

    let info = query.call(Cmd::new("serverinfo")).await.unwrap();
    assert_eq!(info.len(), 1);
    println!("server: {} {}", info[0]["virtualserver_name"], info[0]["virtualserver_version"]);
    let channels = query.call(Cmd::new("channellist")).await.unwrap();
    assert!(!channels.is_empty());
    println!("{} channels, first: {:?}", channels.len(), channels[0]);

    query.call(Cmd::new("servernotifyregister").arg("event", "server")).await.unwrap();
    query.call(Cmd::new("servernotifyregister").arg("event", "channel").arg("id", 0)).await.unwrap();
    let err = query.call(Cmd::new("nosuchcommand")).await.unwrap_err();
    println!("expected error: {err}");
    // Still healthy afterwards.
    assert_eq!(query.call(Cmd::new("whoami")).await.unwrap().len(), 1);
    while let Ok(n) = notes.try_recv() {
        println!("notification: {}", n.name);
    }
}

async fn port_open(port: u16) -> bool {
    TcpStream::connect(("127.0.0.1", port)).await.is_ok()
}

/// `VC_TS3_INSTALL_DIR=/some/dir` is where the server is downloaded to (reused if already installed).
#[tokio::test]
#[ignore]
async fn install_spawn_query_and_stop() {
    let root = PathBuf::from(std::env::var("VC_TS3_INSTALL_DIR").expect("VC_TS3_INSTALL_DIR"));
    let installed = ensure_installed(&root).await.unwrap();
    println!("installed in {} (emulated: {})", installed.dir.display(), installed.emulated);
    assert!(ensure_installed(&root).await.unwrap().dir == installed.dir);

    let state = root.join("state-test");
    let _ = std::fs::remove_dir_all(&state);
    let query_port = 20011;
    let process = TsProcess::spawn(ProcessConfig {
        installed,
        state_dir: state.clone(),
        voice: "127.0.0.1:29987".parse().unwrap(),
        query_port,
        filetransfer: "127.0.0.1:30044".parse().unwrap(),
        admin_password: "testpass123".into(),
    })
    .await
    .unwrap();
    assert_eq!(*process.generation().borrow(), 1);

    let (query, _notes) =
        Query::connect(format!("127.0.0.1:{query_port}").parse().unwrap(), "serveradmin", "testpass123", 1)
            .await
            .unwrap();
    let info = query.call(Cmd::new("serverinfo")).await.unwrap();
    println!("server: {} {}", info[0]["virtualserver_name"], info[0]["virtualserver_version"]);
    assert_eq!(info[0]["virtualserver_port"], "29987");
    assert!(state.join("ts3server.sqlitedb").exists());

    drop(process);
    let mut stopped = false;
    for _ in 0..40 {
        if !port_open(query_port).await {
            stopped = true;
            break;
        }
        sleep(Duration::from_millis(500)).await;
    }
    assert!(stopped, "ts3server still running after drop");
}

/// Unix only (uses `pkill`): killing the server makes the supervisor restart it and bump the generation.
#[cfg(unix)]
#[tokio::test]
#[ignore]
async fn supervisor_restarts_killed_server() {
    let root = PathBuf::from(std::env::var("VC_TS3_INSTALL_DIR").expect("VC_TS3_INSTALL_DIR"));
    let installed = ensure_installed(&root).await.unwrap();
    let state = root.join("state-restart");
    let _ = std::fs::remove_dir_all(&state);
    let process = TsProcess::spawn(ProcessConfig {
        installed,
        state_dir: state.clone(),
        voice: "127.0.0.1:29988".parse().unwrap(),
        query_port: 20012,
        filetransfer: "127.0.0.1:30045".parse().unwrap(),
        admin_password: "testpass123".into(),
    })
    .await
    .unwrap();
    let mut generation = process.generation();
    assert_eq!(*generation.borrow_and_update(), 1);

    let pattern = format!("logpath={}", state.join("logs").display());
    assert!(std::process::Command::new("pkill").args(["-KILL", "-f", &pattern]).status().unwrap().success());
    tokio::time::timeout(Duration::from_secs(120), generation.changed()).await.unwrap().unwrap();
    assert_eq!(*generation.borrow(), 2);

    let (query, _notes) =
        Query::connect("127.0.0.1:20012".parse().unwrap(), "serveradmin", "testpass123", 1).await.unwrap();
    assert_eq!(query.call(Cmd::new("serverinfo")).await.unwrap().len(), 1);
    drop(process);
    for _ in 0..40 {
        if !port_open(20012).await {
            return;
        }
        sleep(Duration::from_millis(500)).await;
    }
    panic!("ts3server still running after drop");
}
