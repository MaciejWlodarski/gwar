//! Operating-system permission to use the microphone.
//!
//! - **macOS** asks through TCC (AVFoundation) before the input stream is built: without
//!   permission CoreAudio would happily deliver silence instead of failing. The prompt text comes from
//!   `NSMicrophoneUsageDescription` in the app's `Info.plist`, and the hardened runtime also needs the
//!   `com.apple.security.device.audio-input` entitlement (both live in `apps/desktop/src-tauri`).
//!   An unbundled binary (`tauri dev`, `cargo run`) is attributed to the terminal that started it,
//!   so the dialog and the entry in System Settings name the terminal, not Gwar.
//! - **Windows** has no API for this that works for desktop apps; when the privacy switch is off, opening
//!   the stream fails and [`is_denied`] recognises the error.
//! - **Linux** has no permission model.

/// The outcome of [`ensure_microphone_access`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicPermission {
    Granted,
    /// Refused by the user, or restricted by policy (parental controls, MDM).
    Denied,
}

/// The input device cannot be used because the OS denies microphone access.
/// Carried inside the `anyhow` error of [`super::io::AudioIo::open`] so callers can tell it apart.
#[derive(Debug, thiserror::Error)]
#[error("microphone permission denied")]
pub struct MicPermissionDenied;

/// Checks the permission and, if the OS has not asked yet, asks and waits for the answer.
/// Blocks (possibly for as long as the dialog is open), so call it off the async runtime.
pub fn ensure_microphone_access() -> MicPermission {
    imp::ensure()
}

/// True if `error` (anywhere in its chain) means the OS refused access to the microphone.
pub fn is_denied(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.is::<MicPermissionDenied>()
            || cause
                .downcast_ref::<cpal::Error>()
                .is_some_and(|e| e.kind() == cpal::ErrorKind::PermissionDenied || message_means_denied(&e.to_string()))
    })
}

/// WASAPI reports a blocked microphone as `E_ACCESSDENIED` (0x80070005), which cpal does not classify
/// and passes on as the OS error text (localized, but always with the numeric code).
fn message_means_denied(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("-2147024891") || m.contains("0x80070005") || m.contains("access is denied")
}

#[cfg(target_os = "macos")]
mod imp {
    use std::sync::mpsc;

    use block2::RcBlock;
    use objc2::runtime::Bool;
    use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};

    use super::MicPermission;

    pub fn ensure() -> MicPermission {
        // SAFETY: plain class-method calls with a valid media type constant.
        let Some(audio) = (unsafe { AVMediaTypeAudio }) else { return MicPermission::Granted };
        let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(audio) };
        if status == AVAuthorizationStatus::Authorized {
            return MicPermission::Granted;
        }
        if status != AVAuthorizationStatus::NotDetermined {
            tracing::warn!("microphone permission: denied or restricted by the system");
            return MicPermission::Denied;
        }
        tracing::info!("microphone permission: asking the user");
        let (tx, rx) = mpsc::channel();
        let handler = RcBlock::new(move |granted: Bool| {
            let _ = tx.send(granted.as_bool());
        });
        unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(audio, &handler) };
        let granted = rx.recv().unwrap_or(false);
        tracing::info!("microphone permission: {}", if granted { "granted" } else { "refused" });
        if granted { MicPermission::Granted } else { MicPermission::Denied }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::MicPermission;

    pub fn ensure() -> MicPermission {
        MicPermission::Granted
    }
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;

    use super::*;

    #[test]
    fn recognises_denied_errors() {
        assert!(is_denied(&anyhow!(MicPermissionDenied)));
        assert!(is_denied(&anyhow::Error::new(cpal::Error::new(cpal::ErrorKind::PermissionDenied))));
        // Context added on top keeps the cause visible.
        let wrapped = anyhow::Error::new(cpal::Error::new(cpal::ErrorKind::PermissionDenied)).context("opening input");
        assert!(is_denied(&wrapped));
        // WASAPI: E_ACCESSDENIED surfaces as a backend error with the OS text.
        let wasapi =
            cpal::Error::with_message(cpal::ErrorKind::BackendError, "Access is denied. (os error -2147024891)");
        assert!(is_denied(&anyhow::Error::new(wasapi)));
    }

    #[test]
    fn other_errors_are_not_denied() {
        assert!(!is_denied(&anyhow!("no microphone found")));
        assert!(!is_denied(&anyhow::Error::new(cpal::Error::new(cpal::ErrorKind::DeviceBusy))));
        let other = cpal::Error::with_message(cpal::ErrorKind::BackendError, "The parameter is incorrect.");
        assert!(!is_denied(&anyhow::Error::new(other)));
    }
}
