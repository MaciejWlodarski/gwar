//! Native vc/4 key policy and proof-before-secret checks.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ed25519_dalek::{Signer, SigningKey};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use vc_client::{ClientError, ConnectOptions, Identity, connect};
use vc_proto::{ClientSoftware, Hello, Platform, server_proof_statement};

fn options(url: String) -> ConnectOptions {
    ConnectOptions::new(
        url,
        Identity::generate(),
        ClientSoftware { name: "test".into(), version: "0".into(), platform: Platform::Desktop },
    )
}

async fn fake_server(mode: &str) -> (String, String, tokio::task::JoinHandle<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/ws", listener.local_addr().unwrap());
    let key = SigningKey::from_bytes(&[3; 32]);
    let pk = if mode == "weak" {
        let mut bytes = [0; 32];
        bytes[0] = 1;
        B64.encode(bytes)
    } else {
        B64.encode(key.verifying_key().as_bytes())
    };
    let server_pk = pk.clone();
    let mode = mode.to_owned();
    let task = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            json!({"ev":"challenge","d":{
                "protocol":4,"server_pk":server_pk,"server_nonce":B64.encode([4;32]),"password_required":true,
                "server":{"name":"test","welcome":"","version":"0","default_channel":1,"max_clients":8,"upload_limit":0}
            }})
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
        let mut received = Vec::new();
        if let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) = ws.next().await {
            let value: Value = serde_json::from_str(&text).unwrap();
            let mut hello: Hello = serde_json::from_value(value["d"].clone()).unwrap();
            received.push(value);
            if mode == "tampered" {
                hello.endpoint = "wss://attacker.example/ws".into();
            }
            let proof = if mode == "missing" {
                json!({})
            } else {
                json!({"server_proof": B64.encode(key.sign(server_proof_statement(&hello).unwrap().as_bytes()).to_bytes())})
            };
            ws.send(tokio_tungstenite::tungstenite::Message::Text(json!({"re":1,"ok":proof}).to_string().into()))
                .await
                .unwrap();
            if let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) = ws.next().await {
                received.push(serde_json::from_str(&text).unwrap());
                ws.send(tokio_tungstenite::tungstenite::Message::Text(
                    json!({"re":2,"err":{"code":"wrong_password","message":"test refusal"}}).to_string().into(),
                ))
                .await
                .unwrap();
            }
        }
        received
    });
    (url, pk, task)
}

#[tokio::test]
async fn tofu_is_off_by_default_and_wrong_expected_key_sends_no_hello() {
    for wrong_pin in [false, true] {
        let (url, _, server) = fake_server("valid").await;
        let mut o = options(url);
        if wrong_pin {
            o.expected_server_key = Some(B64.encode([0; 32]));
        }
        assert!(matches!(connect(o).await, Err(ClientError::Protocol(_))));
        assert!(server.await.unwrap().is_empty());
    }
    let (url, _, server) = fake_server("weak").await;
    let mut o = options(url);
    o.trust_on_first_use = true;
    assert!(matches!(connect(o).await, Err(ClientError::Protocol(_))));
    assert!(server.await.unwrap().is_empty());
}

#[tokio::test]
async fn pinned_and_explicit_tofu_clients_verify_proof_before_sending_join_secrets() {
    for tofu in [false, true] {
        let (url, key, server) = fake_server("valid").await;
        let mut o = options(url);
        o.server_password = Some("secret".into());
        o.invite = Some("CODE".into());
        if tofu {
            o.trust_on_first_use = true;
        } else {
            o.expected_server_key = Some(key);
        }
        assert!(matches!(connect(o).await, Err(ClientError::Server(_))));
        let received = server.await.unwrap();
        assert_eq!(received.len(), 2);
        assert_eq!(received[0]["op"], "hello");
        assert!(received[0]["d"].get("server_password").is_none());
        assert!(received[0]["d"].get("invite").is_none());
        assert_eq!(received[1], json!({"id":2,"op":"join","d":{"server_password":"secret","invite":"CODE"}}));
    }
}

#[tokio::test]
async fn missing_or_changed_transcript_proof_never_sends_join_secrets() {
    for mode in ["missing", "tampered"] {
        let (url, key, server) = fake_server(mode).await;
        let mut o = options(url);
        o.expected_server_key = Some(key);
        o.server_password = Some("secret".into());
        assert!(matches!(connect(o).await, Err(ClientError::Protocol(_))));
        assert_eq!(server.await.unwrap().len(), 1);
    }
}
