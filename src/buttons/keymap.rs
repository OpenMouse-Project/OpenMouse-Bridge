//! HID keyboard and consumer usages as the key codes of Linux (evdev) and macOS
//! (virtual keycodes), for the keys a profile can hold. `buttons::virtual_key`
//! is the same list for Windows, so the three cover the same keys.
//!
//! Plain functions with no system calls, so they are tested on every platform
//! even though each is only used on one.
#![allow(dead_code)]

/// Linux `KEY_A` to `KEY_Z`, in HID order (usage 0x04 is A).
const LINUX_LETTERS: [u16; 26] = [
    30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45,
    21, 44,
];

/// macOS `kVK_ANSI_A` to `kVK_ANSI_Z`, in HID order.
const MAC_LETTERS: [u16; 26] = [
    0, 11, 8, 2, 14, 3, 5, 4, 34, 38, 40, 37, 46, 45, 31, 35, 12, 15, 1, 17, 32, 9, 13, 7, 16, 6,
];

/// macOS keycodes of the digits 1 to 9 and 0, in HID order (usage 0x1e is 1).
const MAC_DIGITS: [u16; 10] = [18, 19, 20, 21, 23, 22, 26, 28, 25, 29];

/// The Linux `KEY_*` code for a HID keyboard usage.
pub fn linux_key(usage: u8) -> Option<u16> {
    Some(match usage {
        0x04..=0x1d => LINUX_LETTERS[usize::from(usage - 0x04)],
        0x1e..=0x27 => 2 + u16::from(usage - 0x1e), // 1-9, then 0 (KEY_0 is 11)
        0x28 => 28,                                 // Enter
        0x29 => 1,                                  // Escape
        0x2a => 14,                                 // Backspace
        0x2b => 15,                                 // Tab
        0x2c => 57,                                 // Space
        0x2d => 12,                                 // -
        0x2e => 13,                                 // =
        0x2f => 26,                                 // [
        0x30 => 27,                                 // ]
        0x31 => 43,                                 // \
        0x33 => 39,                                 // ;
        0x34 => 40,                                 // '
        0x35 => 41,                                 // `
        0x36 => 51,                                 // ,
        0x37 => 52,                                 // .
        0x38 => 53,                                 // /
        0x3a..=0x43 => 59 + u16::from(usage - 0x3a), // F1-F10
        0x44 => 87,                                 // F11
        0x45 => 88,                                 // F12
        0x46 => 99,                                 // Print Screen (KEY_SYSRQ)
        0x47 => 70,                                 // Scroll Lock
        0x48 => 119,                                // Pause
        0x49 => 110,                                // Insert
        0x4a => 102,                                // Home
        0x4b => 104,                                // Page Up
        0x4c => 111,                                // Delete
        0x4d => 107,                                // End
        0x4e => 109,                                // Page Down
        0x4f => 106,                                // Right
        0x50 => 105,                                // Left
        0x51 => 108,                                // Down
        0x52 => 103,                                // Up
        0x68..=0x73 => 183 + u16::from(usage - 0x68), // F13-F24
        _ => return None,
    })
}

/// The Linux `KEY_*` code for a HID consumer usage (media key).
pub fn linux_media_key(usage: u16) -> Option<u16> {
    Some(match usage {
        0xe2 => 113, // Mute
        0xe9 => 115, // Volume up
        0xea => 114, // Volume down
        0xcd => 164, // Play / pause
        0xb5 => 163, // Next track
        0xb6 => 165, // Previous track
        0xb7 => 166, // Stop
        _ => return None,
    })
}

/// The Linux modifier keys a mask names (ctrl, shift, alt, meta), in the order they are pressed.
pub fn linux_modifier_keys(modifiers: u8) -> Vec<u16> {
    modifier_codes(modifiers, [29, 42, 56, 125])
}

/// The macOS virtual keycode for a HID keyboard usage. Print Screen, Scroll
/// Lock, Pause and F21 to F24 have no key on a Mac.
pub fn mac_key(usage: u8) -> Option<u16> {
    Some(match usage {
        0x04..=0x1d => MAC_LETTERS[usize::from(usage - 0x04)],
        0x1e..=0x27 => MAC_DIGITS[usize::from(usage - 0x1e)],
        0x28 => 36,  // Return
        0x29 => 53,  // Escape
        0x2a => 51,  // Delete (backspace)
        0x2b => 48,  // Tab
        0x2c => 49,  // Space
        0x2d => 27,  // -
        0x2e => 24,  // =
        0x2f => 33,  // [
        0x30 => 30,  // ]
        0x31 => 42,  // \
        0x33 => 41,  // ;
        0x34 => 39,  // '
        0x35 => 50,  // `
        0x36 => 43,  // ,
        0x37 => 47,  // .
        0x38 => 44,  // /
        0x3a => 122, // F1
        0x3b => 120,
        0x3c => 99,
        0x3d => 118,
        0x3e => 96,
        0x3f => 97,
        0x40 => 98,
        0x41 => 100,
        0x42 => 101,
        0x43 => 109, // F10
        0x44 => 103, // F11
        0x45 => 111, // F12
        0x49 => 114, // Insert (kVK_Help)
        0x4a => 115, // Home
        0x4b => 116, // Page Up
        0x4c => 117, // Forward delete
        0x4d => 119, // End
        0x4e => 121, // Page Down
        0x4f => 124, // Right
        0x50 => 123, // Left
        0x51 => 125, // Down
        0x52 => 126, // Up
        0x68 => 105, // F13
        0x69 => 107, // F14
        0x6a => 113, // F15
        0x6b => 106, // F16
        0x6c => 64,  // F17
        0x6d => 79,  // F18
        0x6e => 80,  // F19
        0x6f => 90,  // F20
        _ => return None,
    })
}

/// The macOS modifier keys a mask names (control, shift, option, command), in the order they are pressed.
pub fn mac_modifier_keys(modifiers: u8) -> Vec<u16> {
    modifier_codes(modifiers, [59, 56, 58, 55])
}

fn modifier_codes(modifiers: u8, codes: [u16; 4]) -> Vec<u16> {
    codes
        .into_iter()
        .enumerate()
        .filter(|(bit, _)| modifiers & (1 << bit) != 0)
        .map(|(_, code)| code)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buttons::virtual_key;

    #[test]
    fn linux_keys_follow_the_evdev_codes() {
        assert_eq!(linux_key(0x04), Some(30), "A");
        assert_eq!(linux_key(0x1d), Some(44), "Z");
        assert_eq!(linux_key(0x10), Some(50), "M");
        assert_eq!(linux_key(0x1e), Some(2), "1");
        assert_eq!(linux_key(0x26), Some(10), "9");
        assert_eq!(linux_key(0x27), Some(11), "0");
        assert_eq!(linux_key(0x3a), Some(59), "F1");
        assert_eq!(linux_key(0x43), Some(68), "F10");
        assert_eq!(linux_key(0x44), Some(87), "F11");
        assert_eq!(linux_key(0x68), Some(183), "F13");
        assert_eq!(linux_key(0x73), Some(194), "F24");
        assert_eq!(linux_key(0x2c), Some(57), "Space");
        assert_eq!(linux_key(0x52), Some(103), "Up");
        assert_eq!(linux_key(0x01), None);
        assert_eq!(linux_media_key(0xe9), Some(115), "volume up");
        assert_eq!(linux_media_key(0x01), None);
        assert_eq!(linux_modifier_keys(0b0101), [29, 56], "ctrl and alt");
        assert_eq!(linux_modifier_keys(0b1010), [42, 125], "shift and meta");
    }

    #[test]
    fn mac_keys_follow_the_virtual_keycodes() {
        assert_eq!(mac_key(0x04), Some(0), "A");
        assert_eq!(mac_key(0x1d), Some(6), "Z");
        assert_eq!(mac_key(0x0b), Some(4), "H");
        assert_eq!(mac_key(0x1e), Some(18), "1");
        assert_eq!(mac_key(0x22), Some(23), "5");
        assert_eq!(mac_key(0x27), Some(29), "0");
        assert_eq!(mac_key(0x3a), Some(122), "F1");
        assert_eq!(mac_key(0x45), Some(111), "F12");
        assert_eq!(mac_key(0x68), Some(105), "F13");
        assert_eq!(mac_key(0x2c), Some(49), "Space");
        assert_eq!(mac_key(0x52), Some(126), "Up");
        assert_eq!(mac_key(0x46), None, "Print Screen has no Mac key");
        assert_eq!(mac_modifier_keys(0b1001), [59, 55], "control and command");
        assert_eq!(mac_modifier_keys(0), Vec::<u16>::new());
    }

    #[test]
    fn linux_covers_every_key_windows_does() {
        for usage in 0..=u8::MAX {
            assert_eq!(
                linux_key(usage).is_some(),
                virtual_key(usage).is_some(),
                "usage {usage:#x}"
            );
        }
    }

    #[test]
    fn keys_do_not_share_a_code() {
        let mut linux: Vec<u16> = (0..=u8::MAX).filter_map(linux_key).collect();
        let mut mac: Vec<u16> = (0..=u8::MAX).filter_map(mac_key).collect();
        for codes in [&mut linux, &mut mac] {
            let before = codes.len();
            codes.sort_unstable();
            codes.dedup();
            assert_eq!(codes.len(), before, "two usages map to one key code");
        }
    }
}
