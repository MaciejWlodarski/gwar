//! Media plane. Opus frames are forwarded between transports without being
//! decoded: the server only decides *who hears whom*, the receiving client
//! decodes and mixes. The control plane (core actor) publishes a routing
//! snapshot that every transport reads lock-free on the hot path.

pub mod opus;
pub mod webrtc;

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use arc_swap::ArcSwap;
use tokio::sync::mpsc;
use vc_proto::{ChannelId, SessionId};

/// One Opus frame from a speaker. `samples == 0` marks the end of a talk spurt.
#[derive(Debug, Clone)]
pub struct AudioPacket {
    pub from: SessionId,
    pub opus: Arc<[u8]>,
    pub samples: u32,
}

impl AudioPacket {
    pub fn end_of_speech(from: SessionId) -> Self {
        Self { from, opus: Arc::from([]), samples: 0 }
    }

    pub fn is_end(&self) -> bool {
        self.samples == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Member {
    pub channel: ChannelId,
    pub muted: bool,
    pub deafened: bool,
}

/// Who is where, as seen by the media plane.
#[derive(Debug, Default, Clone)]
pub struct Routing {
    pub members: HashMap<SessionId, Member>,
}

impl Routing {
    pub fn can_speak(&self, from: SessionId) -> bool {
        self.members.get(&from).is_some_and(|m| !m.muted && !m.deafened)
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
pub struct Sinks(RwLock<HashMap<SessionId, mpsc::Sender<AudioPacket>>>);

impl Sinks {
    pub fn register(&self, session: SessionId, sink: mpsc::Sender<AudioPacket>) {
        self.0.write().expect("sinks lock").insert(session, sink);
    }

    pub fn unregister(&self, session: SessionId) {
        self.0.write().expect("sinks lock").remove(&session);
    }

    /// Removes `sink` only if it is still the one registered for `session`.
    pub fn unregister_sink(&self, session: SessionId, sink: &mpsc::Sender<AudioPacket>) {
        let mut sinks = self.0.write().expect("sinks lock");
        if sinks.get(&session).is_some_and(|current| current.same_channel(sink)) {
            sinks.remove(&session);
        }
    }

    /// Delivers to the sink registered for `session`, if any.
    pub fn send(&self, session: SessionId, packet: &AudioPacket) {
        if let Some(sink) = self.0.read().expect("sinks lock").get(&session) {
            let _ = sink.try_send(packet.clone());
        }
    }

    /// Delivers to every external recipient of `packet`; returns nothing
    /// because real-time audio is dropped rather than queued when a sink lags.
    pub fn fan_out(&self, routing: &Routing, packet: &AudioPacket) {
        let sinks = self.0.read().expect("sinks lock");
        if sinks.is_empty() {
            return;
        }
        for to in routing.recipients(packet.from) {
            if let Some(sink) = sinks.get(&to) {
                let _ = sink.try_send(packet.clone());
            }
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
        self.sinks.fan_out(&self.routing.load(), &packet);
        let _ = self.webrtc.try_send(webrtc::MediaCmd::Forward(packet));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipients_follow_channel_and_flags() {
        let mut routing = Routing::default();
        let m = |channel, muted, deafened| Member { channel, muted, deafened };
        routing.members.insert(1, m(10, false, false));
        routing.members.insert(2, m(10, false, false));
        routing.members.insert(3, m(10, false, true));
        routing.members.insert(4, m(11, false, false));
        let mut heard: Vec<_> = routing.recipients(1).collect();
        heard.sort();
        assert_eq!(heard, vec![2]);
        routing.members.insert(1, m(10, true, false));
        assert_eq!(routing.recipients(1).count(), 0);
        assert!(!routing.can_speak(1));
    }
}
