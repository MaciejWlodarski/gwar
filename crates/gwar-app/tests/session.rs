//! The FFI surface against a real in-process server, as the Android app drives it.

use std::{
    sync::{Arc, Mutex, atomic::AtomicBool, mpsc},
    thread,
    time::{Duration, Instant},
};

use gwar_app::{
    AppError, CloseReason, ConnectRequest, GwarApp, SessionListener,
    state::{ChatLine, ServerView, Verification},
    voice::VoiceLink,
};
use rtrb::RingBuffer;
use vc_client::voice::io::{AudioIo, AudioPorts, CaptureSample, OutputTiming};

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
    let ania = GwarApp::new(ania_dir.clone(), None).unwrap();
    let bartek = GwarApp::new(bartek_dir.clone(), None).unwrap();

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
    let again = GwarApp::new(ania_dir.clone(), None).unwrap();
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
    let app = GwarApp::new(dir.clone(), None).unwrap();
    let (l, _rx) = listener();
    app.connect(request(&address, "ania"), l).await.unwrap().disconnect();

    // Another server (another key) at the same address.
    let endpoint = gwar_app::parse_server_address(address.clone(), false).unwrap().url;
    let pins = std::fs::read_to_string(std::path::Path::new(&dir).join("servers.json")).unwrap();
    let other = server().await;
    let other_endpoint = gwar_app::parse_server_address(format!("ws://{}", other.http), false).unwrap().url;
    std::fs::write(std::path::Path::new(&dir).join("servers.json"), pins.replace(&endpoint, &other_endpoint)).unwrap();
    let app = GwarApp::new(dir.clone(), None).unwrap();
    let (l, _rx) = listener();
    let refused = app.connect(request(&format!("ws://{}", other.http), "ania"), l).await;
    assert!(matches!(refused, Err(AppError::ServerIdentityChanged { endpoint }) if endpoint == other_endpoint));

    // Forgetting the pin lets the user accept the new key.
    app.forget_server(other_endpoint).unwrap();
    let (l, _rx) = listener();
    app.connect(request(&format!("ws://{}", other.http), "ania"), l).await.unwrap().disconnect();
    std::fs::remove_dir_all(dir).unwrap();
}

/// A 10 ms device: a 440 Hz microphone (or silence) and a recorded speaker.
struct FakeIo {
    heard: Arc<Mutex<Vec<f32>>>,
    tone: bool,
}

impl AudioIo for FakeIo {
    fn open(self: Box<Self>) -> anyhow::Result<AudioPorts> {
        let (mut mic_tx, mic) = RingBuffer::new(48_000);
        let (speaker, mut speaker_rx) = RingBuffer::new(24_000);
        let output = Arc::new(OutputTiming::default());
        let timing = output.clone();
        let (heard, tone) = (self.heard, self.tone);
        thread::spawn(move || {
            let mut t = 0u64;
            loop {
                for _ in 0..480 {
                    let s = if tone { (t as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.3 } else { 0.0 };
                    if mic_tx.push(CaptureSample { captured_at: Instant::now(), position: t, value: s }).is_err() {
                        return;
                    }
                    t += 1;
                }
                let missing = 480usize.saturating_sub(speaker_rx.slots());
                let mut out = heard.lock().unwrap();
                for _ in 0..480 {
                    out.push(speaker_rx.pop().unwrap_or(0.0));
                }
                drop(out);
                timing.report(480, missing);
                thread::sleep(Duration::from_millis(10));
            }
        });
        Ok(AudioPorts { mic, speaker, output, _guard: Box::new(()) })
    }
}

fn rms_of_last_second(heard: &Mutex<Vec<f32>>) -> f32 {
    let samples = heard.lock().unwrap();
    let tail = &samples[samples.len().saturating_sub(48_000)..];
    (tail.iter().map(|s| s * s).sum::<f32>() / tail.len().max(1) as f32).sqrt()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn voice_reaches_the_other_app_and_mute_stops_it() {
    let server = server().await;
    let address = format!("ws://{}", server.http);
    let (a_dir, b_dir) = (data_dir("voice-a"), data_dir("voice-b"));
    let (l, _a_rx) = listener();
    let a = GwarApp::new(a_dir.clone(), None).unwrap().connect(request(&address, "ania"), l).await.unwrap();
    let (l, b_rx) = listener();
    let b = GwarApp::new(b_dir.clone(), None).unwrap().connect(request(&address, "bartek"), l).await.unwrap();

    let silent = Arc::new(AtomicBool::new(false));
    assert!(matches!(
        a.start_voice_with(Box::new(FakeIo { heard: Default::default(), tone: true }), silent.clone()).await,
        Err(AppError::NotInChannel)
    ));
    let lobby = a.view().channels.iter().find(|c| c.is_default).unwrap().id;
    a.join_channel(lobby, None).await.unwrap();
    b.join_channel(lobby, None).await.unwrap();
    let heard = Arc::new(Mutex::new(Vec::new()));
    a.start_voice_with(Box::new(FakeIo { heard: Default::default(), tone: true }), silent.clone()).await.unwrap();
    b.start_voice_with(Box::new(FakeIo { heard: heard.clone(), tone: false }), silent.clone()).await.unwrap();

    tokio::time::sleep(Duration::from_secs(3)).await;
    let status = b.voice_status().unwrap();
    assert_eq!(status.link, VoiceLink::Connected);
    assert!(status.frames_received > 0, "{status:?}");
    assert!(a.voice_status().unwrap().transmitting);
    let level = rms_of_last_second(&heard);
    assert!(level > 0.05, "B heard rms {level} over the last second");

    // Muting stops the tone, and the server tells the others.
    a.set_muted(true).await.unwrap();
    wait(&b_rx, |h| match h {
        Heard::View(v) => {
            v.channels.iter().flat_map(|c| &c.clients).any(|c| c.nickname == "ania" && c.muted).then_some(())
        }
        _ => None,
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let level = rms_of_last_second(&heard);
    assert!(level < 0.01, "B still heard rms {level} after A muted");

    // A threshold above the tone (-10 dBFS > ~-13 dBFS) keeps it out; back to the default lets it in.
    a.set_muted(false).await.unwrap();
    a.set_voice_activity_threshold(-10.0);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let level = rms_of_last_second(&heard);
    assert!(level < 0.01, "B heard rms {level} under the voice activity threshold");
    a.set_voice_activity_threshold(f32::NAN);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let level = rms_of_last_second(&heard);
    assert!(level > 0.05, "B heard rms {level} at the default threshold");

    // Push-to-talk: silent until the button is held, then the tone comes back.
    a.set_muted(false).await.unwrap();
    a.set_push_to_talk(true);
    // The tone was playing just before: let its tail (gate release, playout) leave the window.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let level = rms_of_last_second(&heard);
    assert!(level < 0.01, "B heard rms {level} with push-to-talk released");
    a.set_talk_held(true);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let level = rms_of_last_second(&heard);
    assert!(level > 0.05, "B heard rms {level} with push-to-talk held");
    a.set_push_to_talk(false);

    // Unmuting while deafened also undeafens.
    a.set_deafened(true).await.unwrap();
    a.set_muted(false).await.unwrap();
    assert!(!a.deafened());

    // Leaving the channel stops voice.
    a.leave_channel().await.unwrap();
    assert!(a.voice_status().is_none());
    std::fs::remove_dir_all(a_dir).unwrap();
    std::fs::remove_dir_all(b_dir).unwrap();
}

/// The server drops a connection that sends nothing for 60 s; the session pings.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "takes 75 s"]
async fn an_idle_session_stays_connected() {
    let server = server().await;
    let dir = data_dir("idle");
    let (l, rx) = listener();
    let session = GwarApp::new(dir.clone(), None)
        .unwrap()
        .connect(request(&format!("ws://{}", server.http), "ania"), l)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(75)).await;
    assert!(!rx.try_iter().any(|h| matches!(h, Heard::Closed(_))), "the server closed an idle session");
    session.history(session.view().channels[0].id, None).await.unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}
