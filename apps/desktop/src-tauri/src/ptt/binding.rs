//! Physical DOM codes mapped to OS codes. Unknown saved codes remain saved in the UI.
#[derive(Clone, Copy)]
pub struct Binding {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub mac: u16,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub scan: u32,
    pub mouse: bool,
}

impl Binding {
    pub fn parse(code: &str) -> Option<Self> {
        if let Some(button) =
            code.strip_prefix("Mouse").and_then(|s| s.parse::<u16>().ok()).filter(|b| (3..=5).contains(b))
        {
            return Some(Self { mac: button - 1, scan: u32::from(button - 1), mouse: true });
        }
        let (mac, scan) = match code {
            "KeyA" => (0, 0x1e),
            "KeyS" => (1, 0x1f),
            "KeyD" => (2, 0x20),
            "KeyF" => (3, 0x21),
            "KeyH" => (4, 0x23),
            "KeyG" => (5, 0x22),
            "KeyZ" => (6, 0x2c),
            "KeyX" => (7, 0x2d),
            "KeyC" => (8, 0x2e),
            "KeyV" => (9, 0x2f),
            "KeyB" => (11, 0x30),
            "KeyQ" => (12, 0x10),
            "KeyW" => (13, 0x11),
            "KeyE" => (14, 0x12),
            "KeyR" => (15, 0x13),
            "KeyY" => (16, 0x15),
            "KeyT" => (17, 0x14),
            "Digit1" => (18, 2),
            "Digit2" => (19, 3),
            "Digit3" => (20, 4),
            "Digit4" => (21, 5),
            "Digit6" => (22, 7),
            "Digit5" => (23, 6),
            "Equal" => (24, 0x0d),
            "Digit9" => (25, 0x0a),
            "Digit7" => (26, 8),
            "Minus" => (27, 0x0c),
            "Digit8" => (28, 9),
            "Digit0" => (29, 0x0b),
            "BracketRight" => (30, 0x1b),
            "KeyO" => (31, 0x18),
            "KeyU" => (32, 0x16),
            "BracketLeft" => (33, 0x1a),
            "KeyI" => (34, 0x17),
            "KeyP" => (35, 0x19),
            "Enter" => (36, 0x1c),
            "KeyL" => (37, 0x26),
            "KeyJ" => (38, 0x24),
            "Quote" => (39, 0x28),
            "KeyK" => (40, 0x25),
            "Semicolon" => (41, 0x27),
            "Backslash" => (42, 0x2b),
            "Comma" => (43, 0x33),
            "Slash" => (44, 0x35),
            "KeyN" => (45, 0x31),
            "KeyM" => (46, 0x32),
            "Period" => (47, 0x34),
            "Tab" => (48, 0x0f),
            "Space" => (49, 0x39),
            "Backquote" => (50, 0x29),
            "Backspace" => (51, 0x0e),
            "Escape" => (53, 1),
            "MetaRight" => (54, 0xe05c),
            "MetaLeft" => (55, 0xe05b),
            "ShiftLeft" => (56, 0x2a),
            "CapsLock" => (57, 0x3a),
            "AltLeft" => (58, 0x38),
            "ControlLeft" => (59, 0x1d),
            "ShiftRight" => (60, 0x36),
            "AltRight" => (61, 0xe038),
            "ControlRight" => (62, 0xe01d),
            "F17" => (64, 0x68),
            "F18" => (79, 0x69),
            "F19" => (80, 0x6a),
            "F20" => (90, 0x6b),
            "F21" => (u16::MAX, 0x6c),
            "F22" => (u16::MAX, 0x6d),
            "F23" => (u16::MAX, 0x6e),
            "F24" => (u16::MAX, 0x6f),
            "Insert" | "Help" => (114, 0xe052),
            "NumpadDecimal" => (65, 0x53),
            "NumpadMultiply" => (67, 0x37),
            "NumpadAdd" => (69, 0x4e),
            "NumLock" => (71, 0xe045),
            "NumpadDivide" => (75, 0xe035),
            "NumpadEnter" => (76, 0xe01c),
            "NumpadSubtract" => (78, 0x4a),
            "Numpad0" => (82, 0x52),
            "Numpad1" => (83, 0x4f),
            "Numpad2" => (84, 0x50),
            "Numpad3" => (85, 0x51),
            "Numpad4" => (86, 0x4b),
            "Numpad5" => (87, 0x4c),
            "Numpad6" => (88, 0x4d),
            "Numpad7" => (89, 0x47),
            "Numpad8" => (91, 0x48),
            "Numpad9" => (92, 0x49),
            "F5" => (96, 0x3f),
            "F6" => (97, 0x40),
            "F7" => (98, 0x41),
            "F3" => (99, 0x3d),
            "F8" => (100, 0x42),
            "F9" => (101, 0x43),
            "F11" => (103, 0x57),
            "F13" => (105, 0x64),
            "F16" => (106, 0x67),
            "F14" => (107, 0x65),
            "F10" => (109, 0x44),
            "F12" => (111, 0x58),
            "F15" => (113, 0x66),
            "Home" => (115, 0xe047),
            "PageUp" => (116, 0xe049),
            "Delete" => (117, 0xe053),
            "F4" => (118, 0x3e),
            "End" => (119, 0xe04f),
            "F2" => (120, 0x3c),
            "PageDown" => (121, 0xe051),
            "F1" => (122, 0x3b),
            "ArrowLeft" => (123, 0xe04b),
            "ArrowRight" => (124, 0xe04d),
            "ArrowDown" => (125, 0xe050),
            "ArrowUp" => (126, 0xe048),
            "IntlBackslash" => (10, 0x56),
            _ => return None,
        };
        Some(Self { mac, scan, mouse: false })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keys_modifiers_and_mouse_buttons_keep_their_sides() {
        for key in ["KeyV", "AltRight", "ControlLeft", "CapsLock", "Mouse4", "Mouse5", "F20", "F24"] {
            assert!(Binding::parse(key).is_some());
        }
        assert_ne!(Binding::parse("AltLeft").unwrap().scan, Binding::parse("AltRight").unwrap().scan);
        assert!(Binding::parse("Mouse4").unwrap().mouse);
        assert!(Binding::parse("Unknown").is_none());
    }
}
