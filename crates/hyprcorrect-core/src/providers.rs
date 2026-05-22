//! The correction-provider interface and the bundled offline provider.
//!
//! [`CorrectionProvider`] is the interface; [`OfflineProvider`] is the
//! bundled default — a Hunspell-compatible spell-checker (`spellbook`)
//! that runs fully in-process. Network providers (an LLM backend, a
//! LanguageTool HTTP client) land in milestones M4 and M5. See the
//! "Correction providers" section of `DESIGN.md`.

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
    /// A provider could not be initialized — e.g. a malformed dictionary.
    #[error("could not initialize correction provider: {0}")]
    Init(String),
    /// The provider could not be reached, or the request itself failed.
    #[error("correction request failed: {0}")]
    Request(String),
    /// The provider's response could not be understood.
    #[error("malformed correction response: {0}")]
    Response(String),
}

/// The bundled offline correction provider.
///
/// Wraps [`spellbook`], a pure-Rust, Hunspell-compatible spell-checker,
/// over an English dictionary. Fully local and instant — this is the
/// provider behind `fix-word`. Contextual fixes route elsewhere.
pub struct OfflineProvider {
    dictionary: spellbook::Dictionary,
}

impl OfflineProvider {
    /// Build the provider from Hunspell `.aff` and `.dic` data.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Init`] if the dictionary fails to parse.
    pub fn from_hunspell(aff: &str, dic: &str) -> Result<Self, Error> {
        let dictionary =
            spellbook::Dictionary::new(aff, dic).map_err(|e| Error::Init(format!("{e:?}")))?;
        Ok(Self { dictionary })
    }

    /// Build the provider from the bundled `en_US` dictionary.
    ///
    /// The dictionary is vendored from wooorm/dictionaries (the `en`
    /// dictionary, derived from SCOWL) and embedded at compile time; its
    /// license is at `dictionaries/en_US/LICENSE`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Init`] if the bundled dictionary fails to parse,
    /// which would indicate a packaging bug.
    pub fn en_us() -> Result<Self, Error> {
        Self::from_hunspell(
            include_str!("../dictionaries/en_US/en_US.aff"),
            include_str!("../dictionaries/en_US/en_US.dic"),
        )
    }

    /// Spell-check `text`, returning one [`Correction`] per misspelled
    /// word. This is the synchronous core behind the async trait method.
    pub fn check_text(&self, text: &str) -> Vec<Correction> {
        let mut corrections = Vec::new();
        for (offset, word) in words(text) {
            if self.dictionary.check(word) {
                continue;
            }
            let mut suggestions = Vec::new();
            self.dictionary.suggest(word, &mut suggestions);
            corrections.push(Correction {
                span: offset..offset + word.len(),
                original: word.to_string(),
                suggestions,
            });
        }
        corrections
    }
}

#[async_trait]
impl CorrectionProvider for OfflineProvider {
    async fn check(&self, text: &str, _ctx: &Context) -> Result<Vec<Correction>, Error> {
        Ok(self.check_text(text))
    }
}

/// Iterate the whitespace-delimited words of `text` as
/// `(byte offset, word)` pairs.
fn words(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            if let Some(s) = start.take() {
                out.push((s, &text[s..i]));
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        out.push((s, &text[s..]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // A tiny Hunspell dictionary: an empty `.aff` and a `.dic` of a few
    // words (its first line is the entry count).
    const TEST_AFF: &str = "";
    const TEST_DIC: &str = "5\nhello\nworld\nthe\nquick\nveneer\n";

    fn provider() -> OfflineProvider {
        OfflineProvider::from_hunspell(TEST_AFF, TEST_DIC).unwrap()
    }

    #[test]
    fn correct_words_produce_no_corrections() {
        assert!(provider().check_text("hello world").is_empty());
    }

    #[test]
    fn a_misspelling_is_flagged_with_suggestions() {
        let corrections = provider().check_text("helo");
        assert_eq!(corrections.len(), 1);
        assert_eq!(corrections[0].original, "helo");
        assert!(
            corrections[0].suggestions.iter().any(|s| s == "hello"),
            "expected 'hello' among suggestions, got {:?}",
            corrections[0].suggestions,
        );
    }

    #[test]
    fn correction_span_locates_the_word() {
        let corrections = provider().check_text("the helo");
        assert_eq!(corrections.len(), 1);
        // "helo" sits at bytes 4..8 of "the helo".
        assert_eq!(corrections[0].span, 4..8);
    }

    #[test]
    fn only_misspelled_words_are_reported() {
        let corrections = provider().check_text("the quick fakeword");
        assert_eq!(corrections.len(), 1);
        assert_eq!(corrections[0].original, "fakeword");
    }

    /// The real bundled en_US dictionary, parsed once for the tests below.
    static EN_US: std::sync::LazyLock<OfflineProvider> =
        std::sync::LazyLock::new(|| OfflineProvider::en_us().expect("bundled en_US parses"));

    #[test]
    fn en_us_accepts_common_words() {
        assert!(EN_US.check_text("the quick brown fox").is_empty());
    }

    #[test]
    fn en_us_flags_a_misspelling_with_the_right_fix() {
        let corrections = EN_US.check_text("teh");
        assert_eq!(corrections.len(), 1);
        assert!(
            corrections[0].suggestions.iter().any(|s| s == "the"),
            "expected 'the' among suggestions, got {:?}",
            corrections[0].suggestions,
        );
    }

    #[test]
    fn en_us_suggests_for_the_motivating_typo() {
        // The prototype's example — a real typo should yield suggestions.
        let corrections = EN_US.check_text("vernuer");
        assert_eq!(corrections.len(), 1);
        assert!(
            !corrections[0].suggestions.is_empty(),
            "expected suggestions for 'vernuer'",
        );
    }
}
