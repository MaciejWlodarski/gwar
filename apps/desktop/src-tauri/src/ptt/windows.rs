//! Low-level hooks, always forwarded with CallNextHookEx. No new crate is required.
use super::{Signal, Status, binding::Binding};
use std::{
    cell::Cell,
    ffi::c_void,
    ptr,
    sync::{Arc, atomic::Ordering},
};

type Handle = *mut c_void;
type Hook = unsafe extern "system" fn(i32, usize, isize) -> isize;
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
struct Keyboard {
    vk: u32,
    scan: u32,
    flags: u32,
    time: u32,
    extra: usize,
}
#[repr(C)]
struct Mouse {
    point: Point,
    data: u32,
    flags: u32,
    time: u32,
    extra: usize,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn SetWindowsHookExW(kind: i32, callback: Hook, module: Handle, thread: u32) -> Handle;
    fn UnhookWindowsHookEx(hook: Handle) -> i32;
    fn CallNextHookEx(hook: Handle, code: i32, wparam: usize, lparam: isize) -> isize;
    fn PeekMessageW(message: *mut Message, window: Handle, min: u32, max: u32, remove: u32) -> i32;
    fn GetAsyncKeyState(key: i32) -> i16;
    fn MapVirtualKeyW(code: u32, kind: u32) -> u32;
    fn MapVirtualKeyExW(code: u32, kind: u32, layout: Handle) -> u32;
    fn GetForegroundWindow() -> Handle;
    fn GetWindowThreadProcessId(window: Handle, process: *mut u32) -> u32;
    fn GetKeyboardLayout(thread: u32) -> Handle;
    fn OpenInputDesktop(flags: u32, inherit: i32, access: u32) -> Handle;
    fn CloseDesktop(desktop: Handle) -> i32;
    fn GetUserObjectInformationW(object: Handle, index: i32, info: *mut c_void, length: u32, needed: *mut u32) -> i32;
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
pub struct Observer {
    hook: Handle,
    context: Box<Context>,
    virtual_key: i32,
}

unsafe extern "system" fn keyboard(code: i32, wparam: usize, lparam: isize) -> isize {
    if code >= 0 && lparam != 0 {
        CONTEXT.with(|slot| {
            if let Some(c) = unsafe { slot.get().as_ref() } {
                let event = unsafe { &*(lparam as *const Keyboard) };
                let scan = event.scan | if event.flags & 1 != 0 { 0xe000 } else { 0 };
                if !c.binding.mouse && scan == c.binding.scan && event.flags & 0x10 == 0 {
                    c.signal.down.store(matches!(wparam, 0x100 | 0x104), Ordering::Release);
                }
            }
        });
    }
    // SAFETY: always pass the original event onward, even negative codes and unrelated keys.
    unsafe { CallNextHookEx(ptr::null_mut(), code, wparam, lparam) }
}

unsafe extern "system" fn mouse(code: i32, wparam: usize, lparam: isize) -> isize {
    if code >= 0 && lparam != 0 {
        CONTEXT.with(|slot| {
            if let Some(c) = unsafe { slot.get().as_ref() } {
                let event = unsafe { &*(lparam as *const Mouse) };
                let button = match wparam {
                    0x207 | 0x208 => 2,
                    0x20b | 0x20c => 2 + (event.data >> 16),
                    _ => 0,
                };
                if c.binding.mouse && button == c.binding.scan && event.flags & 1 == 0 {
                    c.signal.down.store(matches!(wparam, 0x207 | 0x20b), Ordering::Release);
                }
            }
        });
    }
    // SAFETY: never swallow the mouse event, including XBUTTON events.
    unsafe { CallNextHookEx(ptr::null_mut(), code, wparam, lparam) }
}

impl Observer {
    pub fn open(binding: Binding, signal: Arc<Signal>) -> Result<Self, Status> {
        let context = Box::new(Context { binding, signal });
        // SAFETY: Context remains pinned in its Box until after UnhookWindowsHookEx.
        unsafe {
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
            let hook = SetWindowsHookExW(
                if binding.mouse { 14 } else { 13 },
                if binding.mouse { mouse } else { keyboard },
                GetModuleHandleW(ptr::null()),
                0,
            );
            if hook.is_null() {
                CONTEXT.with(|slot| slot.set(ptr::null()));
                return Err(Status::Recovering);
            }
            Ok(Self { hook, context, virtual_key })
        }
    }

    pub fn pump(&mut self) {
        // SAFETY: dispatch the low-level callbacks on their installing thread, without a UI loop.
        unsafe {
            let mut message = Message::default();
            while PeekMessageW(&mut message, ptr::null_mut(), 0, 0, 1) != 0 {}
        }
    }

    pub fn physical_down(&self) -> Option<bool> {
        // SAFETY: query only desktop availability and the configured input's high (held) bit.
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
                // Scan codes are physical; the held-state API uses the focused app's layout.
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
        // SAFETY: same installing thread; remove the hook before freeing the context.
        unsafe {
            UnhookWindowsHookEx(self.hook);
        }
        CONTEXT.with(|slot| slot.set(ptr::null()));
    }
}
