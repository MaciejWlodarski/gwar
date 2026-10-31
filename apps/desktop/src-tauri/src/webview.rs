//! What may run in the app's webview (docs/architecture.md, "Native command surface").
//!
//! Only the app's own document runs there: the main window navigates nowhere else and opens no
//! windows of its own (web pages, such as chat links, open in the system browser), and its CSP
//! allows no frames. That is
//! what keeps screen capture to the app: neither WebKit (its own picker path) nor WebView2
//! (Chromium's picker) asks the permission handler about display capture, so no permission
//! answer can limit it. It is limited because no other document can be there to ask.
//!
//! Webview microphone and camera requests are denied (`media_permission`); on Windows a grant
//! saved in the WebView2 profile would skip that question, so it is overridden first.

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

#[cfg(windows)]
use tauri::WebviewUrl;
use tauri::{
    App, Runtime, Url, WebviewWindowBuilder,
    webview::{NewWindowResponse, PermissionKind, PermissionResponse},
};

/// Where the bundled app is served. wry's custom protocol is `tauri:` on macOS and Linux; on
/// Windows WebView2 intercepts this HTTP address instead. Each platform's other address is
/// ordinary networking there, so it is not the app.
const BUNDLED_ORIGIN: &str = if cfg!(windows) { "http://tauri.localhost" } else { "tauri://localhost" };

/// What the main window does with a navigation.
#[derive(Debug, PartialEq, Eq)]
enum Navigation {
    /// The app's own document: its bundled origin, or the dev server under `tauri dev`. An
    /// empty document runs nothing and is how the Windows window starts.
    Load,
    /// A web page, such as a chat link (WebKit asks here before it would open a new window):
    /// the system browser opens it, the window stays.
    Browser,
    Refuse,
}

fn navigation(url: &Url, bundled: &Url, dev: Option<&Url>) -> Navigation {
    let origin = |u: &Url| (u.scheme().to_owned(), u.host_str().map(str::to_owned), u.port_or_known_default());
    if url.as_str() == "about:blank"
        || origin(url) == origin(bundled)
        || dev.is_some_and(|dev| origin(dev) == origin(url))
    {
        Navigation::Load
    } else if web_page(url) {
        Navigation::Browser
    } else {
        Navigation::Refuse
    }
}

fn web_page(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
}

/// When the system browser last opened a link.
static LAST_OPENED: Mutex<Option<Instant>> = Mutex::new(None);

/// At most one link a second: a document cannot flood the browser, and a click that reaches both
/// the navigation and the new-window callback opens once.
fn may_open(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|last| now.duration_since(last) >= Duration::from_secs(1))
}

/// Opens a web page in the system browser instead of the app's window.
fn open_link(url: &Url) {
    if !web_page(url) {
        return;
    }
    {
        let now = Instant::now();
        let mut last = LAST_OPENED.lock().unwrap_or_else(|e| e.into_inner());
        if !may_open(*last, now) {
            return;
        }
        *last = Some(now);
    }
    if let Err(e) = open::that_detached(url.as_str()) {
        tracing::warn!("cannot open a link: {e}");
    }
}

/// Webview media permissions where the backend asks (macOS and Windows ask for the microphone
/// and camera only). Voice is captured natively, so the renderer never needs either; without a
/// handler, wry grants them on macOS.
pub(crate) fn media_permission(kind: PermissionKind) -> PermissionResponse {
    match kind {
        PermissionKind::Microphone | PermissionKind::Camera => PermissionResponse::Deny,
        _ => PermissionResponse::Default,
    }
}

/// The dev server's URL under `tauri dev`; bundled builds load only the app's own origin.
fn dev_url<R: Runtime>(app: &App<R>) -> Option<Url> {
    if cfg!(dev) { app.config().build.dev_url.clone() } else { None }
}

/// Creates the main window from its configuration (`create: false` there) with the navigation
/// and new-window rules above.
pub(crate) fn create_main<R: Runtime>(app: &App<R>) -> tauri::Result<()> {
    let mut config = app.config().app.windows.iter().find(|w| w.label == "main").cloned().expect("main window config");
    config.visible = false;
    let dev = dev_url(app);
    let bundled = Url::parse(BUNDLED_ORIGIN).expect("bundled origin");
    // On Windows the window starts empty: the app is loaded once saved media grants are overridden.
    #[cfg(windows)]
    let (config, app_url) = (
        tauri::utils::config::WindowConfig {
            url: WebviewUrl::External(Url::parse("about:blank").expect("blank")),
            ..config
        },
        dev.clone().unwrap_or_else(|| bundled.clone()),
    );
    let builder = WebviewWindowBuilder::from_config(app.handle(), &config)?
        .on_navigation(move |url| match navigation(url, &bundled, dev.as_ref()) {
            Navigation::Load => true,
            Navigation::Browser => {
                open_link(url);
                false
            }
            Navigation::Refuse => {
                tracing::warn!("blocked a navigation away from the app");
                false
            }
        })
        .on_new_window(|url, _| {
            open_link(&url);
            NewWindowResponse::Deny
        });
    #[cfg(windows)]
    windows_media::deny_saved_media(&builder.build()?, app_url);
    #[cfg(not(windows))]
    builder.build()?;
    Ok(())
}

#[cfg(windows)]
mod windows_media {
    use std::{cell::Cell, rc::Rc};

    use tauri::{Runtime, Url, WebviewWindow};
    use webview2_com::{
        Microsoft::Web::WebView2::Win32::{
            COREWEBVIEW2_PERMISSION_KIND, COREWEBVIEW2_PERMISSION_KIND_CAMERA, COREWEBVIEW2_PERMISSION_KIND_MICROPHONE,
            COREWEBVIEW2_PERMISSION_STATE_DENY, ICoreWebView2_13, ICoreWebView2Profile4,
        },
        SetPermissionStateCompletedHandler,
    };
    use windows::core::{HSTRING, Interface};

    static KINDS: [COREWEBVIEW2_PERMISSION_KIND; 2] =
        [COREWEBVIEW2_PERMISSION_KIND_MICROPHONE, COREWEBVIEW2_PERMISSION_KIND_CAMERA];

    /// WebView2 answers `getUserMedia` from a grant saved in its profile without asking the
    /// permission handler (wry's `with_permission_handler` notes this). Earlier builds had no
    /// handler, so a user may have saved one: the microphone and camera are set to deny for the
    /// app's origin, and only then is the app loaded. Where the runtime cannot do that, the app
    /// still loads and the failure is logged.
    pub(super) fn deny_saved_media<R: Runtime>(window: &WebviewWindow<R>, app_url: Url) {
        let origin = HSTRING::from(app_url.origin().ascii_serialization());
        let target = window.clone();
        let load: Box<dyn FnOnce() + Send> = Box::new(move || {
            if let Err(e) = target.navigate(app_url) {
                tracing::error!("cannot load the app: {e}");
            }
        });
        let result = window.with_webview(move |webview| {
            let profile = unsafe { webview.controller().CoreWebView2() }
                .and_then(|core| core.cast::<ICoreWebView2_13>())
                .and_then(|core| unsafe { core.Profile() })
                .and_then(|profile| profile.cast::<ICoreWebView2Profile4>());
            match profile {
                Ok(profile) => deny(profile, origin, &KINDS, load),
                Err(e) => {
                    tracing::error!("cannot override saved microphone and camera grants: {e}");
                    load();
                }
            }
        });
        if let Err(e) = result {
            tracing::error!("cannot reach the webview: {e}");
        }
    }

    /// Sets each kind to deny in turn, then runs `done`, which runs whatever fails.
    fn deny(
        profile: ICoreWebView2Profile4,
        origin: HSTRING,
        kinds: &'static [COREWEBVIEW2_PERMISSION_KIND],
        done: Box<dyn FnOnce() + Send>,
    ) {
        let Some((&kind, rest)) = kinds.split_first() else { return done() };
        // The handler runs on completion; a call that fails at once never runs it.
        let done = Rc::new(Cell::new(Some(done)));
        let (next, next_profile, next_origin) = (done.clone(), profile.clone(), origin.clone());
        let handler = SetPermissionStateCompletedHandler::create(Box::new(move |result| {
            if let Err(e) = result {
                tracing::error!("cannot deny a saved media grant: {e}");
            }
            if let Some(done) = next.take() {
                deny(next_profile, next_origin, rest, done);
            }
            Ok(())
        }));
        if let Err(e) =
            unsafe { profile.SetPermissionState(kind, &origin, COREWEBVIEW2_PERMISSION_STATE_DENY, &handler) }
            && let Some(done) = done.take()
        {
            tracing::error!("cannot deny a saved media grant: {e}");
            deny(profile, origin, rest, done);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn only_the_apps_own_document_loads() {
        let mac = url("tauri://localhost");
        let windows = url("http://tauri.localhost");
        assert_eq!(url(BUNDLED_ORIGIN), if cfg!(windows) { windows.clone() } else { mac.clone() });
        let at = |u: &str, bundled: &Url| navigation(&url(u), bundled, None);
        for ok in ["tauri://localhost/", "tauri://localhost/index.html#x", "about:blank"] {
            assert_eq!(at(ok, &mac), Navigation::Load, "{ok}");
        }
        assert_eq!(at("http://tauri.localhost/", &windows), Navigation::Load);
        // Each platform's other address is the network there, never the app.
        assert_eq!(at("http://tauri.localhost/", &mac), Navigation::Browser);
        assert_eq!(at("tauri://localhost/", &windows), Navigation::Refuse);
        for bundled in [&mac, &windows] {
            for web in [
                "https://gwar.app/",
                "https://tauri.localhost/",
                "http://tauri.localhost:8080/",
                "http://localhost:5173/",
            ] {
                assert_eq!(at(web, bundled), Navigation::Browser, "{web}");
            }
            for no in [
                "tauri://evil/",
                "tauri://localhost:1/",
                "data:text/html,x",
                "blob:tauri://localhost/1",
                "file:///etc/passwd",
                "about:srcdoc",
                "javascript:alert(1)",
            ] {
                assert_eq!(at(no, bundled), Navigation::Refuse, "{no}");
            }
        }
        let dev = url("http://localhost:5173");
        assert_eq!(navigation(&url("http://localhost:5173/src/main.tsx"), &mac, Some(&dev)), Navigation::Load);
        assert_eq!(navigation(&url("http://localhost:5174/"), &mac, Some(&dev)), Navigation::Browser);
    }

    #[test]
    fn the_browser_opens_one_link_a_second() {
        let now = Instant::now();
        assert!(may_open(None, now));
        assert!(!may_open(Some(now), now), "the same click through both callbacks");
        assert!(!may_open(Some(now), now + Duration::from_millis(999)));
        assert!(may_open(Some(now), now + Duration::from_secs(1)));
    }

    #[test]
    fn the_webview_never_gets_the_microphone_or_camera() {
        for kind in [PermissionKind::Microphone, PermissionKind::Camera] {
            assert_eq!(media_permission(kind), PermissionResponse::Deny, "{kind}");
        }
        // Display capture never reaches the handler on macOS or Windows; see the module docs.
        assert_eq!(media_permission(PermissionKind::DisplayCapture), PermissionResponse::Default);
        assert_eq!(media_permission(PermissionKind::Notifications), PermissionResponse::Default);
    }
}
