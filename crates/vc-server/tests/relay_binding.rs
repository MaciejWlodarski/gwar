//! vc/3 relay binding, transcript proof and admission ordering.

mod common;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use common::*;
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use serde_json::{Value, json};
use tokio_tungstenite::{connect_async, tungstenite::client::IntoClientRequest};
use vc_proto::{ErrorCode, Hello, decode_fixed, normalize_endpoint, server_proof_statement};

const A: &str = "wss://a.example/ws";
const B: &str = "wss://b.example/ws";

async fn server_b() -> TestServer {
    TestServer::start_with(|c| {
        c.public_endpoints = vec![B.into()];
        c.insecure_any_host = false;
    })
    .await
}

fn hello_for(raw: &RawConn, key: &SigningKey, endpoint: &str) -> Value {
    let mut h = raw.hello(key, "victim", None);
    h["endpoint"] = json!(endpoint);
    h["signature"] = json!(sign(key, &h));
    h
}

#[tokio::test]
async fn accepted_endpoint_works_for_local_and_connect_identities() {
    let server = server_b().await;
    let key = new_key();
    let local = Client::connect_with(&server, key.clone(), |raw, key| hello_for(raw, key, B)).await.unwrap();
    assert_eq!(local.welcome.uid, uid_of(&key));
    let (account, device) = (new_key(), new_key());
    let now = vc_server::core::now_ms();
    let client = Client::connect_with(&server, device, |raw, key| {
        let mut h = hello_for(raw, key, B);
        set_certificate(&mut h, certificate(&account, key, now, now + 86400000), key);
        h
    })
    .await
    .unwrap();
    assert_eq!(client.welcome.uid, uid_of(&account));
}

#[tokio::test]
async fn relayed_signatures_for_another_endpoint_are_rejected_local_and_connect() {
    let server = server_b().await;
    for connect in [false, true] {
        let mut raw = open(&server).await;
        let key = new_key();
        let mut h = hello_for(&raw, &key, A);
        if connect {
            let now = vc_server::core::now_ms();
            set_certificate(&mut h, certificate(&new_key(), &key, now, now + 86400000), &key);
        }
        assert_eq!(raw.rejected(h).await.code, ErrorCode::NotAuthenticated);
        raw.expect_closed().await;
    }
}

#[tokio::test]
async fn forged_endpoint_field_is_rejected_local_and_connect() {
    let server = server_b().await;
    for connect in [false, true] {
        let mut raw = open(&server).await;
        let key = new_key();
        let mut h = hello_for(&raw, &key, A);
        if connect {
            let now = vc_server::core::now_ms();
            set_certificate(&mut h, certificate(&new_key(), &key, now, now + 86400000), &key);
        }
        h["endpoint"] = json!(B);
        assert_eq!(raw.rejected(h).await.code, ErrorCode::NotAuthenticated);
        raw.expect_closed().await;
    }
}

#[tokio::test]
async fn wrong_expected_server_key_and_replayed_connection_nonce_are_rejected() {
    let server = server_b().await;
    let other = open(&server).await;
    for field in ["expected_server_pk", "server_nonce"] {
        let mut raw = open(&server).await;
        let key = new_key();
        let mut h = hello_for(&raw, &key, B);
        h[field] = json!(if field == "server_nonce" { other.nonce.clone() } else { public_key(&new_key()) });
        h["signature"] = json!(sign(&key, &h));
        assert_eq!(raw.rejected(h).await.code, ErrorCode::NotAuthenticated);
        raw.expect_closed().await;
    }
}

#[tokio::test]
async fn proof_verifies_and_binds_every_transcript_field_and_client_signature() {
    let server = server_b().await;
    let mut raw = open(&server).await;
    let key = new_key();
    let mut h = hello_for(&raw, &key, B);
    let now = vc_server::core::now_ms();
    set_certificate(&mut h, certificate(&new_key(), &key, now, now + 86400000), &key);
    h.as_object_mut().unwrap().remove("join");
    raw.send_hello(h.clone()).await;
    let proof = raw.recv().await.unwrap();
    let pk = decode_fixed::<32>(h["expected_server_pk"].as_str().unwrap()).unwrap();
    let verifier = VerifyingKey::from_bytes(&pk).unwrap();
    let sig = Signature::from_bytes(&decode_fixed::<64>(proof["ok"]["server_proof"].as_str().unwrap()).unwrap());
    let verifies = |h: &Value| {
        let h: Hello = serde_json::from_value(h.clone()).unwrap();
        server_proof_statement(&h).is_some_and(|s| verifier.verify_strict(s.as_bytes(), &sig).is_ok())
    };
    assert!(verifies(&h));
    for field in [
        "endpoint",
        "expected_server_pk",
        "server_nonce",
        "client_nonce",
        "client_signing_pk",
        "certificate_hash",
        "signature",
    ] {
        let mut changed = h.clone();
        changed[field] = json!(match field {
            "endpoint" => A.to_owned(),
            "signature" => sign(&new_key(), &h),
            _ => public_key(&new_key()),
        });
        assert!(!verifies(&changed), "{field}");
    }
    let mut changed = h.clone();
    changed["certificate"]["expires_at"] = json!(now + 86400001);
    assert!(!verifies(&changed));
    raw.send(json!({"id":2,"op":"join","d":{}})).await;
    assert!(raw.recv().await.unwrap()["ok"]["session"].is_number());
}

#[tokio::test]
async fn join_and_application_requests_before_hello_are_rejected() {
    let server = server_b().await;
    for op in ["join", "ping", "chat.send"] {
        let mut raw = open(&server).await;
        raw.send(json!({"id":5,"op":op,"d":{}})).await;
        let reply = raw.recv().await.unwrap();
        assert!(reply.get("err").is_some(), "{reply}");
        raw.expect_closed().await;
    }
}

#[tokio::test]
async fn application_requests_after_proof_before_join_are_rejected() {
    let server = server_b().await;
    let mut raw = open(&server).await;
    raw.send_hello(hello_for(&raw, &new_key(), B)).await;
    assert!(raw.recv().await.unwrap()["ok"]["server_proof"].is_string());
    raw.send(json!({"id":2,"op":"ping","d":{}})).await;
    assert_eq!(raw.recv().await.unwrap()["err"]["code"], "not_authenticated");
    raw.expect_closed().await;
}

#[tokio::test]
async fn hello_never_accepts_password_or_invite_fields() {
    let server = server_b().await;
    for field in ["server_password", "invite"] {
        let mut raw = open(&server).await;
        let mut h = hello_for(&raw, &new_key(), B);
        h[field] = json!("secret");
        raw.send_hello(h).await;
        let reply = raw.recv().await.unwrap();
        assert_eq!(reply["err"]["code"], "bad_request");
        assert!(reply.get("ok").is_none());
        raw.expect_closed().await;
    }
}

#[tokio::test]
async fn strict_fields_reject_noncanonical_lengths_crlf_and_certificate_substitution() {
    let server = server_b().await;
    for (field, value) in [
        ("client_nonce", "AA"),
        ("client_nonce", "bad\nnonce"),
        ("certificate_hash", "AA"),
        ("signature", "AAAA"),
        ("endpoint", "wss://B.EXAMPLE/ws"),
    ] {
        let mut raw = open(&server).await;
        let mut h = hello_for(&raw, &new_key(), B);
        h[field] = json!(value);
        assert_eq!(raw.rejected(h).await.code, ErrorCode::NotAuthenticated, "{field}");
    }
    let mut raw = open(&server).await;
    let mut h = hello_for(&raw, &new_key(), B);
    h["client_nonce"] = json!(format!("{}=", h["client_nonce"].as_str().unwrap()));
    assert_eq!(raw.rejected(h).await.code, ErrorCode::NotAuthenticated);

    let key = new_key();
    let account = new_key();
    let now = vc_server::core::now_ms();
    let mut raw = open(&server).await;
    let mut h = hello_for(&raw, &key, B);
    set_certificate(&mut h, certificate(&account, &key, now, now + 86400000), &key);
    // Preserve the signed H while substituting the certificate it commits to.
    h["certificate"]["signature"] = json!(sign(&new_key(), &h));
    assert_eq!(raw.rejected(h).await.code, ErrorCode::NotAuthenticated);
    for (issued, expires) in [(-1, now + 86400000), (now, 9_007_199_254_740_992), (now, now + 91 * 86400000)] {
        let mut raw = open(&server).await;
        let mut h = hello_for(&raw, &key, B);
        set_certificate(&mut h, certificate(&account, &key, issued, expires), &key);
        assert_eq!(raw.rejected(h).await.code, ErrorCode::NotAuthenticated);
    }
}

#[tokio::test]
async fn empty_configuration_refuses_start_without_explicit_insecure_mode() {
    let mut config = base_config();
    config.insecure_any_host = false;
    assert!(vc_server::start(config).await.err().unwrap().to_string().contains("no accepted public endpoints"));
    let server = TestServer::start().await;
    for endpoint in [A, B, "ws://[::1]:8790/ws"] {
        Client::connect_with(&server, new_key(), |raw, key| hello_for(raw, key, endpoint)).await.unwrap();
    }
}

#[test]
fn configured_global_public_ip_includes_self_terminated_tls_port_private_ips_do_not() {
    let mut config = base_config();
    for ip in
        ["127.0.0.1", "10.0.0.1", "192.168.1.2", "169.254.1.1", "::1", "fe80::1", "fc00::1", "224.1.2.3", "2001:db8::1"]
    {
        config.public_ip = Some(ip.parse().unwrap());
        assert!(vc_server::accepted_endpoints(&config).unwrap().is_empty(), "{ip}");
    }
    config.http_bind = "127.0.0.1:8443".parse().unwrap();
    config.tls = Some(vc_server::tls::Tls::Files { cert: "cert.pem".into(), key: "key.pem".into() });
    for (ip, endpoint) in [("8.8.8.8", "wss://8.8.8.8:8443/ws"), ("2606:4700::1111", "wss://[2606:4700::1111]:8443/ws")]
    {
        config.public_ip = Some(ip.parse().unwrap());
        assert_eq!(vc_server::accepted_endpoints(&config).unwrap(), [endpoint.into()].into());
    }
    config.http_bind = "127.0.0.1:443".parse().unwrap();
    assert_eq!(vc_server::accepted_endpoints(&config).unwrap(), ["wss://[2606:4700::1111]/ws".into()].into());
    config.tls = None;
    assert!(vc_server::accepted_endpoints(&config).unwrap().is_empty());
}

#[test]
fn domains_public_url_and_explicit_endpoints_form_an_exclusive_set() {
    let mut c = base_config();
    c.public_url = Some("https://B.Example.:443/base".into());
    c.public_endpoints = vec!["wss://Alias.Example:8443/voice/ws".into()];
    c.tls = Some(vc_server::tls::Tls::Acme {
        domains: vec!["One.Example.".into()],
        email: None,
        cache: Default::default(),
        staging: true,
    });
    assert_eq!(
        vc_server::accepted_endpoints(&c).unwrap(),
        [B.into(), "wss://alias.example:8443/voice/ws".into(), "wss://one.example/ws".into()].into()
    );
}

#[tokio::test]
async fn request_headers_cannot_add_endpoints() {
    let server = server_b().await;
    let mut request = server.url().into_client_request().unwrap();
    for (name, value) in [
        ("host", "a.example"),
        ("x-forwarded-host", "a.example"),
        ("x-forwarded-proto", "wss"),
        ("forwarded", "host=a.example;proto=wss"),
    ] {
        request.headers_mut().insert(name, value.parse().unwrap());
    }
    let (mut ws, _) = within("websocket connect", connect_async(request)).await.unwrap();
    let challenge = recv(&mut ws).await.unwrap()["d"].clone();
    let nonce = challenge["server_nonce"].as_str().unwrap().to_owned();
    let mut raw = RawConn { ws, challenge, nonce, endpoint: normalize_endpoint(&server.url()).unwrap() };
    let h = hello_for(&raw, &new_key(), A);
    assert_eq!(raw.rejected(h).await.code, ErrorCode::NotAuthenticated);
    raw.expect_closed().await;
}

#[tokio::test]
async fn invalid_public_endpoint_fails_startup() {
    let mut config = base_config();
    config.public_endpoints = vec!["".into()];
    assert!(vc_server::start(config).await.err().unwrap().to_string().contains("invalid public endpoint"));
}

#[test]
fn nginx_public_url_does_not_infer_a_public_ip_login_endpoint() {
    let mut config = base_config();
    config.http_bind = "127.0.0.1:8800".parse().unwrap();
    config.public_ip = Some("8.8.8.8".parse().unwrap());
    config.public_url = Some("https://voice.gwar.app".into());
    assert_eq!(vc_server::accepted_endpoints(&config).unwrap(), ["wss://voice.gwar.app/ws".into()].into());
}

#[tokio::test]
async fn oversized_join_is_closed_before_parsing_or_password_work() {
    let server = server_b().await;
    let mut raw = open(&server).await;
    let h = hello_for(&raw, &new_key(), B);
    raw.send_hello(h).await;
    assert!(raw.recv().await.unwrap()["ok"]["server_proof"].is_string());
    raw.send(json!({"id":2,"op":"join","d":{"server_password":"x".repeat(96 * 1024)}})).await;
    raw.expect_closed().await;
    assert!(server.running.core.info().await.is_some());
}

#[tokio::test]
async fn revocation_is_account_scoped_before_proof_and_rechecked_at_join() {
    let server = server_b().await;
    let account_a = new_key();
    let account_b = new_key();
    let device = new_key();
    let now = vc_server::core::now_ms();
    let revoke = |account: &SigningKey, seq| vc_server::core::RevokedDevice {
        seq,
        account_key: public_key(account),
        device_key: public_key(&device),
        revoked_at: now,
        signature: URL_SAFE_NO_PAD.encode(
            ed25519_dalek::Signer::sign(
                account,
                vc_server::identity::revoke_statement(&public_key(account), &public_key(&device), now).as_bytes(),
            )
            .to_bytes(),
        ),
    };
    server.running.core.revoked(vec![revoke(&account_a, 1)], 1).await;
    server.running.core.info().await.unwrap();
    let mut denied = open(&server).await;
    let mut h = hello_for(&denied, &device, B);
    set_certificate(&mut h, certificate(&account_a, &device, now, now + 86400000), &device);
    assert_eq!(denied.rejected(h).await.code, ErrorCode::NotAuthenticated);
    denied.expect_closed().await;
    let mut raw = open(&server).await;
    let mut h = hello_for(&raw, &device, B);
    set_certificate(&mut h, certificate(&account_b, &device, now, now + 86400000), &device);
    raw.send_hello(h).await;
    assert!(raw.recv().await.unwrap()["ok"]["server_proof"].is_string());
    server.running.core.revoked(vec![revoke(&account_b, 2)], 2).await;
    server.running.core.info().await.unwrap();
    raw.send(json!({"id":2,"op":"join","d":{}})).await;
    assert_eq!(raw.recv().await.unwrap()["err"]["code"], "not_authenticated");
    raw.expect_closed().await;
}

#[tokio::test]
async fn swapping_a_valid_certificate_under_another_account_breaks_the_hello_commitment() {
    let server = server_b().await;
    let device = new_key();
    let now = vc_server::core::now_ms();
    let mut raw = open(&server).await;
    let mut h = hello_for(&raw, &device, B);
    set_certificate(&mut h, certificate(&new_key(), &device, now, now + 86400000), &device);
    // Both certificates are current and properly signed for this device key.
    // Only the original certificate is committed to by the signed H.
    h["certificate"] = certificate(&new_key(), &device, now, now + 86400000);
    assert_eq!(raw.rejected(h).await.code, ErrorCode::NotAuthenticated);
    raw.expect_closed().await;
}
