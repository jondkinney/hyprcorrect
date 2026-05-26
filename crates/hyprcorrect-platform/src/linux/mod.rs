//! Linux / Wayland platform backend.
//!
//! - Capture: `evdev` (`/dev/input`), with `xkbcommon` mapping keycodes
//!   to characters for the user's keyboard layout.
//! - Synthetic input: `wtype` (the `virtual-keyboard-v1` Wayland
//!   protocol).
//! - Hotkeys: an inline `hyprctl keyword bind` whose `exec` raises
//!   `SIGUSR1` on the daemon. (Hyprland-specific by design; the
//!   `GlobalShortcuts` portal is the planned cross-compositor route —
//!   see `DESIGN.md`.)
//! - Focus: Hyprland's IPC event socket (`.socket2.sock`).
//! - Tray: `ksni`.
//!
//! Implemented from milestone M1. See `DESIGN.md`.

pub mod capture;
pub mod chord_capture;
pub mod clipboard;
pub mod emit;
pub mod focus;
pub mod hotkey;
pub mod tray;
