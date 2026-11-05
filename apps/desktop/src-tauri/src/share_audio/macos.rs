//! ScreenCaptureKit system sound. `excludesCurrentProcessAudio` leaves out everything the Gwar
//! process plays itself: native voice, cues and share playback. It does not cover WebKit's helper
//! processes (measured on macOS 27; docs/share-audio-validation.md), so the webview keeps its
//! own media silent while this runs.
//!
//! Sound needs a stream over a whole display, so unlike the picture it needs Screen Recording.

use std::sync::{Arc, mpsc};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use gwar_client::voice::share::CaptureQueue;
use objc2::{
    AnyThread, DefinedClass, define_class, msg_send,
    rc::Retained,
    runtime::{NSObject, NSObjectProtocol, ProtocolObject},
};
use objc2_core_audio_types::{AudioBuffer, AudioBufferList};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess};
use objc2_core_media::{CMBlockBuffer, CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_foundation::{NSArray, NSError, NSProcessInfo};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate, SCStreamErrorCode,
    SCStreamErrorDomain, SCStreamOutput, SCStreamOutputType,
};

use super::{Refusal, Support};

const START_TIMEOUT: Duration = Duration::from_secs(10);

struct Ivars {
    queue: Arc<CaptureQueue>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; this adds no `Drop`-sensitive state.
    #[unsafe(super(NSObject))]
    #[name = "GwarShareSoundOutput"]
    #[ivars = Ivars]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn received(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind == SCStreamOutputType::Audio {
                write_sample(sample, &self.ivars().queue);
            }
        }
    }

    unsafe impl SCStreamDelegate for Output {
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn stopped(&self, _stream: &SCStream, error: &NSError) {
            tracing::warn!("share sound stopped: {error}");
            // SAFETY: a constant ScreenCaptureKit defines for as long as it is loaded.
            let ours = *error.domain() == *unsafe { SCStreamErrorDomain };
            if withdrawn(ours, SCStreamErrorCode(error.code())) {
                self.ivars().queue.stream_ended();
            } else {
                self.ivars().queue.stream_interrupted();
            }
        }
    }
);

/// Whether a stream's error is the user taking the sound back: the stop in the menu bar, or
/// declining it. Anything else is technical, and the share may capture again.
fn withdrawn(screen_capture_error: bool, code: SCStreamErrorCode) -> bool {
    screen_capture_error && matches!(code, SCStreamErrorCode::UserStopped | SCStreamErrorCode::UserDeclined)
}

impl Output {
    fn new(queue: Arc<CaptureQueue>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars { queue });
        // SAFETY: NSObject's plain initializer.
        unsafe { msg_send![super(this), init] }
    }
}

/// Space for the stereo stream's two planes.
#[repr(C)]
struct StereoList {
    list: AudioBufferList,
    second: AudioBuffer,
}

fn write_sample(sample: &CMSampleBuffer, queue: &CaptureQueue) {
    // SAFETY: an all-zero AudioBufferList is the "no buffers" value the call overwrites.
    let mut planes: StereoList = unsafe { std::mem::zeroed() };
    let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
    // SAFETY: both out-pointers are valid; `planes` has room for the two buffers it declares.
    let status = unsafe {
        sample.audio_buffer_list_with_retained_block_buffer(
            std::ptr::null_mut(),
            &raw mut planes.list,
            size_of::<StereoList>(),
            None,
            None,
            0,
            &raw mut block,
        )
    };
    if status != 0 {
        return;
    }
    // SAFETY: on success the block buffer is returned retained; it keeps the planes alive.
    let _block = std::ptr::NonNull::new(block).map(|block| unsafe { CFRetained::from_raw(block) });
    let count = planes.list.mNumberBuffers as usize;
    let first = planes.list.mBuffers[0];
    let second = if count > 1 { planes.second } else { first };
    let (Some(left), Some(right)) = (plane(&first), plane(&second)) else { return };
    let stereo: Vec<f32> = left.iter().zip(right).flat_map(|(l, r)| [*l, *r]).collect();
    queue.write(&stereo);
}

/// The buffer's samples; the stream is configured for 32-bit float.
fn plane(buffer: &AudioBuffer) -> Option<&[f32]> {
    let data = buffer.mData.cast::<f32>();
    if data.is_null() || !data.is_aligned() {
        return None;
    }
    // SAFETY: CoreMedia describes `mDataByteSize` valid bytes at `mData`, alive with the block buffer.
    Some(unsafe { std::slice::from_raw_parts(data, buffer.mDataByteSize as usize / size_of::<f32>()) })
}

pub fn support() -> Support {
    let version = NSProcessInfo::processInfo().operatingSystemVersion();
    // `excludesCurrentProcessAudio` arrived in macOS 13; sharing itself starts at 14.6.
    if version.majorVersion < 14 {
        Support::Unsupported
    } else if CGPreflightScreenCaptureAccess() {
        Support::Ok
    } else {
        Support::Permission
    }
}

/// The system's one-time prompt, which also lists Gwar in System Settings.
pub fn allow() {
    CGRequestScreenCaptureAccess();
}

/// A display to attach the stream to. The filter chooses no window: sound is system-wide.
fn display() -> Result<Retained<SCContentFilter>, Refusal> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let done = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        // SAFETY: the framework passes a valid or null object for the call's duration.
        let result = match unsafe { (content.as_ref(), error.as_ref()) } {
            (Some(content), _) => {
                // SAFETY: `displays` is a plain property of a live object.
                match unsafe { content.displays() }.firstObject() {
                    Some(display) => {
                        let none = NSArray::new();
                        // SAFETY: a valid display and empty exclusions.
                        Ok(unsafe {
                            SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
                                SCContentFilter::alloc(),
                                &display,
                                &none,
                                &NSArray::new(),
                            )
                        })
                    }
                    None => Err(Refusal::Failed("no display".into())),
                }
            }
            (None, Some(error)) => Err(Refusal::Failed(error.to_string())),
            (None, None) => Err(Refusal::Failed("no shareable content".into())),
        };
        let _ = sender.try_send(result);
    });
    // SAFETY: the block lives until the framework releases it.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            false, false, &done,
        )
    };
    receiver.recv_timeout(START_TIMEOUT).map_err(|_| Refusal::Failed("timed out listing displays".into()))?
}

fn configuration() -> Retained<SCStreamConfiguration> {
    // SAFETY: plain property writes on a new configuration.
    unsafe {
        let config = SCStreamConfiguration::new();
        config.setCapturesAudio(true);
        config.setExcludesCurrentProcessAudio(true);
        config.setSampleRate(48_000);
        config.setChannelCount(2);
        // The stream needs a video side; it is the smallest and slowest allowed, and unread.
        config.setWidth(2);
        config.setHeight(2);
        config.setMinimumFrameInterval(CMTime { value: 1, timescale: 1, flags: CMTimeFlags::Valid, epoch: 0 });
        config
    }
}

struct Stream {
    stream: Retained<SCStream>,
    _output: Retained<Output>,
}

// SAFETY: SCStream's methods are documented thread-safe; `Output` only holds a thread-safe queue.
unsafe impl Send for Stream {}

impl Drop for Stream {
    fn drop(&mut self) {
        let finish = RcBlock::new(|_: *mut NSError| {});
        // SAFETY: the stream is alive; the framework retains the block.
        unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&finish)) };
    }
}

pub fn start(queue: Arc<CaptureQueue>) -> Result<Box<dyn Send>, Refusal> {
    let refused = match support() {
        Support::Ok => false,
        Support::Unsupported => return Err(Refusal::Unsupported),
        Support::Permission => true,
    };
    // The check can stay out of date until a relaunch, while the capture already works: a sharer
    // who has just turned it on keeps their share. Only the capture itself can say no for sure.
    let filter = display().map_err(|error| {
        if !refused {
            return error;
        }
        allow();
        Refusal::Permission
    })?;
    let config = configuration();
    let output = Output::new(queue.clone());
    let delegate = ProtocolObject::from_ref(&*output);
    // SAFETY: valid filter and configuration; the delegate outlives the stream through `Stream`.
    let stream =
        unsafe { SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, Some(delegate)) };
    let handler = DispatchQueue::new("app.gwar.share-sound", None);
    // SAFETY: the output and its queue stay alive with `Stream`.
    unsafe {
        stream.addStreamOutput_type_sampleHandlerQueue_error(
            ProtocolObject::from_ref(&*output),
            SCStreamOutputType::Audio,
            Some(&handler),
        )
    }
    .map_err(|e| Refusal::Failed(e.to_string()))?;
    let (sender, receiver) = mpsc::sync_channel(1);
    let started = RcBlock::new(move |error: *mut NSError| {
        // SAFETY: valid or null for the call's duration.
        let failure = unsafe { error.as_ref() }.map(ToString::to_string);
        if failure.is_none() {
            queue.stream_started();
        }
        let _ = sender.try_send(failure);
    });
    // SAFETY: the block lives until the framework releases it.
    unsafe { stream.startCaptureWithCompletionHandler(Some(&started)) };
    let guard = Stream { stream, _output: output };
    match receiver.recv_timeout(START_TIMEOUT) {
        Ok(None) => Ok(Box::new(guard)),
        Ok(Some(error)) => Err(Refusal::Failed(error)),
        Err(_) => Err(Refusal::Failed("timed out starting capture".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_user_takes_the_sound_back() {
        assert!(withdrawn(true, SCStreamErrorCode::UserStopped));
        assert!(withdrawn(true, SCStreamErrorCode::UserDeclined));
        for technical in [
            SCStreamErrorCode::SystemStoppedStream,
            SCStreamErrorCode::FailedApplicationConnectionInterrupted,
            SCStreamErrorCode::FailedToStartAudioCapture,
            SCStreamErrorCode::InternalError,
        ] {
            assert!(!withdrawn(true, technical), "{technical:?}");
        }
        assert!(!withdrawn(false, SCStreamErrorCode::UserStopped), "another domain's -3817 means something else");
    }
}
