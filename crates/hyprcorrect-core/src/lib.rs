//! Core logic for hyprcorrect: configuration, the keystroke buffer,
//! replacement planning, and the correction-provider interface.
//!
//! This crate has no GUI or platform dependencies. See `DESIGN.md` at
//! the repository root for the architecture.

pub mod buffer;
pub mod config;
pub mod providers;
pub mod replace;

pub use buffer::{Buffer, Key, LastWord};
pub use providers::{Context, Correction, CorrectionProvider};
pub use replace::{Edit, plan_word_replacement};

/// hyprcorrect's version string, surfaced by the CLI and the About pane.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
