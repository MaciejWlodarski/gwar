//! WebRTC client session (str0m) toward the server's SFU: one send-only
//! microphone plus `AUDIO_SLOTS` receive-only slots, as the protocol requires.

use std::{net::IpAddr, net::SocketAddr, time::Instant};

use anyhow::{Context, Result, ensure};
use gwar_proto::{
    AUDIO_SLOTS,
    rtp::{SLOT_GENERATION_ID, SlotGeneration, slot_generation_extension},
};
use str0m::{
    Candidate, Event, Input, Output, Rtc,
    change::SdpAnswer,
    format::Codec,
    media::{Direction, Frequency, MediaKind, Mid},
    net::{Protocol, Receive},
    rtp::{ExtensionValues, RtpWrite},
};
use tokio::{net::UdpSocket, sync::mpsc, time::sleep_until};

use super::{
    Counters, Incoming, LinkState, Outgoing,
    io::OutputTiming,
    jitter::Clock,
    keys::{Crypto, RunGate},
};
use std::sync::Arc;

/// Marks the link lost however the transport task ends.
struct LostOnDrop(Arc<Counters>);
impl Drop for LostOnDrop {
    fn drop(&mut self) {
        self.0.set_link(LinkState::Lost);
    }
}

// The session channels, counters, crypto and the retirement gate are independent inputs.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn start<F, Fut>(
    signal: F,
    local_ip: IpAddr,
    mut outgoing: mpsc::Receiver<Outgoing>,
    incoming: std::sync::mpsc::SyncSender<Incoming>,
    counters: Arc<Counters>,
    wake: Arc<OutputTiming>,
    crypto: Option<Crypto>,
    gate: RunGate,
) -> Result<()>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    let socket = UdpSocket::bind(SocketAddr::new(local_ip, 0)).await.context("bind media socket")?;
    let local = socket.local_addr()?;
    // RTP mode: our jitter buffer orders packets itself. str0m's frame mode would
    // hold up to 15 packets behind every downlink loss and release them as a burst.
    let mut rtc = Rtc::builder()
        .set_extension(SLOT_GENERATION_ID, slot_generation_extension())
        .set_rtp_mode(true)
        .clear_codecs()
        .enable_opus(true, false)
        .build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp")?);
    let (mic, slots, offer, pending) = loop {
        let mut api = rtc.sdp_api();
        let mic = api.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None);
        let slots: Vec<Mid> =
            (0..AUDIO_SLOTS).map(|_| api.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None)).collect();
        // str0m draws each mid at random, unique only among media already applied: media of one
        // offer can collide (about 1 in 6000 offers here), so draw them again.
        if distinct(mic, &slots) {
            let (offer, pending) = api.apply().context("create offer")?;
            break (mic, slots, offer, pending);
        }
    };
    // Unknown SDP attributes are ignored by older servers. Updated servers avoid
    // applying their fixed gate a second time to quiet speech or push-to-talk.
    let sdp = offer.to_sdp_string().replacen("\r\nm=", "\r\na=x-gwar-input-gate:1\r\na=x-gwar-native-vad:1\r\nm=", 1);
    let answer = signal(sdp).await?;
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

    let pt = rtc
        .media(mic)
        .and_then(|m| {
            rtc.codec_config()
                .params()
                .iter()
                .find(|p| m.remote_pts().contains(&p.pt()) && p.spec().codec == Codec::Opus)
        })
        .map(|p| p.pt())
        .context("native voice answer lacks Opus on the microphone")?;

    // Crypto is attached before either loop runs: no frame passes unencrypted.
    let (mut crypto, mut commands) = match crypto {
        Some(Crypto { run, commands }) => (Some(run), Some(commands)),
        None => (None, None),
    };
    tokio::spawn(async move {
        let _lost = LostOnDrop(counters.clone());
        // Stops the keys however the task ends.
        struct StopKeys<'a>(&'a mut Option<super::keys::RunCrypto>);
        impl Drop for StopKeys<'_> {
            fn drop(&mut self) {
                if let Some(crypto) = self.0.as_mut() {
                    crypto.stop();
                }
            }
        }
        let crypto = StopKeys(&mut crypto);
        let mut buf = vec![0u8; 2048];
        let mut last_ticks = None;
        let offset = rand::random::<u32>();
        let mut sequence = u64::from(rand::random::<u16>()); // RFC 3550 §5.1
        loop {
            let deadline = loop {
                match rtc.poll_output() {
                    Ok(Output::Timeout(t)) => break t,
                    Ok(Output::Transmit(t)) => {
                        // Every datagram leaves under the gate: nothing queued is sent after retirement.
                        let Some(_open) = gate.enter() else { return };
                        let _ = socket.try_send_to(&t.contents, t.destination);
                    }
                    Ok(Output::Event(Event::RtpPacket(packet))) => {
                        let mid = rtc.direct_api().stream_rx(&packet.header.ssrc).map(|s| s.mid());
                        if let Some(slot) = slots.iter().position(|m| Some(*m) == mid) {
                            let rtp_time = packet.time.rebase(Frequency::FORTY_EIGHT_KHZ).numer();
                            let generation = packet
                                .header
                                .ext_vals
                                .user_values
                                .get::<SlotGeneration>()
                                .copied()
                                .map(super::Generation::Rtp);
                            // A retired run plays nothing; only an authenticated packet can
                            // reach playout or reset a slot.
                            let Some(_open) = gate.enter() else { return };
                            let opus = match crypto.0.as_mut() {
                                Some(keys) => match keys.decrypt(&packet.payload) {
                                    (Some(opus), _) => opus,
                                    (None, true) => continue,
                                    (None, false) => return,
                                },
                                None => packet.payload.to_vec(),
                            };
                            let item = Incoming {
                                slot,
                                arrived: Instant::now(),
                                clock: Clock::Rtp,
                                rtp_time,
                                generation,
                                opus,
                            };
                            counters.frame_received();
                            let _ = incoming.try_send(item);
                            wake.wake();
                        }
                    }
                    Ok(Output::Event(Event::Connected)) => {
                        // Capture queued while ICE/DTLS was pending is already old.
                        // Bound the drain to this snapshot so a live producer cannot
                        // starve the transport, then resume at its capture clock.
                        for _ in 0..outgoing.len() {
                            let _ = outgoing.try_recv();
                        }
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
            let deadline = match crypto.0.as_mut() {
                Some(keys) => deadline.min(keys.tick(Instant::now())),
                None => deadline,
            };
            tokio::select! {
                () = gate.closed() => return,
                command = async { match commands.as_mut() { Some(c) => c.recv().await, None => std::future::pending().await } } => {
                    let (Some(command), Some(keys)) = (command, crypto.0.as_mut()) else { return };
                    let Some(_open) = gate.enter() else {
                        keys.refuse(command);
                        return;
                    };
                    if !keys.command(command) {
                        return;
                    }
                }
                frame = outgoing.recv() => {
                    let Some(frame) = frame else { return };
                    // str0m buffers writes before SRTP is ready. Replaying that
                    // capture after a handshake retry creates permanent playout lag.
                    if !rtc.is_connected() {
                        continue;
                    }
                    if !frame.fresh_at(Instant::now(), &counters) {
                        continue;
                    }
                    let Some(ticks) = send_ticks(frame.capture_position, last_ticks) else {
                        counters.capture_position_dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        continue;
                    };
                    last_ticks = Some(ticks);
                    if rtc.direct_api().stream_tx_by_mid(mic, None).is_none() {
                        continue;
                    }
                    // Nothing queued is encrypted or sent after retirement.
                    let Some(_open) = gate.enter() else { return };
                    // The send gate and counter reservation sit right before the RTP write.
                    let payload = match crypto.0.as_mut() {
                        Some(keys) => match keys.encrypt(&frame.opus) {
                            (Some(sealed), _) => sealed,
                            (None, true) => continue,
                            (None, false) => return,
                        },
                        None => frame.opus,
                    };
                    let ext_vals = ExtensionValues { audio_level: Some(frame.level), voice_activity: Some(frame.speech), ..Default::default() };
                    let write = RtpWrite::new(pt, sequence.into(), (ticks as u32).wrapping_add(offset), Instant::now(), payload);
                    if let Some(stream) = rtc.direct_api().stream_tx_by_mid(mic, None) {
                        stream.write_rtp(write.ext_vals(ext_vals));
                        sequence += 1;
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

// Capture advances through gates and queue drops. Defend against faulty custom I/O.
fn send_ticks(capture_position: u64, last: Option<u64>) -> Option<u64> {
    last.is_none_or(|previous| capture_position > previous).then_some(capture_position)
}

// str0m discards extmap directions. Read the answerer's (server's) direction
// in the matching m-section; no qualifier inherits the media direction.
fn distinct(mic: Mid, slots: &[Mid]) -> bool {
    let mut mids: Vec<Mid> = slots.iter().copied().chain([mic]).collect();
    mids.sort();
    mids.dedup();
    mids.len() == slots.len() + 1
}

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
    fn media_of_one_offer_have_distinct_mids() {
        let mid = |s: &str| Mid::from(s);
        assert!(distinct(mid("a"), &[mid("b"), mid("c")]));
        assert!(!distinct(mid("a"), &[mid("b"), mid("a")]));
        assert!(!distinct(mid("a"), &[mid("b"), mid("b")]));
    }

    #[tokio::test]
    async fn a_retired_run_sends_no_datagram() {
        use tokio::time::{Duration, timeout};
        let remote_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut remote = Rtc::builder()
            .set_extension(SLOT_GENERATION_ID, slot_generation_extension())
            .clear_codecs()
            .enable_opus(true, false)
            .build(Instant::now());
        remote.add_local_candidate(Candidate::host(remote_socket.local_addr().unwrap(), "udp").unwrap());
        let (outgoing, outgoing_rx) = mpsc::channel(8);
        let (incoming, _incoming_rx) = std::sync::mpsc::sync_channel(16);
        let gate = RunGate::new();
        let start_rtc = |gate: RunGate| {
            start(
                move |offer| {
                    let answer = remote.sdp_api().accept_offer(SdpOffer::from_sdp_string(&offer).unwrap()).unwrap();
                    std::future::ready(Ok(answer.to_sdp_string()))
                },
                remote_socket.local_addr().unwrap().ip(),
                outgoing_rx,
                incoming,
                Arc::new(Counters::default()),
                Arc::new(OutputTiming::default()),
                None,
                gate,
            )
        };
        start_rtc(gate.clone()).await.unwrap();
        // The live run sends ICE checks to the remote candidate.
        let mut buf = [0u8; 2048];
        timeout(Duration::from_secs(3), remote_socket.recv_from(&mut buf))
            .await
            .expect("a live run transmits")
            .unwrap();
        gate.close();
        // Drain anything sent before the close completed, then expect silence.
        while timeout(Duration::from_millis(50), remote_socket.recv_from(&mut buf)).await.is_ok() {}
        for i in 0..4 {
            let _ = outgoing.try_send(Outgoing {
                captured_at: Instant::now(),
                capture_position: i * 480,
                opus: vec![0xf0, 1],
                level: -10,
                speech: true,
            });
        }
        assert!(
            timeout(Duration::from_millis(1500), remote_socket.recv_from(&mut buf)).await.is_err(),
            "a retired run transmitted"
        );
    }

    #[tokio::test]
    async fn connecting_capture_is_not_replayed_after_the_handshake() {
        use std::sync::atomic::Ordering;
        use tokio::time::{Duration, timeout};

        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let local = socket.local_addr().unwrap();
        let mut remote = Rtc::builder()
            .set_extension(SLOT_GENERATION_ID, slot_generation_extension())
            .set_rtp_mode(true)
            .clear_codecs()
            .enable_opus(true, false)
            .build(Instant::now());
        remote.add_local_candidate(Candidate::host(local, "udp").unwrap());
        let (outgoing, outgoing_rx) = mpsc::channel(1);
        let (incoming, _incoming_rx) = std::sync::mpsc::sync_channel(16);
        let counters = Arc::new(Counters::default());
        start(
            |offer| {
                let answer = remote.sdp_api().accept_offer(SdpOffer::from_sdp_string(&offer).unwrap()).unwrap();
                std::future::ready(Ok(answer.to_sdp_string()))
            },
            local.ip(),
            outgoing_rx,
            incoming,
            counters.clone(),
            Arc::new(OutputTiming::default()),
            None,
            RunGate::new(),
        )
        .await
        .unwrap();

        // Do not service the remote handshake yet. The bounded channel plus the
        // final permit proves the native transport consumed every old frame.
        timeout(Duration::from_secs(5), async {
            for i in 0..100 {
                outgoing
                    .send(Outgoing {
                        captured_at: Instant::now(),
                        capture_position: i * 480,
                        opus: vec![0xf0, 1],
                        level: -10,
                        speech: true,
                    })
                    .await
                    .unwrap();
            }
            drop(outgoing.reserve().await.unwrap());
        })
        .await
        .unwrap();
        assert_eq!(counters.link.load(Ordering::Relaxed), 0);

        let (received_tx, mut received) = mpsc::channel(256);
        let task = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            loop {
                let deadline = loop {
                    match remote.poll_output().unwrap() {
                        Output::Timeout(at) => break at,
                        Output::Transmit(packet) => {
                            socket.send_to(&packet.contents, packet.destination).await.unwrap();
                        }
                        Output::Event(Event::RtpPacket(packet)) => {
                            received_tx.send((packet.header.timestamp, packet.payload.to_vec())).await.unwrap();
                        }
                        _ => {}
                    }
                };
                tokio::select! {
                    packet = socket.recv_from(&mut buf) => {
                        let (n, source) = packet.unwrap();
                        remote.handle_input(Input::Receive(Instant::now(), Receive {
                            proto: Protocol::Udp, source, destination: local,
                            contents: buf[..n].try_into().unwrap(),
                        })).unwrap();
                    }
                    _ = sleep_until(deadline.into()) => {}
                }
                remote.handle_input(Input::Timeout(Instant::now())).unwrap();
            }
        });
        let result = timeout(Duration::from_secs(5), async {
            while counters.link.load(Ordering::Relaxed) == 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            assert_eq!(counters.link.load(Ordering::Relaxed), 1);
            // A connected consumer stall must use the same age bound as PCM.
            // Inject time instead of sleeping to create stale encoded frames.
            for position in [46_080, 47_040] {
                outgoing
                    .send(Outgoing {
                        captured_at: Instant::now() - Duration::from_millis(200),
                        capture_position: position,
                        opus: vec![0xf0, 9],
                        level: -10,
                        speech: true,
                    })
                    .await
                    .unwrap();
            }
            outgoing
                .send(Outgoing {
                    captured_at: Instant::now(),
                    capture_position: 48_000,
                    opus: vec![0xf0, 2],
                    level: -10,
                    speech: true,
                })
                .await
                .unwrap();
            let first = received.recv().await.unwrap();
            outgoing
                .send(Outgoing {
                    captured_at: Instant::now(),
                    capture_position: 48_960,
                    opus: vec![0xf0, 3],
                    level: -10,
                    speech: true,
                })
                .await
                .unwrap();
            (first, received.recv().await.unwrap())
        })
        .await;
        task.abort();
        let _ = task.await;
        let (first, next) = result.unwrap();
        assert_eq!(first.1, vec![0xf0, 2], "pre-connection capture must never enter SRTP");
        assert_eq!(next.1, vec![0xf0, 3]);
        assert_eq!(counters.capture_stale_frames.load(Ordering::Relaxed), 2);
        // 5b adds a random run offset; capture-clock deltas remain exact.
        assert_eq!(next.0.wrapping_sub(first.0), 960);
    }

    #[test]
    fn timestamp_quantum_tracks_capture_and_batches_do_not_reuse_ticks() {
        let frame = super::super::FRAME as u64;
        assert_eq!(send_ticks(0, None), Some(0));
        assert_eq!(send_ticks(0, Some(0)), None);
        assert_eq!(send_ticks(100, Some(480)), None);
        assert_eq!(send_ticks(960, Some(frame)), Some(960));
        assert_eq!(send_ticks(48_000, Some(960)), Some(48_000));
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
                None,
                RunGate::new(),
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
                    None,
                    RunGate::new(),
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
