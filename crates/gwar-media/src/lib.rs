//! Platform-neutral contracts for native capture, codecs and presentation.
//!
//! Adapters own their workers and native resources. Callbacks must do bounded work (usually
//! enqueue into a bounded queue), never call back into the producer, and never block voice.
//! Frame timestamps use one caller-chosen monotonic epoch, shared by capture and measurement;
//! they are not wall-clock times or RTP timestamps. See `docs/native-media.md`, B1.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod adapters;
mod frame;
mod rate;
pub mod sframe;
pub mod stamp;
mod synthetic;

pub use adapters::*;
pub use frame::*;
pub use rate::*;
pub use synthetic::SyntheticScreenSource;

/// An adapter or frame validation failure. Platform adapters retain details as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaError {
    /// Invalid size, layout, configuration or stamp.
    Invalid(&'static str),
    /// A lifecycle call was made in the wrong state.
    State(&'static str),
    /// An OS, codec or output queue failed.
    Adapter(String),
}

impl std::fmt::Display for MediaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) | Self::State(message) => f.write_str(message),
            Self::Adapter(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for MediaError {}

/// Result returned by media adapters and checked frame constructors.
pub type Result<T> = std::result::Result<T, MediaError>;
