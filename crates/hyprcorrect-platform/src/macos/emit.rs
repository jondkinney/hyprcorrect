//! Synthetic input via Quartz `CGEvent`.
//!
//! Replacement is "navigate, backspace, type": we post real key events
//! (`CGEventCreateKeyboardEvent` + `CGEventPost`) for Backspace / arrows
//! and inject the correction text with `CGEventKeyboardSetUnicodeString`,
//! which types arbitrary Unicode regardless of the active keyboard
//! layout. This works in every app, terminals included — the same
//! property the Linux `wtype` path relies on.
//!
//! Posting synthetic events needs **Accessibility** (System Settings →
//! Privacy & Security → Accessibility) on macOS 13+.

use std::os::raw::c_void;
use std::sync::Once;
use std::thread::sleep;
use std::time::Duration;

use super::capture;
use super::ffi::*;
use super::keymap::{VK_BACKSPACE, VK_E, VK_LEFT, VK_RIGHT};

/// Wait this long for the trigger chord to release before typing.
const MODS_CLEAR_TIMEOUT_MS: u64 = 250;

#[derive(Debug, thiserror::Error)]
pub enum EmitError {
    #[error(
        "Accessibility permission is not granted — open System Settings → \
         Privacy & Security → Accessibility, enable hyprcorrect, then restart it"
    )]
    Permission,
    #[error("could not create a CGEventSource for synthetic input")]
    Source,
}

/// Press Backspace `backspaces` times, then type `text` (default pauses).
pub fn replace(backspaces: usize, text: &str) -> Result<(), EmitError> {
    replace_with_delay(backspaces, text, 8, 1)
}

/// Like [`replace`] with caller-set per-backspace and per-character
/// pauses (ms).
pub fn replace_with_delay(
    backspaces: usize,
    text: &str,
    pause_per_backspace_ms: u32,
    pause_per_char_ms: u32,
) -> Result<(), EmitError> {
    replace_around_caret_with_delay(
        backspaces,
        0,
        text,
        pause_per_backspace_ms,
        pause_per_char_ms,
    )
}

/// Move Right `deletes` times, Backspace `backspaces + deletes` times,
/// then type `text`. Used for caret-relative replacements (fix-sentence,
/// review-apply). `pause_per_char_ms` is the inter-character sleep
/// during the typing phase (see [`EventSource::type_text`]).
pub fn replace_around_caret_with_delay(
    backspaces: usize,
    deletes: usize,
    text: &str,
    pause_per_backspace_ms: u32,
    pause_per_char_ms: u32,
) -> Result<(), EmitError> {
    capture::wait_mods_clear(Duration::from_millis(MODS_CLEAR_TIMEOUT_MS));
    let src = EventSource::new()?;
    for _ in 0..deletes {
        src.tap_key(VK_RIGHT, 0);
    }
    for _ in 0..(backspaces + deletes) {
        src.tap_key(VK_BACKSPACE, 0);
        sleep(Duration::from_millis(pause_per_backspace_ms as u64));
    }
    src.type_text(text, pause_per_char_ms);
    Ok(())
}

/// Anchor to end-of-line, walk Left `chars_from_end`, Backspace
/// `word_chars`, then type `insert`. End-anchoring dodges the held-arrow
/// caret-drift trap a direct-offset emit would fall into.
pub fn anchored_replace_with_delay(
    chars_from_end: usize,
    word_chars: usize,
    insert: &str,
    pause_per_backspace_ms: u32,
    pause_per_char_ms: u32,
) -> Result<(), EmitError> {
    capture::wait_mods_clear(Duration::from_millis(MODS_CLEAR_TIMEOUT_MS));
    let src = EventSource::new()?;
    // ⌃E → move caret to end of line (see VK_E doc), then walk Left.
    src.tap_key(VK_E, kCGEventFlagMaskControl);
    for _ in 0..chars_from_end {
        src.tap_key(VK_LEFT, 0);
    }
    for _ in 0..word_chars {
        src.tap_key(VK_BACKSPACE, 0);
        sleep(Duration::from_millis(pause_per_backspace_ms as u64));
    }
    src.type_text(insert, pause_per_char_ms);
    Ok(())
}

/// Owns a `CGEventSource` for one replacement burst; releases it on drop.
struct EventSource {
    source: CGEventSourceRef,
}

impl EventSource {
    fn new() -> Result<Self, EmitError> {
        ensure_post_access()?;
        let source = unsafe { CGEventSourceCreate(kCGEventSourceStateHIDSystemState) };
        if source.is_null() {
            return Err(EmitError::Source);
        }
        Ok(Self { source })
    }

    /// Post a key-down then key-up for `keycode` with `flags`. We always
    /// set the flags explicitly — even to 0 — so a modifier the user is
    /// still physically holding from the trigger chord can't poison the
    /// burst (an inherited ⌘ would turn a synthetic Backspace into
    /// delete-to-line-start). `wait_mods_clear` already waits for release,
    /// but this guarantees a clean burst even if it timed out.
    fn tap_key(&self, keycode: u16, flags: u64) {
        for down in [true, false] {
            let ev = unsafe { CGEventCreateKeyboardEvent(self.source, keycode, down) };
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

    /// Type `text`, ONE character per event. Newlines become Shift+Return
    /// (so a multi-line correction inserts line breaks instead of
    /// submitting chat-style inputs).
    ///
    /// Per-character is deliberate: Electron/Chromium apps (Slack, VSCode,
    /// Discord, …) ignore a single `CGEventKeyboardSetUnicodeString` event
    /// that carries a whole multi-char string on a keycode-0 event — the
    /// backspaces land but the text never appears. One char per keyDown/
    /// keyUp pair is what those apps accept (the approach Espanso uses).
    ///
    /// `pause_per_char_ms` is the inter-character sleep (the "Pause per
    /// character" knob). It keeps fast apps from coalescing or dropping
    /// characters; 0 disables the sleep entirely for the snappiest typing.
    fn type_text(&self, text: &str, pause_per_char_ms: u32) {
        if text.is_empty() {
            return;
        }
        let mut first = true;
        for segment in text.split('\n') {
            if !first {
                self.tap_key(0x24 /* Return */, kCGEventFlagMaskShift);
            }
            first = false;
            for ch in segment.chars() {
                self.type_char(ch);
                if pause_per_char_ms > 0 {
                    std::thread::sleep(Duration::from_millis(pause_per_char_ms as u64));
                }
            }
        }
    }

    /// Inject a single character as a keyDown + keyUp pair, each carrying
    /// that character's unicode string (with flags cleared so a still-held
    /// chord modifier can't turn it into a shortcut). Handles characters
    /// outside the BMP via the two-unit UTF-16 surrogate pair.
    fn type_char(&self, ch: char) {
        let mut buf = [0u16; 2];
        let utf16 = ch.encode_utf16(&mut buf);
        let len = utf16.len();
        for down in [true, false] {
            let ev = unsafe { CGEventCreateKeyboardEvent(self.source, 0, down) };
            if ev.is_null() {
                continue;
            }
            unsafe {
                CGEventSetFlags(ev, 0);
                CGEventSetIntegerValueField(ev, kCGEventSourceUserData, SYNTHETIC_MARK);
                CGEventKeyboardSetUnicodeString(ev, len, utf16.as_ptr());
                CGEventPost(kCGHIDEventTap, ev);
                CFRelease(ev);
            }
        }
    }
}

impl Drop for EventSource {
    fn drop(&mut self) {
        if !self.source.is_null() {
            unsafe { CFRelease(self.source as *const c_void) };
        }
    }
}

/// Pre-flight Accessibility (post-event) access. On the first denied
/// call, request it once (registers + prompts) and surface the error so
/// the daemon can fall back / warn.
fn ensure_post_access() -> Result<(), EmitError> {
    if unsafe { CGPreflightPostEventAccess() } {
        return Ok(());
    }
    static REQUEST_ONCE: Once = Once::new();
    REQUEST_ONCE.call_once(|| unsafe {
        CGRequestPostEventAccess();
    });
    Err(EmitError::Permission)
}
