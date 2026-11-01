//! Share sound in the desktop app (docs/share-audio.md). The webview stays the transport: it
//! owns the share's peer connections, and the server sees the same Opus m-lines as from a browser.
//!
//! - Sharing: the OS capture below fills a bounded [`CaptureQueue`]; the webview reads it into its
//!   existing Opus sender. A platform that cannot leave Gwar's own sound out gives no sound at all.
//! - Watching: the webview hands over encoded Opus before anything decodes it, so it cannot play
//!   share sound. [`SharePlayout`] decodes it into the voice mix and the echo reference.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use gwar_client::voice::share::{CaptureQueue, parse_frames};
use serde::Serialize;
use tauri::{
    Manager, State, WebviewWindow,
    ipc::{InvokeBody, Request, Response},
};

use crate::voice::Voice;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use self::windows as platform;
#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use super::*;
    pub fn support() -> Support {
        Support::Unsupported
    }
    pub fn allow() {}
    pub fn start(_queue: Arc<CaptureQueue>) -> Result<Box<dyn Send>, Refusal> {
        Err(Refusal::Unsupported)
    }
}

/// How often an idle capture answers the webview, so a stale reader cannot hang on forever.
const READ_WAIT: Duration = Duration::from_millis(100);
/// A capture nobody reads for this long has lost its webview, however that happened: its
/// JavaScript cannot be trusted to clean up after a crash. Far beyond any healthy read, which
/// takes `READ_WAIT` at most and keeps going in a hidden window.
const READER_LEASE: Duration = Duration::from_secs(5);
const LEASE_CHECK: Duration = Duration::from_secs(1);

/// Whether this system can share its sound with Gwar's own sound left out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    Ok,
    /// Screen Recording is off for Gwar; the user must turn it on in System Settings.
    Permission,
    /// This OS cannot leave Gwar out of the capture.
    Unsupported,
}

/// Why a capture did not start. Never shown as raw text: the UI words each kind itself.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    Permission,
    Unsupported,
    Failed(String),
}

impl Refusal {
    fn code(&self) -> &'static str {
        match self {
            Refusal::Permission => "permission",
            Refusal::Unsupported => "unsupported",
            Refusal::Failed(_) => "failed",
        }
    }
}

struct Running {
    id: u64,
    queue: Arc<CaptureQueue>,
    _capture: Box<dyn Send>,
    last_read: Instant,
}

#[derive(Default)]
struct CaptureState {
    running: Option<Running>,
    /// The start waiting on the OS. Whatever clears it has cancelled that start.
    starting: Option<u64>,
    /// The newest id seen, by a start or a stop: nothing at or below it can start any more.
    newest: u64,
}

/// The running capture. One at a time; a stale id reads nothing and stops nothing. Every change
/// happens under one lock, so a stop cannot fall between a start's checks and its commit.
#[derive(Default)]
pub struct ShareCapture {
    state: Mutex<CaptureState>,
}

impl ShareCapture {
    fn lock(&self) -> MutexGuard<'_, CaptureState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Claims the capture for start `id`; whatever ran before ends. The queue is the new one's.
    fn begin(&self, id: u64) -> Result<Arc<CaptureQueue>, String> {
        let mut state = self.lock();
        if id <= state.newest {
            return Err("superseded".into());
        }
        state.newest = id;
        state.starting = Some(id);
        let old = state.running.take();
        drop(state);
        Self::end(old);
        Ok(Arc::default())
    }

    /// Installs what the OS gave start `id`, unless it was cancelled meanwhile or `current` says
    /// its webview no longer owns the app. Refused, the capture stops here.
    fn commit(
        &self,
        id: u64,
        queue: Arc<CaptureQueue>,
        capture: Box<dyn Send>,
        current: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        let mut state = self.lock();
        let allowed = if state.starting == Some(id) { current() } else { Err("superseded".into()) };
        if state.starting == Some(id) {
            state.starting = None;
        }
        if let Err(refused) = allowed {
            drop(state);
            queue.close();
            return Err(refused);
        }
        state.running = Some(Running { id, queue, _capture: capture, last_read: Instant::now() });
        Ok(())
    }

    /// The OS refused start `id`.
    fn failed(&self, id: u64) {
        let mut state = self.lock();
        if state.starting == Some(id) {
            state.starting = None;
        }
    }

    /// The queue of capture `id`, for a reader. Reading is what keeps a capture alive.
    fn reader(&self, id: u64) -> Option<Arc<CaptureQueue>> {
        let mut state = self.lock();
        let running = state.running.as_mut().filter(|running| running.id == id)?;
        running.last_read = Instant::now();
        Some(running.queue.clone())
    }

    /// Stops capture `id` and cancels it if it is still starting; it can never start after this.
    fn stop(&self, id: u64) {
        let mut state = self.lock();
        state.newest = state.newest.max(id);
        if state.starting == Some(id) {
            state.starting = None;
        }
        let old = state.running.take_if(|running| running.id == id);
        drop(state);
        Self::end(old);
    }

    /// Ends everything, a pending start too: the webview that asked for it is gone.
    pub fn retire(&self) {
        let mut state = self.lock();
        state.starting = None;
        let old = state.running.take();
        drop(state);
        Self::end(old);
    }

    /// Whether capture `id` still has a reader at `now`; if not, it ends here.
    fn keep_alive(&self, id: u64, now: Instant) -> bool {
        let mut state = self.lock();
        let Some(running) = state.running.as_ref().filter(|running| running.id == id) else { return false };
        if now.saturating_duration_since(running.last_read) < READER_LEASE {
            return true;
        }
        let old = state.running.take();
        drop(state);
        tracing::warn!("share sound stopped: nothing is reading it");
        Self::end(old);
        false
    }

    /// Tells whoever is reading. The OS stream stops when it drops.
    fn end(running: Option<Running>) {
        if let Some(running) = running {
            running.queue.close();
        }
    }
}

fn check_caller(window: &WebviewWindow, voice: &Voice, owner: u64) -> Result<(), String> {
    if window.label() != "main" {
        return Err("share sound requires the main window".into());
    }
    voice.check_current_owner(owner)
}

#[tauri::command]
pub fn share_capture_support() -> Support {
    platform::support()
}

/// Asks the OS to list Gwar for this permission. The answer takes a relaunch to apply.
#[tauri::command]
pub fn share_capture_allow(window: WebviewWindow) -> Result<(), String> {
    if window.label() != "main" {
        return Err("share sound requires the main window".into());
    }
    platform::allow();
    Ok(())
}

/// Starts capturing, only ever from the sharer's own confirmation. Sound must never begin on
/// its own: `id` belongs to one share start, and the webview stops it with that share.
#[tauri::command]
pub async fn share_capture_start(
    window: WebviewWindow,
    voice: State<'_, Voice>,
    capture: State<'_, ShareCapture>,
    owner: u64,
    id: u64,
) -> Result<(), String> {
    check_caller(&window, &voice, owner)?;
    let queue = capture.begin(id)?;
    let opened = queue.clone();
    let started = tauri::async_runtime::spawn_blocking(move || platform::start(opened)).await;
    let stream = match started {
        Ok(Ok(stream)) => stream,
        Ok(Err(refusal)) => {
            capture.failed(id);
            tracing::warn!("share sound did not start: {refusal:?}");
            return Err(refusal.code().to_owned());
        }
        Err(error) => {
            capture.failed(id);
            return Err(error.to_string());
        }
    };
    capture.commit(id, queue, stream, || check_caller(&window, &voice, owner))?;
    let app = window.app_handle().clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(LEASE_CHECK).await;
            if !app.state::<ShareCapture>().keep_alive(id, Instant::now()) {
                return;
            }
        }
    });
    Ok(())
}

/// Interleaved 48 kHz stereo `f32` as raw bytes. Waits briefly for sound and answers empty when
/// there is none; an error means this capture is over.
#[tauri::command]
pub async fn share_capture_read(
    window: WebviewWindow,
    voice: State<'_, Voice>,
    capture: State<'_, ShareCapture>,
    owner: u64,
    id: u64,
) -> Result<Response, String> {
    check_caller(&window, &voice, owner)?;
    let queue = capture.reader(id).ok_or("closed")?;
    let samples = queue.read(READ_WAIT).await;
    if samples.is_empty() && queue.is_closed() {
        capture.stop(id);
        return Err("closed".into());
    }
    Ok(Response::new(samples.iter().flat_map(|s| s.to_le_bytes()).collect::<Vec<u8>>()))
}

#[tauri::command]
pub fn share_capture_stop(
    window: WebviewWindow,
    voice: State<'_, Voice>,
    capture: State<'_, ShareCapture>,
    owner: u64,
    id: u64,
) -> Result<(), String> {
    check_caller(&window, &voice, owner)?;
    capture.stop(id);
    Ok(())
}

/// Claims the mix for `slot`'s share on watch transport `run`, at volume `gain`. False when a
/// newer transport has the slot, or no voice engine is running here: nothing could be heard then,
/// and the UI must not offer a volume.
#[tauri::command]
pub fn share_playback_open(
    window: WebviewWindow,
    voice: State<'_, Voice>,
    owner: u64,
    run: u64,
    slot: usize,
    gain: f32,
) -> Result<bool, String> {
    check_caller(&window, &voice, owner)?;
    voice.share_playout(|playout, playing| playout.open(slot, run, gain) && playing)
}

/// Ends `run`'s claim. A close that arrives after a newer transport took the slot changes nothing.
#[tauri::command]
pub fn share_playback_close(
    window: WebviewWindow,
    voice: State<'_, Voice>,
    owner: u64,
    run: u64,
    slot: usize,
) -> Result<(), String> {
    check_caller(&window, &voice, owner)?;
    voice.share_playout(|playout, _| {
        playout.close(slot, run);
    })
}

/// Volume of one watched share, 0 to 1, independent of voice volume; muted is 0.
#[tauri::command]
pub fn share_playback_gain(
    window: WebviewWindow,
    voice: State<'_, Voice>,
    owner: u64,
    run: u64,
    slot: usize,
    gain: f32,
) -> Result<(), String> {
    check_caller(&window, &voice, owner)?;
    voice.share_playout(|playout, _| {
        playout.set_gain(slot, run, gain);
    })
}

/// One batch of encoded frames, as `parse_frames` reads them.
#[tauri::command]
pub async fn share_playback_frames(
    window: WebviewWindow,
    voice: State<'_, Voice>,
    request: Request<'_>,
) -> Result<(), String> {
    let InvokeBody::Raw(body) = request.body() else { return Err("expected raw frames".into()) };
    let (owner, frames) = parse_frames(body).ok_or("malformed share frames")?;
    check_caller(&window, &voice, owner)?;
    let playout = voice.share_playout(|playout, _| playout.clone())?;
    for frame in frames {
        playout.push(frame);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_have_codes_the_web_ui_knows() {
        assert_eq!(Refusal::Permission.code(), "permission");
        assert_eq!(Refusal::Unsupported.code(), "unsupported");
        assert_eq!(Refusal::Failed("anything".into()).code(), "failed");
        assert_eq!(serde_json::to_string(&Support::Permission).unwrap(), "\"permission\"");
    }

    /// A real capture on this Mac, run by hand with the display awake (`caffeinate -u`): another
    /// process's tone arrives; the same tone from this process does not.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "needs Screen Recording, an awake display and a sound output"]
    async fn this_mac_captures_other_processes_and_leaves_this_one_out() {
        let dir = std::env::temp_dir().join(format!("gwar-share-sound-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tone: Vec<f32> =
            (0..96_000).map(|i| 0.003 * (std::f32::consts::TAU * 997.0 * i as f32 / 48_000.0).sin()).collect();
        let path = dir.join("tone.wav");
        std::fs::write(&path, wav(&tone)).unwrap();
        let queue = Arc::<CaptureQueue>::default();
        let source = queue.clone();
        let _capture = tauri::async_runtime::spawn_blocking(move || platform::start(source)).await.unwrap().unwrap();
        async fn level(queue: &CaptureQueue, seconds: u32) -> f32 {
            let mut left = Vec::new();
            let until = std::time::Instant::now() + Duration::from_secs(seconds.into());
            while std::time::Instant::now() < until {
                left.extend(queue.read(READ_WAIT).await.as_chunks::<2>().0.iter().map(|frame| frame[0]));
            }
            let (mut c, mut s) = (0.0, 0.0);
            for (i, v) in left.iter().enumerate() {
                let phase = std::f32::consts::TAU * 997.0 * i as f32 / 48_000.0;
                c += v * phase.cos();
                s += v * phase.sin();
            }
            2.0 * (c * c + s * s).sqrt() / left.len().max(1) as f32
        }
        level(&queue, 1).await;
        let mut player = std::process::Command::new("/usr/bin/afplay").arg(&path).spawn().unwrap();
        let other = level(&queue, 2).await;
        player.wait().unwrap();
        level(&queue, 1).await;
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let playing = std::thread::spawn(move || gwar_client::voice::io::play_sample(None, tone, cancelled));
        let own = level(&queue, 2).await;
        playing.join().unwrap().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        eprintln!("other process {other:.5}, this process {own:.7}");
        assert!(other > 0.0015, "another process was not captured: {other}");
        assert!(own < other / 100.0, "this process leaked into the capture: {own}");
    }

    #[cfg(target_os = "macos")]
    fn wav(samples: &[f32]) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| ((s * 32_767.0) as i16).to_le_bytes()).collect();
        let mut out = Vec::new();
        out.extend(b"RIFF");
        out.extend((36 + data.len() as u32).to_le_bytes());
        out.extend(b"WAVEfmt ");
        out.extend(16u32.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend(48_000u32.to_le_bytes());
        out.extend(96_000u32.to_le_bytes());
        out.extend(2u16.to_le_bytes());
        out.extend(16u16.to_le_bytes());
        out.extend(b"data");
        out.extend((data.len() as u32).to_le_bytes());
        out.extend(data);
        out
    }

    fn starting(capture: &ShareCapture, id: u64) -> (Arc<CaptureQueue>, Box<dyn Send>) {
        (capture.begin(id).unwrap(), Box::new(()))
    }

    fn running(capture: &ShareCapture, id: u64) -> Arc<CaptureQueue> {
        let (queue, stream) = starting(capture, id);
        capture.commit(id, queue.clone(), stream, || Ok(())).unwrap();
        queue
    }

    #[test]
    fn a_stale_capture_reads_nothing_and_stops_nothing() {
        let capture = ShareCapture::default();
        let queue = running(&capture, 5);
        assert!(capture.reader(4).is_none());
        capture.stop(4);
        assert!(capture.reader(5).is_some());
        capture.stop(5);
        assert!(queue.is_closed());
        assert!(capture.reader(5).is_none());
    }

    #[test]
    fn a_stop_during_startup_cancels_that_start() {
        let capture = ShareCapture::default();
        let (queue, stream) = starting(&capture, 5);
        capture.stop(5);
        let refused = capture.commit(5, queue.clone(), stream, || Ok(()));
        assert_eq!(refused.unwrap_err(), "superseded");
        assert!(queue.is_closed(), "the capture that came too late ends at once");
        assert!(capture.reader(5).is_none());
    }

    #[test]
    fn a_stop_that_overtakes_its_start_still_wins() {
        let capture = ShareCapture::default();
        capture.stop(5);
        assert!(capture.begin(5).is_err());
    }

    #[test]
    fn a_new_view_cancels_a_start_the_old_one_left_behind() {
        let capture = ShareCapture::default();
        let (queue, stream) = starting(&capture, 5);
        capture.retire();
        assert!(capture.commit(5, queue.clone(), stream, || Ok(())).is_err());
        assert!(queue.is_closed());
        assert!(capture.reader(5).is_none());
    }

    #[test]
    fn a_start_commits_only_for_the_current_owner() {
        let capture = ShareCapture::default();
        let (queue, stream) = starting(&capture, 5);
        let refused = capture.commit(5, queue.clone(), stream, || Err("stale owner".into()));
        assert_eq!(refused.unwrap_err(), "stale owner");
        assert!(queue.is_closed());
        assert!(capture.reader(5).is_none());
    }

    #[test]
    fn a_newer_start_cancels_the_older_one_and_replaces_a_running_one() {
        let capture = ShareCapture::default();
        let old = running(&capture, 4);
        let (first, first_stream) = starting(&capture, 5);
        assert!(old.is_closed());
        let (second, second_stream) = starting(&capture, 6);
        assert!(capture.commit(5, first.clone(), first_stream, || Ok(())).is_err());
        assert!(first.is_closed());
        capture.commit(6, second.clone(), second_stream, || Ok(())).unwrap();
        assert!(capture.reader(6).is_some() && !second.is_closed());
    }

    #[test]
    fn a_capture_without_a_reader_ends_by_itself() {
        let capture = ShareCapture::default();
        let queue = running(&capture, 5);
        let started = Instant::now();
        assert!(capture.keep_alive(5, started + READER_LEASE / 2));
        assert!(!queue.is_closed());
        assert!(capture.reader(5).is_some(), "a read renews the lease");
        assert!(capture.keep_alive(5, Instant::now() + READER_LEASE - Duration::from_secs(1)));
        assert!(!capture.keep_alive(5, Instant::now() + READER_LEASE + Duration::from_secs(1)));
        assert!(queue.is_closed(), "no JavaScript took part in this");
        assert!(capture.reader(5).is_none());
    }

    #[test]
    fn an_old_lease_cannot_end_the_capture_that_replaced_it() {
        let capture = ShareCapture::default();
        running(&capture, 4);
        let queue = running(&capture, 5);
        assert!(!capture.keep_alive(4, Instant::now() + 2 * READER_LEASE), "the old watch ends");
        assert!(!queue.is_closed());
    }
}
