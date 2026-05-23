//! Global trigger via a Hyprland inline keybind + signals.
//!
//! At startup the daemon adds an inline Hyprland keybind whose `exec`
//! reads the daemon's PID file and `kill -USR1`s that PID
//! specifically. Hyprland intercepts the chord — terminals and other
//! focused apps never see it — and the daemon catches the signal as
//! [`HotkeyEvent::Trigger`].
//!
//! The PID-file-based targeting is deliberate: `pkill -x hyprcorrect`
//! would match the prefs subprocess too (it shares the daemon's
//! binary name and therefore its `/proc/PID/comm`) and silently
//! terminate the prefs window when the user pressed the chord. The
//! file is written by the daemon at startup and removed on shutdown
//! — see [`hyprcorrect_core::runtime`].
//!
//! `SIGHUP` arrives as [`HotkeyEvent::Reload`] and is the prefs
//! window's signal to the running daemon that the config has
//! changed.
//!
//! Hyprland-specific. The cross-compositor route is the
//! `GlobalShortcuts` portal (DESIGN.md); that has its own auto-bind
//! limitation on `xdg-desktop-portal-hyprland` today, so we'll revisit
//! it together with M3's portable backends.

use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};

use hyprcorrect_core::{Chord, runtime};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGUSR1};
use signal_hook::iterator::Signals;

/// A daemon-level event driven by the operating-system signal stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    /// `SIGUSR1` — the trigger chord fired. Run `fix-last-word`.
    Trigger,
    /// `SIGHUP` — the user saved the config. Reload it and rebind the
    /// trigger if the chord letter changed.
    Reload,
    /// `SIGTERM` / `SIGINT` — the daemon should shut down cleanly so
    /// the Hyprland bind and PID file are removed.
    Shutdown,
}

/// An error registering the Hyprland keybind or signal handler.
#[derive(Debug, thiserror::Error)]
pub enum HotkeyError {
    /// `hyprctl` could not bind the trigger chord.
    #[error("hyprctl could not bind the trigger chord: {0}")]
    Hyprctl(String),
    /// `hyprctl` could not unbind the trigger chord.
    #[error("hyprctl could not unbind the trigger chord: {0}")]
    HyprctlUnbind(String),
    /// Could not install the signal handler.
    #[error("could not install signal handler: {0}")]
    Signal(String),
    /// Could not spawn the signal-listener thread.
    #[error("could not spawn the signal-listener thread: {0}")]
    Thread(String),
}

/// Install the Hyprland inline keybind for the given chord.
///
/// Idempotent: first runs `hyprctl keyword unbind` for the same chord
/// so a previous (uncleanly-shut-down) daemon's bind doesn't leave
/// duplicates behind.
///
/// # Errors
///
/// See [`HotkeyError`].
pub fn install_bind(chord: &Chord) -> Result<(), HotkeyError> {
    let _ = uninstall_bind(chord); // dedup any stale prior bind
    // Hyprland's `exec` dispatcher already runs the command through
    // `sh -c`, so we lean on shell substitution to read the PID at
    // chord-press time — that way the bind survives the daemon
    // exiting and restarting (the new PID is in the file by then).
    let pid_path = runtime::pid_path();
    let bind_value = format!(
        "{mods}, {key}, exec, kill -USR1 $(cat \"{pid_path}\")",
        mods = chord.hyprland_modifiers(),
        key = chord.hyprland_key(),
        pid_path = pid_path.display(),
    );
    let output = Command::new("hyprctl")
        .args(["keyword", "bind", &bind_value])
        .output()
        .map_err(|e| HotkeyError::Hyprctl(format!("invoke hyprctl: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(HotkeyError::Hyprctl(format!(
            "hyprctl exited non-zero — stdout: {stdout} stderr: {stderr}"
        )));
    }
    Ok(())
}

/// Remove the Hyprland inline keybind for the given chord. Calling
/// this for an unbound chord is silently fine.
///
/// # Errors
///
/// Returns [`HotkeyError::HyprctlUnbind`] only on `hyprctl` invocation
/// failure (not on "nothing to unbind").
pub fn uninstall_bind(chord: &Chord) -> Result<(), HotkeyError> {
    let unbind_value = format!(
        "{mods}, {key}",
        mods = chord.hyprland_modifiers(),
        key = chord.hyprland_key(),
    );
    let output = Command::new("hyprctl")
        .args(["keyword", "unbind", &unbind_value])
        .output()
        .map_err(|e| HotkeyError::HyprctlUnbind(format!("invoke hyprctl: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(HotkeyError::HyprctlUnbind(stderr.into_owned()));
    }
    Ok(())
}

/// Start the signal listener.
///
/// Installs handlers for `SIGUSR1` (trigger), `SIGHUP` (reload), and
/// `SIGTERM` / `SIGINT` (shutdown) and returns a receiver of
/// [`HotkeyEvent`]s. The shutdown signals let the daemon clean up its
/// Hyprland bind and PID file even when killed via `pkill` or Ctrl-C.
///
/// # Errors
///
/// See [`HotkeyError`].
pub fn signal_channel() -> Result<Receiver<HotkeyEvent>, HotkeyError> {
    let mut signals = Signals::new([SIGUSR1, SIGHUP, SIGTERM, SIGINT])
        .map_err(|e| HotkeyError::Signal(e.to_string()))?;
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("hyprcorrect-signal".into())
        .spawn(move || forward_signals(&mut signals, &tx))
        .map_err(|e| HotkeyError::Thread(e.to_string()))?;
    Ok(rx)
}

fn forward_signals(signals: &mut Signals, tx: &Sender<HotkeyEvent>) {
    for signal in signals.forever() {
        let event = match signal {
            SIGUSR1 => HotkeyEvent::Trigger,
            SIGHUP => HotkeyEvent::Reload,
            SIGTERM | SIGINT => HotkeyEvent::Shutdown,
            _ => continue,
        };
        if tx.send(event).is_err() {
            break; // receiver dropped — daemon is shutting down
        }
    }
}
