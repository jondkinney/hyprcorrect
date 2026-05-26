//! Replacement planning: turning a chosen correction into a concrete
//! edit — a backspace count plus the text to type — over the buffered
//! text.
//!
//! See the "Replacement mechanics" section of `DESIGN.md`.

use crate::buffer::LastWord;

/// A concrete edit for the emulation layer to apply to the focused
/// application: press Backspace `backspaces` times, then type `insert`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    /// Number of Backspace presses to send.
    pub backspaces: usize,
    /// Text to type after the backspaces.
    pub insert: String,
}

/// Plan the edit that replaces the buffer's last word with `correction`,
/// preserving the whitespace the user typed after it.
///
/// Returns `None` when `correction` already equals the word: there is
/// nothing to do, and sending a no-op edit would only risk disturbing
/// the caret.
pub fn plan_word_replacement(last: &LastWord, correction: &str) -> Option<Edit> {
    if correction == last.word {
        return None;
    }
    // The caret sits after `word + trailing`. Delete both, then retype
    // the correction followed by the same trailing whitespace — the
    // caret ends exactly where it started.
    let backspaces = last.word.chars().count() + last.trailing.chars().count();
    Some(Edit {
        backspaces,
        insert: format!("{correction}{}", last.trailing),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn last_word(word: &str, trailing: &str) -> LastWord {
        LastWord {
            word: word.to_string(),
            trailing: trailing.to_string(),
        }
    }

    #[test]
    fn replaces_word_and_keeps_the_trailing_space() {
        let edit = plan_word_replacement(&last_word("vernuer", " "), "veneer").unwrap();
        assert_eq!(
            edit,
            Edit {
                backspaces: 8,
                insert: "veneer ".to_string(),
            }
        );
    }

    #[test]
    fn replaces_word_with_no_trailing_whitespace() {
        let edit = plan_word_replacement(&last_word("vernuer", ""), "veneer").unwrap();
        assert_eq!(edit.backspaces, 7);
        assert_eq!(edit.insert, "veneer");
    }

    #[test]
    fn no_edit_when_the_word_is_already_correct() {
        assert_eq!(
            plan_word_replacement(&last_word("veneer", " "), "veneer"),
            None
        );
    }

    #[test]
    fn backspace_count_covers_all_trailing_whitespace() {
        let edit = plan_word_replacement(&last_word("x", "   "), "y").unwrap();
        assert_eq!(edit.backspaces, 4);
        assert_eq!(edit.insert, "y   ");
    }

    #[test]
    fn backspace_count_is_in_characters_not_bytes() {
        // "café" is 4 characters but 5 UTF-8 bytes; the emulation layer
        // sends one Backspace per character.
        let edit = plan_word_replacement(&last_word("café", " "), "coffee").unwrap();
        assert_eq!(edit.backspaces, 5);
        assert_eq!(edit.insert, "coffee ");
    }
}
