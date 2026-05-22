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

/// Apply an edit at the caret: press Backspace `backspaces` times, then
/// type `text`.
///
/// # Errors
///
/// Returns [`EmitError`] if `wtype` is missing or exits non-zero.
pub fn replace(backspaces: usize, text: &str) -> Result<(), EmitError> {
    // wtype applies its arguments left to right: a press/release pair
    // per Backspace, then the replacement text as a literal argument.
    let mut cmd = Command::new("wtype");
    for _ in 0..backspaces {
        cmd.args(["-P", "BackSpace", "-p", "BackSpace"]);
    }
    if !text.is_empty() {
        cmd.arg(text);
    }
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
