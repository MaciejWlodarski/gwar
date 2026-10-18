//! Voice for a session: the native engine of `vc-client` (Opus, adaptive
//! playout, WebRTC to the server's SFU) on this device's audio. Plaintext
//! channels only for now; encrypted voice needs the MLS client in Rust.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use vc_client::voice::{InputMode, LinkState, VoiceHandle, io::AudioIo};

/// The microphone opens above this level in voice activity mode (dBFS), as on the desktop.
pub const VOICE_ACTIVITY_DB: f32 = -50.0;
/// The range offered for that threshold.
pub const VOICE_ACTIVITY_MIN_DB: f32 = -80.0;
pub const VOICE_ACTIVITY_MAX_DB: f32 = -10.0;

/// The link of the running voice session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum VoiceLink {
    Connecting,
    Connected,
    /// The transport or an audio device failed; start voice again.
    Lost,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct VoiceStatus {
    pub link: VoiceLink,
    /// The microphone is being sent (above the voice activity threshold, not muted).
    pub transmitting: bool,
    /// Microphone level in dBFS, for a meter.
    pub input_level_db: f32,
    pub frames_sent: u64,
    pub frames_received: u64,
    /// Times the speaker had to play silence it was not given.
    pub underruns: u64,
}

/// What the user set, applied to every engine the session starts.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Controls {
    pub muted: bool,
    pub deafened: bool,
    pub push_to_talk: bool,
    /// The push-to-talk button is held.
    pub talk_held: bool,
    /// Voice activity opens the microphone above this level (dBFS).
    pub threshold_db: f32,
}

impl Default for Controls {
    fn default() -> Self {
        Self { muted: false, deafened: false, push_to_talk: false, talk_held: false, threshold_db: VOICE_ACTIVITY_DB }
    }
}

/// A running engine.
pub(crate) struct Voice {
    handle: VoiceHandle,
    /// Set by the audio streams when a device fails.
    audio_lost: Arc<AtomicBool>,
}

impl Voice {
    pub fn new(handle: VoiceHandle, audio_lost: Arc<AtomicBool>, controls: Controls) -> Self {
        let voice = Self { handle, audio_lost };
        voice.apply(controls);
        voice
    }

    pub fn apply(&self, controls: Controls) {
        self.handle.set_muted(controls.muted);
        self.handle.set_deafened(controls.deafened);
        self.handle.set_input_mode(if controls.push_to_talk {
            InputMode::PushToTalk
        } else {
            InputMode::VoiceActivity { threshold_db: controls.threshold_db }
        });
        self.handle.set_ptt(controls.talk_held);
    }

    pub fn status(&self) -> VoiceStatus {
        let stats = self.handle.stats();
        let link = match stats.link {
            _ if self.audio_lost.load(Ordering::Relaxed) => VoiceLink::Lost,
            LinkState::Connecting => VoiceLink::Connecting,
            LinkState::Connected => VoiceLink::Connected,
            LinkState::Lost => VoiceLink::Lost,
        };
        VoiceStatus {
            link,
            transmitting: self.handle.transmitting(),
            input_level_db: self.handle.input_level_db(),
            frames_sent: stats.frames_sent,
            frames_received: stats.frames_received,
            underruns: stats.underruns,
        }
    }
}

/// This device's microphone and speaker, and the flag their streams raise on failure.
pub(crate) fn device_io() -> (Box<dyn AudioIo>, Arc<AtomicBool>) {
    let lost = Arc::new(AtomicBool::new(false));
    #[cfg(target_os = "android")]
    let io: Box<dyn AudioIo> = Box::new(crate::aaudio::AAudioIo { lost: lost.clone() });
    #[cfg(not(target_os = "android"))]
    let io: Box<dyn AudioIo> = Box::new(vc_client::voice::io::DeviceIo::new(None, None));
    (io, lost)
}
