use vc_client::{ConnectOptions, Identity, connect};
use vc_proto::{ChatTarget, ClientSoftware, ErrorCode, Event, Platform};

fn options(url: &str, nickname: &str) -> ConnectOptions {
    ConnectOptions {
        url: url.into(),
        nickname: nickname.into(),
        server_password: None,
        identity: Identity::generate(),
        software: ClientSoftware { name: "vc-client-test".into(), version: "0".into(), platform: Platform::Desktop },
    }
}

#[tokio::test]
async fn connects_chats_and_reports_errors() {
    let any = "127.0.0.1:0".parse().unwrap();
    let server = vc_server::start(vc_server::Config {
        database: None,
        http_bind: any,
        media_bind: any,
        media_advertise: any,
        max_clients: 8,
        server_password: None,
        web_root: None,
        ice_servers: vec![],
        teamspeak: None,
        public_url: None,
        upload_limit: 10 * 1024 * 1024,
        files_dir: None,
        web_origins: Vec::new(),
        tls: None,
        redirect_http: None,
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

    let err = a.connection.join(9999, None).await.unwrap_err();
    assert!(matches!(err, vc_client::ClientError::Server(ref e) if e.code == ErrorCode::NotFound), "{err}");
}
