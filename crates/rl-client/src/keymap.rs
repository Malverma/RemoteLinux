//! Fallback mapping from SDL scancodes (USB HID usages) to Linux evdev key
//! codes. Normally the platform's raw keycode is forwarded as-is; this is only
//! used when SDL doesn't report one.

pub fn hid_to_evdev(hid: i32) -> Option<u16> {
    const LETTERS: [u16; 26] = [30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44];
    Some(match hid {
        4..=29 => LETTERS[(hid - 4) as usize],
        30..=38 => (hid - 28) as u16, // 1..9 -> KEY_1(2)..KEY_9(10)
        39 => 11,                     // 0
        40 => 28,                     // Enter
        41 => 1,                      // Esc
        42 => 14,                     // Backspace
        43 => 15,                     // Tab
        44 => 57,                     // Space
        45 => 12,
        46 => 13,
        47 => 26,
        48 => 27,
        49 | 50 => 43,
        51 => 39,
        52 => 40,
        53 => 41,
        54 => 51,
        55 => 52,
        56 => 53,
        57 => 58, // CapsLock
        58..=67 => (hid + 1) as u16, // F1..F10 -> 59..68
        68 => 87,                    // F11
        69 => 88,                    // F12
        70 => 99,                    // PrintScreen
        71 => 70,                    // ScrollLock
        72 => 119,                   // Pause
        73 => 110,                   // Insert
        74 => 102,                   // Home
        75 => 104,                   // PageUp
        76 => 111,                   // Delete
        77 => 107,                   // End
        78 => 109,                   // PageDown
        79 => 106,                   // Right
        80 => 105,                   // Left
        81 => 108,                   // Down
        82 => 103,                   // Up
        83 => 69,                    // NumLock
        84 => 98,
        85 => 55,
        86 => 74,
        87 => 78,
        88 => 96, // KP Enter
        89 => 79,
        90 => 80,
        91 => 81,
        92 => 75,
        93 => 76,
        94 => 77,
        95 => 71,
        96 => 72,
        97 => 73,
        98 => 82,
        99 => 83,
        100 => 86,  // 102nd
        101 => 127, // Menu/Compose
        224 => 29,  // LCtrl
        225 => 42,  // LShift
        226 => 56,  // LAlt
        227 => 125, // LMeta
        228 => 97,  // RCtrl
        229 => 54,  // RShift
        230 => 100, // RAlt
        231 => 126, // RMeta
        _ => return None,
    })
}

/// SDL mouse button number to evdev button code.
pub fn mouse_button(b: u8) -> Option<u16> {
    Some(match b {
        1 => 0x110, // BTN_LEFT
        2 => 0x112, // BTN_MIDDLE
        3 => 0x111, // BTN_RIGHT
        4 => 0x113, // BTN_SIDE
        5 => 0x114, // BTN_EXTRA
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn common_keys() {
        use super::hid_to_evdev as m;
        assert_eq!(m(4), Some(30)); // A
        assert_eq!(m(29), Some(44)); // Z
        assert_eq!(m(30), Some(2)); // 1
        assert_eq!(m(39), Some(11)); // 0
        assert_eq!(m(58), Some(59)); // F1
        assert_eq!(m(67), Some(68)); // F10
        assert_eq!(m(227), Some(125)); // Super
    }
}
