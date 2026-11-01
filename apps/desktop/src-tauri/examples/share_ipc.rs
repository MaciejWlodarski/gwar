//! Real Tauri IPC under share sound's load, with nothing else of Gwar around it: a small window
//! whose page does what `voice/share-sound.ts` does, against the real `SharePlayout` and
//! `CaptureQueue`. It connects to nothing and keeps its data under its own identifier. Run by hand:
//! `cargo run -p gwar-desktop --example share_ipc -- 30` prints one JSON report. Add
//! `SHARE_IPC_AWAKE=1` for a run longer than a locked or idle Mac leaves this page awake.

use std::{
    borrow::Cow,
    sync::Arc,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use gwar_client::voice::share::{CaptureQueue, SharePlayout, parse_frames};
use tauri::{
    AppHandle, Assets, State, WebviewUrl, WebviewWindowBuilder,
    ipc::{InvokeBody, Request, Response},
    utils::assets::{AssetKey, CspHash},
};

const SLOTS: usize = 4;
const PAGE: &str = include_str!("share_ipc.html");

struct Page;

impl<R: tauri::Runtime> Assets<R> for Page {
    fn get(&self, _key: &AssetKey) -> Option<Cow<'_, [u8]>> {
        Some(Cow::Borrowed(PAGE.as_bytes()))
    }
    fn iter(&self) -> Box<dyn Iterator<Item = (Cow<'_, str>, Cow<'_, [u8]>)> + '_> {
        Box::new(std::iter::empty())
    }
    fn csp_hashes(&self, _key: &AssetKey) -> Box<dyn Iterator<Item = CspHash<'_>> + '_> {
        Box::new(std::iter::empty())
    }
}

struct Probe {
    playout: Arc<SharePlayout>,
    queue: Arc<CaptureQueue>,
    capturing: Arc<AtomicBool>,
    seconds: u64,
}

/// What the page needs to know. Its packets are noise of 64 kb/s Opus's size: a 20 ms stereo
/// header decodes any payload, and nobody listens.
#[tauri::command]
fn probe_setup(probe: State<'_, Probe>) -> serde_json::Value {
    probe.capturing.store(true, Ordering::Relaxed);
    serde_json::json!({ "seconds": probe.seconds, "slots": SLOTS })
}

#[tauri::command]
async fn probe_frames(probe: State<'_, Probe>, request: Request<'_>) -> Result<(), String> {
    let InvokeBody::Raw(body) = request.body() else { return Err("expected raw frames".into()) };
    let (_, frames) = parse_frames(body).ok_or("malformed share frames")?;
    for frame in frames {
        probe.playout.push(frame);
    }
    Ok(())
}

#[tauri::command]
async fn probe_read(probe: State<'_, Probe>) -> Result<Response, String> {
    let samples = probe.queue.read(Duration::from_millis(100)).await;
    Ok(Response::new(samples.iter().flat_map(|s| s.to_le_bytes()).collect::<Vec<u8>>()))
}

#[tauri::command]
fn probe_done(app: AppHandle, probe: State<'_, Probe>, page: serde_json::Value) {
    probe.capturing.store(false, Ordering::Relaxed);
    thread::sleep(Duration::from_millis(300));
    let stats = probe.playout.stats();
    let report = serde_json::json!({
        "page": page,
        "playout": {
            "frames": stats.frames, "decoded": stats.decoded, "concealed": stats.concealed, "late": stats.late_drops,
            "backlog": stats.backlog_drops, "input_drops": stats.input_drops, "ring_underruns": stats.ring_underruns,
        },
        "capture_dropped_frames": probe.queue.dropped(),
    });
    println!("{report}");
    app.exit(0);
}

fn main() {
    let seconds = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(30);
    let playout = Arc::new(SharePlayout::new().unwrap());
    for slot in 0..SLOTS {
        assert!(playout.open(slot, 1, 1.0));
    }
    // The pipeline's share of the work: something must take the sound at the speaker's pace.
    let bus = playout.bus();
    // Computer sound starts with the page's first read, as a share's does.
    let capturing = Arc::new(AtomicBool::new(false));
    let queue = Arc::<CaptureQueue>::default();
    let (running, source) = (capturing.clone(), queue.clone());
    let awake = std::env::var_os("SHARE_IPC_AWAKE").is_some();
    thread::spawn(move || {
        let start = Instant::now();
        let mut block = 0u32;
        loop {
            block += 1;
            bus.mix(&mut [[0.0; 2]; 480], false);
            if running.load(Ordering::Relaxed) {
                source.write(&[0.1; 960]);
            }
            let next = start + Duration::from_millis(10) * block;
            if !awake {
                thread::sleep(next.saturating_duration_since(Instant::now()));
            }
            // App Nap stretches this process's sleeps past 100 ms; a real call's audio exempts it.
            while Instant::now() < next {
                std::hint::spin_loop();
            }
        }
    });
    let mut context = tauri::generate_context!();
    let config = context.config_mut();
    config.identifier = "dev.gwar.share-ipc".into();
    config.build.dev_url = None;
    config.app.windows.clear();
    config.app.security.csp = None;
    context.set_assets(Box::new(Page));
    tauri::Builder::default()
        .manage(Probe { playout, queue, capturing, seconds })
        .invoke_handler(tauri::generate_handler![probe_setup, probe_frames, probe_read, probe_done])
        .setup(move |app| {
            // A locked screen freezes this page, as it would a real one. For a long run on a Mac
            // nobody is at, `SHARE_IPC_AWAKE=1` measures the IPC instead of the lock.
            let mut window = WebviewWindowBuilder::new(app, "probe", WebviewUrl::App("index.html".into()))
                .title("share_ipc")
                .inner_size(320.0, 120.0);
            if awake {
                window = window.background_throttling(tauri::utils::config::BackgroundThrottlingPolicy::Disabled);
            }
            window.build()?;
            Ok(())
        })
        .run(context)
        .expect("share_ipc");
}
