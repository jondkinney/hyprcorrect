//! The correction-provider interface.
//!
//! A [`CorrectionProvider`] checks a slice of text and reports the
//! corrections it would make. The shipped implementations — Harper
//! (bundled, offline), an LLM backend, and a LanguageTool HTTP client —
//! land in milestones M1 and M4. See the "Correction providers" section
//! of `DESIGN.md`.

use std::ops::Range;

use async_trait::async_trait;

/// A spelling/typo correction backend.
#[async_trait]
pub trait CorrectionProvider: Send + Sync {
    /// Check `text` and return the corrections this provider would make.
    ///
    /// `ctx` carries the focused-application id and the text's locale,
    /// which contextual providers may use.
    async fn check(&self, text: &str, ctx: &Context) -> Result<Vec<Correction>, Error>;
}

/// A single suggested fix for one span of the checked text.
#[derive(Debug, Clone)]
pub struct Correction {
    /// Byte range of the flagged word within the checked text.
    pub span: Range<usize>,
    /// The original (flagged) text covered by `span`.
    pub original: String,
    /// Replacement candidates, best first.
    pub suggestions: Vec<String>,
}

/// Context passed to a provider alongside the text to check.
#[derive(Debug, Clone, Default)]
pub struct Context {
    /// The focused application's identifier, when known — the Wayland
    /// app id or the macOS bundle id.
    pub app_id: Option<String>,
    /// BCP-47 locale of the text, e.g. `en-US`.
    pub locale: Option<String>,
}

/// An error returned by a [`CorrectionProvider`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The provider could not be reached, or the request itself failed.
    #[error("correction request failed: {0}")]
    Request(String),
    /// The provider's response could not be understood.
    #[error("malformed correction response: {0}")]
    Response(String),
}
