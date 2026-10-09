//! HTTPS with a certificate from files, and CORS for the web app's origin.

mod common;

use common::*;
use vc_server::{OFFICIAL_WEB_ORIGIN, tls::Tls};

#[tokio::test]
async fn serves_https_with_a_certificate_from_files() {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (cert_path, key_path) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();
    std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();
    let server = TestServer::start_with(|c| c.tls = Some(Tls::Files { cert: cert_path, key: key_path })).await;

    let client = reqwest::Client::builder().danger_accept_invalid_certs(true).build().unwrap();
    let url = format!("https://localhost:{}/health", server.running.http.port());
    let health = client.get(&url).send().await.expect("https answers");
    assert_eq!(health.text().await.unwrap(), "ok");
    // Plain HTTP on the same port is refused.
    assert!(reqwest::get(format!("http://{}/health", server.running.http)).await.is_err());
}

#[tokio::test]
async fn only_the_web_app_origins_may_upload_cross_origin() {
    let server = TestServer::start_with(|c| c.web_origins = vec![OFFICIAL_WEB_ORIGIN.into()]).await;
    let preflight = |origin: &'static str| {
        reqwest::Client::new()
            .request(reqwest::Method::OPTIONS, format!("{}/api/files/abc", server.http()))
            .header("origin", origin)
            .header("access-control-request-method", "PUT")
            .header("access-control-request-headers", "content-type")
            .send()
    };
    let allowed = preflight(OFFICIAL_WEB_ORIGIN).await.unwrap();
    assert_eq!(allowed.headers()["access-control-allow-origin"], OFFICIAL_WEB_ORIGIN);
    assert!(allowed.headers()["access-control-allow-methods"].to_str().unwrap().contains("PUT"));
    let other = preflight("https://evil.example").await.unwrap();
    assert!(other.headers().get("access-control-allow-origin").is_none());
}
