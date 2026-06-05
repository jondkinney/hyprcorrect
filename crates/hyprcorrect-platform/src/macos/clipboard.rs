//! Selection + clipboard secondary path, for when the keystroke buffer
//! is empty (focus moved, a caret-moving key, a paste). Best-effort and
//! not usable in terminals — exactly like the Linux `wl-clipboard` path.
//!
//! `copy_previous_word` synthesizes ⌥⇧← (select the previous word) then
//! ⌘C (copy), reads the general pasteboard, and *leaves the selection
//! active* so a following `type_replacement` overwrites it in place.

use std::thread::sleep;
use std::time::Duration;

use objc2_app_kit::NSPasteboard;
use objc2_foundation::NSString;

use super::emit;
use super::ffi::*;
use super::keymap::VK_LEFT;

#[derive(Debug, thiserror::Error)]
pub enum ClipboardError {
    #[error("the clipboard was empty after the copy step — selection likely failed")]
    Empty,
    #[error("could not create a CGEventSource for the selection step")]
    Source,
}

const VK_C: u16 = 0x08;

/// Select the previous word (⌥⇧←), copy it (⌘C), and return the copied
/// text, leaving the selection active.
pub fn copy_previous_word() -> Result<String, ClipboardError> {
    let source = unsafe { CGEventSourceCreate(kCGEventSourceStateHIDSystemState) };
    if source.is_null() {
        return Err(ClipboardError::Source);
    }
    // ⌥⇧← selects the previous word; ⌘C copies it.
    tap_key(
        source,
        VK_LEFT,
        kCGEventFlagMaskAlternate | kCGEventFlagMaskShift,
    );
    tap_key(source, VK_C, kCGEventFlagMaskCommand);
    unsafe { CFRelease(source) };

    // Give the focused app a moment to service the copy.
    sleep(Duration::from_millis(40));

    match read_pasteboard_string() {
        Some(s) if !s.is_empty() => Ok(s),
        _ => Err(ClipboardError::Empty),
    }
}

/// Type `text`; with the selection from `copy_previous_word` still
/// active, it overwrites in place.
pub fn type_replacement(text: &str) -> Result<(), ClipboardError> {
    // Reuse the emit backend (0 backspaces — the selection is the target).
    // An emit failure here is non-fatal to the fallback; map it to Empty
    // so the daemon reports a single best-effort failure.
    emit::replace(0, text).map_err(|_| ClipboardError::Empty)
}

fn tap_key(source: CGEventSourceRef, keycode: u16, flags: u64) {
    for down in [true, false] {
        let ev = unsafe { CGEventCreateKeyboardEvent(source, keycode, down) };
        if ev.is_null() {
            continue;
        }
        unsafe {
            CGEventSetFlags(ev, flags);
            CGEventSetIntegerValueField(ev, kCGEventSourceUserData, SYNTHETIC_MARK);
            CGEventPost(kCGHIDEventTap, ev);
            CFRelease(ev);
        }
    }
}

fn read_pasteboard_string() -> Option<String> {
    let pb = NSPasteboard::generalPasteboard();
    let ty = NSString::from_str("public.utf8-plain-text");
    let s = pb.stringForType(&ty)?;
    Some(s.to_string())
}
