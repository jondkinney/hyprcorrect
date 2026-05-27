//! Replacement planning: turning a chosen correction into a concrete
//! edit — a backspace count, a delete count, and the text to type —
//! over the buffered text.
//!
//! See the "Replacement mechanics" section of `DESIGN.md`.

use crate::buffer::WordAtCaret;

/// A concrete edit for the emulation layer to apply to the focused
/// application: press Backspace `backspaces` times, press Delete
/// `deletes` times, then type `insert`. Splitting the deletion into
/// a left half (Backspace) and a right half (Delete) lets us
/// rewrite a word the caret sits inside without first having to
/// move the caret to the end of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    /// Number of Backspace presses to send (chars before the caret).
    pub backspaces: usize,
    /// Number of Delete presses to send (chars after the caret).
    pub deletes: usize,
    /// Text to type after the deletions.
    pub insert: String,
}

/// Plan the edit that replaces the word at the caret with `correction`,
/// preserving the whitespace the user typed after it.
///
/// Returns `None` when `correction` already equals the word: there is
/// nothing to do, and sending a no-op edit would only risk disturbing
/// the caret.
pub fn plan_word_replacement(at: &WordAtCaret, correction: &str) -> Option<Edit> {
    if correction == at.word {
        return None;
    }
    let trailing_chars = at.trailing.chars().count();
    Some(Edit {
        // Left of caret: the word's left half + any whitespace
        // between the word's right edge and the caret.
        backspaces: at.chars_before_caret + trailing_chars,
        // Right of caret: the word's right half (zero when caret
        // was at the word's end or in trailing whitespace).
        deletes: at.chars_after_caret,
        // Retype the correction then put the trailing whitespace
        // back so the caret lands where the user expects.
        insert: format!("{correction}{}", at.trailing),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word_at_end(word: &str, trailing: &str) -> WordAtCaret {
        WordAtCaret {
            word: word.to_string(),
            trailing: trailing.to_string(),
            chars_before_caret: word.chars().count(),
            chars_after_caret: 0,
        }
    }

    #[test]
    fn replaces_word_and_keeps_the_trailing_space() {
        let edit = plan_word_replacement(&word_at_end("vernuer", " "), "veneer").unwrap();
        assert_eq!(
            edit,
            Edit {
                backspaces: 8,
                deletes: 0,
                insert: "veneer ".to_string(),
            }
        );
    }

    #[test]
    fn replaces_word_with_no_trailing_whitespace() {
        let edit = plan_word_replacement(&word_at_end("vernuer", ""), "veneer").unwrap();
        assert_eq!(edit.backspaces, 7);
        assert_eq!(edit.deletes, 0);
        assert_eq!(edit.insert, "veneer");
    }

    #[test]
    fn no_edit_when_the_word_is_already_correct() {
        assert_eq!(
            plan_word_replacement(&word_at_end("veneer", " "), "veneer"),
            None
        );
    }

    #[test]
    fn backspace_count_covers_all_trailing_whitespace() {
        let edit = plan_word_replacement(&word_at_end("x", "   "), "y").unwrap();
        assert_eq!(edit.backspaces, 4);
        assert_eq!(edit.deletes, 0);
        assert_eq!(edit.insert, "y   ");
    }

    #[test]
    fn caret_inside_word_splits_into_backspaces_plus_deletes() {
        // Caret sits between "ver" and "nuer" — 3 chars left, 4 right.
        let at = WordAtCaret {
            word: "vernuer".to_string(),
            trailing: String::new(),
            chars_before_caret: 3,
            chars_after_caret: 4,
        };
        let edit = plan_word_replacement(&at, "veneer").unwrap();
        assert_eq!(edit.backspaces, 3);
        assert_eq!(edit.deletes, 4);
        assert_eq!(edit.insert, "veneer");
    }

    #[test]
    fn count_is_in_characters_not_bytes() {
        // "café" is 4 characters but 5 UTF-8 bytes; the emulation
        // layer sends one Backspace / Delete per character.
        let edit = plan_word_replacement(&word_at_end("café", " "), "coffee").unwrap();
        assert_eq!(edit.backspaces, 5);
        assert_eq!(edit.insert, "coffee ");
    }
}
