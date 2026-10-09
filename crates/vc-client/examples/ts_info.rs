//! Connects to a TeamSpeak server in TeamSpeak mode, prints what our client
//! sees (channels and people), and leaves. Sends nothing else.
//!   cargo run --release -p vc-client --example ts_info -- piku.me [nickname]
use std::time::{Duration, Instant};

use vc_client::teamspeak::{self, TsOptions};
use vc_proto::Event;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let address = args.next().unwrap_or_else(|| "127.0.0.1:9987".into());
    let nickname = args.next().unwrap_or_else(|| "vc-probe".into());
    let identity = teamspeak::load_identity(&std::env::temp_dir().join("vc-ts-info-identity.json"))?;
    let started = Instant::now();
    let mut ts = teamspeak::connect(TsOptions { address, nickname, server_password: None, identity }).await?;
    let w = &ts.welcome;
    println!("connected in {:?} as session {} — server \"{}\"", started.elapsed(), w.session, w.server.name);
    let mut channels = w.channels.clone();
    channels.sort_by_key(|c| (c.parent, c.position, c.id));
    for c in &channels {
        let people: Vec<_> =
            w.clients.iter().filter(|cl| cl.channel == Some(c.id)).map(|cl| cl.nickname.as_str()).collect();
        let lock = if c.has_password { " 🔒" } else { "" };
        let indent = if c.parent.is_some() { "    " } else { "  " };
        println!("{indent}#{} {}{lock} {people:?}", c.id, c.name);
    }
    println!("{} channels, {} clients", w.channels.len(), w.clients.len());

    // Stay a moment to see live events, then leave cleanly.
    let until = Instant::now() + Duration::from_secs(3);
    while let Ok(Some(event)) = tokio::time::timeout_at(until.into(), ts.events.recv()).await {
        if !matches!(event, Event::VoiceTalking { .. }) {
            println!("event: {event:?}");
        }
    }
    drop(ts.handle);
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(3), ts.events.recv()).await {
        if let Event::Disconnected { reason } = event {
            println!("left: {reason:?}");
            break;
        }
    }
    Ok(())
}
