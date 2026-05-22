//! Platform layer for hyprcorrect: observe-only key capture, synthetic
//! input, global hotkeys, frontmost-application detection, and the tray.
//!
//! Each capability sits behind a common interface backed by a per-OS
//! implementation. See the "Platform layer" section of `DESIGN.md`.

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

/// Human-readable name of the platform backend compiled into this build.
pub fn backend_name() -> &'static str {
    if cfg!(target_os = "linux") {
        "linux/wayland"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows (stub)"
    } else {
        "unsupported"
    }
}
