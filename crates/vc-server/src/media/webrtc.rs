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
use vc_proto::{AUDIO_SLOTS, ErrorBody, ErrorCode, Event, Response, ServerFrame, SessionId};

use vc_proto::rtp::{SLOT_GENERATION_ID, SlotGeneration, slot_generation_extension};

use super::{AudioPacket, SharedRouting, Sinks, SourceClock, opus};
use crate::core::{CoreHandle, Outbound, OutboundTx, encode};

pub enum MediaCmd {
    Offer {
        session: SessionId,
        id: u32,
        sdp: String,
        out: OutboundTx,
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
const MAX_SOURCE_DELTA: u32 = 5 * SAMPLE_RATE as u32;
/// Late packets from these SSRCs are dropped; the oldest is forgotten first.
const RETIRED_SSRCS: usize = 32;
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

    /// TeamSpeak ids advance per packet, not across silence: the next spurt
    /// needs a fresh anchor so the pause stays a pause.
    fn end_spurt(&mut self) {
        if let Some(SourceClock::TeamSpeak { .. }) = self.clock {
            self.clock = None;
        }
    }

    fn advance(&mut self, now: Instant, samples: u32, clock: SourceClock) -> bool {
        let stale = self.last_write.is_none_or(|last| now.saturating_duration_since(last) > STALE_CLOCK);
        let delta = match self.clock.map(|old| clock.delta(old)).transpose() {
            Err(()) if stale => None,
            Err(()) => return false,
            Ok(delta) => delta.flatten().filter(|delta| *delta <= MAX_SOURCE_DELTA),
        };
        self.ticks += delta.map(u64::from).unwrap_or_else(|| match self.last_write {
            Some(last) => {
                let elapsed = now.duration_since(last).as_nanos() * u128::from(SAMPLE_RATE);
                let frame = u128::from(self.last_samples);
                (elapsed.div_ceil(1_000_000_000 * frame).max(1) * frame) as u64
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
    rtc: Rtc,
    out: OutboundTx,
    mic: Option<Mid>,
    slots: Vec<Slot>,
    talking: bool,
    last_voice: Instant,
    connected: bool,
    clock: Option<SourceClock>,
    retired_ssrcs: Vec<u32>,
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
            self.notify(Event::VoiceSlot { slot, client: None });
        }
    }

    /// Writes one frame from `from` into a receive slot, assigning one if needed.
    fn forward(&mut self, packet: &AudioPacket, now: Instant) {
        if packet.is_end() {
            if let Some(slot) = self.slots.iter_mut().find(|s| s.source == Some(packet.from)) {
                slot.end_spurt();
            }
            return;
        }
        let index = match self.slots.iter().position(|s| s.source == Some(packet.from)) {
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
                self.slots[i].assign(packet.from);
                self.notify(Event::VoiceSlot { slot: i as u32, client: Some(packet.from) });
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
        if !slot.advance(now, packet.samples, packet.clock) {
            return;
        }
        slot.last_used = now;
        let mut extensions = ExtensionValues::default();
        extensions.user_values.set(SlotGeneration(slot.generation));
        stream.write_rtp(
            RtpWrite::new(pt, slot.sequence.into(), slot.ticks as u32, now, packet.opus.clone()).ext_vals(extensions),
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
                deadline = deadline.min(drive(peer, &socket, &routing, &core, &mut forwards));
            }
            if forwards.is_empty() {
                break;
            }
            let routing = routing.load();
            let now = Instant::now();
            for packet in forwards.drain(..) {
                // The uplink first: it has a whole extra hop ahead of it.
                uplinks.send(packet.from, &packet);
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
                core.talking(peer.session, false);
                // Other transports (TeamSpeak) end the talk state explicitly.
                let end = AudioPacket::end_of_speech(peer.session);
                uplinks.send(peer.session, &end);
                sinks.fan_out(&routing.load(), &end);
            }
        }

        tokio::select! {
            command = commands.recv() => match command {
                Some(MediaCmd::Offer { session, id, sdp, out }) => {
                    close_peer(&mut peers, session, &core, None);
                    match accept(session, &sdp, advertise, out.clone()) {
                        Ok((peer, answer)) => {
                            peers.insert(session, peer);
                            let _ = out.try_send(Outbound::Frame(encode(&ServerFrame::Ok { re: id, ok: Response::Answer { sdp: answer } })));
                        }
                        Err(message) => {
                            let err = ErrorBody::new(ErrorCode::BadRequest, message);
                            let _ = out.try_send(Outbound::Frame(encode(&ServerFrame::Err { re: id, err })));
                        }
                    }
                }
                Some(MediaCmd::Remove { session }) => close_peer(&mut peers, session, &core, None),
                Some(MediaCmd::Forward(packet)) => {
                    deliver_local(&mut peers, &routing.load(), &packet, Instant::now());
                }
                Some(MediaCmd::ForwardTo(to, packet)) => {
                    if routing.load().can_whisper(packet.from, to)
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
    for to in routing.recipients(packet.from) {
        if let Some(peer) = peers.get_mut(&to) {
            peer.forward(packet, now);
        }
    }
}

fn accept(
    session: SessionId,
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
    let peer = Peer {
        session,
        rtc,
        out,
        mic: None,
        slots: Vec::with_capacity(AUDIO_SLOTS),
        talking: false,
        last_voice: Instant::now(),
        connected: false,
        clock: None,
        retired_ssrcs: Vec::new(),
    };
    Ok((peer, answer.to_sdp_string()))
}

fn close_peer(peers: &mut HashMap<SessionId, Peer>, session: SessionId, core: &CoreHandle, reason: Option<&str>) {
    let Some(mut peer) = peers.remove(&session) else { return };
    peer.rtc.disconnect();
    if let Some(reason) = reason {
        peer.notify(Event::VoiceClosed { reason: reason.into() });
    }
    if peer.connected {
        core.voice(session, false);
    } else if peer.talking {
        core.talking(session, false);
    }
    for other in peers.values_mut() {
        other.release_source(session);
    }
}

/// Polls one peer until it wants to sleep; returns its next deadline.
fn drive(
    peer: &mut Peer,
    socket: &UdpSocket,
    routing: &SharedRouting,
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
                    core.voice(peer.session, true);
                }
                RtcEvent::IceConnectionStateChange(IceConnectionState::Disconnected) => peer.rtc.disconnect(),
                RtcEvent::MediaAdded(added) if added.kind == MediaKind::Audio => {
                    if added.direction.is_receiving() && peer.mic.is_none() {
                        peer.mic = Some(added.mid);
                    } else if added.direction.is_sending() && peer.slots.len() < AUDIO_SLOTS {
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
                RtcEvent::RtpPacket(data) => {
                    if let Some(packet) = ingest(peer, &data, routing, core) {
                        forwards.push(packet);
                    }
                }
                _ => {}
            },
        }
    }
}

/// Applies the speech gate to one microphone frame.
fn ingest(peer: &mut Peer, data: &RtpPacket, routing: &SharedRouting, core: &CoreHandle) -> Option<AudioPacket> {
    let mid = peer.rtc.direct_api().stream_rx(&data.header.ssrc)?.mid();
    if Some(mid) != peer.mic {
        return None;
    }
    let samples = opus::packet_samples(&data.payload)?;
    let clock = SourceClock::Rtp { ssrc: *data.header.ssrc, sequence: *data.seq_no, ticks: data.header.timestamp };
    if let Some(old) = peer.clock {
        match clock.delta(old) {
            Err(()) => return None,
            Ok(None) => {
                if peer.retired_ssrcs.contains(&*data.header.ssrc) {
                    return None;
                }
                if let SourceClock::Rtp { ssrc, .. } = old {
                    if peer.retired_ssrcs.len() == RETIRED_SSRCS {
                        peer.retired_ssrcs.remove(0);
                    }
                    peer.retired_ssrcs.push(ssrc);
                }
            }
            Ok(Some(_)) => {}
        }
    }
    // Order before touching speech state, including gated packets.
    peer.clock = Some(clock);
    let now = Instant::now();
    if !routing.load().can_speak(peer.session) {
        if peer.talking {
            peer.talking = false;
            core.talking(peer.session, false);
        }
        return None;
    }
    if is_speech(&data.header.ext_vals) {
        peer.last_voice = now;
        if !peer.talking {
            peer.talking = true;
            core.talking(peer.session, true);
        }
    }
    if !peer.talking {
        return None;
    }
    Some(AudioPacket { from: peer.session, opus: Arc::clone(&data.payload), samples, clock })
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
        SourceClock::Rtp { ssrc: 42, sequence, ticks }
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
        assert_eq!(slot.ticks, SAMPLE_RATE + 41 * 960);
        assert!(slot.advance(now + Duration::from_millis(801), 480, rtp(1, 480)));
        assert_eq!(slot.ticks, SAMPLE_RATE + 41 * 960 + 480);
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
        slot.advance(now, 960, SourceClock::TeamSpeak { id: 65535, whisper: false });
        assert!(slot.advance(now, 960, SourceClock::TeamSpeak { id: 0, whisper: false }));
        assert_eq!(slot.ticks, SAMPLE_RATE + 960);
        assert!(!slot.advance(now, 960, SourceClock::TeamSpeak { id: 65535, whisper: false }));
        assert!(!slot.advance(now, 960, SourceClock::TeamSpeak { id: 0, whisper: false }));
    }

    #[test]
    fn teamspeak_pause_and_long_absence_restart_on_wall_time() {
        let now = Instant::now();
        let ts = |id| SourceClock::TeamSpeak { id, whisper: false };
        let mut slot = slot(now);
        slot.advance(now, 960, ts(10));
        // Ids continue across a six-second pause; the end marker re-anchors.
        slot.end_spurt();
        assert!(slot.advance(now + Duration::from_secs(6), 960, ts(11)));
        assert_eq!(slot.ticks, SAMPLE_RATE + 6 * SAMPLE_RATE);
        assert!(slot.advance(now + Duration::from_secs(6), 960, ts(12)));
        assert_eq!(slot.ticks, SAMPLE_RATE + 6 * SAMPLE_RATE + 960);
        // A recent backwards id is reordering; a jump past half the range after
        // a long absence (e.g. deafened) is a new clock, not a stale packet.
        assert!(!slot.advance(now + Duration::from_secs(6), 960, ts(11)));
        let later = now + Duration::from_secs(8);
        assert!(slot.advance(later, 960, ts(12u16.wrapping_add(40_000))));
        assert_eq!(slot.ticks, SAMPLE_RATE + 8 * SAMPLE_RATE + 960);
    }

    #[test]
    fn rtp_end_marker_keeps_the_source_clock() {
        let now = Instant::now();
        let mut slot = slot(now);
        slot.advance(now, 480, rtp(0, 0));
        slot.end_spurt();
        assert!(slot.advance(now, 480, rtp(1, 48_000)));
        assert_eq!(slot.ticks, 2 * SAMPLE_RATE);
    }

    #[test]
    fn teamspeak_voice_and_whisper_have_independent_counters() {
        let now = Instant::now();
        let mut slot = slot(now);
        slot.advance(now, 960, SourceClock::TeamSpeak { id: 100, whisper: false });
        assert!(slot.advance(now + Duration::from_millis(200), 960, SourceClock::TeamSpeak { id: 0, whisper: true }));
        assert_eq!(slot.ticks, SAMPLE_RATE + 9600);
        assert!(slot.advance(now + Duration::from_millis(200), 960, SourceClock::TeamSpeak { id: 1, whisper: true }));
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
            SourceClock::Rtp { ssrc: 43, sequence: 0, ticks: 0 }
        ));
        assert_eq!(slot.ticks, SAMPLE_RATE + 240000 + 1440 + 960 + 960);
    }
}
