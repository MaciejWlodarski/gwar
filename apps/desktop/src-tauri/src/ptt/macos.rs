//! Minimal system-framework FFI. The tap is unconditionally listen-only.
use super::{Signal, Status, binding::Binding};
use std::{
    ffi::c_void,
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

type Ref = *mut c_void;
type Callback = unsafe extern "C" fn(Ref, u32, Ref, Ref) -> Ref;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightListenEventAccess() -> bool;
    fn CGRequestListenEventAccess() -> bool;
    fn CGEventTapCreate(tap: u32, place: u32, options: u32, mask: u64, callback: Callback, info: Ref) -> Ref;
    fn CGEventTapIsEnabled(tap: Ref) -> bool;
    fn CGEventTapEnable(tap: Ref, enable: bool);
    fn CGEventGetFlags(event: Ref) -> u64;
    fn CGEventGetIntegerValueField(event: Ref, field: u32) -> i64;
    fn CGEventSourceKeyState(state: i32, key: u16) -> bool;
    fn CGEventSourceButtonState(state: i32, button: u32) -> bool;
    fn CGSessionCopyCurrentDictionary() -> Ref;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopDefaultMode: Ref;
    fn CFRunLoopGetCurrent() -> Ref;
    fn CFMachPortCreateRunLoopSource(allocator: Ref, port: Ref, order: isize) -> Ref;
    fn CFRunLoopAddSource(loop_: Ref, source: Ref, mode: Ref);
    fn CFRunLoopRemoveSource(loop_: Ref, source: Ref, mode: Ref);
    fn CFRunLoopRunInMode(mode: Ref, seconds: f64, return_after_source: bool) -> i32;
    fn CFMachPortInvalidate(port: Ref);
    fn CFRelease(value: Ref);
    fn CFStringCreateWithCString(allocator: Ref, bytes: *const i8, encoding: u32) -> Ref;
    fn CFDictionaryGetValue(dictionary: Ref, key: Ref) -> Ref;
    fn CFBooleanGetValue(value: Ref) -> bool;
}

struct Context {
    binding: Binding,
    signal: Arc<Signal>,
}
pub struct Observer {
    tap: Ref,
    source: Ref,
    context: Box<Context>,
}

unsafe extern "C" fn callback(_: Ref, kind: u32, event: Ref, info: Ref) -> Ref {
    // SAFETY: Context outlives the tap and its run-loop source, on this same thread.
    let context = unsafe { &*(info as *const Context) };
    if kind == 0xffff_fffe || kind == 0xffff_ffff {
        context.signal.healthy.store(false, Ordering::Release);
    } else if !event.is_null() {
        let b = context.binding;
        // Inspect only the configured key/button; do not decode characters or other fields.
        let matches = if b.mouse {
            matches!(kind, 25 | 26) && unsafe { CGEventGetIntegerValueField(event, 3) } == i64::from(b.mac)
        } else {
            matches!(kind, 10..=12) && unsafe { CGEventGetIntegerValueField(event, 9) } == i64::from(b.mac)
        };
        if matches {
            let down = if b.mouse {
                kind == 25
            } else if kind == 12 {
                // NX_DEVICE*KEYMASK values from IOKit/hidsystem/IOLLEvent.h.
                // Aggregate flags remain set while the opposite side is held.
                let mask = match b.mac {
                    54 => 0x0010,
                    55 => 0x0008,
                    56 => 0x0002,
                    60 => 0x0004,
                    58 => 0x0020,
                    61 => 0x0040,
                    59 => 0x0001,
                    62 => 0x2000,
                    _ => 0,
                };
                unsafe { CGEventGetFlags(event) & mask != 0 }
            } else {
                kind == 10
            };
            context.signal.down.store(down, Ordering::Release);
        }
    }
    event // listen-only also makes suppression impossible, regardless of this return value
}

/// Called only by the settings button. Register the app with TCC once, then let
/// the user manage the grant in System Settings; background retries never prompt.
pub fn request_from_user() {
    static REQUESTED: AtomicBool = AtomicBool::new(false);
    // SAFETY: no pointer arguments; this path runs only after the settings button click.
    unsafe {
        if !CGPreflightListenEventAccess() && !REQUESTED.swap(true, Ordering::AcqRel) {
            CGRequestListenEventAccess();
        }
    }
}

impl Observer {
    pub fn open(binding: Binding, signal: Arc<Signal>) -> Result<Self, Status> {
        // macOS reports Caps Lock's toggled state, not a reliable physical hold/release.
        if !binding.mouse && matches!(binding.mac, 57 | u16::MAX) {
            return Err(Status::UnsupportedKey);
        }
        // SAFETY: OS framework calls; no prompting APIs are used here.
        unsafe {
            if !CGPreflightListenEventAccess() {
                return Err(Status::Permission);
            }
            let mut context = Box::new(Context { binding, signal });
            let mask = if binding.mouse { (1 << 25) | (1 << 26) } else { (1 << 10) | (1 << 11) | (1 << 12) };
            let tap = CGEventTapCreate(1, 0, 1, mask, callback, (&mut *context as *mut Context).cast()); // session, head, listenOnly
            if tap.is_null() {
                return Err(Status::Recovering);
            }
            let source = CFMachPortCreateRunLoopSource(ptr::null_mut(), tap, 0);
            if source.is_null() {
                CFMachPortInvalidate(tap);
                CFRelease(tap);
                return Err(Status::Recovering);
            }
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
            Ok(Self { tap, source, context })
        }
    }

    pub fn pump(&mut self) {
        // SAFETY: source and context remain alive while the run loop invokes the callback.
        unsafe {
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.001, false);
        }
    }

    pub fn check_health(&self) -> bool {
        // SAFETY: queries this tap and its permission only, never input contents.
        unsafe { CGPreflightListenEventAccess() && CGEventTapIsEnabled(self.tap) }
    }

    pub fn physical_down(&self) -> Option<bool> {
        // SAFETY: only the bound key/button and session availability are queried.
        unsafe {
            if !CGPreflightListenEventAccess() || !CGEventTapIsEnabled(self.tap) || !session_active() {
                return None;
            }
            let b = self.context.binding;
            Some(if b.mouse { CGEventSourceButtonState(1, u32::from(b.mac)) } else { CGEventSourceKeyState(1, b.mac) })
        }
    }
}

unsafe fn session_active() -> bool {
    // A locked/off-console session must close PTT, even if the key remains physically held.
    unsafe {
        let session = CGSessionCopyCurrentDictionary();
        if session.is_null() {
            return false;
        }
        let on_console = CFStringCreateWithCString(ptr::null_mut(), c"kCGSSessionOnConsoleKey".as_ptr(), 0x08000100);
        let locked = CFStringCreateWithCString(ptr::null_mut(), c"CGSSessionScreenIsLocked".as_ptr(), 0x08000100);
        let console_value = CFDictionaryGetValue(session, on_console);
        let lock_value = CFDictionaryGetValue(session, locked);
        let active = !console_value.is_null()
            && CFBooleanGetValue(console_value)
            && (lock_value.is_null() || !CFBooleanGetValue(lock_value));
        CFRelease(on_console);
        CFRelease(locked);
        CFRelease(session);
        active
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        // SAFETY: remove the callback source before releasing its context.
        unsafe {
            CGEventTapEnable(self.tap, false);
            CFRunLoopRemoveSource(CFRunLoopGetCurrent(), self.source, kCFRunLoopDefaultMode);
            CFMachPortInvalidate(self.tap);
            CFRelease(self.source);
            CFRelease(self.tap);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ptt::Safety;
    use std::time::{Duration, SystemTime};
    #[repr(C)]
    struct Point {
        x: f64,
        y: f64,
    }
    unsafe extern "C" {
        fn CGEventCreateKeyboardEvent(source: Ref, key: u16, down: bool) -> Ref;
        fn CGEventSetFlags(event: Ref, flags: u64);
        fn CGEventCreateMouseEvent(source: Ref, kind: u32, point: Point, button: u32) -> Ref;
    }

    fn modifier_overlap(binding: &str, other: &str, aggregate: u64, bound_flag: u64, other_flag: u64) {
        let signal = Arc::new(Signal::default());
        let mut context = Context { binding: Binding::parse(binding).unwrap(), signal: signal.clone() };
        let info = (&mut context as *mut Context).cast();
        let bound = context.binding.mac;
        let other = Binding::parse(other).unwrap().mac;
        let both = aggregate | bound_flag | other_flag;
        let start = SystemTime::now();
        let mut safety = Safety::default();
        safety.prove_health(start);
        assert!(!safety.sample(start, false, false, true));
        // Real flagsChanged payloads retain the aggregate bit until both sides
        // are released. No event is posted and no tap or OS permission is needed.
        for (step, (key, flags, physical)) in [
            (bound, aggregate | bound_flag, true),
            (other, both, true),
            (bound, aggregate | other_flag, false),
            (bound, both, true),
            (other, aggregate | bound_flag, true),
            (bound, 0, false),
            (other, aggregate | other_flag, false),
            (bound, both, true),
            (bound, aggregate | other_flag, false),
            (other, 0, false),
        ]
        .into_iter()
        .enumerate()
        {
            unsafe {
                let event = CGEventCreateKeyboardEvent(ptr::null_mut(), key, true);
                assert!(!event.is_null());
                CGEventSetFlags(event, flags);
                assert_eq!(callback(ptr::null_mut(), 12, event, info), event);
                CFRelease(event);
            }
            let observed = signal.down.load(Ordering::Acquire);
            assert_eq!(observed, physical, "{binding}, step {step}: bound side must match its physical state");
            assert!(signal.healthy.load(Ordering::Acquire));
            let now = start + Duration::from_millis((step as u64 + 1) * 20);
            assert_eq!(safety.sample(now, physical, observed, true), physical);
            assert!(!safety.reset, "{binding}, step {step}: overlapping modifiers must not trigger recovery");
        }
    }

    macro_rules! overlapping_modifiers {
        ($($name:ident: ($binding:literal, $other:literal, $aggregate:expr, $bound:expr, $peer:expr)),+ $(,)?) => {
            $(#[test]
            fn $name() {
                modifier_overlap($binding, $other, $aggregate, $bound, $peer);
            })+
        };
    }
    // Device-dependent NX_DEVICE*KEYMASK values from IOKit/hidsystem/IOLLEvent.h.
    overlapping_modifiers! {
        overlapping_shift_left: ("ShiftLeft", "ShiftRight", 1 << 17, 0x0002, 0x0004),
        overlapping_shift_right: ("ShiftRight", "ShiftLeft", 1 << 17, 0x0004, 0x0002),
        overlapping_control_left: ("ControlLeft", "ControlRight", 1 << 18, 0x0001, 0x2000),
        overlapping_control_right: ("ControlRight", "ControlLeft", 1 << 18, 0x2000, 0x0001),
        overlapping_alt_left: ("AltLeft", "AltRight", 1 << 19, 0x0020, 0x0040),
        overlapping_alt_right: ("AltRight", "AltLeft", 1 << 19, 0x0040, 0x0020),
        overlapping_meta_left: ("MetaLeft", "MetaRight", 1 << 20, 0x0008, 0x0010),
        overlapping_meta_right: ("MetaRight", "MetaLeft", 1 << 20, 0x0010, 0x0008),
    }

    #[test]
    fn callback_passes_events_through_and_ignores_every_unbound_key() {
        let signal = Arc::new(Signal::default());
        let mut context = Context { binding: Binding::parse("KeyV").unwrap(), signal: signal.clone() };
        let info = (&mut context as *mut Context).cast();
        // Unposted synthetic events: no input permission, physical input, or app launch.
        unsafe {
            for (key, kind, down, expected) in
                [(0, 10, true, false), (9, 10, true, true), (0, 11, false, true), (9, 11, false, false)]
            {
                let event = CGEventCreateKeyboardEvent(ptr::null_mut(), key, down);
                assert!(!event.is_null());
                assert_eq!(callback(ptr::null_mut(), kind, event, info), event);
                assert_eq!(signal.down.load(Ordering::Acquire), expected);
                CFRelease(event);
            }
            callback(ptr::null_mut(), 0xffff_fffe, ptr::null_mut(), info);
            assert!(!signal.healthy.load(Ordering::Acquire));
        }
    }

    #[test]
    fn lone_right_alt_and_side_buttons_match_without_character_decoding() {
        let signal = Arc::new(Signal::default());
        let mut context = Context { binding: Binding::parse("AltRight").unwrap(), signal: signal.clone() };
        let info = (&mut context as *mut Context).cast();
        unsafe {
            let event = CGEventCreateKeyboardEvent(ptr::null_mut(), 61, true);
            assert!(!event.is_null());
            CGEventSetFlags(event, (1 << 19) | 0x0040);
            assert_eq!(callback(ptr::null_mut(), 12, event, info), event);
            assert!(signal.down.load(Ordering::Acquire));
            CGEventSetFlags(event, 0);
            callback(ptr::null_mut(), 12, event, info);
            assert!(!signal.down.load(Ordering::Acquire));
            CFRelease(event);
            context.binding = Binding::parse("Mouse4").unwrap();
            for (button, kind, expected) in [(4, 25, false), (3, 25, true), (4, 26, true), (3, 26, false)] {
                let event = CGEventCreateMouseEvent(ptr::null_mut(), kind, Point { x: 0.0, y: 0.0 }, button);
                assert!(!event.is_null());
                assert_eq!(callback(ptr::null_mut(), kind, event, info), event);
                assert_eq!(signal.down.load(Ordering::Acquire), expected);
                CFRelease(event);
            }
        }
    }
}
