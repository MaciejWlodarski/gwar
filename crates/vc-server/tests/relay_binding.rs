//! Login signatures cannot be relayed to a server with a different public name.

mod common;

use common::*;
use ed25519_dalek::SigningKey;
use serde_json::{Value, json};
use tokio_tungstenite::{connect_async, tungstenite::client::IntoClientRequest};
use vc_proto::{ErrorCode, normalize_host};

async fn server_b() -> TestServer {
    TestServer::start_with(|c| c.public_hosts = vec!["b.example".into()]).await
}

fn hello_for(raw: &RawConn, key: &SigningKey, host: &str) -> Value {
    let mut hello = raw.hello(key, "victim", None);
    hello["host"] = json!(host);
    hello["signature"] = json!(sign(key, host, &raw.nonce));
    hello
}

#[tokio::test]
async fn accepts_identity_signed_for_its_public_host() {
    let server = server_b().await;
    let key = new_key();
    let client = Client::connect_with(&server, key.clone(), |raw, key| hello_for(raw, key, "b.example")).await.unwrap();
    assert_eq!(client.welcome.uid, uid_of(&key));
    client.close().await;
}

#[tokio::test]
async fn rejects_relayed_identity_signature_for_another_server() {
    let server = server_b().await;
    let mut raw = open(&server).await;
    // A forwards B's fresh nonce to the victim, who signs it for A.
    let hello = hello_for(&raw, &new_key(), "a.example");
    let err = raw.rejected(hello).await;
    assert_eq!(err.code, ErrorCode::NotAuthenticated);
    assert_eq!(err.message, "this signature is for another server");
    raw.expect_closed().await;
}

#[tokio::test]
async fn rejects_identity_signature_with_a_forged_host_field() {
    let server = server_b().await;
    let mut raw = open(&server).await;
    let mut hello = hello_for(&raw, &new_key(), "a.example");
    hello["host"] = json!("b.example");
    let err = raw.rejected(hello).await;
    assert_eq!(err.code, ErrorCode::NotAuthenticated);
    assert_eq!(err.message, "invalid identity signature");
    raw.expect_closed().await;
}

#[tokio::test]
async fn accepts_device_signed_for_its_public_host() {
    let server = server_b().await;
    let (account, device) = (new_key(), new_key());
    let now = vc_server::core::now_ms();
    let cert = certificate(&account, &device, now, now + 86400000);
    let client = Client::connect_with(&server, device, |raw, key| {
        let mut hello = hello_for(raw, key, "b.example");
        hello["device"] = cert;
        hello
    })
    .await
    .unwrap();
    assert_eq!(client.welcome.uid, uid_of(&account));
    client.close().await;
}

#[tokio::test]
async fn rejects_relayed_device_signature_for_another_server() {
    let server = server_b().await;
    let mut raw = open(&server).await;
    let (account, device) = (new_key(), new_key());
    let now = vc_server::core::now_ms();
    let mut hello = hello_for(&raw, &device, "a.example");
    hello["device"] = certificate(&account, &device, now, now + 86400000);
    let err = raw.rejected(hello).await;
    assert_eq!(err.code, ErrorCode::NotAuthenticated);
    assert_eq!(err.message, "this signature is for another server");
    raw.expect_closed().await;
}

#[tokio::test]
async fn rejects_device_signature_with_a_forged_host_field() {
    let server = server_b().await;
    let mut raw = open(&server).await;
    let (account, device) = (new_key(), new_key());
    let now = vc_server::core::now_ms();
    let mut hello = hello_for(&raw, &device, "a.example");
    hello["device"] = certificate(&account, &device, now, now + 86400000);
    hello["host"] = json!("b.example");
    let err = raw.rejected(hello).await;
    assert_eq!(err.code, ErrorCode::NotAuthenticated);
    assert_eq!(err.message, "invalid identity signature");
    raw.expect_closed().await;
}

#[tokio::test]
async fn unconfigured_server_accepts_any_signed_host() {
    let server = TestServer::start().await;
    for host in ["a.example", "b.example", "[::1]:8790"] {
        let client = Client::connect_with(&server, new_key(), |raw, key| hello_for(raw, key, host)).await.unwrap();
        client.close().await;
    }
}

#[tokio::test]
async fn public_url_enables_binding_and_normalizes_its_authority() {
    let server = TestServer::start_with(|c| c.public_url = Some("https://B.Example.:443/base".into())).await;
    let client = Client::connect_with(&server, new_key(), |raw, key| hello_for(raw, key, "b.example")).await.unwrap();
    client.close().await;
    let mut raw = open(&server).await;
    let hello = hello_for(&raw, &new_key(), "a.example");
    assert_eq!(raw.rejected(hello).await.code, ErrorCode::NotAuthenticated);
}

#[tokio::test]
async fn http_host_header_cannot_expand_the_accepted_set() {
    let server = server_b().await;
    let mut request = server.url().into_client_request().unwrap();
    request.headers_mut().insert("host", "a.example".parse().unwrap());
    let (mut ws, _) = within("websocket connect", connect_async(request)).await.unwrap();
    let challenge = recv(&mut ws).await.unwrap()["d"].clone();
    let nonce = challenge["nonce"].as_str().unwrap().to_owned();
    let mut raw = RawConn { ws, challenge, nonce, host: normalize_host(&server.url()).unwrap() };
    let hello = hello_for(&raw, &new_key(), "a.example");
    let err = raw.rejected(hello).await;
    assert_eq!(err.code, ErrorCode::NotAuthenticated);
    assert_eq!(err.message, "this signature is for another server");
    raw.expect_closed().await;
}

#[tokio::test]
async fn invalid_public_host_fails_startup() {
    let mut config = base_config();
    config.public_hosts = vec!["".into()];
    let err = vc_server::start(config).await.err().expect("invalid name must not disable binding");
    assert!(err.to_string().contains("invalid public host"));
}
