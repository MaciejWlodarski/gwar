//! Notifications that the system default devices or the device list changed.
//!
//! - **macOS**: CoreAudio property listeners on the system object
//!   (`kAudioHardwarePropertyDefaultInputDevice`, `…DefaultOutputDevice`, `…Devices`), and on the
//!   default devices' `kAudioDevicePropertyDataSource`: on Intel Macs headphones in the jack are
//!   another data source of the same built-in device.
//! - **Windows**: none needed here. cpal registers an `IMMNotificationClient` for every stream
//!   opened on a default device and reports a default change as a stream error, which `io`
//!   handles by reopening; a chosen device coming back is found by `io`'s poll.
//! - **Linux**: the ALSA `default` device is routed by PulseAudio or PipeWire, which move a
//!   stream on it to a new default themselves, and nothing tells the stream. So the sound server
//!   is asked here, twice a second, which sink and source are the defaults and which port each
//!   is on: headphones in a laptop's jack are another port of the same sink. PipeWire answers
//!   through `pipewire-pulse`. Chosen devices coming back are found by `io`'s poll.

use super::follow::Side;

/// Keeps notifications coming until dropped. Create and drop it on the same thread.
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
pub struct Watch {
    #[cfg(target_os = "macos")]
    _inner: mac::Listeners,
    #[cfg(target_os = "linux")]
    server: linux::Server,
}

impl Watch {
    /// What the default device on `side` really is, where the device the platform reports does
    /// not tell: on Linux, the sound server's default sink or source and its port; on macOS, the
    /// default device's data source. `None` where the device itself says it all, or the server
    /// has not answered. Called on the thread that started the watch.
    pub fn endpoint(&self, side: Side) -> Option<String> {
        #[cfg(target_os = "linux")]
        {
            self.server.endpoint(side)
        }
        #[cfg(target_os = "macos")]
        {
            self._inner.data_source(side).map(|source| format!("source {source}"))
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = side;
            None
        }
    }

    /// A stream on the default device follows a new default by itself, out of sight of the stream.
    pub fn reroutes(&self) -> bool {
        cfg!(target_os = "linux")
    }
}

/// Calls `changed` from another thread whenever a default device or the device list changes.
/// `None` where the platform needs no watcher, or when there is nothing to watch (a chosen device
/// coming back is still found by the poll).
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
    #[cfg(target_os = "linux")]
    {
        linux::Server::start(Box::new(changed)).map(|server| Watch { server })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = changed;
        None
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::{cell::RefCell, ffi::c_void, ptr::NonNull};

    use objc2_core_audio::{
        AudioObjectAddPropertyListener, AudioObjectGetPropertyData, AudioObjectID, AudioObjectPropertyAddress,
        AudioObjectRemovePropertyListener, kAudioDevicePropertyDataSource, kAudioHardwarePropertyDefaultInputDevice,
        kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyDevices, kAudioObjectPropertyElementMain,
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeInput, kAudioObjectPropertyScopeOutput,
        kAudioObjectSystemObject,
    };

    use super::Side;

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

    /// The default device and its data source property, on `side`.
    fn source_address(side: Side) -> (u32, AudioObjectPropertyAddress) {
        let (default, scope) = match side {
            Side::Input => (kAudioHardwarePropertyDefaultInputDevice, kAudioObjectPropertyScopeInput),
            Side::Output => (kAudioHardwarePropertyDefaultOutputDevice, kAudioObjectPropertyScopeOutput),
        };
        let source = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyDataSource,
            mScope: scope,
            mElement: kAudioObjectPropertyElementMain,
        };
        (default, source)
    }

    /// A property that is one `u32`, if the object has it.
    fn read_u32(object: AudioObjectID, address: AudioObjectPropertyAddress) -> Option<u32> {
        let (mut value, mut size) = (0u32, size_of::<u32>() as u32);
        // SAFETY: `value` is a `u32` and `size` says so; CoreAudio writes at most that.
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                NonNull::from(&address),
                0,
                std::ptr::null(),
                NonNull::from(&mut size),
                NonNull::from(&mut value).cast(),
            )
        };
        (status == 0 && size == size_of::<u32>() as u32).then_some(value)
    }

    pub struct Listeners {
        /// Boxed twice so the listener's client data is a thin pointer that stays put.
        callback: Box<Callback>,
        registered: Vec<u32>,
        /// The default device whose data source is listened to, input and output.
        sources: RefCell<[Option<AudioObjectID>; 2]>,
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
            let mut listeners =
                Self { callback: Box::new(callback), registered: Vec::new(), sources: RefCell::new([None, None]) };
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

        /// The data source the default device on `side` plays or records through, if it has
        /// them: built-in speakers or the headphone jack. Its changes are told from now on.
        pub fn data_source(&self, side: Side) -> Option<u32> {
            let (default, source) = source_address(side);
            let device = read_u32(kAudioObjectSystemObject as AudioObjectID, address(default))?;
            self.listen_to_source(side, device);
            read_u32(device, source)
        }

        /// Moves the data source listener of `side` to `device`, if it is not there yet.
        fn listen_to_source(&self, side: Side, device: AudioObjectID) {
            let index = usize::from(side == Side::Output);
            let mut sources = self.sources.borrow_mut();
            if sources[index] == Some(device) {
                return;
            }
            let (_, source) = source_address(side);
            if let Some(old) = sources[index].take() {
                // SAFETY: removes what was added below, with the same client data.
                unsafe { AudioObjectRemovePropertyListener(old, NonNull::from(&source), Some(notify), self.data()) };
            }
            // SAFETY: the client data outlives the registration (removed here or in `drop`). A device
            // without data sources refuses it, and then there is nothing to remove.
            let status =
                unsafe { AudioObjectAddPropertyListener(device, NonNull::from(&source), Some(notify), self.data()) };
            if status == 0 {
                sources[index] = Some(device);
            }
        }
    }

    impl Drop for Listeners {
        fn drop(&mut self) {
            for (side, device) in [Side::Input, Side::Output].into_iter().zip(*self.sources.get_mut()) {
                let Some(device) = device else { continue };
                let (_, source) = source_address(side);
                // SAFETY: removes what `listen_to_source` added, before the callback is freed.
                unsafe { AudioObjectRemovePropertyListener(device, NonNull::from(&source), Some(notify), self.data()) };
            }
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

/// The sound server's defaults, as one string a side: the sink or source, and its port if it has
/// ports. Either changing is another place the sound goes.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn endpoint(name: &str, port: Option<&str>) -> String {
    match port {
        Some(port) => format!("{name}#{port}"),
        None => name.to_owned(),
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        ffi::{CStr, CString},
        os::unix::net::UnixStream,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
            mpsc::sync_channel,
        },
        thread::{self, Thread},
        time::Duration,
    };

    use pulseaudio::{Client, protocol::port_info::PortInfo};

    use super::{Side, endpoint};

    type Callback = Box<dyn Fn() + Send + Sync>;

    /// How often the server is asked: a default that moved is seen within this.
    const POLL: Duration = Duration::from_millis(500);
    /// How long a server that is gone waits before it is looked for again.
    const RETRY: Duration = Duration::from_secs(5);
    /// How long a connection may take, and how long `start` waits for the first answer.
    const ANSWER: Duration = Duration::from_secs(1);

    struct Shared {
        /// Input, output.
        endpoints: Mutex<[Option<String>; 2]>,
        stop: AtomicBool,
    }

    pub struct Server {
        shared: Arc<Shared>,
        thread: Thread,
    }

    impl Server {
        /// Asks the server on a thread of its own. `None` if no server answered at first: then the
        /// ALSA default is not one a server moves, and nothing changes from what ALSA says.
        pub fn start(changed: Callback) -> Option<Self> {
            let shared = Arc::new(Shared { endpoints: Mutex::new([None, None]), stop: AtomicBool::new(false) });
            let (answered, first) = sync_channel(1);
            let asking = shared.clone();
            let spawned = thread::Builder::new().name("gwar-audio-server-watch".into()).spawn(move || {
                let mut client = None;
                let mut answered = Some(answered);
                let mut known = false;
                while !asking.stop.load(Ordering::Acquire) {
                    if client.is_none() {
                        client = connect();
                    }
                    let read = client.as_ref().and_then(|client| futures::executor::block_on(defaults(client)).ok());
                    if let Some(answer) = answered.take() {
                        let _ = answer.send(read.is_some());
                    }
                    let Some(now) = read else {
                        client = None;
                        thread::park_timeout(RETRY);
                        continue;
                    };
                    let moved = {
                        let mut endpoints = asking.endpoints.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                        let moved = known && *endpoints != now;
                        *endpoints = now;
                        known = true;
                        moved
                    };
                    if moved {
                        changed();
                    }
                    thread::park_timeout(POLL);
                }
            });
            let thread = match spawned {
                Ok(handle) => handle.thread().clone(),
                Err(error) => {
                    tracing::warn!("audio server watch unavailable: {error}");
                    return None;
                }
            };
            let server = Self { shared, thread };
            if first.recv_timeout(ANSWER).unwrap_or(false) {
                tracing::info!("following the sound server's default devices");
                Some(server)
            } else {
                None
            }
        }

        pub fn endpoint(&self, side: Side) -> Option<String> {
            let endpoints = self.shared.endpoints.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            endpoints[match side {
                Side::Input => 0,
                Side::Output => 1,
            }]
            .clone()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.shared.stop.store(true, Ordering::Release);
            self.thread.unpark();
        }
    }

    /// The server from the environment, as PulseAudio's own clients find it, or nothing.
    fn connect() -> Option<Client> {
        let path = pulseaudio::socket_path_from_env()?;
        let socket = UnixStream::connect(path).ok()?;
        // The handshake blocks: a server that never answers must not hold this thread forever.
        socket.set_read_timeout(Some(ANSWER)).ok()?;
        socket.set_write_timeout(Some(ANSWER)).ok()?;
        let cookie = pulseaudio::cookie_path_from_env().and_then(|path| std::fs::read(path).ok());
        Client::new_unix(c"gwar", socket, cookie).ok()
    }

    /// The default source and sink, each with its port.
    async fn defaults(client: &Client) -> pulseaudio::Result<[Option<String>; 2]> {
        let info = client.server_info().await?;
        let source = match info.default_source_name {
            Some(name) => {
                let source = client.source_info_by_name(name.clone()).await?;
                Some(identify(&name, &source.ports, source.active_port))
            }
            None => None,
        };
        let sink = match info.default_sink_name {
            Some(name) => {
                let sink = client.sink_info_by_name(name.clone()).await?;
                Some(identify(&name, &sink.ports, sink.active_port))
            }
            None => None,
        };
        Ok([source, sink])
    }

    fn identify(name: &CString, ports: &[PortInfo], active: usize) -> String {
        let text = |value: &CStr| value.to_string_lossy().into_owned();
        endpoint(&text(name), ports.get(active).map(|port| text(&port.name)).as_deref())
    }

    #[cfg(test)]
    mod tests {
        use std::sync::mpsc::channel;

        use super::*;

        /// Needs a PulseAudio server with sinks `gwar_a` and `gwar_b` (null sinks, so nothing
        /// sounds); CI starts one. A default that the server moves is seen, and told.
        #[test]
        #[ignore = "needs a PulseAudio server with two null sinks"]
        fn a_default_the_server_moves_is_seen() {
            let client = connect().expect("a sound server");
            futures::executor::block_on(client.set_default_sink(c"gwar_a".to_owned())).unwrap();
            let (told, heard) = channel();
            let told = Mutex::new(told);
            let server = Server::start(Box::new(move || {
                let _ = told.lock().unwrap().send(());
            }))
            .expect("the server answered");
            assert_eq!(server.endpoint(Side::Output).as_deref(), Some("gwar_a"));
            futures::executor::block_on(client.set_default_sink(c"gwar_b".to_owned())).unwrap();
            heard.recv_timeout(POLL * 4).expect("the move was told");
            assert_eq!(server.endpoint(Side::Output).as_deref(), Some("gwar_b"));
            assert!(heard.recv_timeout(POLL * 3).is_err(), "and told once");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::endpoint;

    #[test]
    fn a_port_of_the_same_sink_is_another_endpoint() {
        let speakers = endpoint("alsa_output.pci-0000_00_1f.3.analog-stereo", Some("analog-output-speaker"));
        let headphones = endpoint("alsa_output.pci-0000_00_1f.3.analog-stereo", Some("analog-output-headphones"));
        assert_ne!(speakers, headphones);
        assert_eq!(endpoint("gwar_a", None), "gwar_a");
    }
}
