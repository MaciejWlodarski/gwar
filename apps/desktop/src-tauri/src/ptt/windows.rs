//! Passive Raw Input: no hot-key reservation, suppression flags, or evictable LL hook.
//! Its OS registration is explicitly verified before renewing listener health.
use super::{
    Signal, Status,
    binding::Binding,
    packet::{Device, Header, INPUT_SINK, Packet},
};
use std::{
    cell::Cell,
    ffi::c_void,
    ptr,
    sync::{Arc, Mutex, OnceLock, atomic::Ordering},
};

type Handle = *mut c_void;
type WindowProc = unsafe extern "system" fn(Handle, u32, usize, isize) -> isize;
const WM_INPUT: u32 = 0xff;
const RID_INPUT: u32 = 0x10000003;
const REMOVE: u32 = 1;
const CLASS: &std::ffi::CStr = c"GwarPassivePtt";
#[repr(C)]
#[derive(Default)]
struct Point {
    x: i32,
    y: i32,
}
#[repr(C)]
#[derive(Default)]
struct Message {
    window: Handle,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    point: Point,
    private: u32,
}
#[repr(C)]
struct WindowClass {
    style: u32,
    callback: WindowProc,
    class_extra: i32,
    window_extra: i32,
    instance: Handle,
    icon: Handle,
    cursor: Handle,
    background: Handle,
    menu: *const i8,
    name: *const i8,
}
#[link(name = "user32")]
unsafe extern "system" {
    fn RegisterClassA(class: *const WindowClass) -> u16;
    fn CreateWindowExA(
        ex: u32,
        class: *const i8,
        name: *const i8,
        style: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        parent: Handle,
        menu: Handle,
        instance: Handle,
        param: Handle,
    ) -> Handle;
    fn DestroyWindow(window: Handle) -> i32;
    fn IsWindow(window: Handle) -> i32;
    #[cfg(target_pointer_width = "64")]
    fn GetWindowLongPtrA(window: Handle, index: i32) -> isize;
    #[cfg(target_pointer_width = "32")]
    fn GetWindowLongA(window: Handle, index: i32) -> i32;
    fn DefWindowProcA(window: Handle, message: u32, wparam: usize, lparam: isize) -> isize;
    fn DispatchMessageA(message: *const Message) -> isize;
    fn RegisterRawInputDevices(devices: *const Device, count: u32, size: u32) -> i32;
    fn GetRegisteredRawInputDevices(devices: *mut Device, count: *mut u32, size: u32) -> u32;
    fn GetRawInputData(input: Handle, command: u32, data: Handle, size: *mut u32, header_size: u32) -> u32;
    fn PeekMessageW(message: *mut Message, window: Handle, min: u32, max: u32, remove: u32) -> i32;
    fn GetAsyncKeyState(key: i32) -> i16;
    fn MapVirtualKeyW(code: u32, kind: u32) -> u32;
    fn MapVirtualKeyExW(code: u32, kind: u32, layout: Handle) -> u32;
    fn GetForegroundWindow() -> Handle;
    fn GetWindowThreadProcessId(window: Handle, process: *mut u32) -> u32;
    fn GetKeyboardLayout(thread: u32) -> Handle;
    fn OpenInputDesktop(flags: u32, inherit: i32, access: u32) -> Handle;
    fn CloseDesktop(desktop: Handle) -> i32;
    fn GetUserObjectInformationW(object: Handle, index: i32, info: Handle, length: u32, needed: *mut u32) -> i32;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(name: *const u16) -> Handle;
}

struct Context {
    binding: Binding,
    signal: Arc<Signal>,
}
thread_local! { static CONTEXT: Cell<*const Context> = const { Cell::new(ptr::null()) }; }
// Preserve Tao/other native subscriptions when PTT ends. A retiring observer may
// never unregister or restore over its replacement's registration.
struct Saved {
    usage: u16,
    owner: usize,
    previous: Option<Device>,
}
static SAVED: Mutex<Vec<Saved>> = Mutex::new(Vec::new());
pub struct Observer {
    window: Handle,
    context: Box<Context>,
    virtual_key: i32,
    usage: u16,
}

unsafe extern "system" fn callback(window: Handle, message: u32, wparam: usize, lparam: isize) -> isize {
    if message == WM_INPUT {
        CONTEXT.with(|slot| {
            if let Some(context) = unsafe { slot.get().as_ref() } {
                // A fixed, callback-local OS packet. Only matching state survives;
                // positions, text, wheel movement and unbound input are discarded.
                let mut packet: Packet = unsafe { std::mem::zeroed() };
                let mut size = std::mem::size_of::<Packet>() as u32;
                let read = unsafe {
                    GetRawInputData(
                        lparam as Handle,
                        RID_INPUT,
                        (&mut packet as *mut Packet).cast(),
                        &mut size,
                        std::mem::size_of::<Header>() as u32,
                    )
                };
                let expected = match packet.header.kind {
                    0 => std::mem::size_of::<Header>() + 24,
                    1 => std::mem::size_of::<Header>() + 16,
                    _ => 0,
                };
                if read == u32::MAX || read as usize != expected || packet.header.size != read {
                    context.signal.healthy.store(false, Ordering::Release);
                } else if let Some(down) = packet.bound_state(context.binding) {
                    context.signal.down.store(down, Ordering::Release);
                }
            }
        });
    }
    // SAFETY: required Raw Input cleanup, including foreground WM_INPUT packets.
    // Standard keyboard/mouse delivery remains enabled: NOLEGACY is never used.
    unsafe { DefWindowProcA(window, message, wparam, lparam) }
}

fn registrations() -> Option<Vec<Device>> {
    // SAFETY: this is subscription metadata for our process, not input contents.
    unsafe {
        let mut count = 0;
        let size = std::mem::size_of::<Device>() as u32;
        let first = GetRegisteredRawInputDevices(ptr::null_mut(), &mut count, size);
        if count == 0 {
            return (first == 0).then(Vec::new);
        }
        if count > 1024 {
            return None;
        }
        let mut devices = vec![Device::default(); count as usize];
        let read = GetRegisteredRawInputDevices(devices.as_mut_ptr(), &mut count, size);
        if read == u32::MAX || read as usize > devices.len() {
            return None;
        }
        devices.truncate(read as usize);
        Some(devices)
    }
}

impl Observer {
    pub fn open(binding: Binding, signal: Arc<Signal>) -> Result<Self, Status> {
        let context = Box::new(Context { binding, signal });
        // SAFETY: Windows copies the class definition. Context stays boxed on its
        // message-pump thread until after the message-only window is destroyed.
        unsafe {
            static REGISTERED: OnceLock<u16> = OnceLock::new();
            let instance = GetModuleHandleW(ptr::null());
            let atom = REGISTERED.get_or_init(|| {
                RegisterClassA(&WindowClass {
                    style: 0,
                    callback,
                    class_extra: 0,
                    window_extra: 0,
                    instance,
                    icon: ptr::null_mut(),
                    cursor: ptr::null_mut(),
                    background: ptr::null_mut(),
                    menu: ptr::null(),
                    name: CLASS.as_ptr(),
                })
            });
            if *atom == 0 {
                return Err(Status::Recovering);
            }
            let virtual_key = if binding.mouse {
                match binding.scan {
                    2 => 4,
                    3 => 5,
                    4 => 6,
                    _ => return Err(Status::UnsupportedKey),
                }
            } else {
                MapVirtualKeyW(binding.scan, 3) as i32
            };
            if virtual_key == 0 {
                return Err(Status::UnsupportedKey);
            }
            CONTEXT.with(|slot| slot.set(&*context));
            let window = CreateWindowExA(
                0,
                CLASS.as_ptr(),
                c"".as_ptr(),
                0,
                0,
                0,
                0,
                0,
                (-3_isize) as Handle,
                ptr::null_mut(),
                instance,
                ptr::null_mut(),
            );
            if window.is_null() {
                CONTEXT.with(|slot| slot.set(ptr::null()));
                return Err(Status::Recovering);
            }
            let usage = if binding.mouse { 2 } else { 6 };
            let observer = Self { window, context, virtual_key, usage };
            let mut saved = SAVED.lock().unwrap_or_else(|e| e.into_inner());
            let Some(devices) = registrations() else {
                drop(saved);
                return Err(Status::Recovering);
            };
            let current = devices.iter().copied().find(|d| d.page == 1 && d.usage == usage);
            let previous = saved
                .iter()
                .find(|s| s.usage == usage)
                .filter(|s| current.is_some_and(|d| d.owned_by(s.owner, usage)))
                .map_or(current, |s| s.previous);
            let registration = Device { page: 1, usage, flags: INPUT_SINK, window: window as usize };
            if RegisterRawInputDevices(&registration, 1, std::mem::size_of::<Device>() as u32) == 0 {
                drop(saved);
                return Err(Status::Recovering);
            }
            saved.retain(|s| s.usage != usage);
            saved.push(Saved { usage, owner: window as usize, previous });
            Ok(observer)
        }
    }

    pub fn pump(&mut self) {
        // SAFETY: dispatch at most one bounded batch on the installing thread;
        // even an input flood must not starve lease expiry or lock-screen checks.
        unsafe {
            let mut message = Message::default();
            for _ in 0..256 {
                if PeekMessageW(&mut message, self.window, 0, 0, 1) == 0 {
                    break;
                }
                DispatchMessageA(&message);
            }
        }
    }

    pub fn check_health(&self) -> bool {
        // Unlike a silently evictable LL hook, this is explicit OS evidence that
        // our passive receiver is still registered. Polling held state is no proof.
        // SAFETY: this window belongs to this process; query its procedure only.
        #[cfg(target_pointer_width = "64")]
        let procedure = unsafe { GetWindowLongPtrA(self.window, -4) as usize };
        #[cfg(target_pointer_width = "32")]
        let procedure = unsafe { GetWindowLongA(self.window, -4) as u32 as usize };
        procedure == callback as *const () as usize
            && (unsafe { IsWindow(self.window) != 0 })
            && registrations()
                .is_some_and(|devices| devices.iter().any(|d| d.owned_by(self.window as usize, self.usage)))
    }

    pub fn physical_down(&self) -> Option<bool> {
        // SAFETY: query only desktop availability and the configured input's high bit.
        unsafe {
            let desktop = OpenInputDesktop(0, 0, 1);
            if desktop.is_null() {
                return None;
            }
            let mut name = [0_u16; 64];
            let mut needed = 0;
            let ok = GetUserObjectInformationW(
                desktop,
                2,
                name.as_mut_ptr().cast(),
                std::mem::size_of_val(&name) as u32,
                &mut needed,
            );
            CloseDesktop(desktop);
            if ok == 0 || !name.iter().copied().take_while(|c| *c != 0).eq("Default".encode_utf16()) {
                return None;
            }
            let key = if self.context.binding.mouse {
                self.virtual_key
            } else {
                let window = GetForegroundWindow();
                if window.is_null() {
                    return None;
                }
                let thread = GetWindowThreadProcessId(window, ptr::null_mut());
                if thread == 0 {
                    return None;
                }
                let key = MapVirtualKeyExW(self.context.binding.scan, 3, GetKeyboardLayout(thread)) as i32;
                if key == 0 {
                    return None;
                }
                key
            };
            Some(GetAsyncKeyState(key) < 0)
        }
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        let mut saved = SAVED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(index) = saved.iter().position(|s| s.usage == self.usage && s.owner == self.window as usize) {
            let previous = saved.remove(index).previous;
            if registrations()
                .is_some_and(|devices| devices.iter().any(|d| d.owned_by(self.window as usize, self.usage)))
            {
                let restore = previous
                    .filter(|d| d.window == 0 || unsafe { IsWindow(d.window as Handle) != 0 })
                    .unwrap_or(Device { page: 1, usage: self.usage, flags: REMOVE, window: 0 });
                // SAFETY: restore only while we still own this usage; never remove a replacement.
                unsafe {
                    RegisterRawInputDevices(&restore, 1, std::mem::size_of::<Device>() as u32);
                }
            }
        }
        drop(saved);
        // SAFETY: remove the callback source before releasing its context.
        unsafe {
            DestroyWindow(self.window);
        }
        CONTEXT.with(|slot| slot.set(ptr::null()));
    }
}
