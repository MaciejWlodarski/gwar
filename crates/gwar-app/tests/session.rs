//! The FFI surface against a real in-process server, as the Android app drives it.

use std::{
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

use gwar_app::{
    AppError, CloseReason, ConnectRequest, GwarApp, SessionListener,
    state::{ChatLine, ServerView, Verification},
};

#[derive(Debug)]
#[allow(dead_code)] // fields show in failure output
enum Heard {
    View(ServerView),
    Chat(ChatLine),
    Deleted(u32, u32),
    Closed(CloseReason),
}

struct Listener(Mutex<mpsc::Sender<Heard>>);

impl SessionListener for Listener {
    fn on_view(&self, view: ServerView) {
        let _ = self.0.lock().unwrap().send(Heard::View(view));
    }
    fn on_chat(&self, line: ChatLine) {
        let _ = self.0.lock().unwrap().send(Heard::Chat(line));
    }
    fn on_chat_deleted(&self, channel: u32, message: u32) {
        let _ = self.0.lock().unwrap().send(Heard::Deleted(channel, message));
    }
    fn on_closed(&self, reason: CloseReason) {
        let _ = self.0.lock().unwrap().send(Heard::Closed(reason));
    }
}

fn listener() -> (Arc<dyn SessionListener>, mpsc::Receiver<Heard>) {
    let (tx, rx) = mpsc::channel();
    (Arc::new(Listener(Mutex::new(tx))), rx)
}

/// Waits for the first update `pick` accepts.
fn wait<T>(rx: &mpsc::Receiver<Heard>, mut pick: impl FnMut(Heard) -> Option<T>) -> T {
    loop {
        let heard = rx.recv_timeout(Duration::from_secs(10)).expect("update in time");
        if let Some(found) = pick(heard) {
            return found;
        }
    }
}

fn data_dir(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("gwar-app-{name}-{}", rand_suffix()));
    dir.to_string_lossy().into_owned()
}

fn rand_suffix() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
}

async fn server() -> vc_server::Running {
    let any = "127.0.0.1:0".parse().unwrap();
    vc_server::start(vc_server::Config {
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
        upload_limit: 0,
        upload_quota: 0,
        storage_limit: 0,
        files_dir: None,
        web_origins: Vec::new(),
        tls: None,
        redirect_http: None,
        connect_url: None,
    })
    .await
    .unwrap()
}

fn request(address: &str, nickname: &str) -> ConnectRequest {
    ConnectRequest { address: address.into(), nickname: nickname.into(), insecure_lan: false, server_password: None }
}

#[tokio::test(flavor = "multi_thread")]
async fn connects_joins_and_chats_like_the_app() {
    let server = server().await;
    let address = format!("ws://{}", server.http);
    let (ania_dir, bartek_dir) = (data_dir("ania"), data_dir("bartek"));
    let ania = GwarApp::new(ania_dir.clone()).unwrap();
    let bartek = GwarApp::new(bartek_dir.clone()).unwrap();

    let (l, ania_rx) = listener();
    let a = ania.connect(request(&address, "ania"), l).await.unwrap();
    let (l, bartek_rx) = listener();
    let b = bartek.connect(request(&address, "bartek"), l).await.unwrap();

    let view = a.view();
    assert_eq!(view.my_uid, ania.uid());
    assert_eq!(view.my_channel, None, "on the server, not in a channel");
    let lobby = view.channels.iter().find(|c| c.is_default).unwrap().id;

    // Bartek joins the lobby; Ania sees him there.
    b.join_channel(lobby, None).await.unwrap();
    wait(&ania_rx, |h| match h {
        Heard::View(v) => {
            v.channels.iter().any(|c| c.id == lobby && c.clients.iter().any(|cl| cl.nickname == "bartek")).then_some(())
        }
        _ => None,
    });

    // A message from Bartek reaches Ania verified, and counts as unread until she opens the chat.
    let sent = b.send_message(lobby, "cześć".into()).await.unwrap();
    assert!(sent.own);
    let line = wait(&ania_rx, |h| match h {
        Heard::Chat(line) => Some(line),
        _ => None,
    });
    assert_eq!((line.text.as_str(), line.author_name.as_str()), ("cześć", "bartek"));
    assert_eq!(line.verification, Verification::Verified);
    assert!(!line.own);
    assert_eq!(a.view().channels.iter().find(|c| c.id == lobby).unwrap().unread, 1);
    a.set_open_channel(Some(lobby));
    assert_eq!(a.view().channels.iter().find(|c| c.id == lobby).unwrap().unread, 0);

    let history = a.history(lobby, None).await.unwrap();
    assert_eq!(history.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["cześć"]);

    // Leaving keeps Bartek on the server.
    b.leave_channel().await.unwrap();
    wait(&ania_rx, |h| match h {
        Heard::View(v) => v.lobby.iter().any(|c| c.nickname == "bartek").then_some(()),
        _ => None,
    });

    b.disconnect();
    wait(&ania_rx, |h| match h {
        Heard::View(v) => (!v.lobby.iter().any(|c| c.nickname == "bartek")).then_some(()),
        _ => None,
    });
    assert!(b.view().my_channel.is_none());
    assert!(matches!(b.send_message(lobby, "x".into()).await, Err(AppError::NotConnected)));
    drop(bartek_rx);

    // The form and the pin survive a restart of the app.
    drop(a);
    let again = GwarApp::new(ania_dir.clone()).unwrap();
    let last = again.last_server().unwrap();
    assert_eq!((last.address.as_str(), last.nickname.as_str()), (address.as_str(), "ania"));
    assert_eq!(again.uid(), ania.uid());

    std::fs::remove_dir_all(ania_dir).unwrap();
    std::fs::remove_dir_all(bartek_dir).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_different_server_key_at_a_pinned_address_is_refused() {
    let first = server().await;
    let address = format!("ws://{}", first.http);
    let dir = data_dir("pin");
    let app = GwarApp::new(dir.clone()).unwrap();
    let (l, _rx) = listener();
    app.connect(request(&address, "ania"), l).await.unwrap().disconnect();

    // Another server (another key) at the same address.
    let endpoint = gwar_app::parse_server_address(address.clone(), false).unwrap().url;
    let pins = std::fs::read_to_string(std::path::Path::new(&dir).join("servers.json")).unwrap();
    let other = server().await;
    let other_endpoint = gwar_app::parse_server_address(format!("ws://{}", other.http), false).unwrap().url;
    std::fs::write(std::path::Path::new(&dir).join("servers.json"), pins.replace(&endpoint, &other_endpoint)).unwrap();
    let app = GwarApp::new(dir.clone()).unwrap();
    let (l, _rx) = listener();
    let refused = app.connect(request(&format!("ws://{}", other.http), "ania"), l).await;
    assert!(matches!(refused, Err(AppError::ServerIdentityChanged { endpoint }) if endpoint == other_endpoint));

    // Forgetting the pin lets the user accept the new key.
    app.forget_server(other_endpoint).unwrap();
    let (l, _rx) = listener();
    app.connect(request(&format!("ws://{}", other.http), "ania"), l).await.unwrap().disconnect();
    std::fs::remove_dir_all(dir).unwrap();
}
