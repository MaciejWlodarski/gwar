//! Share sound in the desktop app (docs/share-audio.md). The webview stays the transport: it
//! owns the share's peer connections, and the server sees the same Opus m-lines as from a browser.
//!
//! - Sharing: the OS capture below fills a bounded [`CaptureQueue`]; the webview reads it into its
//!   existing Opus sender. A platform that cannot leave Gwar's own sound out gives no sound at all.
//! - Watching: the webview hands over encoded Opus before anything decodes it, so it cannot play
//!   share sound. [`SharePlayout`] decodes it into the voice mix and the echo reference.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

use gwar_client::voice::share::{CaptureQueue, ShareFrame};
use serde::Serialize;
use tauri::{State, WebviewWindow, ipc::{InvokeBody, Request, Response}};

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
    pub fn start(_queue: Arc<CaptureQueue>) -> Result<Box<dyn Send>, Refusal> {
        Err(Refusal::Unsupported)
    }
}

/// How often an idle capture answers the webview, so a stale reader cannot hang on forever.
const READ_WAIT: Duration = Duration::from_millis(100);

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
}

/// The running capture. One at a time; a stale id reads nothing and stops nothing.
#[derive(Default)]
pub struct ShareCapture {
    running: Mutex<Option<Running>>,
    latest: AtomicU64,
}

impl ShareCapture {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Running>> {
        self.running.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn queue(&self, id: u64) -> Option<Arc<CaptureQueue>> {
        self.lock().as_ref().filter(|running| running.id == id).map(|running| running.queue.clone())
    }

    /// Closes the capture and tells whoever is reading. The OS stream stops when it drops.
    pub fn stop(&self) {
        if let Some(running) = self.lock().take() {
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
    if id <= capture.latest.fetch_max(id, Ordering::SeqCst) {
        return Err("superseded".into());
    }
    capture.stop();
    let queue = Arc::<CaptureQueue>::default();
    let opened = queue.clone();
    let started = tauri::async_runtime::spawn_blocking(move || platform::start(opened)).await.map_err(|e| e.to_string())?;
    let stream = started.map_err(|refusal| {
        tracing::warn!("share sound did not start: {refusal:?}");
        refusal.code().to_owned()
    })?;
    if capture.latest.load(Ordering::SeqCst) != id {
        return Err("superseded".into());
    }
    *capture.lock() = Some(Running { id, queue, _capture: stream });
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
    let queue = capture.queue(id).ok_or("closed")?;
    let samples = queue.read(READ_WAIT).await;
    if samples.is_empty() && queue.is_closed() {
        capture.stop();
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
    capture.latest.fetch_max(id, Ordering::SeqCst);
    if capture.queue(id).is_some() {
        capture.stop();
    }
    Ok(())
}

/// Claims the mix for `slot`'s share on watch transport `run`. False when no voice engine is
/// running here: without it nothing could be heard, and the UI must not offer a volume.
#[tauri::command]
pub fn share_playback_open(
    window: WebviewWindow,
    voice: State<'_, Voice>,
    owner: u64,
    run: u64,
    slot: usize,
) -> Result<bool, String> {
    check_caller(&window, &voice, owner)?;
    voice.share_playout(|playout, playing| {
        playout.open(slot, run);
        playing
    })
}

#[tauri::command]
pub fn share_playback_close(window: WebviewWindow, voice: State<'_, Voice>, owner: u64, slot: usize) -> Result<(), String> {
    check_caller(&window, &voice, owner)?;
    voice.share_playout(|playout, _| playout.close(slot))
}

/// Volume of one watched share, 0 to 1, independent of voice volume; muted is 0.
#[tauri::command]
pub fn share_playback_gain(
    window: WebviewWindow,
    voice: State<'_, Voice>,
    owner: u64,
    slot: usize,
    gain: f32,
) -> Result<(), String> {
    check_caller(&window, &voice, owner)?;
    voice.share_playout(|playout, _| playout.set_gain(slot, gain))
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

/// The largest batch the webview may send: 4 slots × 16 packets × Opus's largest.
const MAX_BATCH: usize = 16 + 4 * 16 * (7 + 1275);

/// `owner: u64, run: u64`, then per frame `slot: u8, rtp_time: u32, length: u16, opus`, little-endian.
fn parse_frames(body: &[u8]) -> Option<(u64, Vec<ShareFrame>)> {
    if body.len() > MAX_BATCH {
        return None;
    }
    let (header, mut rest) = body.split_at_checked(16)?;
    let owner = u64::from_le_bytes(header[..8].try_into().ok()?);
    let run = u64::from_le_bytes(header[8..].try_into().ok()?);
    let mut frames = Vec::new();
    while !rest.is_empty() {
        let (head, tail) = rest.split_at_checked(7)?;
        let slot = usize::from(head[0]);
        let rtp_time = u32::from_le_bytes(head[1..5].try_into().ok()?);
        let length = usize::from(u16::from_le_bytes(head[5..7].try_into().ok()?));
        let (opus, tail) = tail.split_at_checked(length)?;
        frames.push(ShareFrame { slot, run, rtp_time, opus: opus.to_vec() });
        rest = tail;
    }
    Some((owner, frames))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(frames: &[(u8, u32, &[u8])]) -> Vec<u8> {
        let mut body = [7u64.to_le_bytes(), 9u64.to_le_bytes()].concat();
        for (slot, time, opus) in frames {
            body.push(*slot);
            body.extend(time.to_le_bytes());
            body.extend((opus.len() as u16).to_le_bytes());
            body.extend(*opus);
        }
        body
    }

    #[test]
    fn a_batch_keeps_its_frames_and_their_order() {
        let (owner, frames) = parse_frames(&batch(&[(1, 960, &[1, 2]), (3, 1920, &[3])])).unwrap();
        assert_eq!(owner, 7);
        assert_eq!(frames.len(), 2);
        assert_eq!((frames[0].slot, frames[0].run, frames[0].rtp_time), (1, 9, 960));
        assert_eq!(frames[0].opus, [1, 2]);
        assert_eq!((frames[1].slot, frames[1].rtp_time), (3, 1920));
    }

    #[test]
    fn truncated_or_oversized_batches_are_refused_whole() {
        assert!(parse_frames(&[0; 15]).is_none());
        let mut cut = batch(&[(1, 960, &[1, 2])]);
        cut.pop();
        assert!(parse_frames(&cut).is_none());
        assert!(parse_frames(&vec![0; MAX_BATCH + 1]).is_none());
        assert_eq!(parse_frames(&batch(&[])).unwrap().1.len(), 0);
    }

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
        let tone: Vec<f32> = (0..96_000).map(|i| 0.003 * (std::f32::consts::TAU * 997.0 * i as f32 / 48_000.0).sin()).collect();
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

    #[test]
    fn a_stale_capture_reads_nothing_and_stops_nothing() {
        let capture = ShareCapture::default();
        *capture.lock() = Some(Running { id: 5, queue: Arc::default(), _capture: Box::new(()) });
        assert!(capture.queue(4).is_none());
        assert!(capture.queue(5).is_some());
        let queue = capture.queue(5).unwrap();
        capture.stop();
        assert!(queue.is_closed());
        assert!(capture.queue(5).is_none());
    }
}
