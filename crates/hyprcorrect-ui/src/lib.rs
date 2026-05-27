//! The hyprcorrect GUI: the egui preferences window and (later, M4)
//! the keyboard-navigable suggestion popup.
//!
//! All UI in this crate is platform-independent egui — Linux and
//! macOS share the same code. The `run_preferences` entry handles a
//! best-effort singleton lock so double-clicking "Open Preferences…"
//! in the tray doesn't open two windows.
//!
//! See the "Configuration & GUI" section of `DESIGN.md`.

mod apps;
#[cfg(target_os = "linux")]
pub mod autostart;
mod docker;
pub mod icon;
mod prefs;
mod review;

/// Open the preferences window. Blocks until the user closes it.
///
/// If another prefs window is already open, this returns immediately
/// after best-effort asking the existing one to focus itself.
pub fn run_preferences() {
    prefs::run();
}

/// Open the review popup for the daemon's pending review request.
/// Reads the request from the runtime file, shows the popup, and
/// signals the daemon on exit so the apply happens against the
/// originating window. Blocks until the user closes the popup.
pub fn run_review() {
    review::run();
}
