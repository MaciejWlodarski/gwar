//! Gwar Connect: optional accounts that keep one identity across devices.
//! Design, cryptography and API: docs/connect.md.

pub mod api;
pub mod crypto;
pub mod store;

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use axum::{
    Router,
    http::{HeaderValue, Method, header},
    routing::{get, post, put},
};
use tokio::net::TcpListener;
use tower_http::cors::{AllowOrigin, CorsLayer};

pub use api::Connect;
use store::Store;

pub struct Config {
    /// SQLite database; `None` keeps everything in memory (tests).
    pub database: Option<PathBuf>,
    pub bind: SocketAddr,
    /// Web app origins allowed to call the API from a browser.
    pub origins: Vec<String>,
}

/// Origins of the desktop app's webview on each platform.
pub const DESKTOP_ORIGINS: [&str; 3] = ["tauri://localhost", "http://tauri.localhost", "https://tauri.localhost"];

pub fn router(state: Arc<Connect>, origins: &[String]) -> Router {
    let origins: Vec<HeaderValue> = origins.iter().filter_map(|o| o.parse().ok()).collect();
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_methods([Method::GET, Method::POST, Method::PUT])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
        .max_age(std::time::Duration::from_secs(3600));
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/v1/register", post(api::register))
        .route("/v1/prelogin", post(api::prelogin))
        .route("/v1/login", post(api::login))
        .route("/v1/recover", post(api::recover))
        .route("/v1/account", get(api::account))
        .route("/v1/account/password", put(api::password))
        .route("/v1/devices", get(api::devices).post(api::add_device))
        .route("/v1/devices/revoke", post(api::revoke_device))
        .route("/v1/devices/renew", post(api::renew_devices))
        .route("/v1/vault", get(api::vault).put(api::put_vault))
        .route("/v1/logout", post(api::logout))
        .route("/v1/revocations", get(api::revocations))
        .route("/v1/accounts/by-key/{account_key}", get(api::lookup_by_key))
        .route("/v1/accounts/{handle}", get(api::lookup))
        .layer(cors)
        .with_state(state)
}

/// Starts the service; returns the bound address.
pub async fn start(config: Config) -> Result<SocketAddr> {
    let store = match &config.database {
        Some(path) => Store::open(path)?,
        None => Store::in_memory()?,
    };
    let state = Arc::new(Connect::new(store)?);
    let app = router(state, &config.origins);
    let listener = TcpListener::bind(config.bind).await.context("bind HTTP listener")?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        let app = app.into_make_service_with_connect_info::<SocketAddr>();
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("http server: {e}");
        }
    });
    Ok(addr)
}
