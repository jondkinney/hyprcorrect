//! Linux synthetic text input.
//!
//! Corrections are applied by shelling out to `wtype`, which drives the
//! Wayland `virtual-keyboard-v1` protocol. (A native, dependency-free
//! implementation of that protocol is a later refinement — see
//! `DESIGN.md`.)

use std::io::ErrorKind;
use std::process::Command;
use std::time::Duration;

use super::capture;

/// How long we'll wait for the user to release the chord
/// (Ctrl/Shift/Alt/Super) before giving up and emitting anyway.
/// Tuned to feel instant when the user taps-and-releases, but
/// generous enough to cover a slow release.
const MODS_CLEAR_TIMEOUT_MS: u64 = 250;

/// An error applying a text replacement.
#[derive(Debug, thiserror::Error)]
pub enum EmitError {
    /// `wtype` is not installed.
    #[error(
        "`wtype` is not installed — install it (e.g. `sudo pacman -S wtype`) so hyprcorrect can type corrections"
    )]
    WtypeMissing,
    /// `wtype` ran but exited with a failure.
    #[error("`wtype` failed to apply the correction")]
    WtypeFailed,
}

/// Per-key delay used inside each `wtype` burst. Was 2 ms originally
/// — large enough to give wtype's protocol a flush point per event,
/// small enough to feel instant. But terminals (Ghostty, foot, …)
/// drop the occasional BackSpace under that pressure when the burst
/// is 5+ keys: the result is leftover characters that escape the
/// deletion (e.g., `mothr → motherr` instead of `mother`). 8 ms
/// per key is still imperceptible for normal-length words and is
/// reliably swallowed by every terminal we've tested.
const WTYPE_INTER_KEY_DELAY_MS: u32 = 8;

/// Apply an edit at the caret: press Backspace `backspaces` times, then
/// type `text`. Uses the default per-backspace pause.
///
/// # Errors
///
/// Returns [`EmitError`] if `wtype` is missing or exits non-zero.
pub fn replace(backspaces: usize, text: &str) -> Result<(), EmitError> {
    replace_with_delay(backspaces, text, 8)
}

/// Like [`replace`], but lets the caller set the pause-per-backspace
/// in milliseconds. The pause is applied as a single sleep between
/// the backspace burst and the replacement-text burst, scaled by the
/// number of backspaces so longer edits wait proportionally longer.
///
/// Wayland delivers wtype's events reliably; what this pause covers
/// is the time the focused app needs to *apply* the backspaces
/// through its own event loop before our next `wtype` (the typing
/// burst) starts queueing text events behind the still-processing
/// deletes.
///
/// Backspaces and text are emitted as *two separate* `wtype`
/// invocations so the focused app has a clean event boundary
/// between them.
///
/// # Errors
///
/// Returns [`EmitError`] if `wtype` is missing or exits non-zero.
pub fn replace_with_delay(
    backspaces: usize,
    text: &str,
    pause_per_backspace_ms: u32,
) -> Result<(), EmitError> {
    replace_around_caret_with_delay(backspaces, 0, text, pause_per_backspace_ms)
}

/// Like [`replace_with_delay`] but also emits Delete keys (right of
/// the caret) before typing the replacement. Used by fix-word /
/// fix-sentence when the caret is INSIDE a word or sentence: we
/// can't backspace away text on the right side of the caret, so we
/// hand the focused app `BackSpace × N` then `Delete × M` then the
/// new text.
///
/// `pause_per_backspace_ms` scales the drain pause by the total
/// number of editing keystrokes (backspaces + deletes), since both
/// kinds of edits queue in the focused app's event loop the same
/// way.
///
/// # Errors
///
/// Returns [`EmitError`] if `wtype` is missing or exits non-zero.
pub fn replace_around_caret_with_delay(
    backspaces: usize,
    deletes: usize,
    text: &str,
    pause_per_backspace_ms: u32,
) -> Result<(), EmitError> {
    // Wait for the user to release the trigger chord before we
    // type anything. Many Wayland compositors deliver wtype's
    // synthetic keys ORed with the user's physical modifier
    // state, so a `BackSpace` while Ctrl is still held arrives at
    // the focused window as Ctrl+BackSpace (delete-word, in most
    // terminals). On timeout we fall through and emit anyway —
    // the user may be holding an unrelated modifier on purpose.
    let _ = capture::wait_mods_clear(Duration::from_millis(MODS_CLEAR_TIMEOUT_MS));

    // Implementation strategy: "delete N chars to the right of the
    // caret" is rewritten as "move caret right N, then backspace N
    // more." Every deletion ends up going through `BackSpace`,
    // which TUIs and editors handle uniformly. Sending Delete keys
    // directly worked unreliably — under fast bursts terminals'
    // input parsers were dropping the trailing keystrokes, leaving
    // chars on screen.
    //
    // Three phases, each its own wtype call with a drain pause:
    // 1. Right arrow × `deletes` — moves caret to the right edge of
    //    the region we want gone.
    // 2. BackSpace × (`backspaces` + `deletes`) — drains the whole
    //    region left of the now-rightmost caret position.
    // 3. Type the replacement text.
    if deletes > 0 {
        let mut cmd = Command::new("wtype");
        cmd.args(["-d", &WTYPE_INTER_KEY_DELAY_MS.to_string()]);
        for _ in 0..deletes {
            cmd.args(["-P", "Right", "-p", "Right"]);
        }
        run(cmd)?;
        sleep_ms(pause_per_backspace_ms, deletes);
    }
    let total_backspaces = backspaces + deletes;
    if total_backspaces > 0 {
        let mut cmd = Command::new("wtype");
        cmd.args(["-d", &WTYPE_INTER_KEY_DELAY_MS.to_string()]);
        for _ in 0..total_backspaces {
            cmd.args(["-P", "BackSpace", "-p", "BackSpace"]);
        }
        run(cmd)?;
        sleep_ms(pause_per_backspace_ms, total_backspaces);
    }
    type_text(text)?;
    Ok(())
}

/// Replace a word at a *known position relative to end-of-line*.
/// `chars_from_end` is the number of Left arrows needed to walk
/// from end-of-line back to the END of the word to replace;
/// `word_chars` is the BackSpace count to remove the word once
/// the cursor is on it.
///
/// Anchored at `End` (rather than relative to the user's current
/// caret) so held-arrow undercount / mouse clicks / any other
/// way the buffer's caret can drift from the visible cursor
/// don't cause the emit to land at the wrong spot. The buffer's
/// *text* tracks what's actually on screen reliably — only the
/// caret offset is fragile — so counting chars back from
/// end-of-line is rock solid as long as the focused app's `End`
/// goes to end-of-line (shells, single-line text inputs, most
/// terminals do; multi-line editors may not).
///
/// Same mod-clear gate runs first.
///
/// # Errors
///
/// Returns [`EmitError`] if `wtype` is missing or exits non-zero.
pub fn anchored_replace_with_delay(
    chars_from_end: usize,
    word_chars: usize,
    insert: &str,
    pause_per_backspace_ms: u32,
) -> Result<(), EmitError> {
    let _ = capture::wait_mods_clear(Duration::from_millis(MODS_CLEAR_TIMEOUT_MS));

    // Anchor: jump the cursor to end-of-line.
    {
        let mut cmd = Command::new("wtype");
        cmd.args(["-d", &WTYPE_INTER_KEY_DELAY_MS.to_string()]);
        cmd.args(["-P", "End", "-p", "End"]);
        run(cmd)?;
        sleep_ms(pause_per_backspace_ms, 1);
    }
    if chars_from_end > 0 {
        let mut cmd = Command::new("wtype");
        cmd.args(["-d", &WTYPE_INTER_KEY_DELAY_MS.to_string()]);
        for _ in 0..chars_from_end {
            cmd.args(["-P", "Left", "-p", "Left"]);
        }
        run(cmd)?;
        sleep_ms(pause_per_backspace_ms, chars_from_end);
    }
    if word_chars > 0 {
        let mut cmd = Command::new("wtype");
        cmd.args(["-d", &WTYPE_INTER_KEY_DELAY_MS.to_string()]);
        for _ in 0..word_chars {
            cmd.args(["-P", "BackSpace", "-p", "BackSpace"]);
        }
        run(cmd)?;
        sleep_ms(pause_per_backspace_ms, word_chars);
    }
    type_text(insert)?;
    Ok(())
}

/// Type `text` as a `wtype` burst, emitting embedded newlines as
/// Shift+Enter rather than a bare Return. A plain Return submits
/// chat-style inputs (the Claude Code prompt, Slack, Discord, …);
/// Shift+Enter inserts a line break instead, so applying a multi-line
/// correction never sends the message. Each line is its own text burst
/// with a Shift+Enter key event between them; a string with no newline
/// is a single burst, identical to the old behavior. Empty input is a
/// no-op.
fn type_text(text: &str) -> Result<(), EmitError> {
    let mut first = true;
    for line in text.split('\n') {
        if !first {
            let mut cmd = Command::new("wtype");
            cmd.args(["-M", "shift", "-k", "Return", "-m", "shift"]);
            run(cmd)?;
        }
        first = false;
        if !line.is_empty() {
            let mut cmd = Command::new("wtype");
            cmd.args(["-d", &WTYPE_INTER_KEY_DELAY_MS.to_string()]);
            cmd.arg("--").arg(line);
            run(cmd)?;
        }
    }
    Ok(())
}

fn sleep_ms(pause_per_backspace_ms: u32, count: usize) {
    let total = u64::from(pause_per_backspace_ms).saturating_mul(count as u64);
    if total > 0 {
        std::thread::sleep(std::time::Duration::from_millis(total));
    }
}

fn run(mut cmd: Command) -> Result<(), EmitError> {
    let status = cmd.status().map_err(|e| match e.kind() {
        ErrorKind::NotFound => EmitError::WtypeMissing,
        _ => EmitError::WtypeFailed,
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(EmitError::WtypeFailed)
    }
}
