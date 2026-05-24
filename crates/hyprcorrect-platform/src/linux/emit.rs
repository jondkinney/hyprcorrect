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
/// type `text`. With zero `inter_key_delay_ms`, no `-d` flag is added.
///
/// # Errors
///
/// Returns [`EmitError`] if `wtype` is missing or exits non-zero.
pub fn replace(backspaces: usize, text: &str) -> Result<(), EmitError> {
    replace_with_delay(backspaces, text, 0)
}

/// Like [`replace`], but explicitly sets the inter-key delay (in
/// milliseconds) wtype waits between keystrokes. Higher values
/// help apps that drop characters under wtype's default
/// fast-typing speed — see `config.behavior.inter_key_delay_ms`.
///
/// # Errors
///
/// Returns [`EmitError`] if `wtype` is missing or exits non-zero.
pub fn replace_with_delay(
    backspaces: usize,
    text: &str,
    inter_key_delay_ms: u32,
) -> Result<(), EmitError> {
    let mut cmd = Command::new("wtype");
    if inter_key_delay_ms > 0 {
        cmd.args(["-d", &inter_key_delay_ms.to_string()]);
    }
    // wtype applies its arguments left to right: a press/release pair
    // per Backspace, then the replacement text as a literal argument.
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
