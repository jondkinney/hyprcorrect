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
/// type `text`. Uses default timings (no inter-key delay, no post-
/// backspace pause).
///
/// # Errors
///
/// Returns [`EmitError`] if `wtype` is missing or exits non-zero.
pub fn replace(backspaces: usize, text: &str) -> Result<(), EmitError> {
    replace_with_delay(backspaces, text, 0, 0, 0, 0)
}

/// Like [`replace`], but explicitly sets:
/// - `text_inter_key_delay_ms`: per-key delay during the *typing*
///   burst.
/// - `backspace_inter_key_delay_ms`: per-key delay during the
///   *backspace* burst. Separate from text so the deletes can run
///   at a safer cadence — Wayland's virtual-keyboard pipeline drops
///   backspaces under fast dispatch, leaving leftover prefix chars
///   from the original after a fix.
/// - `post_backspace_pause_ms`: fixed pause between the backspace
///   burst and the replacement-text burst.
/// - `post_backspace_pause_per_char_ms`: additional pause per
///   backspace, so the gap scales with edit length. Total pause =
///   `post_backspace_pause_ms` + per-char × backspace count.
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
    text_inter_key_delay_ms: u32,
    backspace_inter_key_delay_ms: u32,
    post_backspace_pause_ms: u32,
    post_backspace_pause_per_char_ms: u32,
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
        let scaled_pause = u64::from(post_backspace_pause_ms).saturating_add(
            u64::from(post_backspace_pause_per_char_ms).saturating_mul(backspaces as u64),
        );
        if scaled_pause > 0 {
            std::thread::sleep(std::time::Duration::from_millis(scaled_pause));
        }
    }
    if !text.is_empty() {
        let mut cmd = Command::new("wtype");
        if text_inter_key_delay_ms > 0 {
            cmd.args(["-d", &text_inter_key_delay_ms.to_string()]);
        }
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
