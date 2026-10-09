//! WebRTC client session (str0m) toward the server's SFU: one send-only
//! microphone plus `AUDIO_SLOTS` receive-only slots, as the protocol requires.

use std::{net::IpAddr, net::SocketAddr, time::Instant};

use anyhow::{Context, Result};
use str0m::{
    Candidate, Event, Input, Output, Rtc,
    change::SdpAnswer,
    format::Codec,
    media::{Direction, Frequency, MediaKind, MediaTime, Mid},
    net::{Protocol, Receive},
};
use tokio::{net::UdpSocket, sync::mpsc, time::sleep_until};
use vc_proto::AUDIO_SLOTS;

use super::{Counters, Incoming, LinkState, Outgoing};
use std::sync::Arc;

/// Marks the link lost however the transport task ends.
struct LostOnDrop(Arc<Counters>);
impl Drop for LostOnDrop {
    fn drop(&mut self) {
        self.0.set_link(LinkState::Lost);
    }
}

pub(crate) async fn start<F, Fut>(
    signal: F,
    local_ip: IpAddr,
    mut outgoing: mpsc::Receiver<Outgoing>,
    incoming: std::sync::mpsc::SyncSender<Incoming>,
    counters: Arc<Counters>,
) -> Result<()>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    let socket = UdpSocket::bind(SocketAddr::new(local_ip, 0)).await.context("bind media socket")?;
    let local = socket.local_addr()?;
    let mut rtc = Rtc::builder().clear_codecs().enable_opus(true, false).build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp")?);
    let mut api = rtc.sdp_api();
    let mic = api.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None);
    let slots: Vec<Mid> =
        (0..AUDIO_SLOTS).map(|_| api.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None)).collect();
    let (offer, pending) = api.apply().context("create offer")?;
    let answer = signal(offer.to_sdp_string()).await?;
    rtc.sdp_api().accept_answer(pending, SdpAnswer::from_sdp_string(&answer)?)?;

    tokio::spawn(async move {
        let _lost = LostOnDrop(counters.clone());
        let mut buf = vec![0u8; 2048];
        // RTP time follows the wall clock so pauses in speech stay pauses.
        let epoch = Instant::now();
        loop {
            let deadline = loop {
                match rtc.poll_output() {
                    Ok(Output::Timeout(t)) => break t,
                    Ok(Output::Transmit(t)) => {
                        let _ = socket.try_send_to(&t.contents, t.destination);
                    }
                    Ok(Output::Event(Event::MediaData(data))) => {
                        if let Some(slot) = slots.iter().position(|m| *m == data.mid) {
                            let item = Incoming {
                                slot,
                                rtp_time: data.time.rebase(Frequency::FORTY_EIGHT_KHZ).numer(),
                                opus: data.data.to_vec(),
                            };
                            counters.frame_received();
                            let _ = incoming.try_send(item);
                        }
                    }
                    Ok(Output::Event(Event::Connected)) => {
                        tracing::info!("voice transport connected");
                        counters.set_link(LinkState::Connected);
                    }
                    Ok(Output::Event(Event::IceConnectionStateChange(str0m::IceConnectionState::Disconnected))) => {
                        tracing::info!("voice connection lost");
                        return;
                    }
                    Ok(Output::Event(_)) => {}
                    Err(e) => {
                        tracing::warn!("rtc: {e}");
                        return;
                    }
                }
            };
            if !rtc.is_alive() {
                return;
            }
            tokio::select! {
                frame = outgoing.recv() => {
                    let Some(frame) = frame else { return };
                    let ticks = (epoch.elapsed().as_micros() as u64 * 48 / 1000) / 960 * 960;
                    let pt = rtc.writer(mic).and_then(|w| w.payload_params().find(|p| p.spec().codec == Codec::Opus).map(|p| p.pt()));
                    if let (Some(writer), Some(pt)) = (rtc.writer(mic), pt) {
                        {
                            let time = MediaTime::new(ticks, Frequency::FORTY_EIGHT_KHZ);
                            let _ = writer.audio_level(frame.level, frame.speech).write(pt, Instant::now(), time, frame.opus);
                        }
                    }
                }
                received = socket.recv_from(&mut buf) => {
                    let Ok((n, source)) = received else { continue };
                    let Ok(contents) = buf[..n].try_into() else { continue };
                    let input = Input::Receive(Instant::now(), Receive { proto: Protocol::Udp, source, destination: local, contents });
                    if rtc.handle_input(input).is_err() {
                        return;
                    }
                }
                _ = sleep_until(deadline.into()) => {}
            }
            if rtc.handle_input(Input::Timeout(Instant::now())).is_err() {
                return;
            }
        }
    });
    Ok(())
}
