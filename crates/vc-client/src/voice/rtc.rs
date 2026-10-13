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
use vc_proto::{
    AUDIO_SLOTS,
    rtp::{SLOT_GENERATION_ID, SlotGeneration, slot_generation_extension},
};

use super::{Counters, Incoming, LinkState, Outgoing, io::OutputTiming, jitter::Clock};
use std::sync::Arc;

/// Compatibility fallback for older servers without the negotiated generation
/// extension. Large RTP discontinuities reset playout; delivery stalls do not.
#[derive(Default)]
struct Stream {
    last: Option<(u64, u64)>,
    generation: u64,
}

impl Stream {
    fn observe(&mut self, sequence: u64, timestamp: u64) -> u64 {
        if let Some((seq, ts)) = self.last {
            if sequence <= seq {
                return self.generation;
            }
            // Opus supports up to 120 ms per packet. Allow that much media time
            // for every lost packet before declaring a discontinuity.
            if timestamp < ts || timestamp - ts > (sequence - seq).saturating_mul(5760) + 24_000 {
                self.generation += 1;
            }
        }
        self.last = Some((sequence, timestamp));
        self.generation
    }
}

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
    wake: Arc<OutputTiming>,
) -> Result<()>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    let socket = UdpSocket::bind(SocketAddr::new(local_ip, 0)).await.context("bind media socket")?;
    let local = socket.local_addr()?;
    let mut rtc = Rtc::builder()
        .set_extension(SLOT_GENERATION_ID, slot_generation_extension())
        .clear_codecs()
        .enable_opus(true, false)
        .build(Instant::now());
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
        let mut streams: [Stream; AUDIO_SLOTS] = std::array::from_fn(|_| Stream::default());
        loop {
            let deadline = loop {
                match rtc.poll_output() {
                    Ok(Output::Timeout(t)) => break t,
                    Ok(Output::Transmit(t)) => {
                        let _ = socket.try_send_to(&t.contents, t.destination);
                    }
                    Ok(Output::Event(Event::MediaData(data))) => {
                        if let Some(slot) = slots.iter().position(|m| *m == data.mid) {
                            let rtp_time = data.time.rebase(Frequency::FORTY_EIGHT_KHZ).numer();
                            let generation = data
                                .ext_vals
                                .user_values
                                .get::<SlotGeneration>()
                                .map_or_else(|| streams[slot].observe(**data.seq_range.start(), rtp_time), |g| g.0);
                            let item = Incoming {
                                slot,
                                arrived: Instant::now(),
                                clock: Clock::Rtp,
                                rtp_time,
                                generation,
                                opus: data.data.to_vec(),
                            };
                            counters.frame_received();
                            let _ = incoming.try_send(item);
                            wake.wake();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_discontinuity_changes_generation_but_delivery_stalls_and_loss_do_not() {
        let mut stream = Stream::default();
        assert_eq!(stream.observe(0, 48_000), 0);
        for i in 1..=25 {
            // a 500 ms burst, same media timeline
            assert_eq!(stream.observe(i, 48_000 + i * 960), 0);
        }
        assert_eq!(stream.observe(50, 96_000), 0); // packet loss
        assert_eq!(stream.observe(51, 144_960), 1); // large media discontinuity
        assert_eq!(stream.observe(52, 145_920), 1);
        // A discontinuity survives a lost boundary packet too.
        assert_eq!(stream.observe(54, 195_840), 2);
    }
}
