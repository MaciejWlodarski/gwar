//! WASAPI process loopback: every render stream on the system except those of Gwar's process
//! tree. That tree holds native voice, cues, share playback and WebView2's helper processes.
//! There is no fallback to whole-device loopback, which would send the call back to itself.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::Duration;

use gwar_client::voice::share::CaptureQueue;
use windows::{
    Win32::{
        Foundation::{CloseHandle, E_FAIL, HANDLE, WAIT_OBJECT_0},
        Media::Audio::{
            AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDIOCLIENT_ACTIVATION_PARAMS,
            AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
            IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
            IAudioCaptureClient, IAudioClient, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVE_FORMAT_PCM, WAVEFORMATEX,
        },
        System::{
            Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize, StructuredStorage::PROPVARIANT},
            Threading::{
                CreateEventW, GetCurrentProcessId, INFINITE, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
            },
            Variant::VT_BLOB,
        },
    },
    core::{IUnknown, Interface, implement},
};

use super::{Refusal, Support};

/// The documented first build with process loopback. Earlier builds may have it (docs/native-media.md),
/// but nothing proves that they leave Gwar out, and a build that does not would echo the call.
const FIRST_BUILD: u32 = 20348;
const ACTIVATE_TIMEOUT_MS: u32 = 5_000;
/// 20 ms, in the 100 ns units WASAPI uses.
const BUFFER: i64 = 200_000;

fn build() -> Option<u32> {
    windows_registry::LOCAL_MACHINE
        .open(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
        .ok()?
        .get_string("CurrentBuildNumber")
        .ok()?
        .parse()
        .ok()
}

pub fn support() -> Support {
    if build().is_some_and(|build| build >= FIRST_BUILD) { Support::Ok } else { Support::Unsupported }
}

// `implement` answers for IAgileObject itself, as this callback from another apartment needs.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Completed {
    done: Arc<Event>,
    /// Still read by an activation that outlives its timeout.
    _params: Box<AUDIOCLIENT_ACTIVATION_PARAMS>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for Completed_Impl {
    fn ActivateCompleted(
        &self,
        _operation: windows::core::Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        // SAFETY: this handler co-owns the event, so a timed-out wait cannot close it first.
        unsafe { SetEvent(self.done.0) }
    }
}

/// An auto-reset event, closed by its last owner. A late completion after a timed-out wait
/// must find this event, not whatever the system has since given the same handle to.
struct Event(HANDLE);

// SAFETY: a Win32 event may be used from any thread.
unsafe impl Send for Event {}
// SAFETY: as above.
unsafe impl Sync for Event {}

impl Event {
    fn new() -> windows::core::Result<Self> {
        // SAFETY: an auto-reset event with no name, owned from here on.
        unsafe { CreateEventW(None, false, false, None) }.map(Self)
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: only this owner closes it, once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The in-memory layout of a `VT_BLOB` PROPVARIANT.
#[repr(C)]
struct BlobVariant {
    vt: u16,
    reserved: [u16; 3],
    size: u32,
    data: *mut core::ffi::c_void,
}

const _: () = assert!(size_of::<BlobVariant>() == size_of::<PROPVARIANT>());

fn activate() -> windows::core::Result<IAudioClient> {
    let params = Box::new(AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                // SAFETY: no preconditions.
                TargetProcessId: unsafe { GetCurrentProcessId() },
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
            },
        },
    });
    let blob = BlobVariant {
        vt: VT_BLOB.0,
        reserved: [0; 3],
        size: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
        data: (&raw const *params).cast_mut().cast(),
    };
    let done = Arc::new(Event::new()?);
    let handler: IActivateAudioInterfaceCompletionHandler = Completed { done: done.clone(), _params: params }.into();
    // SAFETY: `blob` has PROPVARIANT's layout, and the handler keeps `params` for the operation.
    unsafe {
        let variant = (&raw const blob).cast::<PROPVARIANT>();
        ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(variant), &handler)
    }
    .and_then(|operation| {
        // SAFETY: `done` holds the event open.
        if unsafe { WaitForSingleObject(done.0, ACTIVATE_TIMEOUT_MS) } != WAIT_OBJECT_0 {
            return Err(E_FAIL.into());
        }
        let mut status = windows::core::HRESULT(0);
        let mut unknown: Option<IUnknown> = None;
        // SAFETY: valid out-pointers for a completed operation.
        unsafe { operation.GetActivateResult(&mut status, &mut unknown) }?;
        status.ok()?;
        unknown.ok_or_else(|| windows::core::Error::from(E_FAIL))?.cast()
    })
}

fn capture(
    queue: &CaptureQueue,
    stop: &AtomicBool,
    stop_event: HANDLE,
    opened: mpsc::SyncSender<Result<(), String>>,
) -> windows::core::Result<()> {
    let client = activate();
    let client = match client {
        Ok(client) => client,
        Err(error) => {
            let _ = opened.try_send(Err(error.to_string()));
            return Err(error);
        }
    };
    // Process loopback has no mix format to ask for; it converts to the one given here.
    let format = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: 2,
        nSamplesPerSec: 48_000,
        nAvgBytesPerSec: 192_000,
        nBlockAlign: 4,
        wBitsPerSample: 16,
        cbSize: 0,
    };
    let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM;
    // SAFETY: a valid format; the event outlives the client, which this thread owns.
    let (reader, ready) = unsafe {
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER, 0, &format, None)?;
        let ready = CreateEventW(None, false, false, None)?;
        client.SetEventHandle(ready)?;
        let reader: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        (reader, ready)
    };
    queue.stream_started();
    let _ = opened.try_send(Ok(()));
    let handles = [ready, stop_event];
    let mut stereo = Vec::new();
    let mut result = Ok(());
    while !stop.load(Ordering::Relaxed) {
        // SAFETY: both events stay open for this loop.
        if unsafe { WaitForMultipleObjects(&handles, false, INFINITE) } != WAIT_OBJECT_0 {
            break;
        }
        // SAFETY: GetBuffer's pointer is read before the matching ReleaseBuffer.
        if let Err(error) = unsafe { drain(&reader, &mut stereo, queue) } {
            result = Err(error);
            break;
        }
    }
    // SAFETY: this thread owns the client and created `ready`.
    unsafe {
        let _ = client.Stop();
        let _ = CloseHandle(ready);
    }
    result
}

/// Copies every packet the client holds.
unsafe fn drain(
    reader: &IAudioCaptureClient,
    stereo: &mut Vec<f32>,
    queue: &CaptureQueue,
) -> windows::core::Result<()> {
    // SAFETY: the caller's contract.
    unsafe {
        while reader.GetNextPacketSize()? > 0 {
            let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
            reader.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
            stereo.clear();
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                stereo.resize(2 * frames as usize, 0.0);
            } else {
                // The negotiated format is 16-bit stereo.
                let pcm = std::slice::from_raw_parts(data.cast::<i16>(), 2 * frames as usize);
                stereo.extend(pcm.iter().map(|s| f32::from(*s) / 32_768.0));
            }
            reader.ReleaseBuffer(frames)?;
            queue.write(stereo);
        }
    }
    Ok(())
}

struct Capture {
    stop: Arc<AtomicBool>,
    event: isize,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // SAFETY: the event stays open until after the join below.
        unsafe {
            let _ = SetEvent(HANDLE(self.event as *mut _));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // SAFETY: the capture thread is gone; nothing else uses the event.
        unsafe {
            let _ = CloseHandle(HANDLE(self.event as *mut _));
        }
    }
}

/// Windows has no permission to ask for.
pub fn allow() {}

pub fn start(queue: Arc<CaptureQueue>) -> Result<Box<dyn Send>, Refusal> {
    if support() != Support::Ok {
        return Err(Refusal::Unsupported);
    }
    // SAFETY: an auto-reset event with no name.
    let event = unsafe { CreateEventW(None, false, false, None) }.map_err(|e| Refusal::Failed(e.to_string()))?;
    let raw = event.0 as isize;
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let (opened, ready) = mpsc::sync_channel(1);
    let thread = thread::Builder::new()
        .name("gwar-share-capture".into())
        .spawn(move || {
            let stop_event = HANDLE(raw as *mut _);
            // SAFETY: paired with CoUninitialize on this thread.
            let joined = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
            if let Err(error) = capture(&queue, &flag, stop_event, opened) {
                tracing::warn!("share sound stopped: {error}");
            }
            // Process loopback has no control of the user's to stop it: an ending Gwar did not
            // ask for is technical (a device went away, the audio service restarted), and the
            // share may capture again.
            if flag.load(Ordering::Relaxed) {
                queue.close();
            } else {
                queue.stream_interrupted();
            }
            if joined {
                // SAFETY: this thread initialized COM above.
                unsafe { CoUninitialize() };
            }
        })
        .map_err(|e| Refusal::Failed(e.to_string()))?;
    let guard = Capture { stop, event: raw, thread: Some(thread) };
    match ready.recv_timeout(Duration::from_millis(u64::from(ACTIVATE_TIMEOUT_MS) + 2_000)) {
        Ok(Ok(())) => Ok(Box::new(guard)),
        Ok(Err(error)) => Err(Refusal::Failed(error)),
        Err(_) => Err(Refusal::Failed("capture did not start".into())),
    }
}
