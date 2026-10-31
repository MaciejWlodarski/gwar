//! Windows packet matching, also tested on non-Windows hosts. Nothing unbound is retained.
#![cfg_attr(not(windows), allow(dead_code))]
use super::binding::Binding;

pub const INPUT_SINK: u32 = 0x100;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Device {
    pub page: u16,
    pub usage: u16,
    pub flags: u32,
    pub window: usize,
}
impl Device {
    pub fn owned_by(&self, window: usize, usage: u16) -> bool {
        self.page == 1 && self.usage == usage && self.flags == INPUT_SINK && self.window == window
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Header {
    pub kind: u32,
    pub size: u32,
    pub device: usize,
    pub parameter: usize,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Keyboard {
    pub scan: u16,
    pub flags: u16,
    pub reserved: u16,
    pub virtual_key: u16,
    pub message: u32,
    pub extra: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Mouse {
    pub flags: u16,
    pub padding: u16,
    pub buttons: u16,
    pub button_data: u16,
    pub raw_buttons: u32,
    pub x: i32,
    pub y: i32,
    pub extra: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub union Data {
    pub keyboard: Keyboard,
    pub mouse: Mouse,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Packet {
    pub header: Header,
    pub data: Data,
}

impl Packet {
    pub fn bound_state(&self, binding: Binding) -> Option<bool> {
        if !binding.mouse && self.header.kind == 1 {
            // SAFETY: kind 1 selects the keyboard member of the OS packet union.
            let (code, flags) = unsafe { (self.data.keyboard.scan, self.data.keyboard.flags) };
            let scan = u32::from(code) | if flags & 2 != 0 { 0xe000 } else { 0 };
            if flags & 4 != 0 || scan != binding.scan {
                return None;
            }
            return Some(flags & 1 == 0);
        }
        if binding.mouse && self.header.kind == 0 {
            // SAFETY: kind 0 selects mouse data. Positions, wheel and other buttons are ignored.
            let buttons = unsafe { self.data.mouse.buttons };
            let mask = match binding.scan {
                2 => 0x10,
                3 => 0x40,
                4 => 0x100,
                _ => return None,
            };
            if buttons & (mask << 1) != 0 {
                return Some(false);
            }
            if buttons & mask != 0 {
                return Some(true);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_the_bound_physical_key_and_its_break_state_are_used() {
        for (name, scan, flags) in [("KeyV", 0x2f, 0), ("AltRight", 0x38, 2), ("CapsLock", 0x3a, 0)] {
            let key = Binding::parse(name).unwrap();
            let mut packet = Packet {
                header: Header { kind: 1, ..Header::default() },
                data: Data { keyboard: Keyboard { scan, flags, ..Keyboard::default() } },
            };
            assert_eq!(packet.bound_state(key), Some(true));
            packet.data.keyboard.flags = flags | 1;
            assert_eq!(packet.bound_state(key), Some(false));
            assert_eq!(packet.bound_state(Binding::parse("KeyA").unwrap()), None);
            assert_eq!(packet.bound_state(Binding::parse("Mouse4").unwrap()), None);
        }
    }

    #[test]
    fn side_buttons_ignore_movement_and_other_buttons_and_release_wins() {
        let key = Binding::parse("Mouse4").unwrap();
        for (buttons, state) in
            [(0, None), (1, None), (0x100, None), (0x40, Some(true)), (0x80, Some(false)), (0xc0, Some(false))]
        {
            let packet = Packet {
                header: Header::default(),
                data: Data { mouse: Mouse { buttons, x: 123, y: -123, ..Mouse::default() } },
            };
            assert_eq!(packet.bound_state(key), state);
        }
    }

    #[test]
    fn a_health_proof_requires_our_passive_registration_not_another_receiver() {
        let registration = Device { page: 1, usage: 6, flags: INPUT_SINK, window: 42 };
        assert!(registration.owned_by(42, 6));
        assert!(!registration.owned_by(43, 6));
        assert!(!registration.owned_by(42, 2));
        assert!(!Device { flags: INPUT_SINK | 0x30, ..registration }.owned_by(42, 6));
    }

    #[test]
    fn windows_packet_layouts_have_the_documented_sizes() {
        assert_eq!(std::mem::size_of::<Keyboard>(), 16);
        assert_eq!(std::mem::size_of::<Mouse>(), 24);
        assert_eq!(std::mem::size_of::<Header>(), 8 + 2 * std::mem::size_of::<usize>());
        assert_eq!(std::mem::size_of::<Packet>(), std::mem::size_of::<Header>() + 24);
    }
}
