//! Someone to talk to while trying the mobile app against a local server:
//! joins the default channel, says hello and answers every channel message.
//!
//! cargo run -p gwar-app --example peer -- ws://127.0.0.1:8790 Ola

use vc_client::{ConnectOptions, Identity, connect};
use vc_proto::{ChatTarget, ClientSoftware, Event, Platform};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let address = args.next().unwrap_or_else(|| "ws://127.0.0.1:8790".into());
    let software = ClientSoftware { name: "Gwar peer".into(), version: "0".into(), platform: Platform::Desktop };
    let url = format!("{}/ws", address.trim_end_matches('/'));
    let mut options = ConnectOptions::new(url, Identity::generate(), software);
    options.nickname = args.next().unwrap_or_else(|| "Ola".into());
    options.trust_on_first_use = true;
    let mut connected = connect(options).await?;
    let lobby = connected.welcome.server.default_channel;
    connected.connection.join(lobby, None).await?;
    connected.connection.chat(ChatTarget::Channel(lobby), "Hello from the peer").await?;
    println!("in channel {lobby} as {}; Ctrl-C to quit", connected.welcome.uid);
    while let Some(event) = connected.events.recv().await {
        if let Event::ChatMessage(m) = event
            && m.author_uid != connected.welcome.uid
            && matches!(m.target, ChatTarget::Channel(_))
        {
            connected.connection.chat(m.target, format!("Got it: {}", m.text)).await?;
        }
    }
    Ok(())
}
