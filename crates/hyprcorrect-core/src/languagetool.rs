//! LanguageTool HTTP correction provider (M5).
//!
//! POSTs the text to a self-hosted LanguageTool server's
//! `/v2/check` endpoint and turns the JSON `matches` array into
//! [`crate::providers::Correction`]s.
//!
//! Off until the user enables it in Preferences → LanguageTool.
//! Bring-your-own server — the project does not bundle LanguageTool
//! itself (it's Java + dictionaries; would dwarf the crate).

use std::time::Duration;

use crate::LanguageToolConfig;
use crate::providers::Correction;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Errors from a LanguageTool check.
#[derive(Debug, thiserror::Error)]
pub enum LanguageToolError {
    /// The user hasn't enabled LanguageTool in the config.
    #[error("LanguageTool is disabled in the config")]
    Disabled,
    /// `url` field is empty.
    #[error("LanguageTool URL is empty")]
    NoUrl,
    /// Network or HTTP error reaching the server.
    #[error("LanguageTool request failed: {0}")]
    Request(String),
    /// Couldn't make sense of the response body.
    #[error("LanguageTool response was unparseable: {0}")]
    Response(String),
}

/// The LanguageTool HTTP correction provider.
#[derive(Debug, Clone)]
pub struct LanguageToolProvider {
    endpoint: String,
}

impl LanguageToolProvider {
    /// Build the provider from the user's [`LanguageToolConfig`].
    /// Returns `Err` cleanly when the config is disabled or empty —
    /// the daemon treats either as "fall back to spellbook".
    ///
    /// # Errors
    ///
    /// See [`LanguageToolError`].
    pub fn from_config(lt: &LanguageToolConfig) -> Result<Self, LanguageToolError> {
        if !lt.enabled {
            return Err(LanguageToolError::Disabled);
        }
        let url = lt.url.trim().trim_end_matches('/');
        if url.is_empty() {
            return Err(LanguageToolError::NoUrl);
        }
        Ok(Self {
            endpoint: format!("{url}/v2/check"),
        })
    }

    /// Check `text` against the LanguageTool server. Returns one
    /// [`Correction`] per match (deduplicated implicitly by
    /// LanguageTool's own ranking).
    ///
    /// # Errors
    ///
    /// See [`LanguageToolError`].
    pub fn check_text(&self, text: &str) -> Result<Vec<Correction>, LanguageToolError> {
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }
        let agent = ureq::AgentBuilder::new().timeout(REQUEST_TIMEOUT).build();
        // `level=picky` turns on LanguageTool's extra grammar/style rules
        // (more than the default set). Real-word confusions like
        // wear/where still need the server's optional n-gram data loaded.
        let response = agent
            .post(&self.endpoint)
            .send_form(&[("text", text), ("language", "en-US"), ("level", "picky")])
            .map_err(|e| LanguageToolError::Request(e.to_string()))?;
        let json: serde_json::Value = response
            .into_json()
            .map_err(|e| LanguageToolError::Response(e.to_string()))?;
        Ok(parse_matches(&json, text))
    }
}

fn parse_matches(json: &serde_json::Value, text: &str) -> Vec<Correction> {
    let Some(matches) = json["matches"].as_array() else {
        return Vec::new();
    };
    // LanguageTool reports `offset`/`length` in CHARACTERS, not bytes
    // (Java string positions — UTF-16 units, which equal Unicode
    // codepoints for the BMP, covering ordinary text plus the emoji
    // marker U+FFFC). Build a char-index → byte-offset map once so we can
    // translate each match; treating the char offsets as byte offsets
    // mis-applied every correction after the first multi-byte char (an
    // accent, the marker, …).
    let char_to_byte: Vec<usize> = text
        .char_indices()
        .map(|(b, _)| b)
        .chain(std::iter::once(text.len()))
        .collect();
    let last_char = char_to_byte.len() - 1; // == text.chars().count()
    let mut out = Vec::with_capacity(matches.len());
    for m in matches {
        let offset = match m["offset"].as_u64() {
            Some(n) => n as usize,
            None => continue,
        };
        let length = match m["length"].as_u64() {
            Some(n) => n as usize,
            None => continue,
        };
        if length == 0 {
            continue;
        }
        let char_end = offset.saturating_add(length);
        if char_end > last_char {
            continue;
        }
        let (start, end) = (char_to_byte[offset], char_to_byte[char_end]);
        let suggestions: Vec<String> = m["replacements"]
            .as_array()
            .into_iter()
            .flat_map(|a| a.iter())
            .filter_map(|r| r["value"].as_str().map(str::to_string))
            .collect();
        if suggestions.is_empty() {
            continue;
        }
        out.push(Correction {
            span: start..end,
            original: text[start..end].to_string(),
            suggestions,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "matches": [
            {
                "offset": 4,
                "length": 4,
                "replacements": [{"value": "hello"}, {"value": "helot"}]
            },
            {
                "offset": 9,
                "length": 5,
                "replacements": [{"value": "world"}]
            }
        ]
    }"#;

    #[test]
    fn parses_matches_into_corrections() {
        let json: serde_json::Value = serde_json::from_str(SAMPLE).unwrap();
        let text = "the helo wrold";
        let cs = parse_matches(&json, text);
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].span, 4..8);
        assert_eq!(cs[0].original, "helo");
        assert_eq!(cs[0].suggestions, vec!["hello", "helot"]);
        assert_eq!(cs[1].span, 9..14);
        assert_eq!(cs[1].original, "wrold");
        assert_eq!(cs[1].suggestions, vec!["world"]);
    }

    #[test]
    fn ignores_matches_with_no_replacements() {
        let json: serde_json::Value =
            serde_json::from_str(r#"{"matches":[{"offset":0,"length":3,"replacements":[]}]}"#)
                .unwrap();
        let cs = parse_matches(&json, "the");
        assert!(cs.is_empty());
    }

    #[test]
    fn character_offsets_map_past_multibyte_chars() {
        // LanguageTool counts in characters. With the emoji marker (3
        // bytes) before the flagged word, char offset 9 != byte offset
        // 11 — the correction must still land on "wrold", not 2 bytes off.
        let text = "the \u{FFFC} helo wrold";
        // chars: t(0)h(1)e(2) (3)￼(4) (5)h(6)e(7)l(8)o(9) (10)w(11)...
        let json: serde_json::Value = serde_json::from_str(
            r#"{"matches":[
                {"offset":6,"length":4,"replacements":[{"value":"hello"}]},
                {"offset":11,"length":5,"replacements":[{"value":"world"}]}
            ]}"#,
        )
        .unwrap();
        let cs = parse_matches(&json, text);
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].original, "helo");
        assert_eq!(cs[1].original, "wrold");
        // And the byte span is correct so applying it won't panic/garble.
        assert_eq!(&text[cs[1].span.clone()], "wrold");
    }

    #[test]
    fn ignores_matches_with_out_of_range_spans() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{"matches":[{"offset":10,"length":5,"replacements":[{"value":"x"}]}]}"#,
        )
        .unwrap();
        let cs = parse_matches(&json, "short");
        assert!(cs.is_empty());
    }

    #[test]
    fn disabled_config_errors_cleanly() {
        let lt = LanguageToolConfig {
            enabled: false,
            url: "http://localhost:8081".into(),
            ngram_dir: None,
        };
        assert!(matches!(
            LanguageToolProvider::from_config(&lt),
            Err(LanguageToolError::Disabled)
        ));
    }

    #[test]
    fn empty_url_errors_cleanly() {
        let lt = LanguageToolConfig {
            enabled: true,
            url: "  ".into(),
            ngram_dir: None,
        };
        assert!(matches!(
            LanguageToolProvider::from_config(&lt),
            Err(LanguageToolError::NoUrl)
        ));
    }
}
