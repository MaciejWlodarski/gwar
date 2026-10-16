//! Media plane. Opus frames are forwarded between transports without being
//! decoded: the server only decides *who hears whom*, the receiving client
//! decodes and mixes. The control plane (core actor) publishes a routing
//! snapshot that every transport reads lock-free on the hot path.

mod admission;
pub mod opus;
pub use admission::RtpCounters;
pub mod webrtc;

use std::{
    collections::HashMap,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

use arc_swap::ArcSwap;
use tokio::sync::mpsc;
use vc_proto::{ChannelId, SessionId, VoiceBinding};

/// Immutable source authority captured at transport admission, never reconstructed
/// from a later channel lookup. Equal bindings survive unrelated routing updates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioOrigin {
    pub channel: ChannelId,
    pub encrypted: bool,
    pub session: SessionId,
    pub binding: VoiceBinding,
}

/// One confirmed TeamSpeak route. Replacement invalidates queued packets before
/// the move command can be delayed behind audio. Confirmation cannot revive it.
#[derive(Debug)]
pub struct AudioRoute {
    pub generation: u64,
    pub cid: u64,
    pub origin: Option<Arc<AudioOrigin>>,
    current: AtomicBool,
    confirmed: AtomicBool,
}

impl AudioRoute {
    pub fn new(generation: u64, cid: u64, origin: Option<Arc<AudioOrigin>>) -> Arc<Self> {
        Arc::new(Self { generation, cid, origin, current: AtomicBool::new(true), confirmed: AtomicBool::new(false) })
    }
    pub fn is_current(&self) -> bool {
        self.current.load(Ordering::Acquire)
    }
    pub fn invalidate(&self) {
        self.current.store(false, Ordering::Release);
    }
    pub fn confirm(&self, cid: Option<u64>) {
        self.confirmed.store(cid == Some(self.cid), Ordering::Release);
    }
    pub fn permits(&self, origin: &AudioOrigin) -> bool {
        !origin.encrypted
            && self.current.load(Ordering::Acquire)
            && self.confirmed.load(Ordering::Acquire)
            && self.origin.as_deref() == Some(origin)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioKind {
    Audio,
    EndOfSpeech,
}

/// Opaque encoded audio; only plaintext admission inspects Opus. An empty
/// encrypted payload is never an end-of-speech control.
#[derive(Debug, Clone)]
pub struct AudioPacket {
    pub origin: Arc<AudioOrigin>,
    pub payload: Arc<[u8]>,
    pub kind: AudioKind,
    /// Previous-packet overlap floor to retain when re-anchoring the next packet.
    pub span: u32,
    pub clock: SourceClock,
    pub route: Option<Arc<AudioRoute>>,
    pub target: Option<Arc<AudioOrigin>>,
    pub ingress: Option<Arc<AudioRoute>>,
    /// Puppet egress route captured at RTP admission, before any fanout queue.
    pub egress: Option<Arc<AudioRoute>>,
}

/// Original media clock and ordering identity, before SFU timestamp rewriting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceClock {
    Rtp {
        ssrc: u32,
        sequence: u64,
        ticks: u32,
        generation: u64,
    },
    /// `spurt` changes with each talk spurt: ids do not advance across silence.
    TeamSpeak {
        id: u16,
        whisper: bool,
        spurt: u32,
    },
}

impl SourceClock {
    /// None means a new clock; Err means a duplicate or reordered packet.
    pub(crate) fn delta(self, previous: Self) -> Result<Option<u32>, ()> {
        match (self, previous) {
            (
                Self::Rtp { ssrc, sequence, ticks, generation },
                Self::Rtp { ssrc: old, sequence: seq, ticks: time, generation: old_generation },
            ) if ssrc == old => {
                if sequence <= seq {
                    return Err(());
                }
                if generation != old_generation {
                    return Ok(None);
                }
                if ticks == time {
                    return Err(());
                }
                Ok(Some(ticks.wrapping_sub(time)))
            }
            (
                Self::TeamSpeak { id, whisper, spurt },
                Self::TeamSpeak { id: old, whisper: was_whisper, spurt: old_spurt },
            ) if whisper == was_whisper => {
                match spurt.wrapping_sub(old_spurt) {
                    0 => {}
                    newer if newer < 1 << 31 => return Ok(None),
                    _ => return Err(()),
                }
                let delta = id.wrapping_sub(old);
                if delta == 0 || delta >= 0x8000 {
                    return Err(());
                }
                Ok(Some(u32::from(delta) * 960))
            }
            _ => Ok(None),
        }
    }
}

impl AudioPacket {
    pub fn end_of_speech(origin: Arc<AudioOrigin>, route: Option<Arc<AudioRoute>>) -> Self {
        Self {
            origin,
            route,
            target: None,
            ingress: None,
            egress: None,
            payload: Arc::from([]),
            kind: AudioKind::EndOfSpeech,
            span: 0,
            clock: SourceClock::TeamSpeak { id: 0, whisper: false, spurt: 0 },
        }
    }
    pub fn from(&self) -> SessionId {
        self.origin.session
    }
    pub fn is_end(&self) -> bool {
        self.kind == AudioKind::EndOfSpeech
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub channel: ChannelId,
    pub muted: bool,
    pub deafened: bool,
    pub bridged: bool,
    pub origin: Arc<AudioOrigin>,
}

/// Who is where, as seen by the media plane.
#[derive(Debug, Default, Clone)]
pub struct Routing {
    pub members: HashMap<SessionId, Member>,
}

impl Routing {
    pub fn accepts(&self, packet: &AudioPacket) -> bool {
        self.members.get(&packet.from()).is_some_and(|m| m.origin == packet.origin)
            && packet.target.as_ref().is_none_or(|target| self.origin(target.session).as_ref() == Some(target))
            && packet.route.as_ref().is_none_or(|route| route.permits(&packet.origin))
            && packet.ingress.as_ref().is_none_or(|route| {
                route
                    .origin
                    .as_ref()
                    .is_some_and(|origin| route.permits(origin) && self.origin(origin.session).as_ref() == Some(origin))
            })
    }
    pub fn accepts_whisper(&self, packet: &AudioPacket, to: SessionId) -> bool {
        self.accepts(packet) && !packet.origin.encrypted && self.can_whisper(packet.from(), to)
    }
    pub fn origin(&self, session: SessionId) -> Option<Arc<AudioOrigin>> {
        self.members.get(&session).map(|m| m.origin.clone())
    }

    pub fn can_speak(&self, from: SessionId) -> bool {
        self.members.get(&from).is_some_and(|m| !m.muted && !m.deafened)
    }

    /// Whether `to` should hear a whisper from `from`, wherever both are.
    pub fn can_whisper(&self, from: SessionId, to: SessionId) -> bool {
        from != to
            && self.can_speak(from)
            && self.members.get(&from).is_some_and(|m| m.bridged && !m.origin.encrypted)
            && self.members.get(&to).is_some_and(|m| !m.deafened && m.bridged && !m.origin.encrypted)
    }

    /// Everyone who should hear `from`: same channel, not deafened, not the speaker.
    pub fn recipients(&self, from: SessionId) -> impl Iterator<Item = SessionId> + '_ {
        let channel = self.members.get(&from).filter(|m| !m.muted && !m.deafened).map(|m| m.channel);
        self.members
            .iter()
            .filter(move |(id, m)| Some(m.channel) == channel && **id != from && !m.deafened)
            .map(|(id, _)| *id)
    }
}

pub type SharedRouting = Arc<ArcSwap<Routing>>;

/// Audio receivers living outside the WebRTC loop, keyed by session.
#[derive(Default)]
pub struct Sinks(RwLock<HashMap<SessionId, Sink>>);
struct Sink {
    tx: mpsc::Sender<AudioPacket>,
    route: Option<Arc<AudioRoute>>,
}

impl Sinks {
    pub fn register(&self, session: SessionId, sink: mpsc::Sender<AudioPacket>) {
        self.0.write().expect("sinks lock").insert(session, Sink { tx: sink, route: None });
    }

    pub fn route(&self, session: SessionId) -> Option<Arc<AudioRoute>> {
        self.0.read().expect("sinks lock").get(&session).and_then(|sink| sink.route.clone())
    }

    pub fn set_route(&self, session: SessionId, route: Arc<AudioRoute>) {
        if let Some(sink) = self.0.write().expect("sinks lock").get_mut(&session) {
            sink.route = Some(route);
        }
    }

    pub fn unregister(&self, session: SessionId) {
        self.0.write().expect("sinks lock").remove(&session);
    }

    /// Removes `sink` only if it is still the one registered for `session`.
    pub fn unregister_sink(&self, session: SessionId, sink: &mpsc::Sender<AudioPacket>) {
        let mut sinks = self.0.write().expect("sinks lock");
        if sinks.get(&session).is_some_and(|current| current.tx.same_channel(sink)) {
            sinks.remove(&session);
        }
    }

    /// External sinks are plaintext boundaries, including targeted whispers.
    pub fn send(&self, routing: &Routing, session: SessionId, packet: &AudioPacket) {
        if !routing.accepts(packet)
            || packet.origin.encrypted
            || !routing.members.get(&session).is_some_and(|m| m.bridged && !m.origin.encrypted)
        {
            return;
        }
        if let Some(sink) = self.0.read().expect("sinks lock").get(&session) {
            let mut packet = packet.clone();
            packet.target = routing.origin(session);
            if let Some(route) = &sink.route {
                if !route.permits(&packet.origin)
                    || packet.egress.as_ref().is_none_or(|captured| !Arc::ptr_eq(captured, route))
                {
                    return;
                }
                packet.route = Some(route.clone());
            }
            let _ = sink.tx.try_send(packet);
        }
    }

    pub fn uplink(&self, routing: &Routing, packet: &AudioPacket) {
        if routing.accepts(packet) && (packet.is_end() || routing.can_speak(packet.from())) {
            self.send(routing, packet.from(), packet);
        }
    }

    pub fn fan_out(&self, routing: &Routing, packet: &AudioPacket) {
        if !routing.accepts(packet) || packet.origin.encrypted {
            return;
        }
        for to in routing.recipients(packet.from()) {
            self.send(routing, to, packet);
        }
    }
}

/// Handles shared by every transport that produces or consumes audio.
#[derive(Clone)]
pub struct MediaPlane {
    pub routing: SharedRouting,
    /// Listeners outside WebRTC, fed what their session should hear.
    pub sinks: Arc<Sinks>,
    /// Per-speaker taps fed everything that session says (after the speech
    /// gate), e.g. its stand-in client on a bridged TeamSpeak server.
    pub uplinks: Arc<Sinks>,
    pub webrtc: mpsc::Sender<webrtc::MediaCmd>,
}

impl MediaPlane {
    /// Routes a frame produced by a non-WebRTC transport to all listeners.
    pub fn publish(&self, packet: AudioPacket) {
        if !self.routing.load().accepts(&packet) {
            return;
        }
        self.sinks.fan_out(&self.routing.load(), &packet);
        let _ = self.webrtc.try_send(webrtc::MediaCmd::Forward(packet));
    }

    /// Routes a frame meant for `to` alone (a TeamSpeak whisper).
    pub fn deliver(&self, to: SessionId, mut packet: AudioPacket) {
        packet.target = self.routing.load().origin(to);
        if !self.routing.load().accepts_whisper(&packet, to) {
            return;
        }
        self.sinks.send(&self.routing.load(), to, &packet);
        let _ = self.webrtc.try_send(webrtc::MediaCmd::ForwardTo(to, packet));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn member(session: SessionId, channel: ChannelId, encrypted: bool) -> Member {
        Member {
            channel,
            muted: false,
            deafened: false,
            bridged: !encrypted,
            origin: Arc::new(AudioOrigin {
                channel,
                encrypted,
                session,
                binding: VoiceBinding {
                    call: format!("call-{channel}"),
                    participation: format!("entry-{session}"),
                    run: Some(1),
                },
            }),
        }
    }

    fn frame(m: &Member) -> AudioPacket {
        AudioPacket {
            origin: m.origin.clone(),
            payload: Arc::from([0xf8, 1]),
            kind: AudioKind::Audio,
            span: 960,
            clock: SourceClock::TeamSpeak { id: 1, whisper: false, spurt: 1 },
            route: None,
            target: None,
            ingress: None,
            egress: None,
        }
    }

    #[test]
    fn recipients_follow_channel_and_flags() {
        let mut routing = Routing::default();
        for (id, channel) in [(1, 10), (2, 10), (3, 10), (4, 11)] {
            routing.members.insert(id, member(id, channel, false));
        }
        routing.members.get_mut(&3).unwrap().deafened = true;
        assert_eq!(routing.recipients(1).collect::<Vec<_>>(), [2]);
        routing.members.get_mut(&1).unwrap().muted = true;
        assert_eq!(routing.recipients(1).count(), 0);
        assert!(!routing.can_speak(1));
    }

    #[test]
    fn whispers_cross_channels_but_not_deafness_or_encryption() {
        let mut routing = Routing::default();
        for (id, channel) in [(1, 10), (2, 11), (3, 11)] {
            routing.members.insert(id, member(id, channel, false));
        }
        routing.members.get_mut(&3).unwrap().deafened = true;
        assert!(routing.can_whisper(1, 2));
        for to in [1, 3, 9] {
            assert!(!routing.can_whisper(1, to));
        }
        routing.members.insert(2, member(2, 11, true));
        assert!(!routing.can_whisper(1, 2));
    }

    #[test]
    fn immutable_origin_and_route_fence_every_external_queue() {
        let sinks = Sinks::default();
        let (tx, mut rx) = mpsc::channel(8);
        sinks.register(1, tx.clone());
        sinks.register(2, tx);
        let mut routing = Routing::default();
        for id in [1, 2] {
            routing.members.insert(id, member(id, 10, false));
        }
        let mut packet = frame(&routing.members[&1]);
        sinks.uplink(&routing, &packet);
        assert!(rx.try_recv().is_ok());
        sinks.fan_out(&routing, &packet);
        assert!(rx.try_recv().is_ok());
        let route = AudioRoute::new(1, 50, Some(packet.origin.clone()));
        sinks.set_route(1, route.clone());
        packet.egress = Some(route.clone());
        sinks.uplink(&routing, &packet);
        assert!(rx.try_recv().is_err());
        route.confirm(Some(50));
        sinks.uplink(&routing, &packet);
        let queued = rx.try_recv().unwrap();
        assert!(routing.accepts(&queued));
        route.invalidate();
        route.confirm(Some(50));
        assert!(!routing.accepts(&queued));
        sinks.uplink(&routing, &queued);
        assert!(rx.try_recv().is_err());
        let replacement_route = AudioRoute::new(2, 50, Some(packet.origin.clone()));
        replacement_route.confirm(Some(50));
        sinks.set_route(1, replacement_route.clone());
        // Delayed before fanout: its old egress route cannot become the new one.
        sinks.uplink(&routing, &packet);
        assert!(rx.try_recv().is_err());
        packet.egress = Some(replacement_route.clone());
        sinks.uplink(&routing, &packet);
        assert!(rx.try_recv().is_ok());
        let mut incoming = frame(&routing.members[&2]);
        incoming.ingress = Some(replacement_route.clone());
        assert!(routing.accepts(&incoming));
        replacement_route.invalidate();
        assert!(!routing.accepts(&incoming));
        // The packet's original plaintext channel is retained after a move.
        routing.members.insert(1, member(1, 11, false));
        sinks.fan_out(&routing, &packet);
        assert!(rx.try_recv().is_err());
        routing.members.insert(1, member(1, 10, true));
        let encrypted = frame(&routing.members[&1]);
        sinks.fan_out(&routing, &encrypted);
        sinks.uplink(&routing, &encrypted);
        sinks.send(&routing, 2, &encrypted);
        assert!(rx.try_recv().is_err());
        routing.members.insert(1, member(1, 10, false));
        sinks.fan_out(&routing, &encrypted);
        assert!(rx.try_recv().is_err());
        // Re-entry into the same channel cannot revive queued audio.
        let mut replacement = member(1, 10, false);
        Arc::make_mut(&mut replacement.origin).binding.participation = "new-entry".into();
        routing.members.insert(1, replacement);
        assert!(!routing.accepts(&packet));
    }
}
