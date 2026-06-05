//! Translate a [`hyprcorrect_core::Chord`] into the `(virtual keycode,
//! Carbon modifier mask)` pair `RegisterEventHotKey` expects, and map
//! the chord's uppercase key token to a macOS `kVK_*` virtual keycode.
//!
//! `Chord` stores modifiers as four public bools (`ctrl`/`shift`/`alt`/
//! `super_`) and the key as an UPPERCASE token (`"F"`, `"SPACE"`,
//! `"LEFT"`, `"F5"`, …). We deliberately read `chord.key` directly
//! rather than `Chord::hyprland_key()`, whose xkb-flavoured names
//! (`"Escape"`, `"Return"`) don't help here.

use hyprcorrect_core::Chord;

/// Carbon modifier masks (from `Carbon/HIToolbox/Events.h`).
mod carbon_mods {
    pub const CMD: u32 = 1 << 8; // cmdKey      ← Super / ⌘
    pub const SHIFT: u32 = 1 << 9; // shiftKey  ← Shift / ⇧
    pub const OPTION: u32 = 1 << 11; // optionKey ← Alt / ⌥
    pub const CONTROL: u32 = 1 << 12; // controlKey ← Ctrl / ⌃
}

/// `(vkey, carbon_modifier_mask)` for a chord, or `None` if the key
/// token has no known virtual keycode (the caller treats that as an
/// unbindable chord and logs it).
pub(crate) fn chord_to_carbon(chord: &Chord) -> Option<(u32, u32)> {
    let mut mods = 0u32;
    if chord.shift {
        mods |= carbon_mods::SHIFT;
    }
    if chord.ctrl {
        mods |= carbon_mods::CONTROL;
    }
    if chord.alt {
        mods |= carbon_mods::OPTION;
    }
    if chord.super_ {
        mods |= carbon_mods::CMD;
    }
    let vkey = key_token_to_vkey(&chord.key)? as u32;
    Some((vkey, mods))
}

/// Map an UPPERCASE chord key token to a macOS virtual keycode
/// (`kVK_*`). Covers letters, digits, the common named keys, F1–F20,
/// and ANSI punctuation. Returns `None` for anything unmapped.
pub(crate) fn key_token_to_vkey(token: &str) -> Option<u16> {
    // Single ASCII letter / digit / punctuation.
    if token.chars().count() == 1 {
        if let Some(vk) = char_to_vkey(token.chars().next().unwrap()) {
            return Some(vk);
        }
    }
    Some(match token {
        "SPACE" => 0x31,
        "ENTER" | "RETURN" => 0x24,
        "TAB" => 0x30,
        "ESC" | "ESCAPE" => 0x35,
        "BACKSPACE" => 0x33,
        "DELETE" | "DEL" => 0x75,
        "UP" => 0x7E,
        "DOWN" => 0x7D,
        "LEFT" => 0x7B,
        "RIGHT" => 0x7C,
        "HOME" => 0x73,
        "END" => 0x77,
        "PAGEUP" | "PAGE_UP" | "PRIOR" => 0x74,
        "PAGEDOWN" | "PAGE_DOWN" | "NEXT" => 0x79,
        "F1" => 0x7A,
        "F2" => 0x78,
        "F3" => 0x63,
        "F4" => 0x76,
        "F5" => 0x60,
        "F6" => 0x61,
        "F7" => 0x62,
        "F8" => 0x64,
        "F9" => 0x65,
        "F10" => 0x6D,
        "F11" => 0x67,
        "F12" => 0x6F,
        "F13" => 0x69,
        "F14" => 0x6B,
        "F15" => 0x71,
        "F16" => 0x6A,
        "F17" => 0x40,
        "F18" => 0x4F,
        "F19" => 0x50,
        "F20" => 0x5A,
        _ => return None,
    })
}

/// kVK_* for a single ANSI character (case-insensitive for letters).
fn char_to_vkey(c: char) -> Option<u16> {
    Some(match c.to_ascii_lowercase() {
        'a' => 0x00,
        's' => 0x01,
        'd' => 0x02,
        'f' => 0x03,
        'h' => 0x04,
        'g' => 0x05,
        'z' => 0x06,
        'x' => 0x07,
        'c' => 0x08,
        'v' => 0x09,
        'b' => 0x0B,
        'q' => 0x0C,
        'w' => 0x0D,
        'e' => 0x0E,
        'r' => 0x0F,
        'y' => 0x10,
        't' => 0x11,
        '1' => 0x12,
        '2' => 0x13,
        '3' => 0x14,
        '4' => 0x15,
        '6' => 0x16,
        '5' => 0x17,
        '=' => 0x18,
        '9' => 0x19,
        '7' => 0x1A,
        '-' => 0x1B,
        '8' => 0x1C,
        '0' => 0x1D,
        ']' => 0x1E,
        'o' => 0x1F,
        'u' => 0x20,
        '[' => 0x21,
        'i' => 0x22,
        'p' => 0x23,
        'l' => 0x25,
        'j' => 0x26,
        '\'' => 0x27,
        'k' => 0x28,
        ';' => 0x29,
        '\\' => 0x2A,
        ',' => 0x2B,
        '/' => 0x2C,
        'n' => 0x2D,
        'm' => 0x2E,
        '.' => 0x2F,
        '`' => 0x32,
        _ => return None,
    })
}

/// Reverse of [`key_token_to_vkey`] for the common keys, used by the
/// capture path to reconstruct a chord string while prefs is recording.
/// Returns an UPPERCASE token, or `None` for keycodes we don't name.
pub(crate) fn vkey_to_token(vkey: u16) -> Option<String> {
    let named = match vkey {
        0x31 => "SPACE",
        0x24 => "RETURN",
        0x30 => "TAB",
        0x35 => "ESCAPE",
        0x33 => "BACKSPACE",
        0x75 => "DELETE",
        0x7E => "UP",
        0x7D => "DOWN",
        0x7B => "LEFT",
        0x7C => "RIGHT",
        0x73 => "HOME",
        0x77 => "END",
        0x74 => "PAGEUP",
        0x79 => "PAGEDOWN",
        0x7A => "F1",
        0x78 => "F2",
        0x63 => "F3",
        0x76 => "F4",
        0x60 => "F5",
        0x61 => "F6",
        0x62 => "F7",
        0x64 => "F8",
        0x65 => "F9",
        0x6D => "F10",
        0x67 => "F11",
        0x6F => "F12",
        _ => "",
    };
    if !named.is_empty() {
        return Some(named.to_string());
    }
    // ANSI letters / digits / punctuation: round-trip through the
    // forward table so we share one source of truth.
    for c in ('a'..='z').chain('0'..='9') {
        if char_to_vkey(c) == Some(vkey) {
            return Some(c.to_ascii_uppercase().to_string());
        }
    }
    for c in ['-', '=', '[', ']', '\\', ';', '\'', ',', '.', '/', '`'] {
        if char_to_vkey(c) == Some(vkey) {
            return Some(c.to_string());
        }
    }
    None
}

// Virtual keycodes the emit backend posts directly.
pub(crate) const VK_BACKSPACE: u16 = 0x33;
pub(crate) const VK_LEFT: u16 = 0x7B;
pub(crate) const VK_RIGHT: u16 = 0x7C;
/// `E` — used as Ctrl+E (move-to-end-of-line) for the emit anchor. The
/// physical End key (kVK_End, 0x77) is wrong here: in NSTextView it
/// scrolls to *document* end rather than moving the caret to line end,
/// whereas ⌃E is the standard Cocoa `moveToEndOfLine:` binding and is
/// also end-of-line in readline/terminals.
pub(crate) const VK_E: u16 = 0x0E;
