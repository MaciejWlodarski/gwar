//! Notifications that the system default devices or the device list changed.
//!
//! - **macOS**: CoreAudio property listeners on the system object
//!   (`kAudioHardwarePropertyDefaultInputDevice`, `…DefaultOutputDevice`, `…Devices`).
//! - **Windows**: none needed here. cpal registers an `IMMNotificationClient` for every stream
//!   opened on a default device and reports a default change as a stream error, which `io`
//!   handles by reopening; a chosen device coming back is found by `io`'s poll.
//! - **Linux**: none. The ALSA `default` device is routed by PulseAudio/PipeWire, which move it
//!   to the new default themselves; chosen devices coming back are found by `io`'s poll.

/// Keeps notifications coming until dropped. Create and drop it on the same thread.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub struct Watch {
    #[cfg(target_os = "macos")]
    _inner: mac::Listeners,
}

/// Calls `changed` from an operating-system thread whenever a default device or the device list
/// changes. `None` where the platform needs no watcher, or when registration failed (a chosen
/// device coming back is still found by the poll).
pub fn start(changed: impl Fn() + Send + Sync + 'static) -> Option<Watch> {
    #[cfg(target_os = "macos")]
    {
        match mac::Listeners::new(Box::new(changed)) {
            Ok(inner) => Some(Watch { _inner: inner }),
            Err(status) => {
                tracing::warn!("audio device listener unavailable (OSStatus {status})");
                None
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = changed;
        None
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::{ffi::c_void, ptr::NonNull};

    use objc2_core_audio::{
        AudioObjectAddPropertyListener, AudioObjectID, AudioObjectPropertyAddress, AudioObjectRemovePropertyListener,
        kAudioHardwarePropertyDefaultInputDevice, kAudioHardwarePropertyDefaultOutputDevice,
        kAudioHardwarePropertyDevices, kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal,
        kAudioObjectSystemObject,
    };

    type Callback = Box<dyn Fn() + Send + Sync>;

    const SELECTORS: [u32; 3] = [
        kAudioHardwarePropertyDefaultInputDevice,
        kAudioHardwarePropertyDefaultOutputDevice,
        kAudioHardwarePropertyDevices,
    ];

    fn address(selector: u32) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        }
    }

    pub struct Listeners {
        /// Boxed twice so the listener's client data is a thin pointer that stays put.
        callback: Box<Callback>,
        registered: Vec<u32>,
    }

    unsafe extern "C-unwind" fn notify(
        _object: AudioObjectID,
        _count: u32,
        _addresses: NonNull<AudioObjectPropertyAddress>,
        data: *mut c_void,
    ) -> i32 {
        // SAFETY: `data` is the `Callback` registered below, alive until every listener is removed.
        let callback = unsafe { &*(data as *const Callback) };
        callback();
        0
    }

    impl Listeners {
        pub fn new(callback: Callback) -> Result<Self, i32> {
            let mut listeners = Self { callback: Box::new(callback), registered: Vec::new() };
            for selector in SELECTORS {
                let address = address(selector);
                // SAFETY: the client data outlives the registration (removed in `drop`).
                let status = unsafe {
                    AudioObjectAddPropertyListener(
                        kAudioObjectSystemObject as AudioObjectID,
                        NonNull::from(&address),
                        Some(notify),
                        listeners.data(),
                    )
                };
                if status != 0 {
                    return Err(status);
                }
                listeners.registered.push(selector);
            }
            Ok(listeners)
        }

        fn data(&self) -> *mut c_void {
            &*self.callback as *const Callback as *mut c_void
        }
    }

    impl Drop for Listeners {
        fn drop(&mut self) {
            for selector in std::mem::take(&mut self.registered) {
                let address = address(selector);
                // SAFETY: removes exactly what `new` added, before the callback is freed (as cpal does).
                unsafe {
                    AudioObjectRemovePropertyListener(
                        kAudioObjectSystemObject as AudioObjectID,
                        NonNull::from(&address),
                        Some(notify),
                        self.data(),
                    );
                }
            }
        }
    }
}
