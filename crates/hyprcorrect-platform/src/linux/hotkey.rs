//! Global trigger via a Hyprland inline keybind + `SIGUSR1`.
//!
//! At startup the daemon adds an inline Hyprland keybind via
//! `hyprctl keyword bind = …, exec, pkill -SIGUSR1 -x hyprcorrect`.
//! Hyprland intercepts the chord — terminals and other focused apps
//! never see it — and runs the `pkill`, which raises `SIGUSR1` on the
//! daemon. Each signal arrives as `()` on the returned channel.
//!
//! Hyprland-specific. The cross-compositor route is the
//! `GlobalShortcuts` portal (DESIGN.md); that has its own auto-bind
//! limitation on `xdg-desktop-portal-hyprland` today, so we'll revisit
//! it together with M3's portable backends.

use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};

use signal_hook::consts::SIGUSR1;
use signal_hook::iterator::Signals;

/// An error registering the Hyprland keybind or signal handler.
#[derive(Debug, thiserror::Error)]
pub enum HotkeyError {
    /// `hyprctl` could not bind the trigger chord.
    #[error("hyprctl could not bind the trigger chord: {0}")]
    Hyprctl(String),
    /// Could not install the `SIGUSR1` handler.
    #[error("could not install SIGUSR1 handler: {0}")]
    Signal(String),
    /// Could not spawn the signal-listener thread.
    #[error("could not spawn the signal-listener thread: {0}")]
    Thread(String),
}

/// Start the Hyprland-bound trigger.
///
/// Adds an inline `bind` via `hyprctl keyword` and installs a `SIGUSR1`
/// handler. Each activation arrives as `()` on the returned receiver.
///
/// The trigger letter is taken from `$HYPRCORRECT_TRIGGER` (default
/// `F`); the chord (Super+Ctrl+Shift+Alt) is fixed.
///
/// # Errors
///
/// See [`HotkeyError`].
pub fn start() -> Result<Receiver<()>, HotkeyError> {
    let letter = std::env::var("HYPRCORRECT_TRIGGER").unwrap_or_else(|_| "F".to_string());
    let upper = letter.to_uppercase();
    let bind_value = format!("SUPER CTRL SHIFT ALT, {upper}, exec, pkill -SIGUSR1 -x hyprcorrect");

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

    let mut signals = Signals::new([SIGUSR1]).map_err(|e| HotkeyError::Signal(e.to_string()))?;
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("hyprcorrect-signal".into())
        .spawn(move || forward_signals(&mut signals, &tx))
        .map_err(|e| HotkeyError::Thread(e.to_string()))?;

    Ok(rx)
}

fn forward_signals(signals: &mut Signals, tx: &Sender<()>) {
    for signal in signals.forever() {
        if signal == SIGUSR1 && tx.send(()).is_err() {
            break; // receiver dropped — daemon is shutting down
        }
    }
}
