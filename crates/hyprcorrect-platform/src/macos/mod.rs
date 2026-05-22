//! macOS platform backend.
//!
//! - Capture: a listen-only `CGEventTap`.
//! - Synthetic input: `CGEvent` keyboard events.
//! - Hotkeys: `RegisterEventHotKey`.
//! - Menu bar: `NSStatusItem`.
//!
//! Implemented from milestone M2. See `DESIGN.md`.
