//! Linux keystroke capture.
//!
//! Reads key events from every keyboard under `/dev/input` via `evdev`
//! and translates them — honoring the keyboard layout and modifiers via
//! `xkbcommon` — into [`Key`] values for the keystroke buffer.
//!
//! The trigger chord itself (Super+Ctrl+Shift+Alt+letter) is *not*
//! delivered here — Hyprland intercepts it (via the inline keybind set
//! up by `hotkey`) and signals the daemon over `SIGUSR1`. Capture only
//! suppresses the would-be [`Key::Reset`] that the chord's letter
//! press would otherwise emit, so the buffer survives the chord and
//! the trigger has a word to fix.
//!
//! One OS thread per keyboard device runs for the life of the process;
//! [`start`] returns the channel they feed.
//!
//! Layout note: the keymap is the system / `XKB_DEFAULT_*` default. A
//! layout configured only in the compositor (e.g. `hyprland.conf`) is
//! not yet read — that is M5 polish.

use std::io::ErrorKind;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use evdev::{Device, EventSummary, KeyCode};
use hyprcorrect_core::{Chord, Key};
use xkbcommon::xkb;

use crate::linux::chord_capture::ChordCaptureSlot;

/// An error starting keystroke capture.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// No keyboard devices were found under `/dev/input`.
    #[error("no keyboard devices found under /dev/input")]
    NoKeyboards,
    /// `/dev/input` devices exist but could not be opened.
    #[error(
        "permission denied reading /dev/input — add your user to the 'input' group (`sudo usermod -aG input $USER`) and log back in"
    )]
    Permission,
    /// The keyboard layout could not be compiled by xkbcommon.
    #[error("could not compile the keyboard layout (xkbcommon)")]
    Keymap,
}

/// The trigger chord, expanded into the data capture needs: which
/// modifier flags must match, and the xkb keysyms (upper- and
/// lower-case) of the non-modifier key. Capture uses this only to
/// *suppress* the would-be Reset the chord's key press would
/// otherwise emit; the Hyprland keybind in `hotkey` is what actually
/// fires the trigger.
#[derive(Debug, Clone, Copy)]
struct TriggerSpec {
    sym: u32,
    alt_sym: u32,
    needs_ctrl: bool,
    needs_alt: bool,
    needs_shift: bool,
    needs_super: bool,
}

/// Start capturing keystrokes from every keyboard under `/dev/input`.
///
/// `chords` is the list of trigger chords the daemon has bound.
/// Capture uses them to suppress the would-be [`Key::Reset`] that
/// pressing one of those chords would otherwise emit — without this,
/// pressing e.g. `Super+Ctrl+Shift+Alt+S` would wipe the buffer
/// before the sentence-fix gets a chance to read it.
///
/// Returns a channel of [`Key`] events. One detached OS thread per
/// keyboard device feeds the channel for the life of the process;
/// dropping the [`Receiver`] makes those threads exit.
///
/// # Errors
///
/// See [`CaptureError`].
pub fn start(
    chords: &[Chord],
    chord_capture: Arc<ChordCaptureSlot>,
) -> Result<Receiver<Key>, CaptureError> {
    // Compile the keymap once, up front, so a broken layout fails fast
    // with a clear error rather than a silent no-events daemon.
    let keymap_text = {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or(CaptureError::Keymap)?;
        keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
    };

    let triggers: Vec<TriggerSpec> = chords.iter().map(resolve_trigger).collect();
    let keyboards = keyboard_devices()?;
    let (tx, rx) = mpsc::channel();
    for device in keyboards {
        let tx = tx.clone();
        let keymap_text = keymap_text.clone();
        let triggers = triggers.clone();
        let chord_capture = chord_capture.clone();
        thread::spawn(move || read_device(device, &keymap_text, &triggers, &chord_capture, &tx));
    }
    Ok(rx)
}

/// Resolve the trigger spec for the given chord. Bare modifiers (no
/// non-modifier key) are degenerate; in that case both `sym` fields
/// are 0 and `letter_match` below never fires.
fn resolve_trigger(chord: &Chord) -> TriggerSpec {
    let sym = xkb::keysym_from_name(&chord.key, xkb::KEYSYM_CASE_INSENSITIVE).raw();
    let alt_sym = match sym {
        0x61..=0x7A => sym - 0x20,
        0x41..=0x5A => sym + 0x20,
        _ => 0,
    };
    TriggerSpec {
        sym,
        alt_sym,
        needs_ctrl: chord.ctrl,
        needs_alt: chord.alt,
        needs_shift: chord.shift,
        needs_super: chord.super_,
    }
}

/// Enumerate `/dev/input` and return the devices that look like
/// keyboards (those that can emit letter keys).
fn keyboard_devices() -> Result<Vec<Device>, CaptureError> {
    let entries = match std::fs::read_dir("/dev/input") {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            return Err(CaptureError::Permission);
        }
        Err(_) => return Err(CaptureError::NoKeyboards),
    };

    let mut keyboards = Vec::new();
    let mut permission_denied = false;
    for entry in entries.flatten() {
        let path = entry.path();
        let is_event_node = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("event"));
        if !is_event_node {
            continue;
        }
        match Device::open(&path) {
            Ok(device) if is_keyboard(&device) => keyboards.push(device),
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::PermissionDenied => {
                permission_denied = true;
            }
            Err(_) => {}
        }
    }

    if !keyboards.is_empty() {
        Ok(keyboards)
    } else if permission_denied {
        Err(CaptureError::Permission)
    } else {
        Err(CaptureError::NoKeyboards)
    }
}

/// A device is treated as a keyboard if it can emit letter keys.
fn is_keyboard(device: &Device) -> bool {
    device
        .supported_keys()
        .is_some_and(|keys| keys.contains(KeyCode::KEY_A))
}

/// Read one device forever, translating key events into [`Key`]s and
/// sending them to `tx`. Returns — ending the thread — when the device
/// disappears or the receiver is dropped.
fn read_device(
    mut device: Device,
    keymap_text: &str,
    triggers: &[TriggerSpec],
    chord_capture: &ChordCaptureSlot,
    tx: &Sender<Key>,
) {
    // Each thread builds its own xkb state: Context/Keymap/State hold
    // raw pointers and are not Send, so they cannot cross the thread
    // boundary. The keymap text was already validated by `start`.
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let Some(keymap) = xkb::Keymap::new_from_string(
        &context,
        keymap_text.to_owned(),
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    ) else {
        return;
    };
    let mut state = xkb::State::new(&keymap);

    loop {
        let Ok(events) = device.fetch_events() else {
            return;
        };
        for input in events {
            let EventSummary::Key(_, code, value) = input.destructure() else {
                continue;
            };
            let keycode = xkb::Keycode::new(u32::from(code.0) + 8);

            // value: 0 = release, 1 = press, 2 = auto-repeat. Read the
            // key from the *current* state, before this key updates it
            // (the xkbcommon convention).
            if value != 0 {
                // Chord-record mode pre-empts normal Key handling so
                // pressing the chord doesn't reset the buffer or fire
                // any trigger while prefs is recording.
                if chord_capture.is_armed()
                    && let Some(chord) = chord_from_state(&state, keycode)
                    && chord_capture.try_emit(chord)
                {
                    // Modifier state still needs to update below.
                } else if let Some(key) = translate(&state, keycode, triggers)
                    && tx.send(key).is_err()
                {
                    return; // receiver dropped
                }
            }

            // Track modifiers on press and release; an auto-repeat is
            // not a distinct down/up and must not update the state.
            if value != 2 {
                let direction = if value == 0 {
                    xkb::KeyDirection::Up
                } else {
                    xkb::KeyDirection::Down
                };
                state.update_key(keycode, direction);
            }
        }
    }
}

/// Translate a pressed key into a [`Key`] for the buffer, or `None` to
/// ignore it.
fn translate(state: &xkb::State, keycode: xkb::Keycode, triggers: &[TriggerSpec]) -> Option<Key> {
    let sym = state.key_get_one_sym(keycode).raw();

    // Modifier keys themselves are never buffered and never reset —
    // they only affect xkb state, which `read_device` updates after
    // this call.
    if is_modifier_keysym(sym) {
        return None;
    }

    // Any of the daemon's bound chords match this key+modifier combo?
    // Suppress so pressing the chord doesn't ALSO reset the buffer
    // via the has_action_modifier branch below. Hyprland fires the
    // trigger separately (via SIGUSR1).
    let chord_match = triggers.iter().any(|trigger| {
        let letter_match = trigger.sym != 0
            && (sym == trigger.sym || (trigger.alt_sym != 0 && sym == trigger.alt_sym));
        letter_match && is_trigger_chord(state, *trigger)
    });
    if chord_match {
        return None;
    }

    // A non-modifier key pressed while Ctrl/Alt/Super is held is a
    // shortcut, not typed text — and it may have moved the caret or
    // edited. Reset.
    if has_action_modifier(state) {
        return Some(Key::Reset);
    }

    classify(sym, &state.key_get_utf8(keycode))
}

/// Build the chord string for a key pressed in chord-record mode:
/// the currently-held SUPER/CTRL/SHIFT/ALT modifiers, plus the
/// canonical name of the non-modifier key. Returns `None` for
/// modifier-only presses so prefs can keep recording until the user
/// hits a real key.
///
/// Format matches [`hyprcorrect_core::Chord::parse`] exactly, e.g.
/// `"SUPER+CTRL+SHIFT+ALT+F"` or `"CTRL+SPACE"` or bare `"F1"`.
fn chord_from_state(state: &xkb::State, keycode: xkb::Keycode) -> Option<String> {
    let sym = state.key_get_one_sym(keycode).raw();
    if is_modifier_keysym(sym) {
        return None;
    }
    let key_token = chord_key_token(sym)?;

    let active = |m: &str| state.mod_name_is_active(m, xkb::STATE_MODS_EFFECTIVE);
    let mut parts: Vec<&str> = Vec::new();
    if active(xkb::MOD_NAME_SHIFT) {
        parts.push("SHIFT");
    }
    if active(xkb::MOD_NAME_CTRL) {
        parts.push("CTRL");
    }
    if active(xkb::MOD_NAME_ALT) {
        parts.push("ALT");
    }
    if active(xkb::MOD_NAME_LOGO) {
        parts.push("SUPER");
    }
    Some(if parts.is_empty() {
        key_token
    } else {
        format!("{}+{key_token}", parts.join("+"))
    })
}

/// The token used in chord strings for a non-modifier keysym.
/// Letters become uppercase ASCII; common named keys (Space, F-row,
/// arrows, etc.) use the same UPPERCASE tokens
/// [`hyprcorrect_core::Chord::parse`] accepts; anything else falls
/// back to xkb's canonical keysym name uppercased.
fn chord_key_token(sym: u32) -> Option<String> {
    // Canonical chord tokens. Match the form used by vernier and
    // the rest of the hyprcorrect UI so the recorded chord round-
    // trips cleanly and the chip renderer doesn't need a second
    // translation layer. Hyprland accepts the long xkb keysym
    // names (Escape, Return, BackSpace, ...) case-insensitively,
    // so the only tokens that need translation back to xkb names
    // at hyprctl-bind time are ESC and ENTER — handled in
    // `Chord::hyprland_key`.
    let named = match sym {
        0xff1b => Some("ESC"),       // Escape
        0xff0d | 0xff8d => Some("ENTER"), // Return / KP_Enter
        0xff09 => Some("TAB"),       // Tab
        0xff08 => Some("BACKSPACE"), // BackSpace
        0xffff => Some("DELETE"),    // Delete
        0xff52 => Some("UP"),        // Up
        0xff54 => Some("DOWN"),      // Down
        0xff51 => Some("LEFT"),      // Left
        0xff53 => Some("RIGHT"),     // Right
        0x20 => Some("SPACE"),       // space
        0x2b => Some("PLUS"),        // +  (avoid colliding with the modifier separator)
        0x2d => Some("MINUS"),       // -
        0x3d => Some("EQUAL"),       // =
        _ => None,
    };
    if let Some(token) = named {
        return Some(token.to_string());
    }
    if (0x21..=0x7E).contains(&sym) {
        // Printable ASCII keysyms (letters, digits, punctuation) are
        // identical to their codepoint; lowercase letters get folded
        // up so `Chord::parse` round-trips.
        let ch = char::from_u32(sym)?.to_ascii_uppercase();
        return Some(ch.to_string());
    }
    let name = xkb::keysym_get_name(xkb::Keysym::from(sym));
    if name.is_empty() {
        return None;
    }
    Some(name.to_ascii_uppercase())
}

/// `true` when the currently-held modifier set matches the trigger
/// chord's modifier set exactly.
fn is_trigger_chord(state: &xkb::State, trigger: TriggerSpec) -> bool {
    let active = |m: &str| state.mod_name_is_active(m, xkb::STATE_MODS_EFFECTIVE);
    active(xkb::MOD_NAME_CTRL) == trigger.needs_ctrl
        && active(xkb::MOD_NAME_ALT) == trigger.needs_alt
        && active(xkb::MOD_NAME_SHIFT) == trigger.needs_shift
        && active(xkb::MOD_NAME_LOGO) == trigger.needs_super
}

/// `true` if Ctrl, Alt, or Super is currently held.
fn has_action_modifier(state: &xkb::State) -> bool {
    let active = |m: &str| state.mod_name_is_active(m, xkb::STATE_MODS_EFFECTIVE);
    active(xkb::MOD_NAME_CTRL) || active(xkb::MOD_NAME_ALT) || active(xkb::MOD_NAME_LOGO)
}

/// `true` if `sym` is one of the modifier keysyms — Shift, Control,
/// Caps Lock, Meta, Alt, Super, or Hyper (left or right). xkb assigns
/// these the contiguous range `0xffe1..=0xffee`.
fn is_modifier_keysym(sym: u32) -> bool {
    (0xffe1..=0xffee).contains(&sym)
}

/// Classify an xkb keysym and the UTF-8 it produces into a buffer
/// [`Key`]: Backspace and caret-moving keys are matched by keysym; a
/// single printable character becomes a `Char`; everything else (bare
/// modifiers, function keys) is ignored.
fn classify(sym: u32, utf8: &str) -> Option<Key> {
    use xkb::keysyms::{
        KEY_BackSpace, KEY_Delete, KEY_Down, KEY_End, KEY_Escape, KEY_Home, KEY_ISO_Left_Tab,
        KEY_Insert, KEY_KP_Enter, KEY_Left, KEY_Linefeed, KEY_Next, KEY_Prior, KEY_Return,
        KEY_Right, KEY_Tab, KEY_Up,
    };
    // Left/Right arrow press translates to a buffer caret move so
    // editing in-place during typing (jumping into earlier text to
    // fix a typo, then continuing) keeps the buffer intact. Other
    // caret movers (Up/Down/Home/End) and the cursor-modifying
    // keys (Return/Tab/Esc/Delete/Insert/Prior/Next) still reset:
    // we can't track them from raw evdev.
    const RESET_KEYS: [u32; 14] = [
        KEY_Return,
        KEY_KP_Enter,
        KEY_Linefeed,
        KEY_Tab,
        KEY_ISO_Left_Tab,
        KEY_Escape,
        KEY_Up,
        KEY_Down,
        KEY_Home,
        KEY_End,
        KEY_Prior,
        KEY_Next,
        KEY_Delete,
        KEY_Insert,
    ];

    if sym == KEY_BackSpace {
        Some(Key::Backspace)
    } else if sym == KEY_Left {
        Some(Key::MoveLeft)
    } else if sym == KEY_Right {
        Some(Key::MoveRight)
    } else if RESET_KEYS.contains(&sym) {
        Some(Key::Reset)
    } else {
        let mut chars = utf8.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) if !c.is_control() => Some(Key::Char(c)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xkb::keysyms::{
        KEY_BackSpace, KEY_End, KEY_Escape, KEY_Left, KEY_Return, KEY_Right, KEY_Tab, KEY_Up,
    };

    #[test]
    fn backspace_keysym_maps_to_backspace() {
        assert_eq!(classify(KEY_BackSpace, ""), Some(Key::Backspace));
    }

    #[test]
    fn left_right_arrows_move_the_caret() {
        assert_eq!(classify(KEY_Left, ""), Some(Key::MoveLeft));
        assert_eq!(classify(KEY_Right, ""), Some(Key::MoveRight));
    }

    #[test]
    fn other_navigation_keys_reset_the_buffer() {
        for sym in [KEY_Return, KEY_Tab, KEY_Escape, KEY_Up, KEY_End] {
            assert_eq!(classify(sym, ""), Some(Key::Reset), "keysym {sym:#x}");
        }
    }

    #[test]
    fn a_printable_key_maps_to_a_char() {
        // 0x0061 / 0x0020 are the keysyms for 'a' and space.
        assert_eq!(classify(0x0061, "a"), Some(Key::Char('a')));
        assert_eq!(classify(0x0020, " "), Some(Key::Char(' ')));
    }

    #[test]
    fn a_bare_modifier_is_ignored() {
        // A modifier key (here Shift_L, keysym 0xffe1) produces no UTF-8.
        assert_eq!(classify(0xffe1, ""), None);
    }
}
