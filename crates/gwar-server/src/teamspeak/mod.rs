//! Compatibility with official TeamSpeak clients.
//!
//! Official clients only accept servers holding a license issued by
//! TeamSpeak, so we run the official TeamSpeak server (free license: one
//! virtual server, 32 slots) as a supervised child process and bridge it into
//! our core: TeamSpeak users appear as remote sessions, ours as puppet
//! clients on the TeamSpeak server.

pub mod bridge;
pub mod channels;
pub mod install;
#[cfg(not(feature = "test-hooks"))]
mod listeners;
#[cfg(feature = "test-hooks")]
pub mod listeners;
mod packetizer;
pub mod process;
pub mod puppet;
pub mod query;

pub use bridge::run;

/// Bidi formatting characters: they reorder the text around them, so TeamSpeak names and texts
/// lose them before Gwar shows or stores them.
pub(crate) fn bidi(c: char) -> bool {
    matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}
