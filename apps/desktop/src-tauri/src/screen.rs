//! Screen sharing in the desktop app (docs/screen-share.md, §1 "Desktop app"). The webview
//! captures, encodes and sends a share exactly as the browser does; the shell only says
//! whether this platform may share. Video never crosses into Rust.

use serde::Serialize;

/// Whether the webview may share the screen here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capture {
    /// macOS 14 or later (WebKit's `SCContentSharingPicker`), or Windows (WebView2's picker).
    Ok,
    /// macOS before 14: the owner gated desktop sharing to 14 and later.
    MacosTooOld,
    /// Linux: distro WebKitGTK has no WebRTC, so the app neither shares nor watches (deferred).
    Unsupported,
}

/// The platform, as far as sharing cares.
enum Os {
    /// With its major version.
    Macos(isize),
    Windows,
    Other,
}

/// macOS 14 is the first with `SCContentSharingPicker`, the system picker WebKit uses.
fn capture(os: Os) -> Capture {
    match os {
        Os::Macos(major) if major >= 14 => Capture::Ok,
        Os::Macos(_) => Capture::MacosTooOld,
        Os::Windows => Capture::Ok,
        Os::Other => Capture::Unsupported,
    }
}

#[tauri::command]
pub fn screen_capture_support() -> Capture {
    capture(if cfg!(target_os = "macos") {
        Os::Macos(macos_major())
    } else if cfg!(windows) {
        Os::Windows
    } else {
        Os::Other
    })
}

#[cfg(target_os = "macos")]
fn macos_major() -> isize {
    objc2_foundation::NSProcessInfo::processInfo().operatingSystemVersion().majorVersion
}

#[cfg(not(target_os = "macos"))]
fn macos_major() -> isize {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_14_and_later_and_windows_share_linux_does_not() {
        assert_eq!(capture(Os::Macos(11)), Capture::MacosTooOld);
        assert_eq!(capture(Os::Macos(13)), Capture::MacosTooOld);
        assert_eq!(capture(Os::Macos(14)), Capture::Ok);
        assert_eq!(capture(Os::Macos(26)), Capture::Ok);
        assert_eq!(capture(Os::Windows), Capture::Ok);
        assert_eq!(capture(Os::Other), Capture::Unsupported);
    }

    #[test]
    fn the_answer_is_what_the_web_ui_expects() {
        let names: Vec<String> = [Capture::Ok, Capture::MacosTooOld, Capture::Unsupported]
            .iter()
            .map(|c| serde_json::to_string(c).unwrap())
            .collect();
        assert_eq!(names, ["\"ok\"", "\"macos_too_old\"", "\"unsupported\""]);
    }

    #[test]
    fn this_platform_answers() {
        let answer = screen_capture_support();
        if cfg!(target_os = "linux") {
            assert_eq!(answer, Capture::Unsupported);
        } else if cfg!(windows) {
            assert_eq!(answer, Capture::Ok);
        } else if cfg!(target_os = "macos") {
            // This machine runs a macOS that shares (the build hosts are 14 or later).
            assert_eq!(answer, Capture::Ok);
        }
    }
}
