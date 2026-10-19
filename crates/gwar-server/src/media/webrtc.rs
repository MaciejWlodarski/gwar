//! WebRTC SFU on a single UDP port (ICE-lite, str0m).
//!
//! Each browser offers one `sendonly` microphone and [`AUDIO_SLOTS`]
//! `recvonly` audio transceivers. Incoming Opus frames are forwarded as-is
//! into a receive slot of every listener; a speaker keeps its slot while it
//! talks, so the client can map slot → user for volume and indicators via the
//! `voice.slot` event. Silence is gated on the server using the RFC 6464
//! audio-level header extension, which keeps idle channels at ~0 bandwidth.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use gwar_proto::{AUDIO_SLOTS, Event, ServerFrame, SessionId, VoiceTransportBinding};
use str0m::{
    Candidate, Event as RtcEvent, IceConnectionState, Input, Output, Rtc,
    change::SdpOffer,
    format::Codec,
    media::{MediaKind, Mid},
    net::{Protocol, Receive},
    rtp::{ExtensionValues, RtpPacket, RtpWrite},
};
use tokio::{net::UdpSocket, sync::mpsc, time::sleep_until};
use tracing::{debug, info, warn};

use gwar_proto::rtp::{SLOT_GENERATION_ID, SlotGeneration, slot_generation_extension};

use super::{
    AudioKind, AudioOrigin, AudioPacket, SharedRouting, Sinks, SourceClock,
    admission::{MAX_SOURCE_DELTA, RtpAdmission},
};
use crate::core::{CoreHandle, Outbound, OutboundTx, encode};

/// On-demand, bounded transport diagnostics. No per-packet map or history.
#[derive(Debug)]
pub struct MediaSnapshot {
    pub counters: super::RtpCounters,
    pub registered_probe_ssrcs: usize,
    pub rtcp_reports: u64,
}

pub enum MediaCmd {
    /// Test-only rendezvous after SRTP authentication/admission, before forwarding.
    #[cfg(feature = "test-hooks")]
    HoldNext {
        session: SessionId,
        reply: tokio::sync::oneshot::Sender<AudioPacket>,
    },
    Inspect {
        session: SessionId,
        probe_ssrcs: Vec<u32>,
        reply: tokio::sync::oneshot::Sender<Option<MediaSnapshot>>,
    },
    Offer {
        session: SessionId,
        binding: VoiceTransportBinding,
        origin: Arc<AudioOrigin>,
        ssrc: u32,
        retired: Vec<u32>,
        sdp: String,
        out: OutboundTx,
        reply: tokio::sync::oneshot::Sender<Result<String, &'static str>>,
    },
    Remove {
        session: SessionId,
    },
    /// A frame from another transport, to be written to local WebRTC listeners.
    Forward(AudioPacket),
    /// Like `Forward`, for one listener only.
    ForwardTo(SessionId, AudioPacket),
}

/// A speaker stays "talking" this long after its last voiced frame.
const HANGOVER: Duration = Duration::from_millis(400);
/// A slot idle for this long may be handed to another speaker.
const SLOT_REUSE: Duration = Duration::from_millis(800);
/// RFC 6464 level (negative dBov) above which a frame counts as speech.
const SPEECH_LEVEL: i8 = -50;
const SAMPLE_RATE: u64 = 48_000;
/// An out-of-order source time this long after the last write is a new clock:
/// TeamSpeak's u16 ids cannot tell a reordered packet from a far-ahead one.
const STALE_CLOCK: Duration = Duration::from_secs(1);

struct Slot {
    mid: Mid,
    source: Option<SessionId>,
    last_used: Instant,
    ticks: u64,
    generation: u16,
    last_write: Option<Instant>,
    last_samples: u32,
    clock: Option<SourceClock>,
    sequence: u64,
}

impl Slot {
    fn assign(&mut self, source: SessionId) {
        self.source = Some(source);
        self.generation = self.generation.wrapping_add(1);
        self.clock = None;
    }

    fn advance(&mut self, now: Instant, samples: u32, clock: SourceClock) -> bool {
        let stale = self.last_write.is_none_or(|last| now.saturating_duration_since(last) > STALE_CLOCK);
        let delta = match self.clock.map(|old| clock.delta(old)).transpose() {
            Err(()) if stale && matches!(clock, SourceClock::TeamSpeak { .. }) => None,
            Err(()) => return false,
            Ok(delta) => delta.flatten().filter(|delta| *delta <= MAX_SOURCE_DELTA),
        };
        self.ticks += delta.map(u64::from).unwrap_or_else(|| match self.last_write {
            Some(last) => {
                let elapsed = now.saturating_duration_since(last).as_nanos();
                let rounded = elapsed.div_ceil(10_000_000).max(1) * 480;
                rounded.max(u128::from(self.last_samples)) as u64
            }
            None => SAMPLE_RATE,
        });
        self.clock = Some(clock);
        self.last_write = Some(now);
        self.last_samples = samples;
        true
    }
}

struct Peer {
    session: SessionId,
    binding: VoiceTransportBinding,
    core: CoreHandle,
    rtc: Rtc,
    out: OutboundTx,
    mic: Option<Mid>,
    slots: Vec<Slot>,
    talking: bool,
    last_voice: Instant,
    connected: bool,
    origin: Arc<AudioOrigin>,
    admission: RtpAdmission,
    rtcp_reports: u64,
    #[cfg(feature = "test-hooks")]
    hold_next: Option<tokio::sync::oneshot::Sender<AudioPacket>>,
}

impl Peer {
    fn notify(&self, event: Event) {
        let _ = self.out.try_send(Outbound::Frame(encode(&ServerFrame::Event(event))));
    }

    fn release_source(&mut self, source: SessionId) {
        let freed: Vec<_> = self
            .slots
            .iter_mut()
            .enumerate()
            .filter(|(_, s)| s.source == Some(source))
            .map(|(i, s)| {
                s.source = None;
                i as u32
            })
            .collect();
        for slot in freed {
            self.notify(Event::VoiceSlot { binding: self.binding.clone(), slot, client: None });
        }
    }

    /// Writes one frame from `from` into a receive slot, assigning one if needed.
    fn forward(&mut self, packet: &AudioPacket, now: Instant) {
        if !self.core.voice_binding_matches(self.session, &self.binding)
            || !self.core.audio_binding_matches(&packet.origin)
            || packet.origin.encrypted != self.origin.encrypted
            || packet.route.as_ref().is_some_and(|r| !r.permits(&packet.origin))
            || packet.ingress.as_ref().is_some_and(|route| {
                route
                    .origin
                    .as_ref()
                    .is_none_or(|origin| !route.permits(origin) || !self.core.audio_binding_matches(origin))
            })
            || packet.is_end()
        {
            return;
        }
        let index = match self.slots.iter().position(|s| s.source == Some(packet.from())) {
            Some(i) => i,
            None => {
                let candidate = self
                    .slots
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.source.is_none() || now.duration_since(s.last_used) > SLOT_REUSE)
                    .min_by_key(|(_, s)| (s.source.is_some(), s.last_used))
                    .map(|(i, _)| i);
                let Some(i) = candidate else { return };
                self.slots[i].assign(packet.from());
                self.notify(Event::VoiceSlot {
                    binding: self.binding.clone(),
                    slot: i as u32,
                    client: Some(packet.from()),
                });
                i
            }
        };
        let slot = &mut self.slots[index];
        let Some(pt) = self.rtc.media(slot.mid).and_then(|m| {
            self.rtc
                .codec_config()
                .params()
                .iter()
                .find(|p| m.remote_pts().contains(&p.pt()) && p.spec().codec == Codec::Opus)
                .map(|p| p.pt())
        }) else {
            return;
        };
        let mut direct = self.rtc.direct_api();
        let Some(stream) = direct.stream_tx_by_mid(slot.mid, None) else { return };
        if !slot.advance(now, packet.span, packet.clock) {
            return;
        }
        slot.last_used = now;
        let mut extensions = ExtensionValues::default();
        extensions.user_values.set(SlotGeneration(slot.generation));
        stream.write_rtp(
            RtpWrite::new(pt, slot.sequence.into(), slot.ticks as u32, now, packet.payload.clone())
                .ext_vals(extensions),
        );
        slot.sequence += 1;
    }
}

/// Speech decision for one frame from header extensions alone.
fn is_speech(data: &ExtensionValues) -> bool {
    match (data.voice_activity, data.audio_level) {
        (vad, Some(level)) => level >= SPEECH_LEVEL || (vad == Some(true) && level >= SPEECH_LEVEL - 10),
        (Some(vad), None) => vad,
        // Without the extension we cannot tell; forward everything.
        (None, None) => true,
    }
}

pub struct WebRtcConfig {
    pub socket: Arc<UdpSocket>,
    /// Address browsers should send to (public IP and UDP port).
    pub advertise: SocketAddr,
}

pub async fn run(
    config: WebRtcConfig,
    routing: SharedRouting,
    sinks: Arc<Sinks>,
    uplinks: Arc<Sinks>,
    core: CoreHandle,
    mut commands: mpsc::Receiver<MediaCmd>,
) {
    let WebRtcConfig { socket, advertise } = config;
    info!(%advertise, "WebRTC media listening");
    let mut peers: HashMap<SessionId, Peer> = HashMap::new();
    let mut buf = vec![0u8; 2048];
    let mut forwards: Vec<AudioPacket> = Vec::new();

    loop {
        // Drain outputs; forwarding writes new media, so repeat until quiet.
        let mut deadline = Instant::now() + Duration::from_millis(50);
        loop {
            for peer in peers.values_mut() {
                deadline = deadline.min(drive(peer, &socket, &routing, &uplinks, &core, &mut forwards));
            }
            if forwards.is_empty() {
                break;
            }
            let routing = routing.load();
            let now = Instant::now();
            for packet in forwards.drain(..) {
                // The uplink first: it has a whole extra hop ahead of it.
                uplinks.uplink(&routing, &packet);
                deliver_local(&mut peers, &routing, &packet, now);
                sinks.fan_out(&routing, &packet);
            }
        }

        let dead: Vec<_> = peers.values().filter(|p| !p.rtc.is_alive()).map(|p| p.session).collect();
        for session in dead {
            close_peer(&mut peers, session, &core, Some("audio connection lost"));
        }

        let now = Instant::now();
        for peer in peers.values_mut() {
            if peer.talking && now.duration_since(peer.last_voice) > HANGOVER {
                peer.talking = false;
                core.talking_bound(peer.session, false, &peer.binding);
                // Other transports (TeamSpeak) end the talk state explicitly.
                let mut end = AudioPacket::end_of_speech(peer.origin.clone(), None);
                end.egress = uplinks.route(peer.session);
                uplinks.uplink(&routing.load(), &end);
                sinks.fan_out(&routing.load(), &end);
            }
        }

        tokio::select! {
            command = commands.recv() => match command {
                Some(MediaCmd::Offer { session, binding, origin, ssrc, retired, sdp, out, reply }) => {
                    if reply.is_closed() || !core.voice_binding_matches(session, &binding) {
                        continue;
                    }
                    close_peer(&mut peers, session, &core, None);
                    match accept(session, binding, origin, RtpAdmission::new(ssrc, retired), core.clone(), &sdp, advertise, out) {
                        Ok((peer, answer)) => {peers.insert(session, peer); let _ = reply.send(Ok(answer));}
                        Err(message) => {let _ = reply.send(Err(message));}
                    }
                }
                #[cfg(feature = "test-hooks")]
                Some(MediaCmd::HoldNext { session, reply }) => {
                    peers.get_mut(&session).expect("hold admitted peer").hold_next = Some(reply);
                }
                Some(MediaCmd::Inspect { session, probe_ssrcs, reply }) => {
                    let snapshot = peers.get_mut(&session).map(|peer| MediaSnapshot {
                        counters: peer.admission.counters,
                        registered_probe_ssrcs: probe_ssrcs.into_iter().take(512)
                            .filter(|ssrc| peer.rtc.direct_api().stream_rx(&(*ssrc).into()).is_some()).count(),
                        rtcp_reports: peer.rtcp_reports,
                    });
                    let _ = reply.send(snapshot);
                }
                Some(MediaCmd::Remove { session }) => close_peer(&mut peers, session, &core, None),
                Some(MediaCmd::Forward(packet)) => {
                    deliver_local(&mut peers, &routing.load(), &packet, Instant::now());
                }
                Some(MediaCmd::ForwardTo(to, packet)) => {
                    if routing.load().accepts_whisper(&packet, to)
                        && let Some(peer) = peers.get_mut(&to)
                    {
                        peer.forward(&packet, Instant::now());
                    }
                }
                None => break,
            },
            received = socket.recv_from(&mut buf) => match received {
                Ok((n, source)) => {
                    let Ok(contents) = buf[..n].try_into() else { continue };
                    let input = Input::Receive(Instant::now(), Receive { proto: Protocol::Udp, source, destination: advertise, contents });
                    if let Some(peer) = peers.values_mut().find(|p| p.rtc.accepts(&input))
                        && peer.admission.admits_datagram(&buf[..n], peer.origin.encrypted)
                        && let Err(e) = peer.rtc.handle_input(input) {
                            debug!(session = peer.session, "rtc input: {e}");
                            peer.rtc.disconnect();
                        }
                }
                Err(e) => warn!("udp receive: {e}"),
            },
            _ = sleep_until(deadline.into()) => {}
        }

        let now = Instant::now();
        for peer in peers.values_mut() {
            if peer.rtc.is_alive() && peer.rtc.handle_input(Input::Timeout(now)).is_err() {
                peer.rtc.disconnect();
            }
        }
    }
}

fn deliver_local(peers: &mut HashMap<SessionId, Peer>, routing: &super::Routing, packet: &AudioPacket, now: Instant) {
    if !routing.accepts(packet) {
        return;
    }
    for to in routing.recipients(packet.from()) {
        if let Some(peer) = peers.get_mut(&to) {
            peer.forward(packet, now);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn accept(
    session: SessionId,
    binding: VoiceTransportBinding,
    origin: Arc<AudioOrigin>,
    admission: RtpAdmission,
    core: CoreHandle,
    sdp: &str,
    advertise: SocketAddr,
    out: OutboundTx,
) -> Result<(Peer, String), &'static str> {
    let offer = SdpOffer::from_sdp_string(sdp).map_err(|_| "invalid SDP offer")?;
    let mut rtc = Rtc::builder()
        .set_extension(SLOT_GENERATION_ID, slot_generation_extension())
        .set_ice_lite(true)
        .set_rtp_mode(true)
        .clear_codecs()
        .enable_opus(true, false)
        .build(Instant::now());
    let candidate = Candidate::host(advertise, "udp").map_err(|_| "bad media address")?;
    rtc.add_local_candidate(candidate);
    let answer = rtc.sdp_api().accept_offer(offer).map_err(|_| "offer not acceptable")?;
    let mic = rtc
        .direct_api()
        .stream_rx(&admission.ssrc.into())
        .map(|stream| stream.mid())
        .ok_or("microphone SSRC was not negotiated")?;
    if rtc.media(mic).is_none_or(|m| m.kind() != MediaKind::Audio || !m.direction().is_receiving()) {
        return Err("microphone media was not negotiated");
    }
    let peer = Peer {
        session,
        binding,
        core,
        rtc,
        out,
        mic: Some(mic),
        slots: Vec::with_capacity(AUDIO_SLOTS),
        talking: false,
        last_voice: Instant::now(),
        connected: false,
        origin,
        admission,
        rtcp_reports: 0,
        #[cfg(feature = "test-hooks")]
        hold_next: None,
    };
    Ok((peer, answer.to_sdp_string()))
}

fn close_peer(peers: &mut HashMap<SessionId, Peer>, session: SessionId, core: &CoreHandle, reason: Option<&str>) {
    let Some(mut peer) = peers.remove(&session) else { return };
    peer.rtc.disconnect();
    debug!(session, counters = ?peer.admission.counters, "retired media stream");
    core.voice_bound(session, false, &peer.binding, reason.is_some());
    for other in peers.values_mut() {
        other.release_source(session);
    }
}

/// Polls one peer until it wants to sleep; returns its next deadline.
fn drive(
    peer: &mut Peer,
    socket: &UdpSocket,
    routing: &SharedRouting,
    uplinks: &Sinks,
    core: &CoreHandle,
    forwards: &mut Vec<AudioPacket>,
) -> Instant {
    loop {
        if !peer.rtc.is_alive() {
            return Instant::now();
        }
        let output = match peer.rtc.poll_output() {
            Ok(output) => output,
            Err(e) => {
                debug!(session = peer.session, "rtc poll: {e}");
                peer.rtc.disconnect();
                return Instant::now();
            }
        };
        match output {
            Output::Timeout(at) => return at,
            Output::Transmit(t) => {
                // Dropping under socket pressure is acceptable for real-time media.
                let _ = socket.try_send_to(&t.contents, t.destination);
            }
            Output::Event(event) => match event {
                RtcEvent::Connected => {
                    peer.connected = true;
                    core.voice_bound(peer.session, true, &peer.binding, false);
                }
                RtcEvent::IceConnectionStateChange(IceConnectionState::Disconnected) => peer.rtc.disconnect(),
                RtcEvent::MediaAdded(added) if added.kind == MediaKind::Audio => {
                    if Some(added.mid) != peer.mic && added.direction.is_sending() && peer.slots.len() < AUDIO_SLOTS {
                        peer.slots.push(Slot {
                            mid: added.mid,
                            source: None,
                            last_used: Instant::now(),
                            ticks: 0,
                            generation: 0,
                            last_write: None,
                            last_samples: 960,
                            clock: None,
                            sequence: 0,
                        });
                    }
                }
                RtcEvent::SenderFeedback(_) => peer.rtcp_reports += 1,
                RtcEvent::RtpPacket(data) => {
                    if let Some(packet) = ingest(peer, &data, routing, uplinks, core) {
                        #[cfg(feature = "test-hooks")]
                        if let Some(reply) = peer.hold_next.take() {
                            let _ = reply.send(packet);
                            continue;
                        }
                        forwards.push(packet);
                    }
                }
                _ => {}
            },
        }
    }
}

/// Applies the speech gate to one microphone frame.
fn ingest(
    peer: &mut Peer,
    data: &RtpPacket,
    routing: &SharedRouting,
    uplinks: &Sinks,
    core: &CoreHandle,
) -> Option<AudioPacket> {
    if !core.voice_binding_matches(peer.session, &peer.binding)
        || routing.load().origin(peer.session).as_deref() != Some(&peer.origin)
    {
        return None;
    }
    if !peer.admission.admits_ssrc(*data.header.ssrc) {
        return None;
    }
    let mid = peer.rtc.direct_api().stream_rx(&data.header.ssrc)?.mid();
    if Some(mid) != peer.mic {
        return None;
    }
    let (clock, span) = peer.admission.admit(
        *data.header.ssrc,
        *data.seq_no,
        data.header.timestamp,
        peer.origin.encrypted,
        &data.payload,
    )?;
    let now = Instant::now();
    if !routing.load().can_speak(peer.session) {
        if peer.talking {
            peer.talking = false;
            core.talking_bound(peer.session, false, &peer.binding);
        }
        return None;
    }
    if is_speech(&data.header.ext_vals) {
        peer.last_voice = now;
        if !peer.talking {
            peer.talking = true;
            core.talking_bound(peer.session, true, &peer.binding);
        }
    }
    if !peer.talking {
        return None;
    }
    Some(AudioPacket {
        admitted_at: std::time::Instant::now(),
        origin: peer.origin.clone(),
        payload: Arc::clone(&data.payload),
        kind: AudioKind::Audio,
        span,
        clock,
        route: None,
        target: None,
        ingress: None,
        egress: uplinks.route(peer.session),
    })
}

#[cfg(test)]
mod clock_tests {
    use super::*;

    fn slot(now: Instant) -> Slot {
        Slot {
            mid: "0".into(),
            source: Some(1),
            last_used: now,
            ticks: 0,
            generation: 1,
            last_write: None,
            last_samples: 960,
            clock: None,
            sequence: 0,
        }
    }

    fn rtp(sequence: u64, ticks: u32) -> SourceClock {
        SourceClock::Rtp { ssrc: 42, sequence, ticks, generation: 0 }
    }

    #[test]
    fn uplink_stall_burst_keeps_capture_deltas() {
        let now = Instant::now();
        let mut slot = slot(now);
        assert!(slot.advance(now, 480, rtp(0, 0)));
        for i in 1..=27 {
            assert!(slot.advance(now + Duration::from_millis(270), 480, rtp(i, i as u32 * 480)));
            assert_eq!(slot.ticks, SAMPLE_RATE + i * 480);
        }
    }

    #[test]
    fn true_pause_and_mixed_durations_follow_source_even_in_a_batch() {
        let now = Instant::now();
        let mut slot = slot(now);
        for (i, (ticks, duration)) in
            [(0, 480), (480, 960), (1440, 2880), (4320, 5760), (10080, 480), (58080, 960)].into_iter().enumerate()
        {
            assert!(slot.advance(now, duration, rtp(i as u64, ticks)));
            assert_eq!(slot.ticks, SAMPLE_RATE + u64::from(ticks));
            assert_eq!(slot.last_samples, duration);
        }
    }

    #[test]
    fn speaker_change_increments_generation_and_uses_wall_time() {
        let now = Instant::now();
        let mut slot = slot(now);
        slot.advance(now, 960, rtp(100, 9000));
        slot.assign(2);
        assert_eq!(slot.generation, 2);
        assert!(slot.advance(now + Duration::from_millis(801), 480, rtp(0, 0)));
        assert_eq!(slot.ticks, SAMPLE_RATE + 81 * 480);
        assert!(slot.advance(now + Duration::from_millis(801), 480, rtp(1, 480)));
        assert_eq!(slot.ticks, SAMPLE_RATE + 81 * 480 + 480);
    }

    #[test]
    fn rtp_and_teamspeak_wrap_without_discontinuity() {
        let now = Instant::now();
        let mut slot = slot(now);
        slot.advance(now, 480, rtp(65535, u32::MAX - 479));
        assert!(slot.advance(now, 480, rtp(65536, 0)));
        assert_eq!(slot.ticks, SAMPLE_RATE + 480);
        assert!(!slot.advance(now, 480, rtp(65535, u32::MAX - 479)));
        let mut slot = self::slot(now);
        slot.advance(now, 960, SourceClock::TeamSpeak { id: 65535, whisper: false, spurt: 0 });
        assert!(slot.advance(now, 960, SourceClock::TeamSpeak { id: 0, whisper: false, spurt: 0 }));
        assert_eq!(slot.ticks, SAMPLE_RATE + 960);
        assert!(!slot.advance(now, 960, SourceClock::TeamSpeak { id: 65535, whisper: false, spurt: 0 }));
        assert!(!slot.advance(now, 960, SourceClock::TeamSpeak { id: 0, whisper: false, spurt: 0 }));
    }

    #[test]
    fn teamspeak_spurts_restart_on_wall_time_even_without_an_end_marker() {
        let now = Instant::now();
        let ts = |id, spurt| SourceClock::TeamSpeak { id, whisper: false, spurt };
        let mut slot = slot(now);
        slot.advance(now, 960, ts(10, 1));
        // Ids continue across a six-second pause. This listener never saw the
        // end marker (e.g. deafened), but the next spurt re-anchors anyway.
        assert!(slot.advance(now + Duration::from_secs(6), 960, ts(11, 2)));
        assert_eq!(slot.ticks, SAMPLE_RATE + 6 * SAMPLE_RATE);
        assert!(slot.advance(now + Duration::from_secs(6), 960, ts(12, 2)));
        assert_eq!(slot.ticks, SAMPLE_RATE + 6 * SAMPLE_RATE + 960);
        // Ordering survives the new spurt: late audio from the old one drops.
        assert!(!slot.advance(now + Duration::from_secs(6), 960, ts(9, 1)));
        assert!(!slot.advance(now + Duration::from_secs(6), 960, ts(11, 2)));
        // Within one spurt, a jump past half the id range after a long
        // absence is a new clock rather than a stale packet.
        let later = now + Duration::from_secs(8);
        assert!(slot.advance(later, 960, ts(12u16.wrapping_add(40_000), 2)));
        assert_eq!(slot.ticks, SAMPLE_RATE + 8 * SAMPLE_RATE + 960);
    }

    #[test]
    fn teamspeak_voice_and_whisper_have_independent_counters() {
        let now = Instant::now();
        let mut slot = slot(now);
        slot.advance(now, 960, SourceClock::TeamSpeak { id: 100, whisper: false, spurt: 0 });
        assert!(slot.advance(
            now + Duration::from_millis(200),
            960,
            SourceClock::TeamSpeak { id: 0, whisper: true, spurt: 0 }
        ));
        assert_eq!(slot.ticks, SAMPLE_RATE + 9600);
        assert!(slot.advance(
            now + Duration::from_millis(200),
            960,
            SourceClock::TeamSpeak { id: 1, whisper: true, spurt: 0 }
        ));
        assert_eq!(slot.ticks, SAMPLE_RATE + 10560);
    }

    #[test]
    fn duplicate_timestamp_and_old_sequence_do_not_touch_slot_state() {
        let now = Instant::now();
        let mut slot = slot(now);
        slot.advance(now, 480, rtp(10, 4800));
        for stamp in [rtp(10, 4800), rtp(9, 4320), rtp(11, 4800), rtp(0, 0)] {
            assert!(!slot.advance(now + Duration::from_secs(1), 960, stamp));
            assert_eq!(slot.ticks, SAMPLE_RATE);
            assert_eq!(slot.last_write, Some(now));
            assert_eq!(slot.last_samples, 480);
        }
        assert!(slot.advance(now, 480, rtp(12, 5760)));
        assert_eq!(slot.ticks, SAMPLE_RATE + 960);
    }

    #[test]
    fn clock_resets_use_previous_duration_and_accept_exactly_five_seconds() {
        let now = Instant::now();
        let mut slot = slot(now);
        slot.advance(now, 960, rtp(0, 9000));
        slot.advance(now, 480, rtp(1, 249000));
        assert_eq!(slot.ticks, SAMPLE_RATE + 240000);
        slot.advance(now + Duration::from_millis(21), 960, rtp(2, 100));
        assert_eq!(slot.ticks, SAMPLE_RATE + 240000 + 1440);
        slot.advance(now + Duration::from_millis(21), 480, rtp(3, 240101));
        assert_eq!(slot.ticks, SAMPLE_RATE + 240000 + 1440 + 960);
        assert!(slot.advance(
            now + Duration::from_millis(41),
            480,
            SourceClock::Rtp { ssrc: 43, sequence: 0, ticks: 0, generation: 0 }
        ));
        assert_eq!(slot.ticks, SAMPLE_RATE + 240000 + 1440 + 960 + 960);
    }
    #[test]
    fn all_reanchors_round_to_480_and_keep_previous_span() {
        let now = Instant::now();
        for previous in [480, 960, 2880, 5760] {
            for elapsed_ms in [0, 1, 9, 10, 11, 21, 61, 6001] {
                for cause in 0..5 {
                    let mut slot = slot(now);
                    assert!(slot.advance(now, previous, rtp(1, 100_000)));
                    let next = match cause {
                        0 => {
                            slot.assign(2);
                            rtp(2, 100_480)
                        }
                        1 => SourceClock::Rtp { ssrc: 43, sequence: 1, ticks: 0, generation: 0 },
                        2 => SourceClock::Rtp { ssrc: 42, sequence: 2, ticks: 100_480, generation: 1 },
                        3 => rtp(2, 340_001),
                        _ => SourceClock::TeamSpeak { id: 5, whisper: false, spurt: 2 },
                    };
                    assert!(slot.advance(now + Duration::from_millis(elapsed_ms), 480, next));
                    let floor = (elapsed_ms.div_ceil(10).max(1) * 480).max(u64::from(previous));
                    assert_eq!(slot.ticks, 48_000 + floor, "span {previous}, elapsed {elapsed_ms}, cause {cause}");
                }
            }
        }
        let mut batched = slot(now);
        for spurt in 1..=3 {
            assert!(batched.advance(now, 2880, SourceClock::TeamSpeak { id: spurt as u16, whisper: false, spurt }));
            assert_eq!(batched.ticks, 48_000 + u64::from(spurt - 1) * 2880);
        }
        let mut source = RtpAdmission::new(42, vec![]);
        let mut output = slot(now);
        // A gated reset is carried in the source generation even if its next
        // timestamp happens to look continuous with the last forwarded one.
        for (seq, ticks) in [100_000, 100_960, 100_000, 100_960].into_iter().enumerate() {
            let (clock, span) = source.admit(42, seq as u64, ticks, true, &[0xff, 0]).unwrap();
            if seq == 0 || seq == 3 {
                assert!(output.advance(now + Duration::from_millis(seq as u64 * 20), span, clock));
            }
        }
        assert_eq!(output.ticks, 50_880);
        // Learned 10 ms steps still reserve 20 ms for the next mixed packet.
        for ticks in [101_440, 101_920] {
            let (clock, span) = source.admit(42, ticks as u64, ticks, true, &[0xff, 0]).unwrap();
            assert_eq!(span, 960);
            output.advance(now + Duration::from_millis(60), span, clock);
        }
        let before = output.ticks;
        let (clock, span) = source.admit(42, 101_921, 0, true, &[0xff, 0]).unwrap();
        output.advance(now + Duration::from_millis(60), span, clock);
        assert_eq!(output.ticks - before, 960);
        // Documented isolated 120 ms encrypted exception: estimate stays 960,
        // so an immediate reset can overlap by at most 5760-960 = 4800 ticks.
        assert_eq!(5760 - span, 4800);
        assert!(!output.advance(now + Duration::from_secs(10), span, rtp(1, 0)));
    }
    #[test]
    fn encrypted_clock_reset_preserves_post_reset_native_decoded_tone() {
        use audiopus::{
            Application, Channels, MutSignals, SampleRate,
            coder::{Decoder, Encoder},
            packet::Packet,
        };
        use gwar_client::voice::jitter::{JitterBuffer, Playout};
        let now = Instant::now();
        let mut source = RtpAdmission::new(42, vec![]);
        let mut output = slot(now);
        let mut jitter = JitterBuffer::new();
        let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Audio).unwrap();
        let mut decoder = Decoder::new(SampleRate::Hz48000, Channels::Mono).unwrap();
        let mut energy = Vec::new();
        for (seq, ticks) in [100_000, 0, 960, 1920, 2880].into_iter().enumerate() {
            let pcm: Vec<_> = (0..960)
                .map(|i| (std::f32::consts::TAU * 1500.0 * (seq * 960 + i) as f32 / 48_000.0).sin() * 0.4)
                .collect();
            let mut bytes = vec![0; 1000];
            let n = encoder.encode_float(&pcm, &mut bytes).unwrap();
            bytes.truncate(n);
            // Plaintext Opus through the production encrypted-clock estimator.
            let (clock, span) = source.admit(42, seq as u64, ticks, true, &bytes).unwrap();
            assert!(output.advance(now, span, clock));
            assert_eq!(output.generation, 1); // no decoder reset hides overlaps
            jitter.push(output.ticks, 960, bytes);
            let Playout::Packet { data, .. } = jitter.pop() else { panic!("post-reset audio was late or missing") };
            let packet = Packet::try_from(data.as_slice()).unwrap();
            let mut decoded = [0.0f32; 5760];
            let n = decoder.decode_float(Some(packet), MutSignals::try_from(&mut decoded[..]).unwrap(), false).unwrap();
            assert_eq!(n, 960);
            let component = |cosine: bool| {
                (0..n)
                    .map(|i| {
                        let phase = std::f64::consts::TAU * 1500.0 * i as f64 / 48_000.0;
                        f64::from(decoded[i]) * if cosine { phase.cos() } else { phase.sin() }
                    })
                    .sum::<f64>()
                    / n as f64
            };
            let amplitude = component(false).hypot(component(true));
            energy.push(amplitude);
        }
        assert!(energy[1..].iter().all(|amplitude| *amplitude > 0.05), "post-reset tone: {energy:?}");
    }
}
