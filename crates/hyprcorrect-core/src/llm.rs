//! LLM-backed correction provider (M4).
//!
//! Wires three request shapes that between them cover every hosted
//! backend the prefs UI offers:
//!
//! * **Anthropic** Messages API (`anthropic`).
//! * **OpenAI-compatible** Chat Completions — `openai`, `openrouter`,
//!   `mistral`, `groq`, `deepseek`, `xai`, and `openai-compatible` (a
//!   user-supplied base URL for a local Ollama / LM Studio server or any
//!   other OpenAI-style endpoint). One code path, different base URLs.
//! * **Gemini** `generateContent` (`gemini`).
//!
//! Synchronous on purpose: the daemon's main loop calls this from the
//! trigger handler and we expect ~1s round-trip; an async runtime would
//! be overkill.
//!
//! Construction reads the API key out of the OS keychain via
//! [`crate::secrets`]. A missing key → `Err(LlmError::NoApiKey)` for the
//! cloud backends (local endpoints accept an empty key) — the daemon
//! then falls back to LanguageTool/Spellbook so the trigger never
//! silently no-ops.

use std::time::Duration;

use crate::runtime::WordSuggestions;
use crate::secrets;

/// A resolved backend: which request shape to use and where to send it.
/// Produced by [`resolve_backend`] from the config's `backend` string
/// (and `base_url`, for the custom endpoint).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Backend {
    /// Anthropic Messages API.
    Anthropic,
    /// Google Gemini `generateContent`.
    Gemini,
    /// Any OpenAI-style `/chat/completions` endpoint.
    OpenAiCompatible {
        /// Base URL up to but not including `/chat/completions`.
        base_url: String,
        /// Send the token cap as `max_completion_tokens` (OpenAI's
        /// newer param, required by its `o*` reasoning models) rather
        /// than the legacy `max_tokens` other vendors still expect.
        max_completion_tokens: bool,
        /// Whether a non-empty API key is mandatory. Cloud vendors: yes.
        /// The custom/local endpoint: no (Ollama et al. need none).
        requires_key: bool,
    },
}

/// An OpenAI-compatible cloud backend at `base`. `completion_tokens`
/// picks the token-cap field name (see [`Backend::OpenAiCompatible`]).
fn openai_cloud(base: &str, completion_tokens: bool) -> Backend {
    Backend::OpenAiCompatible {
        base_url: base.to_string(),
        max_completion_tokens: completion_tokens,
        requires_key: true,
    }
}

/// Map a `(backend, base_url)` pair to a [`Backend`], or `None` if the
/// backend id isn't one we implement. Case-insensitive. The custom
/// `openai-compatible` (alias `custom`) backend resolves only when a
/// non-empty base URL is supplied.
fn resolve_backend(backend: &str, base_url: Option<&str>) -> Option<Backend> {
    match backend.trim().to_ascii_lowercase().as_str() {
        "anthropic" => Some(Backend::Anthropic),
        "gemini" => Some(Backend::Gemini),
        // OpenAI proper takes `max_completion_tokens`; the rest still
        // want plain `max_tokens`.
        "openai" => Some(openai_cloud("https://api.openai.com/v1", true)),
        "openrouter" => Some(openai_cloud("https://openrouter.ai/api/v1", false)),
        "mistral" => Some(openai_cloud("https://api.mistral.ai/v1", false)),
        "groq" => Some(openai_cloud("https://api.groq.com/openai/v1", false)),
        "deepseek" => Some(openai_cloud("https://api.deepseek.com/v1", false)),
        "xai" => Some(openai_cloud("https://api.x.ai/v1", false)),
        "openai-compatible" | "custom" => {
            let base = base_url.map(str::trim).filter(|s| !s.is_empty())?;
            Some(Backend::OpenAiCompatible {
                base_url: base.trim_end_matches('/').to_string(),
                max_completion_tokens: false,
                requires_key: false,
            })
        }
        _ => None,
    }
}

/// Whether `backend` has a working integration. The named cloud
/// backends and the custom `openai-compatible` endpoint are all wired;
/// an unrecognized id is not (the prefs UI flags it inline and the
/// daemon falls back to the offline Spellbook). Independent of whether a
/// base URL is actually set — that's surfaced separately in the UI.
pub fn is_backend_wired(backend: &str) -> bool {
    // Pass a dummy base URL so the custom endpoint counts as wired even
    // before the user fills its URL in.
    resolve_backend(backend, Some("https://example.invalid")).is_some()
}

/// OS-keychain entry name for a backend's API key: `llm.<backend>`. The
/// prefs UI writes here and the daemon reads here — kept in lock-step.
/// Anthropic's historical key lived at `llm.anthropic`, which is exactly
/// `key_name("anthropic")`, so existing keys keep working unchanged.
pub fn key_name(backend: &str) -> String {
    format!("llm.{backend}")
}

const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const GEMINI_URL_PREFIX: &str = "https://generativelanguage.googleapis.com/v1beta/models";
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

const ALTERNATIVES_SYSTEM_PROMPT: &str = "You are a spelling, typo, and minor-grammar corrector. \
     Correct the user's text and reply with ONLY a JSON object — no preamble, no commentary, no code \
     fences — shaped exactly like: {\"corrected\": \"<the corrected text>\", \"alternatives\": \
     [{\"word\": \"<a word you changed>\", \"options\": [\"best\", \"next\", \"...\"]}]}. Include an \
     `alternatives` entry only for words you changed; give 3 to 5 ranked options each, best first, with \
     the option you actually used in `corrected` listed first. Use sentence context for homophones \
     (their/there/they're, its/it's, your/you're). Preserve the user's voice, register, casing, and \
     punctuation. If the text is already correct, return it unchanged with an empty `alternatives` array.";

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
    backend: Backend,
    api_key: String,
    model: String,
}

impl std::fmt::Debug for LlmProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the API key — Debug is used in tests and logs.
        f.debug_struct("LlmProvider")
            .field("backend", &self.backend)
            .field("model", &self.model)
            .field("api_key", &"[redacted]")
            .finish()
    }
}

impl LlmProvider {
    /// Build the provider from the user's [`crate::LlmConfig`] —
    /// resolves the backend and reads its API key out of the OS
    /// keychain.
    ///
    /// # Errors
    ///
    /// [`LlmError::UnsupportedBackend`] for an unknown backend id (or a
    /// custom `openai-compatible` backend with no base URL set), and
    /// [`LlmError::NoApiKey`] when a cloud backend has no stored key.
    /// See [`LlmError`].
    pub fn from_config(llm: &crate::LlmConfig) -> Result<Self, LlmError> {
        let backend = resolve_backend(&llm.backend, llm.base_url.as_deref())
            .ok_or_else(|| LlmError::UnsupportedBackend(llm.backend.clone()))?;
        let requires_key = match &backend {
            Backend::OpenAiCompatible { requires_key, .. } => *requires_key,
            // Anthropic and Gemini always need a key.
            Backend::Anthropic | Backend::Gemini => true,
        };
        let api_key = secrets::get(&key_name(&llm.backend))
            .map_err(|e| LlmError::Keychain(e.to_string()))?
            .unwrap_or_default();
        if requires_key && api_key.is_empty() {
            return Err(LlmError::NoApiKey);
        }
        Ok(Self {
            backend,
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

    /// Correct `text` AND return ranked alternative spellings for each
    /// word the model changed, in one structured (JSON) call — this
    /// powers the review popup's per-word suggestion dropdown. Returns
    /// the corrected sentence and the alternatives (best-first, the
    /// applied option first).
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Response`] if the reply isn't the expected
    /// JSON — the daemon falls back to `rewrite` + offline suggestions,
    /// so the dropdown still appears.
    pub fn rewrite_with_alternatives(
        &self,
        text: &str,
    ) -> Result<(String, Vec<WordSuggestions>), LlmError> {
        if text.trim().is_empty() {
            return Ok((text.to_string(), Vec::new()));
        }
        let reply = self.request(ALTERNATIVES_SYSTEM_PROMPT, text.to_string())?;
        parse_alternatives(&reply)
    }

    /// Dispatch one correction request to whichever API shape this
    /// provider's backend uses. `system` is the instruction prompt,
    /// `content` the user payload. Returns the model's reply text with a
    /// trailing newline trimmed.
    fn request(&self, system: &str, content: String) -> Result<String, LlmError> {
        match &self.backend {
            Backend::Anthropic => self.request_anthropic(system, content),
            Backend::Gemini => self.request_gemini(system, content),
            Backend::OpenAiCompatible {
                base_url,
                max_completion_tokens,
                ..
            } => self.request_openai(base_url, *max_completion_tokens, system, content),
        }
    }

    fn request_anthropic(&self, system: &str, content: String) -> Result<String, LlmError> {
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": DEFAULT_MAX_TOKENS,
            "system": system,
            "messages": [{ "role": "user", "content": content }],
        });
        let json = agent()
            .post(ANTHROPIC_URL)
            .set("x-api-key", &self.api_key)
            .set("anthropic-version", ANTHROPIC_VERSION)
            .set("content-type", "application/json")
            .send_json(body)
            .map_err(|e| LlmError::Request(e.to_string()))?
            .into_json::<serde_json::Value>()
            .map_err(|e| LlmError::Response(e.to_string()))?;
        parse_anthropic_reply(&json)
    }

    /// OpenAI-style Chat Completions — shared by every OpenAI-compatible
    /// backend (OpenAI, OpenRouter, Mistral, Groq, DeepSeek, xAI, and a
    /// custom/local endpoint). `base` is the URL up to `/chat/completions`.
    fn request_openai(
        &self,
        base: &str,
        max_completion_tokens: bool,
        system: &str,
        content: String,
    ) -> Result<String, LlmError> {
        let token_field = if max_completion_tokens {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": content },
            ],
        });
        body[token_field] = DEFAULT_MAX_TOKENS.into();

        let url = format!("{base}/chat/completions");
        let mut req = agent().post(&url).set("content-type", "application/json");
        // Local endpoints (Ollama, LM Studio) take no key; only send the
        // header when we actually have one.
        if !self.api_key.is_empty() {
            req = req.set("authorization", &format!("Bearer {}", self.api_key));
        }
        let json = req
            .send_json(body)
            .map_err(|e| LlmError::Request(e.to_string()))?
            .into_json::<serde_json::Value>()
            .map_err(|e| LlmError::Response(e.to_string()))?;
        parse_openai_reply(&json)
    }

    fn request_gemini(&self, system: &str, content: String) -> Result<String, LlmError> {
        // Gemini puts the model in the path and the key in a header.
        let url = format!("{GEMINI_URL_PREFIX}/{}:generateContent", self.model);
        let body = serde_json::json!({
            "system_instruction": { "parts": [{ "text": system }] },
            "contents": [{ "parts": [{ "text": content }] }],
            "generationConfig": { "maxOutputTokens": DEFAULT_MAX_TOKENS },
        });
        let json = agent()
            .post(&url)
            .set("x-goog-api-key", &self.api_key)
            .set("content-type", "application/json")
            .send_json(body)
            .map_err(|e| LlmError::Request(e.to_string()))?
            .into_json::<serde_json::Value>()
            .map_err(|e| LlmError::Response(e.to_string()))?;
        parse_gemini_reply(&json)
    }
}

/// Shared HTTP agent with the per-request timeout we expect of an LLM
/// round-trip.
fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(20))
        .build()
}

/// Pull the text out of an Anthropic Messages reply:
/// `{ "content": [ { "type": "text", "text": "..." }, ... ] }`.
fn parse_anthropic_reply(json: &serde_json::Value) -> Result<String, LlmError> {
    let text = json["content"]
        .as_array()
        .and_then(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .next()
        })
        .ok_or_else(|| LlmError::Response("no `content[*].text` in response".into()))?;
    Ok(text.trim_end_matches('\n').to_string())
}

/// Pull the text out of an OpenAI Chat Completions reply:
/// `{ "choices": [ { "message": { "content": "..." } } ] }`.
fn parse_openai_reply(json: &serde_json::Value) -> Result<String, LlmError> {
    let text = json["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| LlmError::Response("no `choices[0].message.content` in response".into()))?;
    Ok(text.trim_end_matches('\n').to_string())
}

/// Pull the text out of a Gemini `generateContent` reply:
/// `{ "candidates": [ { "content": { "parts": [ { "text": "..." } ] } } ] }`.
fn parse_gemini_reply(json: &serde_json::Value) -> Result<String, LlmError> {
    let text = json["candidates"][0]["content"]["parts"]
        .as_array()
        .and_then(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .next()
        })
        .ok_or_else(|| {
            LlmError::Response("no `candidates[0].content.parts[*].text` in response".into())
        })?;
    Ok(text.trim_end_matches('\n').to_string())
}

/// Parse the JSON reply from [`LlmProvider::rewrite_with_alternatives`]
/// into the corrected text and per-word alternatives. Tolerates a model
/// that wraps the object in prose or ``` fences by slicing to the outer
/// braces first.
fn parse_alternatives(reply: &str) -> Result<(String, Vec<WordSuggestions>), LlmError> {
    let json = json_object_slice(reply);
    let v: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| LlmError::Response(format!("alternatives JSON: {e}")))?;
    let corrected = v["corrected"]
        .as_str()
        .ok_or_else(|| LlmError::Response("no `corrected` string in response".into()))?
        .to_string();
    let mut alternatives = Vec::new();
    if let Some(arr) = v["alternatives"].as_array() {
        for item in arr {
            let Some(word) = item["word"].as_str() else {
                continue;
            };
            let options: Vec<String> = item["options"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|o| o.as_str().map(str::to_string))
                .collect();
            if !options.is_empty() {
                alternatives.push(WordSuggestions {
                    word: word.to_string(),
                    options,
                });
            }
        }
    }
    Ok((corrected, alternatives))
}

/// The substring from the first `{` to the last `}`, so a stray prose
/// preamble or ```json fence around the object doesn't break parsing.
fn json_object_slice(s: &str) -> &str {
    match (s.find('{'), s.rfind('}')) {
        (Some(a), Some(b)) if b >= a => &s[a..=b],
        _ => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LlmConfig;

    #[test]
    fn parses_alternatives_reply() {
        let reply = r#"{"corrected":"the quick brown fox",
            "alternatives":[
                {"word":"the","options":["the","then","they"]},
                {"word":"brown","options":["brown","browne","crown"]}
            ]}"#;
        let (corrected, alts) = parse_alternatives(reply).unwrap();
        assert_eq!(corrected, "the quick brown fox");
        assert_eq!(alts.len(), 2);
        assert_eq!(alts[0].word, "the");
        assert_eq!(alts[0].options, vec!["the", "then", "they"]);
        assert_eq!(alts[1].word, "brown");
    }

    #[test]
    fn tolerates_code_fences_and_preamble() {
        let reply = "Here you go:\n```json\n{\"corrected\":\"hi there\",\"alternatives\":[]}\n```";
        let (corrected, alts) = parse_alternatives(reply).unwrap();
        assert_eq!(corrected, "hi there");
        assert!(alts.is_empty());
    }

    #[test]
    fn non_json_reply_is_an_error() {
        assert!(parse_alternatives("sorry, I cannot do that").is_err());
    }

    #[test]
    fn unsupported_backend_is_rejected_cleanly() {
        // An unrecognized backend is rejected *before* any keychain
        // access, so this test never touches the OS keychain.
        let cfg = LlmConfig {
            backend: "made-up-vendor".into(),
            model: "whatever".into(),
            base_url: None,
        };
        match LlmProvider::from_config(&cfg) {
            Err(LlmError::UnsupportedBackend(name)) => assert_eq!(name, "made-up-vendor"),
            other => panic!("expected UnsupportedBackend, got {other:?}"),
        }
    }

    #[test]
    fn custom_endpoint_without_base_url_is_unsupported() {
        // The custom backend needs a base URL to resolve; without one
        // it's treated as unsupported (→ silent offline fallback) and,
        // like above, never reaches the keychain.
        let cfg = LlmConfig {
            backend: "openai-compatible".into(),
            model: "llama3.1".into(),
            base_url: None,
        };
        assert!(matches!(
            LlmProvider::from_config(&cfg),
            Err(LlmError::UnsupportedBackend(_))
        ));
    }

    #[test]
    fn key_name_and_wiring_are_stable() {
        // Anthropic's key keeps its historical entry name, so existing
        // keys survive the move to per-backend keys.
        assert_eq!(key_name("anthropic"), "llm.anthropic");
        assert_eq!(key_name("openai"), "llm.openai");
        // Every backend the prefs dropdown offers is wired now.
        for b in [
            "anthropic",
            "openai",
            "gemini",
            "openrouter",
            "mistral",
            "groq",
            "deepseek",
            "xai",
            "openai-compatible",
        ] {
            assert!(is_backend_wired(b), "{b} should be wired");
        }
        // Case-insensitive, and unknown ids are not wired.
        assert!(is_backend_wired("OpenAI"));
        assert!(!is_backend_wired("made-up-vendor"));
    }

    #[test]
    fn resolve_backend_picks_the_right_shape_and_url() {
        // OpenAI proper uses the newer token-cap field; the others don't.
        assert_eq!(
            resolve_backend("openai", None),
            Some(Backend::OpenAiCompatible {
                base_url: "https://api.openai.com/v1".into(),
                max_completion_tokens: true,
                requires_key: true,
            })
        );
        assert_eq!(
            resolve_backend("groq", None),
            Some(Backend::OpenAiCompatible {
                base_url: "https://api.groq.com/openai/v1".into(),
                max_completion_tokens: false,
                requires_key: true,
            })
        );
        assert_eq!(resolve_backend("anthropic", None), Some(Backend::Anthropic));
        assert_eq!(resolve_backend("gemini", None), Some(Backend::Gemini));
        // Custom endpoint adopts the supplied URL (trailing slash
        // trimmed) and needs no key.
        assert_eq!(
            resolve_backend("openai-compatible", Some("http://localhost:11434/v1/")),
            Some(Backend::OpenAiCompatible {
                base_url: "http://localhost:11434/v1".into(),
                max_completion_tokens: false,
                requires_key: false,
            })
        );
        assert_eq!(resolve_backend("openai-compatible", Some("  ")), None);
        assert_eq!(resolve_backend("nope", Some("http://x")), None);
    }

    #[test]
    fn parses_each_provider_reply_shape() {
        let anthropic = serde_json::json!({
            "content": [{ "type": "text", "text": "fixed\n" }]
        });
        assert_eq!(parse_anthropic_reply(&anthropic).unwrap(), "fixed");

        let openai = serde_json::json!({
            "choices": [{ "message": { "role": "assistant", "content": "fixed\n" } }]
        });
        assert_eq!(parse_openai_reply(&openai).unwrap(), "fixed");

        let gemini = serde_json::json!({
            "candidates": [{ "content": { "parts": [{ "text": "fixed\n" }] } }]
        });
        assert_eq!(parse_gemini_reply(&gemini).unwrap(), "fixed");

        // A shape mismatch is a clean Response error, not a panic.
        assert!(parse_openai_reply(&anthropic).is_err());
        assert!(parse_gemini_reply(&openai).is_err());
    }
}
