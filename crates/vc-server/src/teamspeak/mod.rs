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
pub mod process;
pub mod puppet;
pub mod query;

pub use bridge::run;
