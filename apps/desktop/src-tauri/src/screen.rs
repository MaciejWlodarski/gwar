//! Screen sharing in the desktop app (docs/screen-share.md, §1 "Desktop app"). The webview
//! captures, encodes and sends a share exactly as the browser does; the shell only says
//! whether this platform may share. Video never crosses into Rust.

use serde::Serialize;

/// Whether the webview may share the screen here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capture {
    /// macOS 14.6 or later (WebKit's `SCContentSharingPicker`), or Windows (WebView2's picker).
    Ok,
    /// macOS before 14 (the owner gated desktop sharing to 14 and later), or a WebKit that
    /// refuses `getDisplayMedia` before showing the picker (macOS 14.0 to 14.5). Also the
    /// answer when the WebKit version cannot be read: the gate fails closed.
    MacosTooOld,
    /// Linux: distro WebKitGTK has no WebRTC, so the app neither shares nor watches (deferred).
    Unsupported,
}

/// The platform, as far as sharing cares.
enum Os {
    /// With its major version and the loaded WebKit's version, if it could be read.
    Macos {
        major: isize,
        webkit: Option<WebKit>,
    },
    Windows,
    Other,
}

/// A WebKit version, `(major, minor, tiny)`, as in 618.3.11.
type WebKit = (u32, u32, u32);

/// The first WebKit that shows the picker for wry's webview: 618.3.11, Safari 17.6, shipped
/// with macOS 14.6. Before it (WebKit bug 274896, fixed in 279566@main and cherry-picked as
/// 618.3.6), WebKit denied `getDisplayMedia` without showing the picker whenever the UI
/// delegate implements `webView:requestMediaCapturePermissionForOrigin:…` (wry 0.57 does) but
/// not WebKit's private display-capture delegate. Fixed builds show the full picker (windows
/// and displays) when that private delegate is absent, so the app implements none.
const FIRST_WEBKIT_WITH_PICKER: WebKit = (618, 3, 11);

/// macOS 14 is the first with `SCContentSharingPicker`, the system picker WebKit uses.
fn capture(os: Os) -> Capture {
    match os {
        Os::Macos { major, webkit: Some(webkit) } if major >= 14 && webkit >= FIRST_WEBKIT_WITH_PICKER => Capture::Ok,
        Os::Macos { .. } => Capture::MacosTooOld,
        Os::Windows => Capture::Ok,
        Os::Other => Capture::Unsupported,
    }
}

/// WebKit's `CFBundleVersion` carries a platform prefix before the version ("19618.3.11.11.5"
/// is 618.3.11 on macOS 14); the last three digits of the first field are the major version.
fn parse_webkit(bundle_version: &str) -> Option<WebKit> {
    let mut fields = bundle_version.split('.').map(|f| f.parse::<u32>().ok());
    let major = fields.next()?? % 1000;
    Some((major, fields.next()??, fields.next()??))
}

#[tauri::command]
pub fn screen_capture_support() -> Capture {
    capture(if cfg!(target_os = "macos") {
        Os::Macos { major: macos_major(), webkit: webkit_bundle_version().as_deref().and_then(parse_webkit) }
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

/// The WebKit this process loaded (the system one, or a Safari update's), not a path guess.
#[cfg(target_os = "macos")]
fn webkit_bundle_version() -> Option<String> {
    use objc2_foundation::{NSBundle, NSString, ns_string};
    let bundle = NSBundle::bundleWithIdentifier(ns_string!("com.apple.WebKit"))?;
    let version = bundle.objectForInfoDictionaryKey(ns_string!("CFBundleVersion"))?;
    Some(version.downcast::<NSString>().ok()?.to_string())
}

#[cfg(not(target_os = "macos"))]
fn macos_major() -> isize {
    0
}

#[cfg(not(target_os = "macos"))]
fn webkit_bundle_version() -> Option<String> {
    None
}

/// Opens only the OS screen-recording privacy page, from the main app window.
#[tauri::command]
pub fn open_screen_privacy_settings(window: tauri::WebviewWindow) -> Result<(), String> {
    if window.label() != "main" {
        return Err("screen commands require the main window".into());
    }
    let url = if cfg!(target_os = "macos") {
        "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
    } else if cfg!(target_os = "windows") {
        "ms-settings:privacy-graphicscaptureprogrammatic"
    } else {
        return Err("this system has no screen recording privacy settings".into());
    };
    open::that(url).map_err(|e| format!("cannot open {url}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mac(major: isize, webkit: &str) -> Os {
        Os::Macos { major, webkit: parse_webkit(webkit) }
    }

    #[test]
    fn macos_14_6_and_later_and_windows_share_linux_does_not() {
        assert_eq!(capture(mac(11, "17618.3.11.10.2")), Capture::MacosTooOld);
        assert_eq!(capture(mac(13, "18619.1.26.31.6")), Capture::MacosTooOld);
        // macOS 14.0 to 14.5: WebKit refuses before the picker.
        assert_eq!(capture(mac(14, "19616.1.27.211.1")), Capture::MacosTooOld);
        assert_eq!(capture(mac(14, "19618.2.12.11.6")), Capture::MacosTooOld);
        assert_eq!(capture(mac(14, "19618.3.10.0.0")), Capture::MacosTooOld);
        // macOS 14.6 and later.
        assert_eq!(capture(mac(14, "19618.3.11.11.5")), Capture::Ok);
        assert_eq!(capture(mac(15, "20619.1.26.31.6")), Capture::Ok);
        assert_eq!(capture(mac(27, "22625.1.29.11.27")), Capture::Ok);
        assert_eq!(capture(Os::Windows), Capture::Ok);
        assert_eq!(capture(Os::Other), Capture::Unsupported);
    }

    #[test]
    fn an_unreadable_webkit_version_fails_closed() {
        assert_eq!(capture(Os::Macos { major: 27, webkit: None }), Capture::MacosTooOld);
        assert_eq!(capture(mac(27, "")), Capture::MacosTooOld);
        assert_eq!(capture(mac(27, "22625.x.29")), Capture::MacosTooOld);
    }

    #[test]
    fn webkit_versions_parse_with_or_without_the_platform_prefix() {
        assert_eq!(parse_webkit("19618.3.11.11.5"), Some((618, 3, 11)));
        assert_eq!(parse_webkit("618.3.11"), Some((618, 3, 11)));
        assert_eq!(parse_webkit("22625.1.29.11.27"), Some((625, 1, 29)));
        assert_eq!(parse_webkit("19618.3"), None);
        assert_eq!(parse_webkit("WebKit"), None);
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
        load_webkit();
        let answer = screen_capture_support();
        if cfg!(target_os = "linux") {
            assert_eq!(answer, Capture::Unsupported);
        } else if cfg!(windows) {
            assert_eq!(answer, Capture::Ok);
        } else if cfg!(target_os = "macos") {
            // This machine runs a macOS that shares (the build hosts are 14.6 or later), and
            // the version is read from the WebKit this process loaded.
            assert_eq!(answer, Capture::Ok, "WebKit {:?}", webkit_bundle_version());
        }
    }

    /// The test binary links no webview; load WebKit so its bundle is found by identifier.
    fn load_webkit() {
        #[cfg(target_os = "macos")]
        {
            use objc2_foundation::{NSBundle, ns_string};
            let bundle = NSBundle::bundleWithPath(ns_string!("/System/Library/Frameworks/WebKit.framework"));
            // SAFETY: loads the system WebKit framework, which wry links anyway.
            assert!(bundle.is_some_and(|b| unsafe { b.load() }));
        }
    }
}
