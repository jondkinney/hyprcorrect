//! The keystroke buffer: a bounded, in-memory record of recently typed
//! text in the focused element. It lets hyprcorrect answer "what was the
//! last word?" without reading back from the focused application — which
//! is what makes correction work in terminals.
//!
//! See the "keystroke buffer" section of `DESIGN.md`.

/// Default cap on buffered characters — comfortably larger than any one
/// word or sentence. Older characters are dropped from the front.
const DEFAULT_CAPACITY: usize = 1024;

/// One unit of input fed to the [`Buffer`] by the platform capture layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A printable character was typed.
    Char(char),
    /// The Backspace key — delete the character before the caret.
    Backspace,
    /// Left arrow — move the caret left by one character within the
    /// buffer. Buffer contents are unchanged.
    MoveLeft,
    /// Right arrow — move the caret right by one character within the
    /// buffer.
    MoveRight,
    /// Anything we can't track precisely: Up/Down/Home/End/Tab/Enter/
    /// Esc, focus change, mouse click, or a Ctrl/Alt/Super shortcut.
    /// After one of these the buffer's contents and caret are no
    /// longer trustworthy, so the buffer clears itself.
    Reset,
}

/// The last word in the buffer, with any whitespace that follows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastWord {
    /// The word itself, with no surrounding whitespace.
    pub word: String,
    /// Whitespace between the word and the caret (the buffer's end) —
    /// usually the space the user typed after the word.
    pub trailing: String,
}

/// The last sentence in the buffer, with any whitespace that follows it.
/// "Sentence" here is "text after the previous `.`/`!`/`?`" (or, if
/// there isn't one, the whole buffer's trimmed content).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastSentence {
    /// The sentence itself, with no surrounding whitespace.
    pub sentence: String,
    /// Whitespace between the sentence and the caret.
    pub trailing: String,
}

/// A bounded record of recently typed text in the focused element.
///
/// Carries a `caret` byte offset into `text`. Char/Backspace operate
/// at the caret; MoveLeft/MoveRight slide the caret without changing
/// the text. `last_word` / `last_sentence` extract from the text
/// *behind* the caret, so navigating left into already-typed text and
/// hitting the correction chord still operates on the right region.
#[derive(Debug)]
pub struct Buffer {
    text: String,
    /// Byte offset into `text`. Invariant: always at a UTF-8 char
    /// boundary, `0 <= caret <= text.len()`.
    caret: usize,
    capacity: usize,
}

impl Default for Buffer {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }
}

impl Buffer {
    /// Create a buffer holding at most `capacity` characters (at least 1).
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            text: String::new(),
            caret: 0,
            capacity: capacity.max(1),
        }
    }

    /// Feed one unit of input to the buffer.
    pub fn push(&mut self, key: Key) {
        match key {
            Key::Char(c) => {
                self.text.insert(self.caret, c);
                self.caret += c.len_utf8();
                self.trim_to_capacity();
            }
            Key::Backspace => {
                if self.caret == 0 {
                    return;
                }
                let prev = prev_char_boundary(&self.text, self.caret);
                self.text.drain(prev..self.caret);
                self.caret = prev;
            }
            Key::MoveLeft => {
                if self.caret == 0 {
                    return;
                }
                self.caret = prev_char_boundary(&self.text, self.caret);
            }
            Key::MoveRight => {
                if self.caret >= self.text.len() {
                    return;
                }
                self.caret = next_char_boundary(&self.text, self.caret);
            }
            Key::Reset => {
                self.text.clear();
                self.caret = 0;
            }
        }
    }

    /// Clear the buffer.
    pub fn clear(&mut self) {
        self.text.clear();
        self.caret = 0;
    }

    /// `true` when the buffer holds no text.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// The full buffered text, oldest character first.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The buffered text up to the caret — the part the daemon
    /// treats as "what sits behind the cursor right now."
    pub fn text_before_caret(&self) -> &str {
        &self.text[..self.caret]
    }

    /// The last sentence in the buffer with the whitespace that
    /// follows it, or `None` when the buffer holds no sentence
    /// (empty / only whitespace).
    ///
    /// "Sentence" means the run of text bounded by sentence-enders
    /// (`.`/`!`/`?`). The buffer's final sentence-ender, if any, is
    /// included — so pressing the chord right after typing
    /// `"The quick brown fox."` operates on `"The quick brown fox."`
    /// rather than no-opping. If the buffer doesn't end with an
    /// ender the sentence is the in-progress text after the previous
    /// one.
    pub fn last_sentence(&self) -> Option<LastSentence> {
        let before = self.text_before_caret();
        let trimmed = before.trim_end();
        if trimmed.is_empty() {
            return None;
        }
        // Look for the previous sentence boundary — the ender BEFORE
        // the current sentence. If trimmed ends in an ender, that one
        // closes the current sentence, so we search the slice before
        // it; otherwise we search the whole trimmed text.
        let last_char = trimmed.chars().next_back().expect("non-empty");
        let ends_with_ender = matches!(last_char, '.' | '!' | '?');
        let search_end = if ends_with_ender {
            trimmed.len() - last_char.len_utf8()
        } else {
            trimmed.len()
        };
        let after_prev_ender = trimmed[..search_end]
            .char_indices()
            .rev()
            .find_map(|(i, c)| {
                if matches!(c, '.' | '!' | '?') {
                    Some(i + c.len_utf8())
                } else {
                    None
                }
            })
            .unwrap_or(0);
        // Skip whitespace after the boundary to find the sentence's
        // first character.
        let sentence_start = trimmed[after_prev_ender..]
            .char_indices()
            .find_map(|(i, c)| {
                if !c.is_whitespace() {
                    Some(after_prev_ender + i)
                } else {
                    None
                }
            })
            .unwrap_or(trimmed.len());
        if sentence_start >= trimmed.len() {
            return None;
        }
        let before = self.text_before_caret();
        Some(LastSentence {
            sentence: trimmed[sentence_start..].to_string(),
            trailing: before[trimmed.len()..].to_string(),
        })
    }

    /// The last word in the buffer with the whitespace that follows it,
    /// or `None` when the buffer holds no word (it is empty or holds
    /// only whitespace).
    pub fn last_word(&self) -> Option<LastWord> {
        let before = self.text_before_caret();
        let trimmed = before.trim_end();
        if trimmed.is_empty() {
            return None;
        }
        // Byte length of the run of non-whitespace at the end of `trimmed`.
        let word_bytes: usize = trimmed
            .chars()
            .rev()
            .take_while(|c| !c.is_whitespace())
            .map(char::len_utf8)
            .sum();
        Some(LastWord {
            word: trimmed[trimmed.len() - word_bytes..].to_string(),
            trailing: before[trimmed.len()..].to_string(),
        })
    }

    /// Mirror an external edit in the buffer: delete `backspaces`
    /// characters going LEFT from the caret, then insert `insert`
    /// at the caret. Called after the emulation layer applies a
    /// correction, so that a follow-up correction sees the
    /// corrected text.
    pub fn apply(&mut self, backspaces: usize, insert: &str) {
        for _ in 0..backspaces {
            if self.caret == 0 {
                break;
            }
            let prev = prev_char_boundary(&self.text, self.caret);
            self.text.drain(prev..self.caret);
            self.caret = prev;
        }
        self.text.insert_str(self.caret, insert);
        self.caret += insert.len();
        self.trim_to_capacity();
    }

    /// Drop characters from the front until the buffer fits `capacity`.
    /// Shifts the caret back by the same number of bytes so the
    /// before/after caret split stays consistent.
    fn trim_to_capacity(&mut self) {
        while self.text.chars().count() > self.capacity {
            let first = self.text.chars().next().map_or(0, char::len_utf8);
            self.text.drain(..first);
            self.caret = self.caret.saturating_sub(first);
        }
    }
}

/// Return the byte offset of the char that ENDS at `pos` in `s`.
/// `pos` must be > 0 and a char boundary.
fn prev_char_boundary(s: &str, pos: usize) -> usize {
    s[..pos]
        .char_indices()
        .next_back()
        .map_or(0, |(i, _)| i)
}

/// Return the byte offset that ENDS the char STARTING at `pos` in `s`.
/// `pos` must be < `s.len()` and a char boundary.
fn next_char_boundary(s: &str, pos: usize) -> usize {
    s[pos..]
        .chars()
        .next()
        .map_or(pos, |c| pos + c.len_utf8())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed each character of `s` to the buffer as a `Char` key.
    fn type_str(buf: &mut Buffer, s: &str) {
        for c in s.chars() {
            buf.push(Key::Char(c));
        }
    }

    #[test]
    fn empty_buffer_has_no_last_word() {
        let buf = Buffer::default();
        assert!(buf.is_empty());
        assert_eq!(buf.last_word(), None);
    }

    #[test]
    fn last_word_without_trailing_whitespace() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "vernuer");
        assert_eq!(
            buf.last_word(),
            Some(LastWord {
                word: "vernuer".to_string(),
                trailing: String::new(),
            })
        );
    }

    #[test]
    fn last_word_keeps_trailing_whitespace() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "vernuer ");
        let last = buf.last_word().unwrap();
        assert_eq!(last.word, "vernuer");
        assert_eq!(last.trailing, " ");
    }

    #[test]
    fn last_word_picks_the_final_word() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "the quick vernuer ");
        let last = buf.last_word().unwrap();
        assert_eq!(last.word, "vernuer");
        assert_eq!(last.trailing, " ");
    }

    #[test]
    fn all_whitespace_has_no_last_word() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "   ");
        assert_eq!(buf.last_word(), None);
    }

    #[test]
    fn backspace_removes_the_last_character() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "vernuer");
        buf.push(Key::Backspace);
        assert_eq!(buf.text(), "vernue");
    }

    #[test]
    fn backspace_on_empty_buffer_is_a_no_op() {
        let mut buf = Buffer::default();
        buf.push(Key::Backspace);
        assert!(buf.is_empty());
    }

    #[test]
    fn reset_clears_the_buffer() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "vernuer ");
        buf.push(Key::Reset);
        assert!(buf.is_empty());
        assert_eq!(buf.last_word(), None);
    }

    #[test]
    fn buffer_is_bounded_by_capacity() {
        let mut buf = Buffer::with_capacity(5);
        type_str(&mut buf, "abcdefgh");
        assert_eq!(buf.text(), "defgh");
    }

    #[test]
    fn last_word_handles_multibyte_characters() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "café ");
        let last = buf.last_word().unwrap();
        assert_eq!(last.word, "café");
        assert_eq!(last.trailing, " ");
    }

    #[test]
    fn last_sentence_after_an_ender() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "Hello there. how are you ");
        let last = buf.last_sentence().unwrap();
        assert_eq!(last.sentence, "how are you");
        assert_eq!(last.trailing, " ");
    }

    #[test]
    fn last_sentence_when_no_ender_yet() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "the quick brown fox");
        let last = buf.last_sentence().unwrap();
        assert_eq!(last.sentence, "the quick brown fox");
        assert_eq!(last.trailing, "");
    }

    #[test]
    fn last_sentence_with_multiple_enders() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "Hi! Hello there. How are yu");
        let last = buf.last_sentence().unwrap();
        assert_eq!(last.sentence, "How are yu");
    }

    #[test]
    fn last_sentence_includes_the_trailing_ender() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "Hello there.");
        let last = buf.last_sentence().unwrap();
        assert_eq!(last.sentence, "Hello there.");
        assert_eq!(last.trailing, "");
    }

    #[test]
    fn last_sentence_picks_the_final_of_multiple_complete_sentences() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "First sentence. Second sentence!");
        let last = buf.last_sentence().unwrap();
        assert_eq!(last.sentence, "Second sentence!");
        assert_eq!(last.trailing, "");
    }

    #[test]
    fn last_sentence_after_complete_one_then_trailing_ws() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "Hello there.   ");
        let last = buf.last_sentence().unwrap();
        assert_eq!(last.sentence, "Hello there.");
        assert_eq!(last.trailing, "   ");
    }

    #[test]
    fn last_sentence_returns_none_for_whitespace() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "   ");
        assert!(buf.last_sentence().is_none());
    }

    #[test]
    fn apply_mirrors_a_correction() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "vernuer ");
        // Replace "vernuer " (8 characters) with "veneer ".
        buf.apply(8, "veneer ");
        assert_eq!(buf.text(), "veneer ");
        assert_eq!(buf.last_word().unwrap().word, "veneer");
    }

    #[test]
    fn move_left_walks_caret_back_without_clearing_text() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "hello world");
        for _ in 0..6 {
            buf.push(Key::MoveLeft);
        }
        // Text unchanged, but the caret is now before "world".
        assert_eq!(buf.text(), "hello world");
        assert_eq!(buf.text_before_caret(), "hello");
    }

    #[test]
    fn last_sentence_uses_text_before_caret() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "Hi! how are you doing today");
        // Move caret back 6 chars so it sits after "doing".
        for _ in 0..6 {
            buf.push(Key::MoveLeft);
        }
        let last = buf.last_sentence().unwrap();
        assert_eq!(last.sentence, "how are you doing");
    }

    #[test]
    fn typing_after_move_left_inserts_at_caret() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "helloworld");
        for _ in 0..5 {
            buf.push(Key::MoveLeft);
        }
        type_str(&mut buf, " ");
        assert_eq!(buf.text(), "hello world");
    }

    #[test]
    fn backspace_after_move_left_removes_the_char_before_caret() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "hello world");
        for _ in 0..6 {
            buf.push(Key::MoveLeft);
        }
        buf.push(Key::Backspace);
        assert_eq!(buf.text(), "hell world");
    }

    #[test]
    fn move_right_at_end_is_a_no_op() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "abc");
        buf.push(Key::MoveRight);
        assert_eq!(buf.text_before_caret(), "abc");
    }

    #[test]
    fn move_left_at_start_is_a_no_op() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "abc");
        for _ in 0..10 {
            buf.push(Key::MoveLeft);
        }
        assert_eq!(buf.text_before_caret(), "");
        assert_eq!(buf.text(), "abc");
    }

    #[test]
    fn apply_acts_at_caret_not_end() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "vernuer trailing");
        // Park the caret right after "vernuer".
        for _ in 0..9 {
            buf.push(Key::MoveLeft);
        }
        // Replace the 7 chars before the caret ("vernuer") with "veneer".
        buf.apply(7, "veneer");
        assert_eq!(buf.text(), "veneer trailing");
    }
}
