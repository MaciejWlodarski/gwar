//! Connects to a TeamSpeak server in TeamSpeak mode and sends a chat message,
//! printing how long the reply takes.
//!   cargo run -p vc-desktop --example ts_probe -- 127.0.0.1:9987 nick "text"
use std::time::Instant;

use vc_client::teamspeak::{self, TsOptions};
use vc_proto::{ChatTarget, Request};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let address = args.next().unwrap_or_else(|| "127.0.0.1:9987".into());
    let nickname = args.next().unwrap_or_else(|| "probe".into());
    let text = args.next().unwrap_or_else(|| "hello from ts_probe".into());
    let ts = teamspeak::connect(TsOptions {
        address,
        nickname,
        server_password: None,
        identity: teamspeak::load_identity(&std::env::temp_dir().join("vc-ts-probe-identity.json"))?,
    })
    .await?;
    let me = ts.welcome.session;
    let channel = ts.welcome.clients.iter().find(|c| c.id == me).map(|c| c.channel).unwrap_or(1);
    println!("connected as {me} in channel {channel}");
    let t = Instant::now();

    let r = ts.handle.request(Request::ChatSend { target: ChatTarget::Channel(channel), text }).await;
    println!("chat reply after {:?}: {r:?}", t.elapsed());
    Ok(())
}
