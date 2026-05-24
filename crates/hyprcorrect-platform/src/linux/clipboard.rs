//! Clipboard / selection fallback (M5).
//!
//! When the focused window's keystroke buffer is empty — typically
//! because focus moved and the user wants to fix something they
//! *didn't just type* — we simulate Ctrl+Shift+Left to select the
//! previous word, Ctrl+C to copy it, read the clipboard via
//! `wl-paste`, run the offline corrector over it, and then type
//! the correction. The selection is still active when we type, so
//! the replacement overwrites it in place.
//!
//! Best-effort: doesn't work in terminals (no select-previous-word
//! shortcut) or in apps that interpret Ctrl+Shift+Left differently.
//! Per `DESIGN.md`'s secondary-mode notes.

use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

/// Errors from a clipboard-fallback round trip.
#[derive(Debug, thiserror::Error)]
pub enum ClipboardError {
    /// `wtype` or `wl-paste` could not be spawned (typically not
    /// installed). The daemon falls back silently — no fix happens,
    /// but the user can install `wl-clipboard` to enable it.
    #[error("could not run {0}: {1}")]
    Spawn(String, String),
    /// The helper exited non-zero — usually means the protocol or
    /// the compositor refused the request.
    #[error("{0} exited non-zero: {1}")]
    Exit(String, String),
    /// `wl-paste` returned empty — the select-previous-word
    /// keystroke didn't land (terminal, no-text-field, …).
    #[error("clipboard was empty after the copy step — selection likely failed")]
    Empty,
    /// The clipboard contained non-UTF-8 bytes — we don't try to
    /// correct image / PDF / etc. payloads.
    #[error("clipboard contents were not valid UTF-8")]
    NotUtf8,
}

/// Select the previous word, copy it, and return the contents.
/// Leaves the selection active so a subsequent
/// [`type_replacement`] call overwrites it.
pub fn copy_previous_word() -> Result<String, ClipboardError> {
    // Select previous word: Ctrl+Shift+Left (press, then release in
    // the reverse order). wtype's `-M` is modifier-press, `-k` is
    // keysym press+release, `-m` is modifier-release.
    wtype(&[
        "-M", "ctrl", "-M", "shift", "-k", "Left", "-m", "shift", "-m", "ctrl",
    ])?;
    sleep(Duration::from_millis(30));

    // Copy: Ctrl+C.
    wtype(&["-M", "ctrl", "-k", "c", "-m", "ctrl"])?;
    sleep(Duration::from_millis(80)); // compositor + clipboard manager

    let output = Command::new("wl-paste")
        .arg("-n") // strip trailing newline
        .output()
        .map_err(|e| ClipboardError::Spawn("wl-paste".into(), e.to_string()))?;
    if !output.status.success() {
        return Err(ClipboardError::Exit(
            "wl-paste".into(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| ClipboardError::NotUtf8)?
        .to_string();
    if text.is_empty() {
        return Err(ClipboardError::Empty);
    }
    Ok(text)
}

/// Type the replacement text. With a selection still active (from
/// [`copy_previous_word`]), this overwrites the selection in
/// place. Safe to call standalone too.
pub fn type_replacement(text: &str) -> Result<(), ClipboardError> {
    // `--` ends wtype's option parsing so a replacement that starts
    // with `-` won't be parsed as a flag.
    let output = Command::new("wtype")
        .arg("--")
        .arg(text)
        .output()
        .map_err(|e| ClipboardError::Spawn("wtype".into(), e.to_string()))?;
    if !output.status.success() {
        return Err(ClipboardError::Exit(
            "wtype".into(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    Ok(())
}

fn wtype(args: &[&str]) -> Result<(), ClipboardError> {
    let output = Command::new("wtype")
        .args(args)
        .output()
        .map_err(|e| ClipboardError::Spawn("wtype".into(), e.to_string()))?;
    if !output.status.success() {
        return Err(ClipboardError::Exit(
            "wtype".into(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    Ok(())
}
