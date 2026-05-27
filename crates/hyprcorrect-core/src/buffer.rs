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
    /// `Ctrl+Left` (word-jump) — move the caret to the start of the
    /// previous word (or the start of the current word if the caret
    /// sits inside one).
    WordLeft,
    /// `Ctrl+Right` — move the caret past the end of the next word
    /// (or the end of the current word if the caret sits inside one).
    WordRight,
    /// `Home` — move the caret to the start of the buffer. (The
    /// daemon's buffer holds at most one line — `Enter` still
    /// resets — so "line start" and "buffer start" coincide.)
    LineStart,
    /// `End` — move the caret to the end of the buffer.
    LineEnd,
    /// Anything we can't track precisely: Up/Down/Tab/Enter/Esc,
    /// Page Up/Down, focus change, mouse click, or any Ctrl/Alt/
    /// Super shortcut we haven't taught the buffer about. After one
    /// of these the buffer's contents and caret are no longer
    /// trustworthy, so the buffer clears itself.
    Reset,
}

/// The word at (or immediately to the left of) the caret, with the
/// metadata an emit-side replace needs to delete the right characters
/// before retyping. Returned by [`Buffer::word_at_caret`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordAtCaret {
    /// The full word — both halves around the caret if the caret
    /// sits inside the word.
    pub word: String,
    /// Whitespace between the right edge of the word and the caret
    /// when the caret is in trailing whitespace, otherwise empty.
    pub trailing: String,
    /// How many characters of `word` sit BEFORE the caret. Emit
    /// uses this as the BackSpace count.
    pub chars_before_caret: usize,
    /// How many characters of `word` sit AFTER the caret. Emit
    /// uses this as the Delete-key count.
    pub chars_after_caret: usize,
}

/// The sentence containing (or immediately to the left of) the caret,
/// with metadata for the emit-side replace. Returned by
/// [`Buffer::sentence_at_caret`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentenceAtCaret {
    /// The full sentence — both halves around the caret if the
    /// caret sits inside the sentence.
    pub sentence: String,
    /// Whitespace between the right edge of the sentence and the
    /// caret when the caret is in trailing whitespace.
    pub trailing: String,
    /// How many characters of `sentence` sit BEFORE the caret.
    pub chars_before_caret: usize,
    /// How many characters of `sentence` sit AFTER the caret.
    pub chars_after_caret: usize,
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
            Key::WordLeft => {
                self.caret = prev_word_boundary(&self.text, self.caret);
            }
            Key::WordRight => {
                self.caret = next_word_boundary(&self.text, self.caret);
            }
            Key::LineStart => {
                self.caret = 0;
            }
            Key::LineEnd => {
                self.caret = self.text.len();
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
    pub fn sentence_at_caret(&self) -> Option<SentenceAtCaret> {
        let text = &self.text;
        let caret = self.caret;
        if text.is_empty() {
            return None;
        }
        // Build the buffer's sentence ranges as `[start, end)` byte
        // offsets, where `end` is the position AFTER the closing
        // ender (or text.len() for an in-progress trailing sentence).
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        let mut start = 0;
        for (i, c) in text.char_indices() {
            if matches!(c, '.' | '!' | '?') {
                ranges.push((start, i + c.len_utf8()));
                start = i + c.len_utf8();
            }
        }
        if start < text.len() {
            ranges.push((start, text.len()));
        }
        if ranges.is_empty() {
            return None;
        }
        // Pick the latest range that contains the caret. When the
        // caret sits exactly on a boundary (`caret == end`) the
        // later range wins; if that later range is whitespace-only
        // the caret is really in the previous sentence's trailing
        // whitespace, so step back one.
        let mut idx = ranges
            .iter()
            .rposition(|&(s, e)| s <= caret && caret <= e)?;
        if text[ranges[idx].0..ranges[idx].1].trim().is_empty() && idx > 0 {
            idx -= 1;
        }
        let (range_start, range_end) = ranges[idx];
        let raw = &text[range_start..range_end];
        let leading_ws = raw.len() - raw.trim_start().len();
        let sentence_start = range_start + leading_ws;
        let sentence_end = range_start + raw.trim_end().len();
        if sentence_start >= sentence_end {
            return None;
        }
        let sentence = text[sentence_start..sentence_end].to_string();
        let caret_in_range = caret.clamp(sentence_start, sentence_end);
        let chars_before = text[sentence_start..caret_in_range].chars().count();
        let chars_after = text[caret_in_range..sentence_end].chars().count();
        // Trailing whitespace between the sentence's right edge and
        // the caret. Present only when the caret has walked past the
        // sentence into trailing space.
        let trailing = if caret > sentence_end {
            text[sentence_end..caret].to_string()
        } else {
            String::new()
        };
        Some(SentenceAtCaret {
            sentence,
            trailing,
            chars_before_caret: chars_before,
            chars_after_caret: chars_after,
        })
    }

    /// The word at (or immediately left of) the caret. Decides by
    /// looking at the char immediately BEFORE the caret:
    /// - If it's a word char, the caret is in / at the end of a
    ///   word; expand both directions.
    /// - If it's whitespace (or the caret is at position 0), pick
    ///   the previous word — matches the "fix the word your cursor
    ///   just passed" mental model.
    ///
    /// Returns `None` when there's no word to operate on.
    pub fn word_at_caret(&self) -> Option<WordAtCaret> {
        let caret = self.caret;
        let text = &self.text;

        let prev_is_word = text[..caret].chars().next_back().is_some_and(is_word_char);
        if prev_is_word {
            // Caret sits in or at the end of a word — expand both ways.
            let right_span: usize = text[caret..]
                .chars()
                .take_while(|&c| is_word_char(c))
                .map(char::len_utf8)
                .sum();
            let left_span: usize = text[..caret]
                .chars()
                .rev()
                .take_while(|&c| is_word_char(c))
                .map(char::len_utf8)
                .sum();
            let word_start = caret - left_span;
            let word_end = caret + right_span;
            if word_start == word_end {
                return None;
            }
            return Some(WordAtCaret {
                word: text[word_start..word_end].to_string(),
                trailing: String::new(),
                chars_before_caret: text[word_start..caret].chars().count(),
                chars_after_caret: text[caret..word_end].chars().count(),
            });
        }
        // Caret follows whitespace or punctuation, or sits at
        // position 0. Look LEFT for the previous word, skipping
        // anything that isn't a word char (commas, periods,
        // whitespace, …) so the captured "trailing" carries the
        // punctuation back through the replacement intact.
        let before = &text[..caret];
        let trimmed_right = before.trim_end_matches(|c: char| !is_word_char(c));
        if trimmed_right.is_empty() {
            return None;
        }
        let word_chars: usize = trimmed_right
            .chars()
            .rev()
            .take_while(|&c| is_word_char(c))
            .map(char::len_utf8)
            .sum();
        if word_chars == 0 {
            return None;
        }
        let word_end = trimmed_right.len();
        let word_start = word_end - word_chars;
        Some(WordAtCaret {
            word: text[word_start..word_end].to_string(),
            trailing: text[word_end..caret].to_string(),
            chars_before_caret: text[word_start..word_end].chars().count(),
            chars_after_caret: 0,
        })
    }

    /// Mirror an external edit that happens AROUND the caret: delete
    /// `backspaces` characters going LEFT and `deletes` characters
    /// going RIGHT, then insert at the caret. Called after the
    /// emulation layer fires `BackSpace × N` + `Delete × M` + the
    /// replacement text.
    pub fn apply_around_caret(&mut self, backspaces: usize, deletes: usize, insert: &str) {
        for _ in 0..backspaces {
            if self.caret == 0 {
                break;
            }
            let prev = prev_char_boundary(&self.text, self.caret);
            self.text.drain(prev..self.caret);
            self.caret = prev;
        }
        for _ in 0..deletes {
            if self.caret >= self.text.len() {
                break;
            }
            let next = next_char_boundary(&self.text, self.caret);
            self.text.drain(self.caret..next);
        }
        self.text.insert_str(self.caret, insert);
        self.caret += insert.len();
        self.trim_to_capacity();
    }

    /// Mirror an end-of-caret edit (no right-side deletes). Shim
    /// over [`apply_around_caret`] so end-of-text call-sites stay
    /// readable.
    pub fn apply(&mut self, backspaces: usize, insert: &str) {
        self.apply_around_caret(backspaces, 0, insert);
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
    s[..pos].char_indices().next_back().map_or(0, |(i, _)| i)
}

/// Return the byte offset that ENDS the char STARTING at `pos` in `s`.
/// `pos` must be < `s.len()` and a char boundary.
fn next_char_boundary(s: &str, pos: usize) -> usize {
    s[pos..].chars().next().map_or(pos, |c| pos + c.len_utf8())
}

/// The "word char" rule shared by `word_at_caret`, `Ctrl+Left`,
/// and `Ctrl+Right`. Alphanumerics plus apostrophe — so
/// contractions like `don't` stay one word, but commas, periods,
/// quotes, and brackets are word boundaries. Matches what bash
/// readline and most terminals/editors do for Ctrl+arrow, which
/// is what the buffer's caret needs to mirror.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '\''
}

/// Where the caret lands on `Ctrl+Left`. Walk past any non-word
/// chars immediately to the caret's left, then to the start of
/// the next word over.
fn prev_word_boundary(s: &str, from: usize) -> usize {
    let left = &s[..from];
    let trim: usize = left
        .chars()
        .rev()
        .take_while(|&c| !is_word_char(c))
        .map(char::len_utf8)
        .sum();
    let trimmed_end = left.len() - trim;
    let word_chars: usize = left[..trimmed_end]
        .chars()
        .rev()
        .take_while(|&c| is_word_char(c))
        .map(char::len_utf8)
        .sum();
    trimmed_end - word_chars
}

/// Where the caret lands on `Ctrl+Right`. Walk past any non-word
/// chars to the caret's right, then past the end of that word.
fn next_word_boundary(s: &str, from: usize) -> usize {
    let right = &s[from..];
    let skip: usize = right
        .chars()
        .take_while(|&c| !is_word_char(c))
        .map(char::len_utf8)
        .sum();
    let word_chars: usize = right[skip..]
        .chars()
        .take_while(|&c| is_word_char(c))
        .map(char::len_utf8)
        .sum();
    from + skip + word_chars
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
    fn empty_buffer_has_no_word() {
        let buf = Buffer::default();
        assert!(buf.is_empty());
        assert_eq!(buf.word_at_caret(), None);
    }

    #[test]
    fn word_at_caret_at_end_of_word() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "vernuer");
        let at = buf.word_at_caret().unwrap();
        assert_eq!(at.word, "vernuer");
        assert_eq!(at.trailing, "");
        assert_eq!(at.chars_before_caret, 7);
        assert_eq!(at.chars_after_caret, 0);
    }

    #[test]
    fn word_at_caret_in_trailing_whitespace_picks_the_left_word() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "vernuer ");
        let at = buf.word_at_caret().unwrap();
        assert_eq!(at.word, "vernuer");
        assert_eq!(at.trailing, " ");
        assert_eq!(at.chars_before_caret, 7);
        assert_eq!(at.chars_after_caret, 0);
    }

    #[test]
    fn word_at_caret_inside_word_expands_both_directions() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "vernuer");
        // Land the caret between "ver" and "nuer".
        for _ in 0..4 {
            buf.push(Key::MoveLeft);
        }
        let at = buf.word_at_caret().unwrap();
        assert_eq!(at.word, "vernuer");
        assert_eq!(at.chars_before_caret, 3);
        assert_eq!(at.chars_after_caret, 4);
    }

    #[test]
    fn word_at_caret_picks_the_final_word() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "the quick vernuer ");
        let at = buf.word_at_caret().unwrap();
        assert_eq!(at.word, "vernuer");
    }

    #[test]
    fn all_whitespace_has_no_word_at_caret() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "   ");
        assert_eq!(buf.word_at_caret(), None);
    }

    #[test]
    fn word_at_caret_handles_multibyte_chars() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "café ");
        let at = buf.word_at_caret().unwrap();
        assert_eq!(at.word, "café");
        assert_eq!(at.chars_before_caret, 4);
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
        assert_eq!(buf.word_at_caret(), None);
    }

    #[test]
    fn buffer_is_bounded_by_capacity() {
        let mut buf = Buffer::with_capacity(5);
        type_str(&mut buf, "abcdefgh");
        assert_eq!(buf.text(), "defgh");
    }

    #[test]
    fn sentence_at_caret_after_an_ender() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "Hello there. how are you ");
        let at = buf.sentence_at_caret().unwrap();
        assert_eq!(at.sentence, "how are you");
        assert_eq!(at.trailing, " ");
        assert_eq!(at.chars_after_caret, 0);
    }

    #[test]
    fn sentence_at_caret_when_no_ender_yet() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "the quick brown fox");
        let at = buf.sentence_at_caret().unwrap();
        assert_eq!(at.sentence, "the quick brown fox");
        assert_eq!(at.trailing, "");
    }

    #[test]
    fn sentence_at_caret_with_multiple_enders_picks_current() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "Hi! Hello there. How are yu");
        let at = buf.sentence_at_caret().unwrap();
        assert_eq!(at.sentence, "How are yu");
    }

    #[test]
    fn sentence_at_caret_includes_the_trailing_ender() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "Hello there.");
        let at = buf.sentence_at_caret().unwrap();
        assert_eq!(at.sentence, "Hello there.");
    }

    #[test]
    fn sentence_at_caret_picks_the_final_of_multiple_complete_sentences() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "First sentence. Second sentence!");
        let at = buf.sentence_at_caret().unwrap();
        assert_eq!(at.sentence, "Second sentence!");
    }

    #[test]
    fn sentence_at_caret_after_complete_one_then_trailing_ws() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "Hello there.   ");
        let at = buf.sentence_at_caret().unwrap();
        assert_eq!(at.sentence, "Hello there.");
        assert_eq!(at.trailing, "   ");
    }

    #[test]
    fn sentence_at_caret_returns_none_for_whitespace_only() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "   ");
        assert!(buf.sentence_at_caret().is_none());
    }

    #[test]
    fn sentence_at_caret_in_middle_spans_both_sides() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "the quick brown fox jumps");
        // Land caret after "brown".
        for _ in 0..10 {
            buf.push(Key::MoveLeft);
        }
        let at = buf.sentence_at_caret().unwrap();
        assert_eq!(at.sentence, "the quick brown fox jumps");
        assert_eq!(at.chars_before_caret, 15);
        assert_eq!(at.chars_after_caret, 10);
    }

    #[test]
    fn apply_mirrors_a_correction_at_end() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "vernuer ");
        buf.apply(8, "veneer ");
        assert_eq!(buf.text(), "veneer ");
        assert_eq!(buf.word_at_caret().unwrap().word, "veneer");
    }

    #[test]
    fn apply_around_caret_mirrors_a_mid_word_fix() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "vernuer trailing");
        // Park caret right after "vernuer".
        for _ in 0..9 {
            buf.push(Key::MoveLeft);
        }
        // Mid-word edit emitting BackSpace × 3 + Delete × 4 + "veneer":
        // pretend caret was between "ver" and "nuer" instead of after
        // "vernuer". Functionally checks the both-sides drain.
        for _ in 0..4 {
            buf.push(Key::MoveLeft);
        }
        buf.apply_around_caret(3, 4, "veneer");
        assert_eq!(buf.text(), "veneer trailing");
    }

    #[test]
    fn move_left_walks_caret_back_without_clearing_text() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "hello world");
        for _ in 0..6 {
            buf.push(Key::MoveLeft);
        }
        assert_eq!(buf.text(), "hello world");
        assert_eq!(buf.text_before_caret(), "hello");
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
    fn line_start_and_line_end_jump_to_the_edges() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "hello world");
        buf.push(Key::LineStart);
        assert_eq!(buf.text_before_caret(), "");
        buf.push(Key::LineEnd);
        assert_eq!(buf.text_before_caret(), "hello world");
    }

    #[test]
    fn word_left_jumps_to_previous_word_start() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "the quick brown fox");
        buf.push(Key::WordLeft);
        assert_eq!(buf.text_before_caret(), "the quick brown ");
        buf.push(Key::WordLeft);
        assert_eq!(buf.text_before_caret(), "the quick ");
        buf.push(Key::WordLeft);
        assert_eq!(buf.text_before_caret(), "the ");
        buf.push(Key::WordLeft);
        assert_eq!(buf.text_before_caret(), "");
    }

    #[test]
    fn word_right_jumps_to_next_word_end() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "the quick brown fox");
        buf.push(Key::LineStart);
        buf.push(Key::WordRight);
        assert_eq!(buf.text_before_caret(), "the");
        buf.push(Key::WordRight);
        assert_eq!(buf.text_before_caret(), "the quick");
    }

    #[test]
    fn word_left_from_mid_word_lands_at_word_start() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "hello world");
        for _ in 0..3 {
            buf.push(Key::MoveLeft);
        }
        // caret is between "wo" and "rld"
        assert_eq!(buf.text_before_caret(), "hello wo");
        buf.push(Key::WordLeft);
        assert_eq!(buf.text_before_caret(), "hello ");
    }

    #[test]
    fn ctrl_left_skips_commas_like_a_typical_editor() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "hello, world");
        buf.push(Key::WordLeft);
        assert_eq!(buf.text_before_caret(), "hello, ");
        buf.push(Key::WordLeft);
        // Should land at start of "hello", not start of "hello,"
        // — punctuation isn't part of the word.
        assert_eq!(buf.text_before_caret(), "");
    }

    #[test]
    fn word_at_caret_excludes_trailing_punctuation() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "recieve,");
        let at = buf.word_at_caret().expect("word at caret");
        assert_eq!(at.word, "recieve");
        assert_eq!(at.trailing, ",");
    }

    #[test]
    fn word_at_caret_keeps_apostrophes_for_contractions() {
        let mut buf = Buffer::default();
        type_str(&mut buf, "don't");
        let at = buf.word_at_caret().expect("word at caret");
        assert_eq!(at.word, "don't");
    }
}
