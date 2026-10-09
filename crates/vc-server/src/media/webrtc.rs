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
    media::{Frequency, MediaData, MediaKind, MediaTime, Mid},
    net::{Protocol, Receive},
};
use tokio::{net::UdpSocket, sync::mpsc, time::sleep_until};
use tracing::{debug, info, warn};
use vc_proto::{AUDIO_SLOTS, ErrorBody, ErrorCode, Event, Response, ServerFrame, SessionId};

use super::{AudioPacket, SharedRouting, Sinks, opus};
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
}

/// A speaker stays "talking" this long after its last voiced frame.
const HANGOVER: Duration = Duration::from_millis(400);
/// A slot idle for this long may be handed to another speaker.
const SLOT_REUSE: Duration = Duration::from_millis(800);
/// RFC 6464 level (negative dBov) above which a frame counts as speech.
const SPEECH_LEVEL: i8 = -50;
const SAMPLE_RATE: u64 = 48_000;

struct Slot {
    mid: Mid,
    source: Option<SessionId>,
    last_used: Instant,
    ticks: u64,
    last_write: Option<Instant>,
    last_samples: u32,
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
                self.slots[i].source = Some(packet.from);
                self.notify(Event::VoiceSlot { slot: i as u32, client: Some(packet.from) });
                i
            }
        };
        let slot = &mut self.slots[index];
        slot.last_used = now;
        // RTP time must advance with real time across pauses and speaker
        // changes so the browser's jitter buffer sees a clean discontinuity.
        slot.ticks += match slot.last_write {
            Some(last) => {
                let elapsed = now.duration_since(last).as_micros() as u64 * SAMPLE_RATE / 1_000_000;
                let frame = u64::from(slot.last_samples);
                if elapsed > frame * 2 { elapsed.div_ceil(frame) * frame } else { frame }
            }
            None => SAMPLE_RATE,
        };
        slot.last_write = Some(now);
        slot.last_samples = packet.samples;
        let Some(writer) = self.rtc.writer(slot.mid) else { return };
        let Some(pt) = writer.payload_params().find(|p| p.spec().codec == Codec::Opus).map(|p| p.pt()) else {
            return;
        };
        let time = MediaTime::new(slot.ticks, Frequency::FORTY_EIGHT_KHZ);
        if let Err(e) = writer.write(pt, now, time, packet.opus.clone()) {
            debug!(session = self.session, "write failed: {e}");
        }
    }
}

/// Speech decision for one frame from header extensions alone.
fn is_speech(data: &MediaData) -> bool {
    match (data.ext_vals.voice_activity, data.ext_vals.audio_level) {
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
    let mut rtc = Rtc::builder().set_ice_lite(true).clear_codecs().enable_opus(true, false).build(Instant::now());
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
                            last_write: None,
                            last_samples: 960,
                        });
                    }
                }
                RtcEvent::MediaData(data) if Some(data.mid) == peer.mic => {
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
fn ingest(peer: &mut Peer, data: &MediaData, routing: &SharedRouting, core: &CoreHandle) -> Option<AudioPacket> {
    let now = Instant::now();
    if !routing.load().can_speak(peer.session) {
        if peer.talking {
            peer.talking = false;
            core.talking(peer.session, false);
        }
        return None;
    }
    if is_speech(data) {
        peer.last_voice = now;
        if !peer.talking {
            peer.talking = true;
            core.talking(peer.session, true);
        }
    }
    if !peer.talking {
        return None;
    }
    let samples = opus::packet_samples(&data.data)?;
    Some(AudioPacket { from: peer.session, opus: Arc::clone(&data.data), samples })
}
