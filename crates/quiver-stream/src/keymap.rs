//! SDL scancodes to Linux evdev key codes (`KEY_*`), by position.
//!
//! SDL's scancodes 0–231 are USB HID usage ids (page 7); the table below is the kernel's own
//! HID to evdev table (`hid_keyboard[]`, drivers/hid/hid-input.c). The media and app-control
//! keys SDL numbers from 257 up are matched by name.
//!
//! Keys that type no text go by their symbol instead ([`evdev_for_key`]): an XKB option such
//! as `caps:swapescape` (the person's Hyprland `kb_options`) changes what the Caps Lock key
//! *means* in the keymap, not the key code Wayland or X11 delivers, so a purely positional
//! map would lose it. SDL resolves the symbol from the same keymap the compositor uses.

use sdl3_sys::keycode::*;
use sdl3_sys::scancode::*;

pub const KEY_ESC: u16 = 1;
pub const KEY_BACKSPACE: u16 = 14;
pub const KEY_TAB: u16 = 15;
pub const KEY_Q: u16 = 16;
pub const KEY_ENTER: u16 = 28;
pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_S: u16 = 31;
pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_Z: u16 = 44;
pub const KEY_RIGHTSHIFT: u16 = 54;
pub const KEY_LEFTALT: u16 = 56;
pub const KEY_CAPSLOCK: u16 = 58;
pub const KEY_RIGHTCTRL: u16 = 97;
pub const KEY_RIGHTALT: u16 = 100;
pub const KEY_UP: u16 = 103;
pub const KEY_LEFT: u16 = 105;
pub const KEY_RIGHT: u16 = 106;
pub const KEY_DOWN: u16 = 108;
pub const KEY_LEFTMETA: u16 = 125;
pub const KEY_RIGHTMETA: u16 = 126;

const UNK: u8 = 0;

/// HID usage (= SDL scancode, 0..=255) to evdev, from the kernel. 0 is no key.
#[rustfmt::skip]
static HID_KEYBOARD: [u8; 256] = [
      0,  0,  0,  0, 30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38,
     50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44,  2,  3,
      4,  5,  6,  7,  8,  9, 10, 11, 28,  1, 14, 15, 57, 12, 13, 26,
     27, 43, 43, 39, 40, 41, 51, 52, 53, 58, 59, 60, 61, 62, 63, 64,
     65, 66, 67, 68, 87, 88, 99, 70,119,110,102,104,111,107,109,106,
    105,108,103, 69, 98, 55, 74, 78, 96, 79, 80, 81, 75, 76, 77, 71,
     72, 73, 82, 83, 86,127,116,117,183,184,185,186,187,188,189,190,
    191,192,193,194,134,138,130,132,128,129,131,137,133,135,136,113,
    115,114,UNK,UNK,UNK,121,UNK, 89, 93,124, 92, 94, 95,UNK,UNK,UNK,
    122,123, 90, 91, 85,UNK,UNK,UNK,UNK,UNK,UNK,UNK,111,UNK,UNK,UNK,
    UNK,UNK,UNK,UNK,UNK,UNK,UNK,UNK,UNK,UNK,UNK,UNK,UNK,UNK,UNK,UNK,
    UNK,UNK,UNK,UNK,UNK,UNK,179,180,UNK,UNK,UNK,UNK,UNK,UNK,UNK,UNK,
    UNK,UNK,UNK,UNK,UNK,UNK,UNK,UNK,UNK,UNK,UNK,UNK,UNK,UNK,UNK,UNK,
    UNK,UNK,UNK,UNK,UNK,UNK,UNK,UNK,111,UNK,UNK,UNK,UNK,UNK,UNK,UNK,
     29, 42, 56,125, 97, 54,100,126,164,166,165,163,161,115,114,113,
    150,158,159,128,136,177,178,176,142,152,173,140,UNK,UNK,UNK,UNK,
];

/// The evdev code of the key at this position, if it has one.
pub fn evdev_for_scancode(sc: SDL_Scancode) -> Option<u16> {
    let n = sc.0;
    if (0..=231).contains(&n) {
        let code = HID_KEYBOARD[n as usize];
        return (code != 0).then_some(code as u16);
    }
    let code = match sc {
        SDL_SCANCODE_MODE => 100, // AltGr-like mode switch: KEY_RIGHTALT
        SDL_SCANCODE_SLEEP => 142,
        SDL_SCANCODE_WAKE => 143,
        SDL_SCANCODE_CHANNEL_INCREMENT => 402,
        SDL_SCANCODE_CHANNEL_DECREMENT => 403,
        SDL_SCANCODE_MEDIA_PLAY => 207,
        SDL_SCANCODE_MEDIA_PAUSE => 201,
        SDL_SCANCODE_MEDIA_RECORD => 167,
        SDL_SCANCODE_MEDIA_FAST_FORWARD => 208,
        SDL_SCANCODE_MEDIA_REWIND => 168,
        SDL_SCANCODE_MEDIA_NEXT_TRACK => 163,
        SDL_SCANCODE_MEDIA_PREVIOUS_TRACK => 165,
        SDL_SCANCODE_MEDIA_STOP => 166,
        SDL_SCANCODE_MEDIA_EJECT => 161,
        SDL_SCANCODE_MEDIA_PLAY_PAUSE => 164,
        SDL_SCANCODE_MEDIA_SELECT => 226,
        SDL_SCANCODE_AC_NEW => 181,
        SDL_SCANCODE_AC_OPEN => 134,
        SDL_SCANCODE_AC_CLOSE => 206,
        SDL_SCANCODE_AC_EXIT => 174,
        SDL_SCANCODE_AC_SAVE => 234,
        SDL_SCANCODE_AC_PRINT => 210,
        SDL_SCANCODE_AC_PROPERTIES => 130,
        SDL_SCANCODE_AC_SEARCH => 217,
        SDL_SCANCODE_AC_HOME => 172,
        SDL_SCANCODE_AC_BACK => 158,
        SDL_SCANCODE_AC_FORWARD => 159,
        SDL_SCANCODE_AC_STOP => 128,
        SDL_SCANCODE_AC_REFRESH => 173,
        SDL_SCANCODE_AC_BOOKMARKS => 156,
        _ => return None,
    };
    Some(code)
}

/// The evdev code for a key that types no text, by its symbol; `None` for every other symbol.
fn evdev_for_symbol(key: SDL_Keycode) -> Option<u16> {
    Some(match key {
        SDLK_ESCAPE => KEY_ESC,
        SDLK_CAPSLOCK => KEY_CAPSLOCK,
        SDLK_LCTRL => KEY_LEFTCTRL,
        SDLK_RCTRL => KEY_RIGHTCTRL,
        SDLK_LSHIFT => KEY_LEFTSHIFT,
        SDLK_RSHIFT => KEY_RIGHTSHIFT,
        SDLK_LALT => KEY_LEFTALT,
        SDLK_RALT => KEY_RIGHTALT,
        SDLK_LGUI => KEY_LEFTMETA,
        SDLK_RGUI => KEY_RIGHTMETA,
        SDLK_RETURN => KEY_ENTER,
        SDLK_TAB => KEY_TAB,
        SDLK_BACKSPACE => KEY_BACKSPACE,
        _ => return None,
    })
}

/// The evdev code to send for a key event: by symbol for keys that type no text, by position
/// otherwise. A press's code must be remembered and reused for its release
/// ([`crate::input::Keys`]), because the symbol can change while the key is held.
pub fn evdev_for_key(sc: SDL_Scancode, key: SDL_Keycode) -> Option<u16> {
    evdev_for_symbol(key).or_else(|| evdev_for_scancode(sc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_digits_and_keypad_by_position() {
        assert_eq!(evdev_for_scancode(SDL_SCANCODE_A), Some(30));
        assert_eq!(evdev_for_scancode(SDL_SCANCODE_Q), Some(KEY_Q));
        assert_eq!(evdev_for_scancode(SDL_SCANCODE_W), Some(17));
        assert_eq!(evdev_for_scancode(SDL_SCANCODE_Z), Some(KEY_Z));
        assert_eq!(evdev_for_scancode(SDL_SCANCODE_1), Some(2));
        assert_eq!(evdev_for_scancode(SDL_SCANCODE_0), Some(11));
        assert_eq!(evdev_for_scancode(SDL_SCANCODE_GRAVE), Some(41));
        assert_eq!(evdev_for_scancode(SDL_SCANCODE_NONUSBACKSLASH), Some(86));
        assert_eq!(evdev_for_scancode(SDL_SCANCODE_KP_ENTER), Some(96));
        assert_eq!(evdev_for_scancode(SDL_SCANCODE_KP_0), Some(82));
        assert_eq!(evdev_for_scancode(SDL_SCANCODE_F1), Some(59));
        assert_eq!(evdev_for_scancode(SDL_SCANCODE_F12), Some(88));
        assert_eq!(evdev_for_scancode(SDL_SCANCODE_F24), Some(194));
        assert_eq!(evdev_for_scancode(SDL_SCANCODE_UP), Some(KEY_UP));
        assert_eq!(evdev_for_scancode(SDL_SCANCODE_PRINTSCREEN), Some(99));
        assert_eq!(evdev_for_scancode(SDL_SCANCODE_LGUI), Some(KEY_LEFTMETA));
        assert_eq!(evdev_for_scancode(SDL_SCANCODE_RALT), Some(KEY_RIGHTALT));
        assert_eq!(evdev_for_scancode(SDL_SCANCODE_MEDIA_PLAY_PAUSE), Some(164));
        assert_eq!(evdev_for_scancode(SDL_SCANCODE_UNKNOWN), None);
    }

    #[test]
    fn every_positional_code_is_a_key() {
        // KEY_MAX is 0x2ff; codes 0x100.. are buttons, never produced here.
        for n in 0..512 {
            if let Some(c) = evdev_for_scancode(SDL_Scancode(n)) {
                assert!(c > 0 && (c < 0x100 || (0x160..0x2ff).contains(&c)), "scancode {n} -> {c}");
            }
        }
    }

    #[test]
    fn caps_swapped_with_escape_carries_over() {
        // What SDL reports with `caps:swapescape`: the physical key, and the keymap's symbol.
        assert_eq!(evdev_for_key(SDL_SCANCODE_CAPSLOCK, SDLK_ESCAPE), Some(KEY_ESC));
        assert_eq!(evdev_for_key(SDL_SCANCODE_ESCAPE, SDLK_CAPSLOCK), Some(KEY_CAPSLOCK));
        // `ctrl:nocaps`.
        assert_eq!(evdev_for_key(SDL_SCANCODE_CAPSLOCK, SDLK_LCTRL), Some(KEY_LEFTCTRL));
        // No remap.
        assert_eq!(evdev_for_key(SDL_SCANCODE_ESCAPE, SDLK_ESCAPE), Some(KEY_ESC));
    }

    #[test]
    fn letters_ignore_the_layout() {
        // AZERTY: the key at Q's position types 'a'; the game gets the position.
        assert_eq!(evdev_for_key(SDL_SCANCODE_Q, SDLK_A), Some(KEY_Q));
        assert_eq!(evdev_for_key(SDL_SCANCODE_W, SDLK_Z), Some(17));
    }
}
