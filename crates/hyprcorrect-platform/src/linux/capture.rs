//! Linux keystroke capture.
//!
//! Reads key events from every keyboard under `/dev/input` via `evdev`
//! and translates them — honoring the keyboard layout and modifiers via
//! `xkbcommon` — into [`Key`] values for the keystroke buffer.
//!
//! The trigger chord itself (Super+Ctrl+Shift+Alt+letter) is *not*
//! delivered here — the `GlobalShortcuts` portal (`hotkey`) handles
//! that at the compositor level. Capture only suppresses the would-be
//! [`Key::Reset`] that the chord's letter press would otherwise emit,
//! so the buffer survives the chord and the portal trigger has a word
//! to fix.
//!
//! One OS thread per keyboard device runs for the life of the process;
//! [`start`] returns the channel they feed.
//!
//! Layout note: the keymap is the system / `XKB_DEFAULT_*` default. A
//! layout configured only in the compositor (e.g. `hyprland.conf`) is
//! not yet read — that is M5 polish.

use std::io::ErrorKind;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use evdev::{Device, EventSummary, KeyCode};
use hyprcorrect_core::Key;
use xkbcommon::xkb;

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

/// The trigger chord — a letter pressed while Ctrl, Alt, Shift, and
/// Super are all held. Capture uses this only to *suppress* the
/// would-be Reset the chord's letter press would otherwise emit; the
/// portal (`hotkey`) is what actually fires the trigger.
#[derive(Debug, Clone, Copy)]
struct TriggerSpec {
    sym: u32,
    alt_sym: u32,
}

/// Start capturing keystrokes from every keyboard under `/dev/input`.
///
/// Returns a channel of [`Key`] events. One detached OS thread per
/// keyboard device feeds the channel for the life of the process;
/// dropping the [`Receiver`] makes those threads exit.
///
/// # Errors
///
/// See [`CaptureError`].
pub fn start() -> Result<Receiver<Key>, CaptureError> {
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

    let trigger = resolve_trigger();
    let keyboards = keyboard_devices()?;
    let (tx, rx) = mpsc::channel();
    for device in keyboards {
        let tx = tx.clone();
        let keymap_text = keymap_text.clone();
        thread::spawn(move || read_device(device, &keymap_text, trigger, &tx));
    }
    Ok(rx)
}

/// Resolve the trigger spec from `$HYPRCORRECT_TRIGGER` (default `F`).
fn resolve_trigger() -> TriggerSpec {
    let name = std::env::var("HYPRCORRECT_TRIGGER").unwrap_or_else(|_| "F".to_string());
    let sym = xkb::keysym_from_name(&name, xkb::KEYSYM_CASE_INSENSITIVE).raw();
    let alt_sym = match sym {
        0x61..=0x7A => sym - 0x20,
        0x41..=0x5A => sym + 0x20,
        _ => 0,
    };
    TriggerSpec { sym, alt_sym }
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
fn read_device(mut device: Device, keymap_text: &str, trigger: TriggerSpec, tx: &Sender<Key>) {
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
            if value != 0
                && let Some(key) = translate(&state, keycode, trigger)
                && tx.send(key).is_err()
            {
                return; // receiver dropped
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
fn translate(state: &xkb::State, keycode: xkb::Keycode, trigger: TriggerSpec) -> Option<Key> {
    let sym = state.key_get_one_sym(keycode).raw();

    // Modifier keys themselves are never buffered and never reset —
    // they only affect xkb state, which `read_device` updates after
    // this call.
    if is_modifier_keysym(sym) {
        return None;
    }

    // The trigger chord's letter press: ignored here. The portal fires
    // the trigger separately; suppressing this prevents the buffer
    // from being reset right when the user is about to ask for a fix.
    let letter_match = trigger.sym != 0
        && (sym == trigger.sym || (trigger.alt_sym != 0 && sym == trigger.alt_sym));
    if letter_match && is_trigger_chord(state) {
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

/// `true` if Ctrl, Alt, Shift, and Super are all currently held — the
/// trigger chord's modifier set.
fn is_trigger_chord(state: &xkb::State) -> bool {
    let active = |m: &str| state.mod_name_is_active(m, xkb::STATE_MODS_EFFECTIVE);
    active(xkb::MOD_NAME_CTRL)
        && active(xkb::MOD_NAME_ALT)
        && active(xkb::MOD_NAME_SHIFT)
        && active(xkb::MOD_NAME_LOGO)
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
    const RESET_KEYS: [u32; 16] = [
        KEY_Return,
        KEY_KP_Enter,
        KEY_Linefeed,
        KEY_Tab,
        KEY_ISO_Left_Tab,
        KEY_Escape,
        KEY_Left,
        KEY_Right,
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
    use xkb::keysyms::{KEY_BackSpace, KEY_End, KEY_Escape, KEY_Left, KEY_Return, KEY_Tab, KEY_Up};

    #[test]
    fn backspace_keysym_maps_to_backspace() {
        assert_eq!(classify(KEY_BackSpace, ""), Some(Key::Backspace));
    }

    #[test]
    fn caret_moving_keys_reset_the_buffer() {
        for sym in [KEY_Return, KEY_Tab, KEY_Escape, KEY_Left, KEY_Up, KEY_End] {
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
