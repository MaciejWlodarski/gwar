use gwar_client::{ConnectOptions, Identity, connect};
use gwar_proto::{ChatTarget, ClientSoftware, ErrorCode, Event, Platform};

fn options(url: &str, nickname: &str) -> ConnectOptions {
    ConnectOptions {
        url: url.into(),
        nickname: nickname.into(),
        invite: None,
        certificate: None,
        expected_server_key: None,
        trust_on_first_use: true,
        insecure_lan: false,
        server_password: None,
        identity: Identity::generate(),
        software: ClientSoftware { name: "gwar-client-test".into(), version: "0".into(), platform: Platform::Desktop },
    }
}

#[tokio::test]
async fn connects_chats_and_reports_errors() {
    let any = "127.0.0.1:0".parse().unwrap();
    let server = gwar_server::start(gwar_server::Config {
        database: None,
        http_bind: any,
        media_bind: any,
        media_advertise: any,
        max_clients: 8,
        encrypted_server_chat: false,
        server_password: None,
        web_root: None,
        ice_servers: vec![],
        teamspeak: None,
        public_url: None,
        data_dir: None,
        public_ip: None,
        insecure_any_host: true,
        public_endpoints: Vec::new(),
        client_ip_header: None,
        upload_limit: 10 * 1024 * 1024,
        upload_quota: 0,
        storage_limit: 0,
        files_dir: None,
        web_origins: Vec::new(),
        tls: None,
        redirect_http: None,
        connect_url: None,
    })
    .await
    .unwrap();
    let url = format!("ws://{}/ws", server.http);

    let a = connect(options(&url, "ania")).await.unwrap();
    let mut b = connect(options(&url, "bartek")).await.unwrap();
    let lobby = a.welcome.server.default_channel;
    assert_eq!(a.welcome.channels.len(), 3);

    let sent = a.connection.chat(ChatTarget::Channel(lobby), "héllo wörld").await.unwrap();
    assert_eq!(sent.text, "héllo wörld");
    loop {
        match b.events.recv().await.unwrap() {
            Event::ChatMessage(m) => {
                assert_eq!(m.text, "héllo wörld");
                break;
            }
            _ => continue,
        }
    }

    let err = a.connection.chat(ChatTarget::Client(b.welcome.session), "plaintext DM").await.unwrap_err();
    assert!(
        matches!(err, gwar_client::ClientError::Server(ref e) if e.code == ErrorCode::Forbidden && e.message.contains("chat.sealed.send")),
        "{err}"
    );
    a.connection.chat(ChatTarget::Channel(lobby), "still connected").await.unwrap();
    let ts = server
        .core
        .remote_join(gwar_server::core::bridge::RemoteClient {
            uid: "ts:peer".into(),
            nickname: "TS".into(),
            channel: lobby,
            platform: Platform::Ts3,
            muted: false,
            deafened: false,
            away: None,
        })
        .await
        .unwrap();
    // The actor's joined event must reach the native client's recipient map first.
    while !matches!(b.events.recv().await.unwrap(), Event::ClientJoined(ref c) if c.id == ts) {}
    let message = b.connection.chat(ChatTarget::Client(ts), "to TS").await.unwrap();
    assert!(matches!(b.connection.verify_message(&message), gwar_proto::MessageCheck::Verified { .. }));
    server
        .core
        .bridge(gwar_server::core::bridge::BridgeMsg::Chat(ts, ChatTarget::Client(b.welcome.session), "from TS".into()))
        .await;
    loop {
        if let Event::ChatMessage(m) = b.events.recv().await.unwrap()
            && m.text == "from TS"
        {
            assert_eq!(b.connection.verify_message(&m), gwar_proto::MessageCheck::UnsignedTeamSpeak);
            break;
        }
    }
    let err = a.connection.join(9999, None).await.unwrap_err();
    assert!(matches!(err, gwar_client::ClientError::Server(ref e) if e.code == ErrorCode::NotFound), "{err}");
}
