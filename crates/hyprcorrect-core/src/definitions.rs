//! Word definitions for the review popup's suggestion dropdown.
//!
//! Two sources, selected by [`crate::DefinitionSource`]:
//!
//! - **Local** (default): a bundled WordNet 3.1 gloss set
//!   (`assets/definitions-en.tsv.gz` — the sense-1 gloss of each
//!   single-word lemma), lazily gunzipped into an in-memory map on first
//!   lookup. Fully offline; ~83k words.
//! - **Online**: `api.dictionaryapi.dev` (no API key). The request blocks
//!   on DNS/TLS/HTTP, so [`define_online`] is meant to run on a worker
//!   thread — never the egui loop — and sends the looked-up word to a
//!   third party.
//!
//! Words with no entry (proper nouns, most function words, misspellings)
//! return `None`; the UI shows that gracefully.

use std::collections::HashMap;
use std::io::Read;
use std::sync::OnceLock;
use std::time::Duration;

use crate::DefinitionSource;

/// Bundled, gzipped `word\tdefinition\n` set derived from WordNet 3.1.
/// See `assets/WORDNET-LICENSE.txt`.
static DEFS_GZ: &[u8] = include_bytes!("../assets/definitions-en.tsv.gz");

/// Lazily-decompressed `word -> definition` map (lowercased keys). Built
/// once per process on the first local lookup.
fn local_map() -> &'static HashMap<String, String> {
    static MAP: OnceLock<HashMap<String, String>> = OnceLock::new();
    MAP.get_or_init(|| {
        let mut text = String::new();
        if flate2::read::GzDecoder::new(DEFS_GZ)
            .read_to_string(&mut text)
            .is_err()
        {
            return HashMap::new();
        }
        text.lines()
            .filter_map(|line| line.split_once('\t'))
            .map(|(w, d)| (w.to_string(), d.to_string()))
            .collect()
    })
}

/// A bundled offline definition for `word`, if WordNet has one.
/// Case-insensitive.
pub fn define_local(word: &str) -> Option<String> {
    let key = word.trim().to_ascii_lowercase();
    if key.is_empty() {
        return None;
    }
    local_map().get(&key).cloned()
}

/// A definition from the configured `source`, for the synchronous paths.
/// `Online` returns `None` here because it blocks — callers fetch it via
/// [`define_online`] on a worker thread instead.
pub fn define(word: &str, source: DefinitionSource) -> Option<String> {
    match source {
        DefinitionSource::Local => define_local(word),
        DefinitionSource::Off | DefinitionSource::Online => None,
    }
}

/// Fetch a one-line definition from `api.dictionaryapi.dev`. Blocking, so
/// run it on a background thread. Returns `None` on any error or an
/// unknown word.
pub fn define_online(word: &str) -> Option<String> {
    let word = word.trim();
    if word.is_empty() {
        return None;
    }
    let url = format!(
        "https://api.dictionaryapi.dev/api/v2/entries/en/{}",
        urlencode(word)
    );
    let agent: ureq::Agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(8))
        .build();
    let json: serde_json::Value = agent.get(&url).call().ok()?.into_json().ok()?;
    // Shape: [ { "meanings": [ { "definitions": [ { "definition": "…" } ] } ] } ]
    let def = json
        .get(0)?
        .get("meanings")?
        .as_array()?
        .iter()
        .find_map(|m| {
            m.get("definitions")?
                .as_array()?
                .iter()
                .find_map(|d| d.get("definition").and_then(serde_json::Value::as_str))
        })?;
    Some(def.trim().to_string())
}

/// Percent-encode a single URL path segment (the looked-up word). Words
/// are mostly ASCII letters; anything outside the unreserved set is
/// encoded so the URL stays valid.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_covers_common_words_and_misses_gracefully() {
        // Real words WordNet covers.
        assert!(define_local("difference").is_some());
        assert!(define_local("acquiesce").is_some());
        // Case-insensitive.
        assert!(define_local("Veneer").is_some());
        // Not in WordNet → graceful None (joke word, empty).
        assert!(define_local("recombobulate").is_none());
        assert!(define_local("").is_none());
        assert!(define_local("   ").is_none());
    }

    #[test]
    fn define_routes_by_source() {
        assert!(define("acquiesce", DefinitionSource::Local).is_some());
        // Online is resolved off-thread, Off is silent — both None here.
        assert!(define("acquiesce", DefinitionSource::Online).is_none());
        assert!(define("acquiesce", DefinitionSource::Off).is_none());
    }
}
