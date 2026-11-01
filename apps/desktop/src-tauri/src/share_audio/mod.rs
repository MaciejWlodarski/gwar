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
/// A capture nobody reads for this long is paused: the OS stops listening until the webview comes
/// back. Far beyond any healthy read, which takes `READ_WAIT` at most and goes on in a hidden
/// window. A stall, a suspended webview and a sleeping machine all look like this, and all of them
/// recover. A webview that is gone never asks, so its JavaScript need not clean up after a crash.
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
    owner: u64,
    queue: Arc<CaptureQueue>,
    _capture: Box<dyn Send>,
    last_read: Instant,
}

/// A capture that lost its reader. It holds nothing of the OS, only the share's consent: stopping
/// the share, a newer start and a lost webview all clear it, and then nothing can resume. The old
/// queue is how an OS ending reaches it, however late: that takes the consent away too.
struct Paused {
    id: u64,
    owner: u64,
    queue: Arc<CaptureQueue>,
}

impl CaptureState {
    /// Whether capture `id` is paused and can still come back. One the OS ended no longer is.
    fn paused(&mut self, id: u64) -> bool {
        self.paused.take_if(|paused| paused.id == id && paused.queue.was_ended_by_system());
        self.paused.as_ref().is_some_and(|paused| paused.id == id)
    }
}

/// What a reader finds under an id.
enum Reading {
    Queue(Arc<CaptureQueue>),
    Paused,
    Over,
}

#[derive(Default)]
struct CaptureState {
    running: Option<Running>,
    paused: Option<Paused>,
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
        state.paused = None;
        let old = state.running.take();
        drop(state);
        Self::end(old);
        Ok(Arc::default())
    }

    /// Lets paused capture `id` start again, for the same share of the same owner only. The
    /// consent stays until the capture's commit, so a refused try can be repeated.
    fn resume(&self, id: u64, owner: u64) -> Result<Arc<CaptureQueue>, String> {
        let mut state = self.lock();
        if !state.paused(id) || state.paused.as_ref().is_some_and(|p| p.owner != owner) || state.starting.is_some() {
            return Err("closed".into());
        }
        state.starting = Some(id);
        Ok(state.paused.as_ref().map_or_else(Arc::default, |paused| paused.queue.restarted()))
    }

    /// Installs what the OS gave start `id`, unless it was cancelled meanwhile or `current` says
    /// its webview no longer owns the app. Refused, the capture stops here.
    fn commit(
        &self,
        id: u64,
        owner: u64,
        queue: Arc<CaptureQueue>,
        capture: Box<dyn Send>,
        current: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        let mut state = self.lock();
        let allowed = match state.starting == Some(id) {
            // Last, so an ending that comes during `current` is seen, and one after it finds this queue.
            true => current().and_then(|()| if queue.adopt() { Ok(()) } else { Err("closed".into()) }),
            false => Err("superseded".into()),
        };
        if state.starting == Some(id) {
            state.starting = None;
        }
        if let Err(refused) = allowed {
            drop(state);
            queue.close();
            return Err(refused);
        }
        state.paused = None;
        state.running = Some(Running { id, owner, queue, _capture: capture, last_read: Instant::now() });
        Ok(())
    }

    /// The OS refused start `id`.
    fn failed(&self, id: u64) {
        let mut state = self.lock();
        if state.starting == Some(id) {
            state.starting = None;
        }
    }

    /// What a reader of capture `id` gets. Reading is what keeps a capture alive.
    fn reader(&self, id: u64) -> Reading {
        let mut state = self.lock();
        if state.paused(id) {
            return Reading::Paused;
        }
        match state.running.as_mut().filter(|running| running.id == id) {
            Some(running) => {
                running.last_read = Instant::now();
                Reading::Queue(running.queue.clone())
            }
            None => Reading::Over,
        }
    }

    /// Why a read of capture `id` found its queue closed: a pause, or the end of it.
    fn closed(&self, id: u64) -> &'static str {
        if self.lock().paused(id) {
            return "paused";
        }
        self.stop(id);
        "closed"
    }

    /// Stops capture `id` and cancels it if it is still starting; it can never start after this.
    fn stop(&self, id: u64) {
        let mut state = self.lock();
        state.newest = state.newest.max(id);
        if state.starting == Some(id) {
            state.starting = None;
        }
        state.paused.take_if(|paused| paused.id == id);
        let old = state.running.take_if(|running| running.id == id);
        drop(state);
        Self::end(old);
    }

    /// Ends everything, a pending start too: the webview that asked for it is gone.
    pub fn retire(&self) {
        let mut state = self.lock();
        state.starting = None;
        state.paused = None;
        let old = state.running.take();
        drop(state);
        Self::end(old);
    }

    /// Whether capture `id` still has a reader at `now`; if not, it pauses here.
    fn keep_alive(&self, id: u64, now: Instant) -> bool {
        let mut state = self.lock();
        let Some(running) = state.running.as_ref().filter(|running| running.id == id) else { return false };
        // Ended by the OS, the consent went with it: the stream stops now, with or without a reader.
        let ended = running.queue.was_ended_by_system();
        if !ended && now.saturating_duration_since(running.last_read) < READER_LEASE {
            return true;
        }
        if !ended {
            tracing::info!("share sound paused: nothing is reading it");
            state.paused = Some(Paused { id, owner: running.owner, queue: running.queue.clone() });
        }
        let old = state.running.take();
        drop(state);
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
    open(&window, &voice, &capture, owner, id, queue).await
}

/// Brings back the capture that lost its reader, for the same share of the same owner: the
/// sharer's consent to `id` still stands. The OS can still say no, as in `share_capture_start`.
#[tauri::command]
pub async fn share_capture_resume(
    window: WebviewWindow,
    voice: State<'_, Voice>,
    capture: State<'_, ShareCapture>,
    owner: u64,
    id: u64,
) -> Result<(), String> {
    check_caller(&window, &voice, owner)?;
    let queue = capture.resume(id, owner)?;
    open(&window, &voice, &capture, owner, id, queue).await
}

/// Opens the OS capture of a claimed `id`, and watches its reader from then on.
async fn open(
    window: &WebviewWindow,
    voice: &Voice,
    capture: &ShareCapture,
    owner: u64,
    id: u64,
    queue: Arc<CaptureQueue>,
) -> Result<(), String> {
    let opened = queue.clone();
    let started = tauri::async_runtime::spawn_blocking(move || platform::start(opened)).await;
    let stream = match started {
        Ok(Ok(stream)) => stream,
        Ok(Err(refusal)) => {
            queue.close();
            capture.failed(id);
            tracing::warn!("share sound did not start: {refusal:?}");
            return Err(refusal.code().to_owned());
        }
        Err(error) => {
            queue.close();
            capture.failed(id);
            return Err(error.to_string());
        }
    };
    capture.commit(id, owner, queue, stream, || check_caller(window, voice, owner))?;
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
/// there is none. The error `paused` asks for `share_capture_resume`; any other ends this capture.
#[tauri::command]
pub async fn share_capture_read(
    window: WebviewWindow,
    voice: State<'_, Voice>,
    capture: State<'_, ShareCapture>,
    owner: u64,
    id: u64,
) -> Result<Response, String> {
    check_caller(&window, &voice, owner)?;
    let queue = match capture.reader(id) {
        Reading::Queue(queue) => queue,
        Reading::Paused => return Err("paused".into()),
        Reading::Over => return Err("closed".into()),
    };
    let samples = queue.read(READ_WAIT).await;
    if samples.is_empty() && queue.is_closed() {
        return Err(capture.closed(id).into());
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

/// How long a slot's sound waits for its picture, in milliseconds.
#[tauri::command]
pub fn share_playback_hold(
    window: WebviewWindow,
    voice: State<'_, Voice>,
    owner: u64,
    run: u64,
    slot: usize,
    ms: u32,
) -> Result<(), String> {
    check_caller(&window, &voice, owner)?;
    voice.share_playout(|playout, _| {
        playout.set_hold(slot, run, ms);
    })
}

/// When a recent sound sample of a slot leaves the speaker, for lining the picture up with it.
#[tauri::command]
pub fn share_playback_clock(
    window: WebviewWindow,
    voice: State<'_, Voice>,
    owner: u64,
    run: u64,
    slot: usize,
) -> Result<Option<SoundClock>, String> {
    check_caller(&window, &voice, owner)?;
    voice.share_playout(|playout, _| playout.clock(slot, run).map(SoundClock::from))
}

/// `rtpTime` plays at `unixMs`: the system clock, which `Date.now()` in the webview also reads.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SoundClock {
    rtp_time: u32,
    unix_ms: f64,
}

impl From<gwar_client::voice::share::ShareClock> for SoundClock {
    fn from(clock: gwar_client::voice::share::ShareClock) -> Self {
        Self { rtp_time: clock.rtp_time, unix_ms: clock.unix_ms() }
    }
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

    const OWNER: u64 = 9;

    impl Reading {
        fn is_queue(&self) -> bool {
            matches!(self, Reading::Queue(_))
        }

        fn is_over(&self) -> bool {
            matches!(self, Reading::Over)
        }
    }

    fn starting(capture: &ShareCapture, id: u64) -> (Arc<CaptureQueue>, Box<dyn Send>) {
        (capture.begin(id).unwrap(), Box::new(()))
    }

    fn running(capture: &ShareCapture, id: u64) -> Arc<CaptureQueue> {
        let (queue, stream) = starting(capture, id);
        capture.commit(id, OWNER, queue.clone(), stream, || Ok(())).unwrap();
        queue
    }

    #[test]
    fn a_stale_capture_reads_nothing_and_stops_nothing() {
        let capture = ShareCapture::default();
        let queue = running(&capture, 5);
        assert!(capture.reader(4).is_over());
        capture.stop(4);
        assert!(capture.reader(5).is_queue());
        capture.stop(5);
        assert!(queue.is_closed());
        assert!(capture.reader(5).is_over());
    }

    #[test]
    fn a_stop_during_startup_cancels_that_start() {
        let capture = ShareCapture::default();
        let (queue, stream) = starting(&capture, 5);
        capture.stop(5);
        let refused = capture.commit(5, OWNER, queue.clone(), stream, || Ok(()));
        assert_eq!(refused.unwrap_err(), "superseded");
        assert!(queue.is_closed(), "the capture that came too late ends at once");
        assert!(capture.reader(5).is_over());
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
        assert!(capture.commit(5, OWNER, queue.clone(), stream, || Ok(())).is_err());
        assert!(queue.is_closed());
        assert!(capture.reader(5).is_over());
    }

    #[test]
    fn a_start_commits_only_for_the_current_owner() {
        let capture = ShareCapture::default();
        let (queue, stream) = starting(&capture, 5);
        let refused = capture.commit(5, OWNER, queue.clone(), stream, || Err("stale owner".into()));
        assert_eq!(refused.unwrap_err(), "stale owner");
        assert!(queue.is_closed());
        assert!(capture.reader(5).is_over());
    }

    #[test]
    fn a_newer_start_cancels_the_older_one_and_replaces_a_running_one() {
        let capture = ShareCapture::default();
        let old = running(&capture, 4);
        let (first, first_stream) = starting(&capture, 5);
        assert!(old.is_closed());
        let (second, second_stream) = starting(&capture, 6);
        assert!(capture.commit(5, OWNER, first.clone(), first_stream, || Ok(())).is_err());
        assert!(first.is_closed());
        capture.commit(6, OWNER, second.clone(), second_stream, || Ok(())).unwrap();
        assert!(capture.reader(6).is_queue() && !second.is_closed());
    }

    /// Past the lease from now: a stalled renderer, a suspended webview or a machine that slept.
    fn stalled(capture: &ShareCapture, id: u64) -> bool {
        capture.keep_alive(id, Instant::now() + READER_LEASE + Duration::from_secs(1))
    }

    fn resumed(capture: &ShareCapture, id: u64) -> Arc<CaptureQueue> {
        let queue = capture.resume(id, OWNER).unwrap();
        capture.commit(id, OWNER, queue.clone(), Box::new(()), || Ok(())).unwrap();
        queue
    }

    #[test]
    fn a_capture_without_a_reader_stops_listening_by_itself() {
        let capture = ShareCapture::default();
        let queue = running(&capture, 5);
        let started = Instant::now();
        assert!(capture.keep_alive(5, started + READER_LEASE / 2));
        assert!(!queue.is_closed());
        assert!(capture.reader(5).is_queue(), "a read renews the lease");
        assert!(capture.keep_alive(5, Instant::now() + READER_LEASE - Duration::from_secs(1)));
        assert!(!stalled(&capture, 5));
        assert!(queue.is_closed(), "no JavaScript took part in this");
        assert!(capture.lock().running.is_none(), "the OS stream dropped with it");
    }

    #[test]
    fn a_stalled_reader_gets_its_capture_back() {
        let capture = ShareCapture::default();
        let queue = running(&capture, 5);
        assert!(!stalled(&capture, 5));
        assert!(matches!(capture.reader(5), Reading::Paused));
        assert_eq!(capture.closed(5), "paused", "the read in flight does not end the share's sound");
        let back = resumed(&capture, 5);
        assert!(!Arc::ptr_eq(&queue, &back) && !back.is_closed());
        assert!(capture.reader(5).is_queue());
        assert!(capture.keep_alive(5, Instant::now()), "and it is watched again");
        assert!(!stalled(&capture, 5), "a second stall pauses it just the same");
        resumed(&capture, 5);
    }

    #[test]
    fn a_refused_resume_can_be_tried_again() {
        let capture = ShareCapture::default();
        running(&capture, 5);
        stalled(&capture, 5);
        capture.resume(5, OWNER).unwrap();
        assert!(capture.resume(5, OWNER).is_err(), "one at a time");
        capture.failed(5);
        resumed(&capture, 5);
    }

    #[test]
    fn only_the_same_share_of_the_same_owner_can_resume() {
        let capture = ShareCapture::default();
        running(&capture, 5);
        stalled(&capture, 5);
        assert!(capture.resume(4, OWNER).is_err());
        assert!(capture.resume(6, OWNER).is_err());
        assert!(capture.resume(5, OWNER + 1).is_err());
        assert!(capture.resume(5, OWNER).is_ok());
    }

    #[test]
    fn a_paused_capture_cannot_outlive_its_consent() {
        type End = fn(&ShareCapture);
        let ended: [(&str, End); 3] = [
            ("the sharer stopped it", |capture| capture.stop(5)),
            ("the webview was lost", |capture| capture.retire()),
            ("a new share started", |capture| drop(capture.begin(6).unwrap())),
        ];
        for (why, end) in ended {
            let capture = ShareCapture::default();
            running(&capture, 5);
            stalled(&capture, 5);
            end(&capture);
            assert!(capture.resume(5, OWNER).is_err(), "{why}");
            assert!(capture.reader(5).is_over(), "{why}");
        }
    }

    #[test]
    fn a_stop_or_a_lost_webview_during_a_resume_wins() {
        let ended: [fn(&ShareCapture); 2] = [|capture| capture.stop(5), |capture| capture.retire()];
        for end in ended {
            let capture = ShareCapture::default();
            running(&capture, 5);
            stalled(&capture, 5);
            let queue = capture.resume(5, OWNER).unwrap();
            end(&capture);
            assert!(capture.commit(5, OWNER, queue.clone(), Box::new(()), || Ok(())).is_err());
            assert!(queue.is_closed());
            assert!(capture.reader(5).is_over());
        }
    }

    #[test]
    fn a_resume_commits_only_for_the_current_owner() {
        let capture = ShareCapture::default();
        running(&capture, 5);
        stalled(&capture, 5);
        let queue = capture.resume(5, OWNER).unwrap();
        assert!(capture.commit(5, OWNER, queue.clone(), Box::new(()), || Err("stale owner".into())).is_err());
        assert!(queue.is_closed());
    }

    #[test]
    fn an_os_ending_stays_an_ending() {
        let capture = ShareCapture::default();
        let queue = running(&capture, 5);
        queue.end_by_system();
        assert_eq!(capture.closed(5), "closed");
        assert!(capture.resume(5, OWNER).is_err());
    }

    /// Stopped from the menu bar while the renderer hangs: reads coming back must not undo it.
    #[test]
    fn an_os_ending_before_the_stall_cannot_be_resumed() {
        let capture = ShareCapture::default();
        let queue = running(&capture, 5);
        queue.end_by_system();
        assert!(!stalled(&capture, 5));
        assert!(matches!(capture.reader(5), Reading::Over), "the reader hears it ended");
        assert_eq!(capture.closed(5), "closed");
        assert!(capture.resume(5, OWNER).is_err());
    }

    /// The OS stream outlives the pause by a moment, until `running` has dropped.
    #[test]
    fn an_os_ending_during_the_pause_cannot_be_resumed() {
        let capture = ShareCapture::default();
        let queue = running(&capture, 5);
        assert!(!stalled(&capture, 5));
        queue.end_by_system();
        assert!(matches!(capture.reader(5), Reading::Over), "no paused answer asks for a resume");
        assert_eq!(capture.closed(5), "closed");
        assert!(capture.resume(5, OWNER).is_err());
    }

    #[test]
    fn an_os_ending_during_a_resume_cannot_be_committed() {
        let capture = ShareCapture::default();
        let queue = running(&capture, 5);
        assert!(!stalled(&capture, 5));
        let back = capture.resume(5, OWNER).unwrap();
        queue.end_by_system();
        let reopened = capture.commit(5, OWNER, back.clone(), Box::new(()), || Ok(()));
        assert!(reopened.is_err());
        assert!(back.is_closed(), "the stream it opened stops again");
        assert!(capture.lock().running.is_none());
        assert!(capture.resume(5, OWNER).is_err(), "and there is no second try");
    }

    #[test]
    fn an_os_ending_before_a_commit_refuses_it() {
        let capture = ShareCapture::default();
        let queue = capture.begin(5).unwrap();
        queue.end_by_system();
        assert!(capture.commit(5, OWNER, queue.clone(), Box::new(()), || Ok(())).is_err());
        assert!(capture.lock().running.is_none(), "no capture runs after the stop");
    }

    #[test]
    fn an_os_ending_during_a_commit_refuses_it() {
        let capture = ShareCapture::default();
        let queue = capture.begin(5).unwrap();
        let ending = queue.clone();
        let started = capture.commit(5, OWNER, queue.clone(), Box::new(()), move || {
            ending.end_by_system();
            Ok(())
        });
        assert!(started.is_err());
        assert!(capture.lock().running.is_none());

        let capture = ShareCapture::default();
        let queue = running(&capture, 5);
        assert!(!stalled(&capture, 5));
        let back = capture.resume(5, OWNER).unwrap();
        let reopened = capture.commit(5, OWNER, back.clone(), Box::new(()), move || {
            queue.end_by_system();
            Ok(())
        });
        assert!(reopened.is_err(), "the check before this one cannot see it");
        assert!(back.is_closed(), "the stream it opened stops again");
        assert!(capture.lock().running.is_none());
        assert!(capture.resume(5, OWNER).is_err(), "and there is no second try");
    }

    #[test]
    fn an_os_ending_after_a_commit_stops_the_replacement() {
        let capture = ShareCapture::default();
        let queue = running(&capture, 5);
        assert!(!stalled(&capture, 5));
        let back = resumed(&capture, 5);
        queue.end_by_system();
        assert!(back.is_closed(), "the old stream's late callback reaches the new one");
        assert!(!capture.keep_alive(5, Instant::now()), "and the OS stream stops at once, with no read");
        assert!(capture.lock().running.is_none());
        assert!(matches!(capture.reader(5), Reading::Over));
        assert!(capture.resume(5, OWNER).is_err());
    }

    #[test]
    fn an_os_ending_reaches_every_later_resume() {
        let capture = ShareCapture::default();
        let first = running(&capture, 5);
        assert!(!stalled(&capture, 5));
        resumed(&capture, 5);
        assert!(!stalled(&capture, 5));
        let last = resumed(&capture, 5);
        first.end_by_system();
        assert!(last.is_closed());
        assert!(!capture.keep_alive(5, Instant::now()));
        assert!(capture.resume(5, OWNER).is_err());
    }

    #[test]
    fn a_newer_share_is_not_stopped_by_an_older_ones_ending() {
        let capture = ShareCapture::default();
        let old = running(&capture, 5);
        let new = running(&capture, 6);
        old.end_by_system();
        assert!(!new.is_closed());
        assert!(capture.reader(6).is_queue());
    }

    #[test]
    fn a_resume_that_failed_to_start_keeps_the_consent() {
        let capture = ShareCapture::default();
        running(&capture, 5);
        assert!(!stalled(&capture, 5));
        for _ in 0..3 {
            capture.resume(5, OWNER).unwrap().stream_ended();
            capture.failed(5);
        }
        assert!(matches!(capture.reader(5), Reading::Paused), "the bar stays on restoring");
        let back = resumed(&capture, 5);
        assert!(!back.is_closed());
        assert!(capture.reader(5).is_queue());
    }

    #[test]
    fn a_failed_resume_stops_only_its_own_try() {
        let capture = ShareCapture::default();
        let queue = running(&capture, 5);
        assert!(!stalled(&capture, 5));
        let back = capture.resume(5, OWNER).unwrap();
        back.stream_ended();
        assert!(back.is_closed() && !back.was_ended_by_system() && !queue.was_ended_by_system());
    }

    #[test]
    fn a_start_given_up_on_keeps_the_consent_if_it_comes_up_late() {
        let capture = ShareCapture::default();
        running(&capture, 5);
        assert!(!stalled(&capture, 5));
        let back = capture.resume(5, OWNER).unwrap();
        back.close();
        back.stream_started();
        back.stream_ended();
        capture.failed(5);
        assert!(capture.resume(5, OWNER).is_ok());
    }

    #[test]
    fn a_started_stream_that_ends_takes_the_consent() {
        let capture = ShareCapture::default();
        let queue = capture.begin(5).unwrap();
        queue.stream_started();
        capture.commit(5, OWNER, queue.clone(), Box::new(()), || Ok(())).unwrap();
        queue.stream_ended();
        assert!(queue.was_ended_by_system());
        assert!(!capture.keep_alive(5, Instant::now()));
        assert!(capture.resume(5, OWNER).is_err());
    }

    #[test]
    fn a_started_stream_that_ends_before_its_commit_refuses_it() {
        let capture = ShareCapture::default();
        running(&capture, 5);
        assert!(!stalled(&capture, 5));
        let back = capture.resume(5, OWNER).unwrap();
        back.stream_started();
        back.stream_ended();
        assert!(capture.commit(5, OWNER, back.clone(), Box::new(()), || Ok(())).is_err());
        assert!(capture.resume(5, OWNER).is_err());
    }

    #[test]
    fn a_replaced_streams_late_ending_stops_the_replacement() {
        let capture = ShareCapture::default();
        let queue = capture.begin(5).unwrap();
        queue.stream_started();
        capture.commit(5, OWNER, queue.clone(), Box::new(()), || Ok(())).unwrap();
        assert!(!stalled(&capture, 5));
        let back = resumed(&capture, 5);
        queue.stream_ended();
        assert!(back.is_closed());
        assert!(!capture.keep_alive(5, Instant::now()));
        assert!(capture.resume(5, OWNER).is_err());
    }

    #[test]
    fn gwars_own_stop_stays_resumable() {
        let capture = ShareCapture::default();
        let queue = running(&capture, 5);
        assert!(!stalled(&capture, 5));
        assert!(queue.is_closed() && !queue.was_ended_by_system());
        assert!(capture.resume(5, OWNER).is_ok());
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
