//! Real SRTP peer for opaque-media and official-server boundary probes.
#![allow(dead_code)]
use super::Client;
use serde_json::json;
use std::time::{Duration, Instant};
use str0m::{
    Candidate, Event, Input, Output, Rtc,
    change::SdpAnswer,
    format::Codec,
    media::{Direction, MediaKind},
    net::{Protocol, Receive},
    rtp::{ExtensionValues, RtpWrite},
};
use tokio::{net::UdpSocket, sync::mpsc};
use vc_proto::rtp::{SLOT_GENERATION_ID, SlotGeneration, slot_generation_extension};

pub struct Send {
    pub ssrc: u32,
    pub sequence: u64,
    pub ticks: u32,
    pub payload: Vec<u8>,
    pub level: i8,
}
#[derive(Debug)]
pub struct Received {
    pub payload: Vec<u8>,
    pub ticks: u32,
    pub generation: Option<u16>,
}
pub struct Peer {
    pub ssrc: u32,
    pub tx: mpsc::Sender<Send>,
    pub rx: mpsc::Receiver<Received>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Peer {
    pub async fn start(client: &mut Client, run: u64) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let local = socket.local_addr().unwrap();
        let mut rtc = Rtc::builder()
            .set_rtp_mode(true)
            .set_extension(SLOT_GENERATION_ID, slot_generation_extension())
            .clear_codecs()
            .enable_opus(true, false)
            .build(Instant::now());
        rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
        let mut api = rtc.sdp_api();
        let mic = api.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None);
        for _ in 0..vc_proto::AUDIO_SLOTS {
            api.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None);
        }
        let (offer, pending) = api.apply().unwrap();
        let sdp = offer.to_sdp_string();
        let ssrc: u32 =
            sdp.lines().find_map(|line| line.strip_prefix("a=ssrc:")?.split_whitespace().next()?.parse().ok()).unwrap();
        let state = client.ok("voice.state", json!({})).await["voice"].clone();
        let answer = client
            .ok(
                "voice.offer",
                json!({"channel":state["channel"],"call":state["call"],
            "participation":state["participation"],"run":run,"group":state["group"],"ssrc":ssrc,"sdp":sdp}),
            )
            .await;
        rtc.sdp_api()
            .accept_answer(pending, SdpAnswer::from_sdp_string(answer["sdp"].as_str().unwrap()).unwrap())
            .unwrap();
        let pt = rtc.codec_config().params().iter().find(|p| p.spec().codec == Codec::Opus).unwrap().pt();
        let (tx, mut commands) = mpsc::channel::<Send>(256);
        let (received, rx) = mpsc::channel(512);
        let (ready, connected) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut ready = Some(ready);
            let mut bytes = [0; 2048];
            let mut active_ssrc = ssrc;
            loop {
                let deadline = loop {
                    match rtc.poll_output().unwrap() {
                        Output::Timeout(at) => break at,
                        Output::Transmit(t) => {
                            socket.send_to(&t.contents, t.destination).await.unwrap();
                        }
                        Output::Event(Event::Connected) => {
                            if let Some(tx) = ready.take() {
                                let _ = tx.send(());
                            }
                        }
                        Output::Event(Event::RtpPacket(p)) => {
                            let _ = received.try_send(Received {
                                payload: p.payload.to_vec(),
                                ticks: p.header.timestamp,
                                generation: p.header.ext_vals.user_values.get::<SlotGeneration>().map(|g| g.0),
                            });
                        }
                        _ => {}
                    }
                };
                tokio::select! {
                    packet=commands.recv()=> {
                        let Some(p)=packet else {return};
                        let mut direct=rtc.direct_api();
                        if p.ssrc != active_ssrc {
                            direct.reset_stream_tx(mic,None,p.ssrc.into(),None).unwrap();
                            active_ssrc=p.ssrc;
                        }
                        let stream=direct.stream_tx_by_mid(mic,None).unwrap();
                        stream.write_rtp(RtpWrite::new(pt,p.sequence.into(),p.ticks,Instant::now(),p.payload)
                            .ext_vals(ExtensionValues {audio_level:Some(p.level),voice_activity:Some(p.level > -50),..Default::default()}));
                    },
                    got=socket.recv_from(&mut bytes)=> {
                        let (n,source)=got.unwrap();
                        rtc.handle_input(Input::Receive(Instant::now(),Receive {proto:Protocol::Udp,source,destination:local,contents:bytes[..n].try_into().unwrap()})).unwrap();
                    },
                    _=tokio::time::sleep_until(deadline.into())=>{}
                }
                rtc.handle_input(Input::Timeout(Instant::now())).unwrap();
            }
        });
        tokio::time::timeout(Duration::from_secs(5), connected).await.unwrap().unwrap();
        Self { ssrc, tx, rx, task }
    }
    pub async fn send(&self, sequence: u64, ticks: u32, payload: Vec<u8>) {
        self.tx.send(Send { ssrc: self.ssrc, sequence, ticks, payload, level: -20 }).await.unwrap();
    }
    pub async fn receive(&mut self) -> Received {
        tokio::time::timeout(Duration::from_secs(3), self.rx.recv()).await.expect("SRTP media delivery").unwrap()
    }
    pub async fn silent(&mut self) {
        assert!(
            tokio::time::timeout(Duration::from_millis(100), self.rx.recv()).await.is_err(),
            "unexpected admitted payload"
        );
    }
}
