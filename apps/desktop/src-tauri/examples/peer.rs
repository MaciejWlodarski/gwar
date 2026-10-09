//! A scriptable second participant for verifying the desktop app against a
//! local server, without a microphone or speakers.
//!
//!   cargo run -p vc-desktop --example peer -- ws://127.0.0.1:8790/ws [nick] [tone|silent] [seconds]
//!
//! `tone` transmits a 440 Hz tone; the peer always reports what its fake
//! speaker received from other participants.

use std::{sync::Arc, time::Duration};

use vc_client::{ConnectOptions, Identity, connect, voice};
use vc_desktop_lib::fake_audio::{FakeIo, Heard};
use vc_proto::{ClientSoftware, Platform};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let url = args.next().unwrap_or_else(|| "ws://127.0.0.1:8790/ws".into());
    let nickname = args.next().unwrap_or_else(|| "peer".into());
    let tone = args.next().is_none_or(|m| m == "tone");
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(20);

    let mut connected = connect(ConnectOptions {
        url: url.clone(),
        nickname: nickname.clone(),
        server_password: None,
        identity: Identity::generate(),
        software: ClientSoftware { name: "peer".into(), version: "0".into(), platform: Platform::Desktop },
    })
    .await?;
    println!("peer {nickname}: connected to {url}, {} clients online", connected.welcome.clients.len());

    // PEER_SAY="text": say it in the channel shortly after joining (chat checks).
    if let Ok(text) = std::env::var("PEER_SAY") {
        let me = connected.welcome.session;
        if let Some(channel) = connected.welcome.clients.iter().find(|c| c.id == me).map(|c| c.channel) {
            let connection = connected.connection.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(4)).await;
                match connection.chat(vc_proto::ChatTarget::Channel(channel), text).await {
                    Ok(m) => println!("peer: said {:?}", m.text),
                    Err(e) => println!("peer: chat failed: {e}"),
                }
            });
        }
    }

    let heard = Arc::new(Heard::default());
    let server =
        tokio::net::lookup_host(url.trim_start_matches("ws://").trim_end_matches("/ws")).await?.next().unwrap();
    let handle = voice::start(&connected.connection, Box::new(FakeIo { tone, heard: heard.clone() }), server).await?;
    println!("peer {nickname}: voice started (tone={tone})");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let s = handle.stats();
                println!(
                    "peer {nickname}: link={:?} sent={} received={} speaker_rms={:.4} peak={:.4}",
                    s.link, s.frames_sent, s.frames_received, heard.rms(), heard.peak_rms()
                );
            }
            ev = connected.events.recv() => match ev {
                Some(ev) => println!("peer {nickname}: event {}", serde_json::to_string(&ev)?.chars().take(400).collect::<String>()),
                None => break,
            },
            _ = tokio::time::sleep_until(deadline) => break,
        }
    }
    Ok(())
}
