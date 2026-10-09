//! Measures how long a TeamSpeak server takes to relay one voice frame
//! between two clients on the same host.
//!   cargo run --release -p vc-client --example ts_latency -- 127.0.0.1:9987 [frames]
use std::time::{Duration, Instant};

use audiopus::{Application, Channels, SampleRate, coder::Encoder};
use vc_client::teamspeak::{self, TsOptions};

async fn client(address: &str, name: &str) -> anyhow::Result<teamspeak::TsConnected> {
    let identity = teamspeak::load_identity(&std::env::temp_dir().join(format!("vc-ts-latency-{name}.json")))?;
    teamspeak::connect(TsOptions { address: address.into(), nickname: name.into(), server_password: None, identity })
        .await
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let address = args.next().unwrap_or_else(|| "127.0.0.1:9987".into());
    let frames: usize = args.next().map(|n| n.parse()).transpose()?.unwrap_or(100);
    let speaker = client(&address, "latency-a").await?;
    let mut listener = client(&address, "latency-b").await?;

    let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)?;
    let pcm: Vec<f32> = (0..960).map(|i| (i as f32 * 0.05).sin() * 0.3).collect();
    let mut packet = vec![0u8; 1275];
    let len = encoder.encode_float(&pcm, &mut packet)?;
    let frame = packet[..len].to_vec();

    let mut samples = Vec::new();
    for _ in 0..frames {
        let sent = Instant::now();
        speaker.handle.audio_out.send(frame.clone()).await?;
        match tokio::time::timeout(Duration::from_millis(500), listener.audio_in.recv()).await {
            Ok(Some(_)) => samples.push(sent.elapsed()),
            _ => println!("frame lost"),
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    samples.sort();
    if samples.is_empty() {
        anyhow::bail!("no frames relayed");
    }
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
    println!(
        "relayed {}/{frames}: min {:?} p50 {:?} p95 {:?} max {:?}",
        samples.len(),
        samples[0],
        at(0.5),
        at(0.95),
        samples[samples.len() - 1]
    );
    Ok(())
}
