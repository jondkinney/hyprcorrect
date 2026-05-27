//! LLM-backed correction provider (M4).
//!
//! Currently wires the Anthropic Messages API only — the `backend`
//! field on [`crate::LlmConfig`] is read but only `"anthropic"` is
//! implemented. Synchronous on purpose: the daemon's main loop calls
//! this from the trigger handler and we expect ~1s round-trip; an
//! async runtime would be overkill.
//!
//! Construction reads the API key out of the OS keychain via
//! [`crate::secrets`]. Missing key → `Err(LlmError::NoApiKey)` — the
//! daemon falls back to the offline provider so the trigger never
//! silently no-ops.

use std::time::Duration;

use crate::secrets;

/// The keyring entry name the prefs UI writes to and the daemon
/// reads from. Kept in lock-step with the prefs constant.
const ANTHROPIC_KEY_NAME: &str = "llm.anthropic";

const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 1024;

const SYSTEM_PROMPT: &str = "You are a spelling, typo, and minor-grammar corrector. Return ONLY the \
     corrected version of the user's text — no preamble, no commentary, no \
     quotation marks. Preserve the user's voice, register, and punctuation \
     style. If the text is already fine, return it unchanged.";

const WORD_SYSTEM_PROMPT: &str = "You correct ONE word at a time using sentence context. The \
     user gives you a SENTENCE and one WORD from it to correct. Return ONLY the corrected \
     version of that word — nothing else: no quotes, no punctuation, no commentary, no rest \
     of the sentence. Use the rest of the sentence to disambiguate homophones \
     (their/there/they're, its/it's, your/you're, etc.) and to pick the right fix for typos. \
     Preserve the original casing of the word's first letter. If the word is already correct \
     in context, return it unchanged.";

/// Errors from an LLM correction request.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// No API key is stored in the OS keychain under the expected entry.
    #[error("no API key for the LLM provider — set one in Preferences → Providers")]
    NoApiKey,
    /// The keychain itself returned an error.
    #[error("keychain: {0}")]
    Keychain(String),
    /// The configured backend ID isn't one we support yet.
    #[error("unsupported LLM backend: {0}")]
    UnsupportedBackend(String),
    /// The network request itself failed (DNS / TLS / non-2xx, …).
    #[error("LLM request failed: {0}")]
    Request(String),
    /// We reached the API but couldn't read what came back.
    #[error("LLM response was unparseable: {0}")]
    Response(String),
}

/// The LLM correction provider.
pub struct LlmProvider {
    api_key: String,
    model: String,
}

impl std::fmt::Debug for LlmProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the API key — Debug is used in tests and logs.
        f.debug_struct("LlmProvider")
            .field("model", &self.model)
            .field("api_key", &"[redacted]")
            .finish()
    }
}

impl LlmProvider {
    /// Build the provider from the user's [`crate::LlmConfig`] —
    /// reads the API key out of the OS keychain.
    ///
    /// # Errors
    ///
    /// See [`LlmError`].
    pub fn from_config(llm: &crate::LlmConfig) -> Result<Self, LlmError> {
        if llm.backend != "anthropic" {
            return Err(LlmError::UnsupportedBackend(llm.backend.clone()));
        }
        let api_key = secrets::get(ANTHROPIC_KEY_NAME)
            .map_err(|e| LlmError::Keychain(e.to_string()))?
            .ok_or(LlmError::NoApiKey)?;
        Ok(Self {
            api_key,
            model: llm.model.clone(),
        })
    }

    /// Rewrite `text` into its corrected form. Returns the corrected
    /// string verbatim; callers compare against the input to decide
    /// whether an edit is needed.
    ///
    /// # Errors
    ///
    /// See [`LlmError`].
    pub fn rewrite(&self, text: &str) -> Result<String, LlmError> {
        if text.trim().is_empty() {
            return Ok(text.to_string());
        }
        self.request(SYSTEM_PROMPT, text.to_string())
    }

    /// Correct a single word using the surrounding sentence as
    /// context. The LLM is told to return ONLY the corrected word,
    /// not the rest of the sentence — callers splice it back in at
    /// the caret. Good for homophones and context-dependent typos
    /// where the offline spellbook either can't see the error
    /// (their/there) or picks the wrong nearest neighbor.
    ///
    /// # Errors
    ///
    /// See [`LlmError`].
    pub fn fix_word_in_context(&self, sentence: &str, word: &str) -> Result<String, LlmError> {
        if word.trim().is_empty() {
            return Ok(word.to_string());
        }
        let content = format!("SENTENCE: {sentence}\nWORD: {word}");
        let corrected = self.request(WORD_SYSTEM_PROMPT, content)?;
        // Defensive: strip any wrapping whitespace or quotation
        // marks the LLM may include despite the system prompt
        // telling it not to.
        Ok(corrected
            .trim()
            .trim_matches(|c: char| c == '"' || c == '\'')
            .to_string())
    }

    fn request(&self, system: &str, content: String) -> Result<String, LlmError> {
        let agent: ureq::Agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(20))
            .build();
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": DEFAULT_MAX_TOKENS,
            "system": system,
            "messages": [{
                "role": "user",
                "content": content,
            }],
        });
        let response = agent
            .post(ANTHROPIC_URL)
            .set("x-api-key", &self.api_key)
            .set("anthropic-version", ANTHROPIC_VERSION)
            .set("content-type", "application/json")
            .send_json(body)
            .map_err(|e| LlmError::Request(e.to_string()))?;
        let json: serde_json::Value = response
            .into_json()
            .map_err(|e| LlmError::Response(e.to_string()))?;
        // Anthropic's response: { "content": [ { "type": "text", "text": "..." }, ... ], ... }
        let corrected = json["content"]
            .as_array()
            .and_then(|parts| {
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .next()
            })
            .ok_or_else(|| LlmError::Response("no `content[*].text` in response".into()))?;
        Ok(corrected.trim_end_matches('\n').to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LlmConfig;

    #[test]
    fn unsupported_backend_is_rejected_cleanly() {
        let cfg = LlmConfig {
            backend: "openai".into(),
            model: "gpt-5".into(),
        };
        match LlmProvider::from_config(&cfg) {
            Err(LlmError::UnsupportedBackend(name)) => assert_eq!(name, "openai"),
            other => panic!("expected UnsupportedBackend, got {other:?}"),
        }
    }
}
