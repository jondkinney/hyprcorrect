//! Replacement planning: turning a chosen correction into a concrete
//! edit — a backspace count, a delete count, and the text to type —
//! over the buffered text.
//!
//! See the "Replacement mechanics" section of `DESIGN.md`.

use crate::buffer::{EMOJI_MARKER, WordAtCaret};

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

/// One primitive the emit layer sends to the focused app, in order.
/// A whole-sentence correction is a list of these so the emit can step
/// *over* an emoji (the user's real glyph, which we can't reproduce)
/// instead of deleting and retyping it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmitOp {
    /// Move the caret right `n` characters (one per on-screen glyph,
    /// so one press skips one emoji).
    MoveRight(usize),
    /// Move the caret left `n` characters.
    MoveLeft(usize),
    /// Press Backspace `n` times — delete `n` chars left of the caret.
    Backspace(usize),
    /// Type this literal text at the caret.
    Type(String),
}

/// Plan the keystrokes that turn the on-screen `original` sentence into
/// `corrected`, **preserving every emoji** the user typed. Both strings
/// carry [`EMOJI_MARKER`]s where the app rendered a `:shortcode:` as a
/// glyph (the same markers, in the same order — corrections never touch
/// them), and `removed[k]` asks to delete the k-th emoji instead of
/// keeping it.
///
/// The caret is assumed to sit `chars_before` chars into the sentence
/// (the rest, `chars_after`, plus any `trailing` whitespace, lie to its
/// right). The plan walks the text **right-to-left** between emojis:
/// for each text run it backspaces the old chars and types the new,
/// then either steps left over the emoji glyph (keep) or backspaces it
/// (remove) before handling the run to its left. Text runs are plain
/// (no markers), so their char counts match the screen exactly — the
/// emoji's unknown display width never enters the arithmetic.
///
/// With no markers this reduces to the plain "move to the right edge,
/// backspace the whole region, retype it" edit.
pub fn plan_sentence_replacement(
    original: &str,
    corrected: &str,
    chars_before: usize,
    chars_after: usize,
    trailing: &str,
    removed: &[bool],
) -> Vec<EmitOp> {
    let o_segs: Vec<&str> = original.split(EMOJI_MARKER).collect();
    let c_segs: Vec<&str> = corrected.split(EMOJI_MARKER).collect();
    // Both sides must agree on the emoji count; if they somehow don't,
    // fall back to a plain whole-region replace rather than misalign.
    if o_segs.len() != c_segs.len() || removed.len() + 1 != o_segs.len() {
        let mut ops = Vec::new();
        if chars_after > 0 {
            ops.push(EmitOp::MoveRight(chars_after));
        }
        ops.push(EmitOp::Backspace(
            chars_before + chars_after + trailing.chars().count(),
        ));
        ops.push(EmitOp::Type(format!("{corrected}{trailing}")));
        return ops;
    }

    let mut ops = Vec::new();
    // Walk to the sentence's right edge. `trailing` is folded into the
    // rightmost segment below, so we only move over the in-sentence
    // chars to the caret's right here.
    if chars_after > 0 {
        ops.push(EmitOp::MoveRight(chars_after));
    }
    let last = o_segs.len() - 1;
    for i in (0..o_segs.len()).rev() {
        let (old, new) = if i == last {
            (
                format!("{}{}", o_segs[i], trailing),
                format!("{}{}", c_segs[i], trailing),
            )
        } else {
            (o_segs[i].to_string(), c_segs[i].to_string())
        };
        let old_len = old.chars().count();
        let new_len = new.chars().count();
        if old_len > 0 {
            ops.push(EmitOp::Backspace(old_len));
        }
        if !new.is_empty() {
            ops.push(EmitOp::Type(new));
        }
        if i > 0 {
            // The emoji separating segment i-1 from i is marker i-1.
            if removed[i - 1] {
                // Delete it: step left over the freshly-typed text, then
                // backspace the glyph away.
                if new_len > 0 {
                    ops.push(EmitOp::MoveLeft(new_len));
                }
                ops.push(EmitOp::Backspace(1));
            } else {
                // Keep it: step left over the text *and* the glyph so the
                // caret lands on the right edge of the next segment left.
                ops.push(EmitOp::MoveLeft(new_len + 1));
            }
        }
    }
    ops
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

    /// Apply a plan to a model of the on-screen text (the emoji marker
    /// stands in for the real glyph, one char wide) so we can assert the
    /// end result rather than the exact keystrokes. `caret` is a char
    /// index into `screen`.
    fn simulate(screen: &str, caret: usize, ops: &[EmitOp]) -> String {
        let mut chars: Vec<char> = screen.chars().collect();
        let mut caret = caret.min(chars.len());
        for op in ops {
            match op {
                EmitOp::MoveRight(n) => caret = (caret + n).min(chars.len()),
                EmitOp::MoveLeft(n) => caret = caret.saturating_sub(*n),
                EmitOp::Backspace(n) => {
                    for _ in 0..*n {
                        if caret > 0 {
                            chars.remove(caret - 1);
                            caret -= 1;
                        }
                    }
                }
                EmitOp::Type(t) => {
                    for c in t.chars() {
                        chars.insert(caret, c);
                        caret += 1;
                    }
                }
            }
        }
        chars.into_iter().collect()
    }

    const M: char = EMOJI_MARKER;

    #[test]
    fn no_emoji_is_a_plain_whole_region_replace() {
        let ops = plan_sentence_replacement("teh fixx", "the fix", 8, 0, "", &[]);
        assert_eq!(simulate("teh fixx", 8, &ops), "the fix");
    }

    #[test]
    fn an_emoji_in_the_middle_is_stepped_over() {
        // "the fixx <E> butt"  ->  "the fix <E> but", caret at end.
        let original = format!("the fixx {M} butt");
        let corrected = format!("the fix {M} but");
        let n = original.chars().count();
        let ops = plan_sentence_replacement(&original, &corrected, n, 0, "", &[false]);
        // The emoji (M) survives in place; both words are corrected.
        assert_eq!(simulate(&original, n, &ops), format!("the fix {M} but"));
    }

    #[test]
    fn removing_an_emoji_backspaces_it_away() {
        let original = format!("the fixx {M} butt");
        let corrected = format!("the fix {M} but");
        let n = original.chars().count();
        let ops = plan_sentence_replacement(&original, &corrected, n, 0, "", &[true]);
        // M is gone; the surrounding spaces remain as typed.
        assert_eq!(simulate(&original, n, &ops), "the fix  but");
    }

    #[test]
    fn two_emojis_each_preserved() {
        let original = format!("a {M} b {M} c");
        let corrected = format!("A {M} B {M} C");
        let n = original.chars().count();
        let ops = plan_sentence_replacement(&original, &corrected, n, 0, "", &[false, false]);
        assert_eq!(simulate(&original, n, &ops), format!("A {M} B {M} C"));
    }

    #[test]
    fn trailing_whitespace_is_preserved() {
        let original = format!("teh {M} fixx");
        let corrected = format!("the {M} fix");
        // Caret sits in two trailing spaces past the sentence end.
        let sentence_chars = original.chars().count();
        let ops =
            plan_sentence_replacement(&original, &corrected, sentence_chars, 0, "  ", &[false]);
        let screen = format!("{original}  ");
        assert_eq!(
            simulate(&screen, screen.chars().count(), &ops),
            format!("the {M} fix  ")
        );
    }

    #[test]
    fn caret_in_the_middle_still_replaces_the_whole_sentence() {
        let original = format!("teh {M} fixx");
        let corrected = format!("the {M} fix");
        // Caret between "teh" and the emoji: 4 chars before, the rest after.
        let total = original.chars().count();
        let before = 4;
        let ops =
            plan_sentence_replacement(&original, &corrected, before, total - before, "", &[false]);
        assert_eq!(simulate(&original, before, &ops), format!("the {M} fix"));
    }
}
