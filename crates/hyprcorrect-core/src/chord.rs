//! A keyboard chord: a set of modifiers plus a single non-modifier
//! key. Stored in `config.toml` as a `+`-separated uppercase string
//! (`CTRL+SHIFT+ALT+SUPER+F`); parsed everywhere it's used.
//!
//! Storage uses an accelerator string (vernier-style) rather than a
//! struct so the file stays human-readable.

/// A parsed chord description.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Chord {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub super_: bool,
    /// The non-modifier key, normalized to UPPERCASE for letters /
    /// special-key tokens like `SPACE`, `ENTER`, `F1`, `LEFT`.
    pub key: String,
}

/// An error parsing an accelerator string.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ChordError {
    /// The string contained no `+`-separated parts.
    #[error("chord is empty")]
    Empty,
    /// The last segment (the key) was empty.
    #[error("chord has no key after the modifiers")]
    NoKey,
    /// An unknown modifier token appeared. Known: SHIFT, CTRL, ALT,
    /// SUPER (and synonyms META / CMD / WIN).
    #[error("unknown modifier: {0}")]
    UnknownModifier(String),
}

impl Chord {
    /// Parse an accelerator string like `"SUPER+CTRL+SHIFT+ALT+F"`.
    ///
    /// Case-insensitive. Whitespace around tokens is trimmed. The
    /// last `+`-separated segment is the key; everything before
    /// must be modifier tokens.
    ///
    /// # Errors
    ///
    /// See [`ChordError`].
    pub fn parse(s: &str) -> Result<Self, ChordError> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err(ChordError::Empty);
        }
        let mut parts: Vec<&str> = trimmed.split('+').map(str::trim).collect();
        let Some(key) = parts.pop() else {
            return Err(ChordError::Empty);
        };
        if key.is_empty() {
            return Err(ChordError::NoKey);
        }

        let mut chord = Self {
            key: key.to_ascii_uppercase(),
            ..Self::default()
        };
        for part in parts {
            if part.is_empty() {
                continue;
            }
            match part.to_ascii_uppercase().as_str() {
                "SHIFT" => chord.shift = true,
                "CTRL" | "CONTROL" => chord.ctrl = true,
                "ALT" | "OPTION" => chord.alt = true,
                "SUPER" | "META" | "CMD" | "COMMAND" | "WIN" | "WINDOWS" => chord.super_ = true,
                other => return Err(ChordError::UnknownModifier(other.to_string())),
            }
        }
        Ok(chord)
    }

    /// The Hyprland `bind = MODS, KEY, ...` modifier list. Empty when
    /// no modifiers are set. Order matches the chip-display
    /// convention: CTRL, SHIFT, ALT, SUPER — same as macOS native
    /// menus (⌃⇧⌥⌘) and the Electron `CommandOrControl` token.
    pub fn hyprland_modifiers(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if self.ctrl {
            parts.push("CTRL");
        }
        if self.shift {
            parts.push("SHIFT");
        }
        if self.alt {
            parts.push("ALT");
        }
        if self.super_ {
            parts.push("SUPER");
        }
        parts.join(" ")
    }

    /// The Hyprland key token. Most of our canonical tokens
    /// (`BACKSPACE`, `DELETE`, `UP`, `SPACE`, …) match an xkb
    /// keysym name case-insensitively and resolve fine on the
    /// Hyprland side. A couple don't — `ESC` and `ENTER` are
    /// human-friendly shortenings that xkb doesn't know — so
    /// translate those back to their xkb names before handing
    /// the bind to `hyprctl`.
    pub fn hyprland_key(&self) -> &str {
        match self.key.as_str() {
            "ESC" => "Escape",
            "ENTER" => "Return",
            _ => &self.key,
        }
    }

    /// `true` if every active modifier flag matches `target` and the
    /// pressed key matches our key. Used by capture to recognize and
    /// suppress the trigger letter under the chord.
    pub fn modifiers_match(&self, ctrl: bool, alt: bool, shift: bool, super_: bool) -> bool {
        self.ctrl == ctrl && self.alt == alt && self.shift == shift && self.super_ == super_
    }
}

impl std::fmt::Display for Chord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts: Vec<&str> = Vec::new();
        if self.ctrl {
            parts.push("CTRL");
        }
        if self.shift {
            parts.push("SHIFT");
        }
        if self.alt {
            parts.push("ALT");
        }
        if self.super_ {
            parts.push("SUPER");
        }
        if parts.is_empty() {
            f.write_str(&self.key)
        } else {
            write!(f, "{}+{}", parts.join("+"), self.key)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_chord() {
        let c = Chord::parse("SUPER+CTRL+SHIFT+ALT+F").unwrap();
        assert!(c.super_ && c.ctrl && c.shift && c.alt);
        assert_eq!(c.key, "F");
    }

    #[test]
    fn parses_unmodified_chord() {
        let c = Chord::parse("F1").unwrap();
        assert!(!c.super_ && !c.ctrl && !c.shift && !c.alt);
        assert_eq!(c.key, "F1");
    }

    #[test]
    fn parses_lowercase_and_synonyms() {
        let c = Chord::parse("meta+ctrl+space").unwrap();
        assert!(c.super_ && c.ctrl);
        assert_eq!(c.key, "SPACE");
    }

    #[test]
    fn display_uses_canonical_order() {
        // Parse any order, render in canonical (CTRL, SHIFT, ALT, SUPER).
        let c = Chord::parse("SUPER+CTRL+ALT+J").unwrap();
        assert_eq!(c.to_string(), "CTRL+ALT+SUPER+J");
        let c = Chord::parse("SUPER+CTRL+SHIFT+ALT+F").unwrap();
        assert_eq!(c.to_string(), "CTRL+SHIFT+ALT+SUPER+F");
    }

    #[test]
    fn rejects_empty_and_unknown() {
        assert_eq!(Chord::parse(""), Err(ChordError::Empty));
        assert_eq!(Chord::parse("  "), Err(ChordError::Empty));
        assert_eq!(
            Chord::parse("FOO+F"),
            Err(ChordError::UnknownModifier("FOO".into()))
        );
    }

    #[test]
    fn hyprland_modifiers_uses_spaces_not_pluses() {
        let c = Chord::parse("SUPER+CTRL+SHIFT+ALT+F").unwrap();
        assert_eq!(c.hyprland_modifiers(), "CTRL SHIFT ALT SUPER");
        assert_eq!(c.hyprland_key(), "F");
    }
}
