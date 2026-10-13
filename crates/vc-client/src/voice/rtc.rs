//! WebRTC client session (str0m) toward the server's SFU: one send-only
//! microphone plus `AUDIO_SLOTS` receive-only slots, as the protocol requires.

use std::{net::IpAddr, net::SocketAddr, time::Instant};

use anyhow::{Context, Result, ensure};
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
    for mid in &slots {
        let extension_id =
            rtc.media(*mid).and_then(|media| media.remote_extmap().id_of(slot_generation_extension())).with_context(
                || format!("native voice answer lacks the slot-generation extension on receive media {mid}"),
            )?;
        ensure!(
            answer_sends_generation(&answer, *mid, extension_id),
            "native voice answer does not send the slot-generation extension on receive media {mid}"
        );
    }

    tokio::spawn(async move {
        let _lost = LostOnDrop(counters.clone());
        let mut buf = vec![0u8; 2048];
        // RTP time follows the wall clock so pauses in speech stay pauses.
        let epoch = Instant::now();
        let mut last_ticks = None;
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
                            let generation =
                                data.ext_vals.user_values.get::<SlotGeneration>().copied().map(super::Generation::Rtp);
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
                    let ticks = send_ticks(epoch.elapsed(), last_ticks);
                    last_ticks = Some(ticks);
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

// Quantize to capture duration, but never reuse a timestamp when callbacks or
// transport wakeups batch multiple packets. Keep wall-clock gaps across pauses.
fn send_ticks(elapsed: std::time::Duration, last: Option<u64>) -> u64 {
    let frame = super::FRAME as u64;
    let wall = (elapsed.as_micros() as u64 * 48 / 1000) / frame * frame;
    wall.max(last.map_or(0, |ticks| ticks + frame))
}

// str0m discards extmap directions. Read the answerer's (server's) direction
// in the matching m-section; no qualifier inherits the media direction.
fn answer_sends_generation(answer: &str, mid: Mid, id: u8) -> bool {
    let Some(media) = answer
        .split("\nm=")
        .skip(1)
        .map(|section| section.trim_end_matches('\r'))
        .find(|section| section.lines().any(|line| line.strip_prefix("a=mid:") == Some(&*mid)))
    else {
        return false;
    };
    media.lines().filter_map(|line| line.strip_prefix("a=extmap:")).any(|extmap| {
        let mut fields = extmap.split_whitespace();
        let Some(mapping) = fields.next() else { return false };
        let (value, direction) =
            mapping.split_once('/').map_or((mapping, None), |(value, direction)| (value, Some(direction)));
        value.parse::<u8>() == Ok(id)
            && fields.next() == Some(slot_generation_extension().as_uri())
            && matches!(direction, None | Some("sendonly" | "sendrecv"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use str0m::change::SdpOffer;

    #[test]
    fn timestamp_quantum_tracks_capture_and_batches_do_not_reuse_ticks() {
        use std::time::Duration;
        let frame = super::super::FRAME as u64;
        assert_eq!(send_ticks(Duration::ZERO, None), 0);
        assert_eq!(send_ticks(Duration::ZERO, Some(0)), frame);
        assert_eq!(send_ticks(Duration::from_millis(20), Some(frame)), 960.max(2 * frame));
        assert_eq!(send_ticks(Duration::from_secs(1), Some(960)), 48_000);
    }

    #[test]
    fn generation_direction_matches_the_media_and_mapping() {
        let extension = slot_generation_extension();
        let uri = extension.as_uri();
        for newline in ["\r\n", "\n"] {
            // Put extmap before mid and include extension attributes.
            let answer = [
                "v=0".to_owned(),
                "m=audio 9 UDP/TLS/RTP/SAVPF 111".to_owned(),
                format!("a=extmap:9/sendonly {uri}"),
                "a=mid:mic".to_owned(),
                "m=audio 9 UDP/TLS/RTP/SAVPF 111".to_owned(),
                format!("a=extmap:9/recvonly {uri} extra"),
                "a=mid:slot".to_owned(),
            ]
            .join(newline);
            assert!(answer_sends_generation(&answer, "mic".into(), 9));
            assert!(!answer_sends_generation(&answer, "slot".into(), 9));
            assert!(!answer_sends_generation(&answer, "mic".into(), 8));
            assert!(!answer_sends_generation(&answer, "missing".into(), 9));
            assert!(answer_sends_generation(&answer.replace("/recvonly", "/sendonly"), "slot".into(), 9));
            assert!(!answer_sends_generation(&answer.replace(uri, &format!("{uri}:other")), "mic".into(), 9));
        }
    }

    #[tokio::test]
    async fn start_requires_generation_on_every_receive_slot() {
        // Keep the extension on the microphone, then omit it from each receive
        // slot in turn. A session-wide or first-slot-only check is insufficient.
        for omitted in (1..=AUDIO_SLOTS).map(Some).chain([None, Some(0)]) {
            let (_outgoing, receiver) = mpsc::channel(1);
            let (incoming, _receiver) = std::sync::mpsc::sync_channel(1);
            let result = start(
                |offer| async move {
                    assert!(offer.contains("minptime=10"));
                    assert!(offer.contains("useinbandfec=1"));
                    assert!(!offer.contains("a=ptime:") && !offer.contains("a=maxptime:"));
                    let mut server = Rtc::builder()
                        .set_extension(SLOT_GENERATION_ID, slot_generation_extension())
                        .clear_codecs()
                        .enable_opus(true, false)
                        .build(Instant::now());
                    let answer = server.sdp_api().accept_offer(SdpOffer::from_sdp_string(&offer)?)?.to_sdp_string();
                    assert!(answer.contains("minptime=10"));
                    assert!(!answer.contains("a=ptime:") && !answer.contains("a=maxptime:"));
                    let mut media = None;
                    let answer = answer
                        .lines()
                        .filter(|line| {
                            if line.starts_with("m=") {
                                media = Some(media.map_or(0, |i| i + 1));
                            }
                            !(omitted.is_some()
                                && media == omitted
                                && line.starts_with("a=extmap:")
                                && line.contains(slot_generation_extension().as_uri()))
                        })
                        .collect::<Vec<_>>()
                        .join("\r\n");
                    Ok(format!("{answer}\r\n"))
                },
                "127.0.0.1".parse().unwrap(),
                receiver,
                incoming,
                Arc::new(Counters::default()),
                Arc::new(OutputTiming::default()),
            )
            .await;
            if omitted.is_some_and(|i| i > 0) {
                let error = result.unwrap_err().to_string();
                assert!(error.contains("answer lacks the slot-generation extension on receive media"), "{error}");
            } else {
                result.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn start_requires_sending_generation_on_every_receive_slot() {
        for direction in [None, Some("inactive"), Some("recvonly"), Some("sendonly"), Some("sendrecv")] {
            // Changing only the microphone must not affect receive slots.
            for changed in 0..=AUDIO_SLOTS {
                let (_outgoing, receiver) = mpsc::channel(1);
                let (incoming, _receiver) = std::sync::mpsc::sync_channel(1);
                let result = start(
                    |offer| async move {
                        let mut server = Rtc::builder()
                            .set_extension(SLOT_GENERATION_ID, slot_generation_extension())
                            .clear_codecs()
                            .enable_opus(true, false)
                            .build(Instant::now());
                        let answer = server.sdp_api().accept_offer(SdpOffer::from_sdp_string(&offer)?)?.to_sdp_string();
                        let mut media = None;
                        let answer = answer
                            .lines()
                            .map(|line| {
                                if line.starts_with("m=") {
                                    media = Some(media.map_or(0, |i| i + 1));
                                }
                                if media == Some(changed)
                                    && let Some(extmap) = line.strip_prefix("a=extmap:")
                                    && let Some((id, uri)) = extmap.split_once(' ')
                                    && uri == slot_generation_extension().as_uri()
                                {
                                    let qualifier = direction.map_or(String::new(), |d| format!("/{d}"));
                                    return format!("a=extmap:{id}{qualifier} {uri}");
                                }
                                line.to_owned()
                            })
                            .collect::<Vec<_>>()
                            .join("\r\n");
                        Ok(format!("{answer}\r\n"))
                    },
                    "127.0.0.1".parse().unwrap(),
                    receiver,
                    incoming,
                    Arc::new(Counters::default()),
                    Arc::new(OutputTiming::default()),
                )
                .await;
                if changed > 0 && matches!(direction, Some("inactive" | "recvonly")) {
                    let error = result.unwrap_err().to_string();
                    assert!(error.contains("slot-generation extension on receive media"), "{error}");
                } else {
                    result.unwrap();
                }
            }
        }
    }
}
