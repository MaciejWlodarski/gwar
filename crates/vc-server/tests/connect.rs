//! Gwar Connect accounts on a Gwar server: one identity for every device, and
//! revoked devices are signed out.

mod common;

use std::time::Duration;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use common::*;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Value, json};

fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

fn now() -> i64 {
    vc_server::core::now_ms()
}

/// A device certificate signed by `account` for `device`.
fn certificate(account: &SigningKey, device: &SigningKey, issued_at: i64, expires_at: i64) -> Value {
    let (account_key, device_key) = (b64(account.verifying_key().as_bytes()), b64(device.verifying_key().as_bytes()));
    let statement = format!("gwar device v1\n{account_key}\n{device_key}\n{issued_at}\n{expires_at}");
    json!({"account_key": account_key, "device_key": device_key, "issued_at": issued_at, "expires_at": expires_at,
           "signature": b64(&account.sign(statement.as_bytes()).to_bytes())})
}

/// Connects as `device`, presenting `cert`.
async fn as_device(server: &TestServer, device: &SigningKey, cert: Value) -> Result<Client, vc_proto::ErrorBody> {
    Client::connect_with(server, device.clone(), move |raw, key| {
        let mut hello = raw.hello(key, "acct", None);
        hello["device"] = cert;
        hello
    })
    .await
}

#[tokio::test]
async fn devices_of_one_account_are_one_person_until_revoked() {
    // Gwar Connect with one account and its first device.
    let connect = gwar_connect::start(gwar_connect::Config {
        database: None,
        bind: "127.0.0.1:0".parse().unwrap(),
        origins: vec![],
    })
    .await
    .unwrap();
    let url = format!("http://{connect}");
    let account = SigningKey::from_bytes(&rand::random());
    let laptop = SigningKey::from_bytes(&rand::random());
    let phone = SigningKey::from_bytes(&rand::random());
    let year = 365 * 24 * 3600 * 1000;
    let mut first = certificate(&account, &laptop, now(), now() + year);
    first["name"] = json!("laptop");
    let blob = b64(&[0u8; 60]);
    let register = json!({
        "handle": "ann", "account_key": first["account_key"],
        "kdf": {"salt": b64(&[1u8; 16]), "m": 19456, "t": 2, "p": 1},
        "auth_key": b64(&[2u8; 32]), "key_blob": blob, "recovery_auth": b64(&[3u8; 32]), "recovery_blob": blob,
        "device": first,
    });
    let http = reqwest::Client::new();
    let reply: Value =
        http.post(format!("{url}/v1/register")).json(&register).send().await.unwrap().json().await.unwrap();
    let token = reply["token"].as_str().unwrap().to_owned();

    let server = TestServer::start().await;
    tokio::spawn(vc_server::connect::follow_every(
        url.clone(),
        server.running.core.clone(),
        0,
        Duration::from_millis(200),
    ));

    // Both devices are the same member: the account's uid.
    let laptop_client = as_device(&server, &laptop, certificate(&account, &laptop, now(), now() + year)).await.unwrap();
    let mut phone_client =
        as_device(&server, &phone, certificate(&account, &phone, now(), now() + year)).await.unwrap();
    let account_uid = vc_server::identity::uid_for_key(account.verifying_key().as_bytes());
    assert_eq!(laptop_client.welcome.uid, account_uid);
    assert_eq!(phone_client.welcome.uid, account_uid);

    // Forged, expired or mismatched certificates are refused.
    let thief = SigningKey::from_bytes(&rand::random());
    assert!(as_device(&server, &thief, certificate(&thief, &phone, now(), now() + year)).await.is_err());
    assert!(as_device(&server, &phone, certificate(&account, &phone, now() - year - 2, now() - 1)).await.is_err());
    assert!(as_device(&server, &thief, certificate(&account, &phone, now(), now() + year)).await.is_err());

    // The phone signed in on Connect too.
    let mut phone_cert = certificate(&account, &phone, now(), now() + year);
    phone_cert["name"] = json!("phone");
    let added = http.post(format!("{url}/v1/devices")).bearer_auth(&token).json(&phone_cert).send().await.unwrap();
    assert_eq!(added.status(), 200);

    // Revoking the phone on Connect signs it out here, and keeps it out.
    let (account_key, phone_key) = (b64(account.verifying_key().as_bytes()), b64(phone.verifying_key().as_bytes()));
    let revoked_at = now();
    let statement = format!("gwar revoke v1\n{account_key}\n{phone_key}\n{revoked_at}");
    let revoke = json!({"device_key": phone_key, "revoked_at": revoked_at,
                        "signature": b64(&account.sign(statement.as_bytes()).to_bytes())});
    let status = http.post(format!("{url}/v1/devices/revoke")).bearer_auth(&token).json(&revoke).send().await.unwrap();
    assert_eq!(status.status(), 200);
    let gone = phone_client.expect_disconnect().await;
    assert_eq!(gone["kind"], "kicked");
    let again = as_device(&server, &phone, certificate(&account, &phone, now(), now() + year)).await;
    assert_eq!(again.err().expect("revoked").code, vc_proto::ErrorCode::NotAuthenticated);
    // The laptop is unaffected.
    as_device(&server, &laptop, certificate(&account, &laptop, now(), now() + year)).await.expect("laptop still in");
}
