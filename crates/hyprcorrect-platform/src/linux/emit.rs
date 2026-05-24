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

/// Per-key delay used for the *typing* burst. Fixed because there's
/// almost never a reason to slow new-text typing in practice — the
/// drops happen on the backspace burst, not on text.
const TYPING_INTER_KEY_DELAY_MS: u32 = 2;

/// Apply an edit at the caret: press Backspace `backspaces` times, then
/// type `text`. Uses the fixed default backspace delay.
///
/// # Errors
///
/// Returns [`EmitError`] if `wtype` is missing or exits non-zero.
pub fn replace(backspaces: usize, text: &str) -> Result<(), EmitError> {
    replace_with_delay(backspaces, text, 8)
}

/// Like [`replace`], but lets the caller set the per-key delay for
/// the backspace burst. Raise it if the focused app drops events
/// under fast dispatch and leaves leftover prefix characters from
/// the original after a fix.
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
    backspace_inter_key_delay_ms: u32,
) -> Result<(), EmitError> {
    if backspaces > 0 {
        let mut cmd = Command::new("wtype");
        if backspace_inter_key_delay_ms > 0 {
            cmd.args(["-d", &backspace_inter_key_delay_ms.to_string()]);
        }
        for _ in 0..backspaces {
            cmd.args(["-P", "BackSpace", "-p", "BackSpace"]);
        }
        run(cmd)?;
    }
    if !text.is_empty() {
        let mut cmd = Command::new("wtype");
        cmd.args(["-d", &TYPING_INTER_KEY_DELAY_MS.to_string()]);
        cmd.arg("--").arg(text);
        run(cmd)?;
    }
    Ok(())
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
