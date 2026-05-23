//! Core logic for hyprcorrect: configuration, the keystroke buffer,
//! replacement planning, and the correction-provider interface.
//!
//! This crate has no GUI or platform dependencies. See `DESIGN.md` at
//! the repository root for the architecture.

pub mod buffer;
pub mod chord;
pub mod config;
pub mod providers;
pub mod replace;
pub mod runtime;
pub mod secrets;

pub use buffer::{Buffer, Key, LastWord};
pub use chord::{Chord, ChordError};
pub use config::{
    Behavior, Config, ConfigError, Hotkeys, LanguageToolConfig, LlmConfig, Privacy, ProviderId,
    Providers,
};
pub use providers::{Context, Correction, CorrectionProvider, OfflineProvider};
pub use replace::{Edit, plan_word_replacement};

/// hyprcorrect's version string, surfaced by the CLI and the About pane.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
