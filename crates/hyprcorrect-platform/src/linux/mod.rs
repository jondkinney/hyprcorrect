//! Linux / Wayland platform backend.
//!
//! - Capture: `evdev` (`/dev/input`), with `xkbcommon` mapping keycodes
//!   to characters for the user's keyboard layout.
//! - Synthetic input: the `virtual-keyboard-v1` Wayland protocol.
//! - Hotkeys: the `ashpd` GlobalShortcuts portal.
//! - Tray: `ksni`.
//!
//! Implemented from milestone M1. See `DESIGN.md`.
