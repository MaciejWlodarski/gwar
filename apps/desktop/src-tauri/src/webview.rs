//! What may run in the app's webview (docs/architecture.md, "Native command surface").
//!
//! Only the app's own document runs there: the main window navigates nowhere else, it opens no
//! windows of its own (links open in the system browser), and its CSP allows no frames. That is
//! what keeps screen capture to the app: neither WebKit (its own picker path) nor WebView2
//! (Chromium's picker) asks the permission handler about display capture, so no permission
//! answer can limit it. It is limited because no other document can be there to ask.
//!
//! Webview microphone and camera requests are denied (`media_permission`); on Windows a grant
//! saved in the WebView2 profile would skip that question, so it is overridden first.

#[cfg(windows)]
use tauri::WebviewUrl;
use tauri::{
    App, Runtime, Url, WebviewWindowBuilder,
    webview::{NewWindowResponse, PermissionKind, PermissionResponse},
};

/// Whether `url` is the app's document: its bundled origin, or the dev server under
/// `tauri dev`. An empty document runs nothing and is how the Windows window starts.
pub(crate) fn app_document(url: &Url, dev: Option<&Url>) -> bool {
    let origin = |u: &Url| (u.scheme().to_owned(), u.host_str().map(str::to_owned), u.port_or_known_default());
    if url.as_str() == "about:blank" {
        return true;
    }
    let bundled =
        matches!((url.scheme(), url.host_str()), ("tauri", Some("localhost")) | ("http", Some("tauri.localhost")))
            && url.port().is_none();
    bundled || dev.is_some_and(|dev| origin(dev) == origin(url))
}

/// A link the app's document opens in a new window, opened in the system browser instead.
fn open_link(url: &Url) {
    if matches!(url.scheme(), "http" | "https")
        && let Err(e) = open::that_detached(url.as_str())
    {
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
    let config = app.config().app.windows.iter().find(|w| w.label == "main").cloned().expect("main window config");
    let dev = dev_url(app);
    // On Windows the window starts empty: the app is loaded once saved media grants are overridden.
    #[cfg(windows)]
    let (config, app_url) = (
        tauri::utils::config::WindowConfig {
            url: WebviewUrl::External(Url::parse("about:blank").expect("blank")),
            ..config
        },
        dev.clone().unwrap_or_else(|| Url::parse("http://tauri.localhost/").expect("app URL")),
    );
    let builder = WebviewWindowBuilder::from_config(app.handle(), &config)?
        .on_navigation(move |url| {
            let allowed = app_document(url, dev.as_ref());
            if !allowed {
                tracing::warn!("blocked a navigation away from the app");
            }
            allowed
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
        for ok in ["tauri://localhost/", "tauri://localhost/index.html#x", "http://tauri.localhost/", "about:blank"] {
            assert!(app_document(&url(ok), None), "{ok}");
        }
        for no in [
            "https://gwar.app/",
            "https://tauri.localhost/",
            "http://tauri.localhost:8080/",
            "http://localhost:5173/",
            "tauri://evil/",
            "data:text/html,x",
            "blob:tauri://localhost/1",
            "file:///etc/passwd",
            "about:srcdoc",
        ] {
            assert!(!app_document(&url(no), None), "{no}");
        }
        let dev = url("http://localhost:5173");
        assert!(app_document(&url("http://localhost:5173/src/main.tsx"), Some(&dev)));
        assert!(!app_document(&url("http://localhost:5174/"), Some(&dev)));
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
