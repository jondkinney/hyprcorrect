//! Configuration: loading and saving `config.toml`, plus the hotkey,
//! provider, behavior, and privacy settings it holds.
//!
//! Cross-platform: paths resolve via the `directories` crate so the
//! file lives at the OS-conventional location (`~/.config/hyprcorrect/`
//! on Linux, `~/Library/Application Support/io.hyprcorrect.hyprcorrect/`
//! on macOS, `%APPDATA%\hyprcorrect\hyprcorrect\config\` on Windows).
//!
//! Every field has a default — a missing file or partial TOML still
//! produces a valid [`Config`]. See the "Configuration & GUI" section
//! of `DESIGN.md`.

use std::fs;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

/// An error loading or saving the config.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// No suitable OS config dir was found (extremely rare — happens
    /// in restricted sandboxes with no `$HOME`).
    #[error("no OS config directory is available")]
    NoConfigDir,
    /// The config file could not be read or written.
    #[error("config I/O: {0}")]
    Io(String),
    /// The TOML on disk could not be parsed.
    #[error("config TOML: {0}")]
    Parse(String),
    /// The config could not be serialized.
    #[error("could not serialize config: {0}")]
    Serialize(String),
}

/// The full hyprcorrect configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    pub hotkeys: Hotkeys,
    pub providers: Providers,
    pub behavior: Behavior,
    pub privacy: Privacy,
}

/// Hotkey settings. Each action is fully configurable — pick any
/// combination of modifiers plus a single non-modifier key. Stored
/// as `+`-separated accelerator strings (see [`crate::Chord`]) so
/// the file stays human-readable. An empty string means "unbound".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Hotkeys {
    /// Accelerator for `fix-last-word`. Example: `"CTRL+SHIFT+ALT+SUPER+F"`.
    pub fix_word: String,
    /// Accelerator for `fix-last-sentence`. Empty = unbound.
    pub fix_sentence: String,
    /// Accelerator for the review popup — shows the proposed
    /// correction in a small egui window and waits for Apply / Cancel
    /// before emitting. Empty = unbound.
    pub review: String,
    /// Accelerator that, while the review popup is open, re-processes the
    /// original sentence through the LLM and reloads the popup with its
    /// suggestions — for escalating past a weak LanguageTool/spellbook
    /// correction without calling the LLM on every fix. Empty = unbound.
    pub review_llm: String,
}
impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            fix_word: "CTRL+SHIFT+ALT+SUPER+F".into(),
            fix_sentence: "CTRL+SHIFT+ALT+SUPER+S".into(),
            review: "CTRL+SHIFT+ALT+SUPER+R".into(),
            review_llm: "CTRL+SHIFT+ALT+SUPER+L".into(),
        }
    }
}

/// Provider routing settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Providers {
    /// Provider used for `fix-last-word` (instant, ideally local).
    pub default: ProviderId,
    /// Provider used for `fix-last-sentence` and the review popup.
    pub smart: ProviderId,
    pub llm: LlmConfig,
    pub languagetool: LanguageToolConfig,
}
impl Default for Providers {
    fn default() -> Self {
        Self {
            default: ProviderId::Spellbook,
            smart: ProviderId::Llm,
            llm: LlmConfig::default(),
            languagetool: LanguageToolConfig::default(),
        }
    }
}

/// The set of correction providers the UI lets the user choose between.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderId {
    /// Offline pure-Rust spell checker (Hunspell-compatible).
    #[default]
    Spellbook,
    /// Network LLM (model and backend per [`LlmConfig`]).
    Llm,
    /// Self-hosted LanguageTool over HTTP. Serialized as
    /// `"languagetool"` (one word) so the TOML enum value matches
    /// the `[providers.languagetool]` section header and the
    /// product's own one-word branding — overriding the
    /// container-level snake_case default that would otherwise
    /// produce `"language_tool"`.
    #[serde(rename = "languagetool")]
    LanguageTool,
}

/// LLM provider settings. The API key lives in the OS keychain — see
/// [`crate::secrets`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct LlmConfig {
    /// LLM vendor. Today only `"anthropic"` is wired in (M4).
    pub backend: String,
    /// Model name passed to the vendor API.
    pub model: String,
}
impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            backend: "anthropic".into(),
            model: "claude-haiku-4-5".into(),
        }
    }
}

/// LanguageTool HTTP settings. Off by default — the user supplies their
/// own self-hosted URL.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct LanguageToolConfig {
    pub enabled: bool,
    pub url: String,
    /// Host folder of LanguageTool's n-gram dataset (the unzipped
    /// directory that contains an `en/` subfolder). When set, the
    /// Install-with-Docker convenience mounts it and points the server at
    /// it so real-word confusions (wear/where) get caught. `None` = the
    /// server runs without n-grams.
    pub ngram_dir: Option<String>,
}
impl Default for LanguageToolConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: "http://localhost:8081".into(),
            ngram_dir: None,
        }
    }
}

/// Behavior knobs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Behavior {
    /// Wait time per backspace, applied as a single pause between
    /// the backspace burst and the replacement-text burst. Total
    /// pause = `pause_per_backspace_ms` × backspace count.
    ///
    /// The only emit-side knob most users need. The reason behind
    /// it: Wayland delivers all dispatched backspaces reliably, but
    /// the focused app drains them through its own event loop at
    /// its own pace — if the daemon's next `wtype` (the typing
    /// burst) starts before the app has finished applying the
    /// backspaces, those text events queue behind the still-
    /// processing deletes and visually leave a prefix of the
    /// original on screen. This pause covers that drain time.
    /// Raise it if you still see leftover prefix characters.
    pub pause_per_backspace_ms: u32,

    /// Which keys clear the per-window typing buffer when pressed.
    /// Useful trade-off: a reset is the safest response to a key
    /// we can't precisely track (so fix-word never lands at the
    /// wrong spot), but disabling some resets lets the buffer
    /// survive an autocomplete (Tab), a mode switch (Esc), and so
    /// on so a subsequent fix-word can still operate on the
    /// already-typed text.
    pub reset_keys: ResetKeys,
}
impl Default for Behavior {
    fn default() -> Self {
        Self {
            pause_per_backspace_ms: 8,
            reset_keys: ResetKeys::default(),
        }
    }
}

/// Per-key toggles for "this key clears the typing buffer." See
/// [`Behavior::reset_keys`]. Defaults match what the daemon needs
/// to stay safe — Enter, the arrow keys above/below, Page Up/Down,
/// forward Delete, and Insert all reset; Tab and Escape do not
/// because they typically don't change typed text and resetting
/// drops the buffer for no gain.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ResetKeys {
    pub enter: bool,
    pub tab: bool,
    pub escape: bool,
    pub up: bool,
    pub down: bool,
    pub page_up: bool,
    pub page_down: bool,
    pub delete: bool,
    pub insert: bool,
}

impl Default for ResetKeys {
    fn default() -> Self {
        Self {
            enter: true,
            tab: false,
            escape: false,
            up: true,
            down: true,
            page_up: true,
            page_down: true,
            delete: true,
            insert: true,
        }
    }
}

/// Privacy settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Privacy {
    /// Window classes (lowercase, exact match) for which the daemon
    /// will not buffer keystrokes. Useful for password managers.
    pub app_blocklist: Vec<String>,
}

impl Config {
    /// The OS-conventional path to `config.toml`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::NoConfigDir`] when the platform exposes
    /// no usable config directory (e.g. a sandbox with no `$HOME`).
    pub fn path() -> Result<PathBuf, ConfigError> {
        let dirs = ProjectDirs::from("io", "hyprcorrect", "hyprcorrect")
            .ok_or(ConfigError::NoConfigDir)?;
        Ok(dirs.config_dir().join("config.toml"))
    }
}

/// The OS-conventional data folder where prefs downloads the LanguageTool
/// n-gram dataset (`<data_dir>/ngrams`). `None` when no data directory is
/// available (e.g. a sandbox with no `$HOME`).
pub fn ngram_data_dir() -> Option<PathBuf> {
    ProjectDirs::from("io", "hyprcorrect", "hyprcorrect").map(|dirs| dirs.data_dir().join("ngrams"))
}

impl Config {
    /// Load from the OS-conventional path. A missing file yields a
    /// default [`Config`] (not an error).
    ///
    /// # Errors
    ///
    /// See [`ConfigError`].
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from(&Self::path()?)
    }

    /// Load from a specific path. A missing file is not an error.
    ///
    /// # Errors
    ///
    /// See [`ConfigError`].
    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        match fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|e| ConfigError::Parse(e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(ConfigError::Io(e.to_string())),
        }
    }

    /// Save to the OS-conventional path, creating parent dirs as needed.
    ///
    /// # Errors
    ///
    /// See [`ConfigError`].
    pub fn save(&self) -> Result<(), ConfigError> {
        self.save_to(&Self::path()?)
    }

    /// Save to a specific path.
    ///
    /// # Errors
    ///
    /// See [`ConfigError`].
    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| ConfigError::Io(e.to_string()))?;
        }
        let text =
            toml::to_string_pretty(self).map_err(|e| ConfigError::Serialize(e.to_string()))?;
        fs::write(path, text).map_err(|e| ConfigError::Io(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_roundtrip_through_toml() {
        let cfg = Config::default();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn empty_file_yields_defaults() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn partial_file_fills_missing_with_defaults() {
        let cfg: Config = toml::from_str(
            r#"[hotkeys]
fix_word = "CTRL+J"
"#,
        )
        .unwrap();
        assert_eq!(cfg.hotkeys.fix_word, "CTRL+J");
        // Untouched sections still hold defaults.
        assert_eq!(cfg.behavior.pause_per_backspace_ms, 8);
        assert_eq!(cfg.providers.default, ProviderId::Spellbook);
        assert!(cfg.privacy.app_blocklist.is_empty());
    }

    #[test]
    fn save_then_load_round_trips_through_disk() {
        let dir = unique_tempdir();
        let path = dir.join("config.toml");
        let mut cfg = Config::default();
        cfg.hotkeys.fix_word = "CTRL+ALT+K".into();
        cfg.privacy.app_blocklist = vec!["1password".into(), "keepassxc".into()];
        cfg.save_to(&path).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded, cfg);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_missing_path_yields_defaults() {
        let path = unique_tempdir().join("does-not-exist.toml");
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg, Config::default());
    }

    fn unique_tempdir() -> PathBuf {
        let nano = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("hyprcorrect-cfg-{nano}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
