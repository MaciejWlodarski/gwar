//! HTTPS without a reverse proxy: a certificate from files, or one obtained
//! and renewed automatically from Let's Encrypt (TLS-ALPN-01 on the HTTPS
//! port itself, so nothing else has to listen on port 80).
//!
//! Browsers on the official web app (https) can only reach servers over
//! `wss://`, so self-hosted servers need a certificate either way.

use std::{net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result};
use axum::{
    Router,
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use futures::StreamExt;
use rustls_acme::{AcmeConfig, caches::DirCache};
use tokio::{net::TcpListener, task::JoinHandle};
use tracing::{error, info, warn};

pub enum Tls {
    /// PEM certificate chain and private key (e.g. from your own ACME client).
    Files { cert: PathBuf, key: PathBuf },
    /// Let's Encrypt for `domains`; account and certificates are cached in `cache`.
    Acme { domains: Vec<String>, email: Option<String>, cache: PathBuf, staging: bool },
}

/// Serves `app` on `listener`, with TLS when configured.
pub async fn serve(listener: TcpListener, app: Router, tls: Option<Tls>) -> Result<Vec<JoinHandle<()>>> {
    let service = app.into_make_service_with_connect_info::<SocketAddr>();
    let Some(tls) = tls else {
        return Ok(vec![tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, service).await {
                error!("http server: {e}");
            }
        })]);
    };
    let listener = listener.into_std()?;
    match tls {
        Tls::Files { cert, key } => {
            let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
                .await
                .with_context(|| format!("load TLS certificate {} / {}", cert.display(), key.display()))?;
            let server = axum_server::from_tcp_rustls(listener, config)?;
            Ok(vec![tokio::spawn(async move {
                if let Err(e) = server.serve(service).await {
                    error!("https server: {e}");
                }
            })])
        }
        Tls::Acme { domains, email, cache, staging } => {
            std::fs::create_dir_all(&cache).with_context(|| format!("create {}", cache.display()))?;
            info!(?domains, staging, "obtaining TLS certificates from Let's Encrypt");
            let mut state = AcmeConfig::new(domains)
                .contact(email.iter().map(|e| format!("mailto:{e}")))
                .cache(DirCache::new(cache))
                .directory_lets_encrypt(!staging)
                .state();
            let acceptor = state.axum_acceptor(state.default_rustls_config());
            let renewals = tokio::spawn(async move {
                while let Some(event) = state.next().await {
                    match event {
                        Ok(ok) => info!("acme: {ok:?}"),
                        Err(e) => warn!("acme: {e}"),
                    }
                }
            });
            let server = axum_server::from_tcp(listener)?.acceptor(acceptor);
            Ok(vec![
                renewals,
                tokio::spawn(async move {
                    if let Err(e) = server.serve(service).await {
                        error!("https server: {e}");
                    }
                }),
            ])
        }
    }
}

/// Plain HTTP that sends everyone to the same path over HTTPS.
pub async fn redirect_to_https(bind: SocketAddr) -> Result<JoinHandle<()>> {
    let listener = TcpListener::bind(bind).await.with_context(|| format!("bind HTTP redirect on {bind}"))?;
    let app =
        Router::new().fallback(|headers: axum::http::HeaderMap, uri: Uri| async move { redirect(&headers, &uri) });
    Ok(tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            error!("http redirect: {e}");
        }
    }))
}

fn redirect(headers: &axum::http::HeaderMap, uri: &Uri) -> Response {
    let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    // Drop the port: HTTPS is on the default one.
    let host = host.rsplit_once(':').filter(|(_, port)| port.parse::<u16>().is_ok()).map_or(host, |(h, _)| h);
    let path = uri.path_and_query().map_or("/", |p| p.as_str());
    let location = format!("https://{host}{path}");
    (StatusCode::PERMANENT_REDIRECT, [(header::LOCATION, location)]).into_response()
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue, Uri, header};

    use super::redirect;

    #[test]
    fn redirects_keep_the_path_and_drop_the_port() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("voice.example.com:80"));
        let response = redirect(&headers, &Uri::from_static("/files/a/b.png?x=1"));
        assert_eq!(response.status(), 308);
        assert_eq!(response.headers()[header::LOCATION], "https://voice.example.com/files/a/b.png?x=1");
    }
}
