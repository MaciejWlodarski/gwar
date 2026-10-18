//! Voice for a session: the native engine of `vc-client` (Opus, adaptive
//! playout, WebRTC to the server's SFU) on this device's audio. Plaintext
//! channels only for now; encrypted voice needs the MLS client in Rust.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use vc_client::voice::{LinkState, VoiceHandle, io::AudioIo};

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

/// A running engine.
pub(crate) struct Voice {
    handle: VoiceHandle,
    /// Set by the audio streams when a device fails.
    audio_lost: Arc<AtomicBool>,
}

impl Voice {
    pub fn new(handle: VoiceHandle, audio_lost: Arc<AtomicBool>, muted: bool, deafened: bool) -> Self {
        handle.set_muted(muted);
        handle.set_deafened(deafened);
        Self { handle, audio_lost }
    }

    pub fn set_muted(&self, muted: bool) {
        self.handle.set_muted(muted);
    }

    pub fn set_deafened(&self, deafened: bool) {
        self.handle.set_deafened(deafened);
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
