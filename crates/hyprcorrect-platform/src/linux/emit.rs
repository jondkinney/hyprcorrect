//! Linux synthetic text input.
//!
//! Corrections are applied by shelling out to `wtype`, which drives the
//! Wayland `virtual-keyboard-v1` protocol. (A native, dependency-free
//! implementation of that protocol is a later refinement — see
//! `DESIGN.md`.)

use std::io::ErrorKind;
use std::process::Command;

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

/// Per-key delay used inside each `wtype` burst. Small but nonzero so
/// the protocol's flush points line up with each event — a couple of
/// in-the-weeds bug reports against wtype have been fixed by ensuring
/// `-d` is set rather than left at 0.
const WTYPE_INTER_KEY_DELAY_MS: u32 = 2;

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
    if !text.is_empty() {
        let mut cmd = Command::new("wtype");
        cmd.args(["-d", &WTYPE_INTER_KEY_DELAY_MS.to_string()]);
        cmd.arg("--").arg(text);
        run(cmd)?;
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
