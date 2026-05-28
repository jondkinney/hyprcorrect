//! Word-level diff between the original sentence and the smart
//! provider's proposed correction, for the review popup's word-edit
//! mode. The diff decides *which* corrected words the user can edit
//! inline: words the corrector changed (or added) become editable
//! fields; everything carried through unchanged stays static text.
//!
//! Pure and egui-free so it can be unit-tested directly; the popup
//! ([`crate::review`]) assigns widget ids to the [`Segment::Field`]s
//! by their order and renders the rest as labels.

use std::collections::HashSet;

/// One ordered piece of the *corrected* sentence.
///
/// Concatenating every segment's text back together — see
/// [`reconstruct`] — yields the corrected string exactly, so after the
/// user edits the [`Field`](Segment::Field) segments in place the same
/// concatenation yields the edited sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Text carried through from the original unchanged, plus every
    /// separator (whitespace/punctuation). Rendered as a static label;
    /// never edited. Adjacent statics are merged into one segment.
    Static(String),
    /// A corrected word the diff flagged as changed or newly inserted.
    /// Rendered as an editable single-line field; the popup mutates
    /// this `String` in place.
    Field(String),
}

impl Segment {
    /// The segment's text regardless of variant.
    pub fn text(&self) -> &str {
        match self {
            Segment::Static(t) | Segment::Field(t) => t,
        }
    }
}

/// Split `corrected` into ordered [`Segment`]s, marking as editable
/// [`Field`](Segment::Field)s the words that differ from `original`.
///
/// Words common to both (in longest-common-subsequence order) stay
/// [`Static`](Segment::Static); corrected words with no LCS match are
/// editable. Words deleted by the correction simply don't appear (the
/// output spans the corrected text, not the original). When the change
/// touched only separators (e.g. an added comma) the result has zero
/// `Field`s — the caller falls back to "nothing to tab through, apply
/// or Ctrl+E".
pub fn diff(original: &str, corrected: &str) -> Vec<Segment> {
    let orig_toks = tokenize(original);
    let corr_toks = tokenize(corrected);

    let orig_words: Vec<&str> = orig_toks.iter().filter_map(Tok::word).collect();
    let corr_words: Vec<&str> = corr_toks.iter().filter_map(Tok::word).collect();
    let matched = lcs_matched_b_indices(&orig_words, &corr_words);

    let mut segments: Vec<Segment> = Vec::new();
    let mut corr_word_idx = 0usize;
    for tok in &corr_toks {
        match tok {
            Tok::Sep(s) => push_static(&mut segments, s),
            Tok::Word(w) => {
                let unchanged = matched.contains(&corr_word_idx);
                corr_word_idx += 1;
                if unchanged {
                    push_static(&mut segments, w);
                } else {
                    segments.push(Segment::Field(w.clone()));
                }
            }
        }
    }
    segments
}

/// Concatenate every segment's text — the inverse of [`diff`]'s split,
/// and the way the popup turns edited segments back into a sentence.
pub fn reconstruct(segments: &[Segment]) -> String {
    let mut out = String::new();
    for seg in segments {
        out.push_str(seg.text());
    }
    out
}

/// Byte offset, within [`reconstruct`]'s output, where the
/// `ordinal`-th editable field begins (0-based). Lets the popup drop
/// the vim cursor onto the word the user had focused when they hit
/// Ctrl+E. `None` if there are fewer than `ordinal + 1` fields.
pub fn field_start_offset(segments: &[Segment], ordinal: usize) -> Option<usize> {
    let mut offset = 0usize;
    let mut seen = 0usize;
    for seg in segments {
        if let Segment::Field(_) = seg {
            if seen == ordinal {
                return Some(offset);
            }
            seen += 1;
        }
        offset += seg.text().len();
    }
    None
}

/// Append `text` to the trailing [`Static`](Segment::Static) if there
/// is one, otherwise start a new static segment — keeps consecutive
/// unchanged words and separators in a single label.
fn push_static(segments: &mut Vec<Segment>, text: &str) {
    if let Some(Segment::Static(last)) = segments.last_mut() {
        last.push_str(text);
    } else {
        segments.push(Segment::Static(text.to_string()));
    }
}

/// A tokenized run of `corrected`/`original`: either a word or the
/// separator run between words.
enum Tok {
    Word(String),
    Sep(String),
}

impl Tok {
    fn word(&self) -> Option<&str> {
        match self {
            Tok::Word(w) => Some(w),
            Tok::Sep(_) => None,
        }
    }
}

/// Split into maximal runs of word-chars vs non-word-chars, preserving
/// every character so the pieces re-join losslessly.
fn tokenize(s: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_is_word: Option<bool> = None;
    for c in s.chars() {
        let is_word = is_word_char(c);
        match cur_is_word {
            Some(prev) if prev == is_word => cur.push(c),
            Some(prev) => {
                out.push(finish(prev, std::mem::take(&mut cur)));
                cur.push(c);
                cur_is_word = Some(is_word);
            }
            None => {
                cur.push(c);
                cur_is_word = Some(is_word);
            }
        }
    }
    if let Some(prev) = cur_is_word {
        out.push(finish(prev, cur));
    }
    out
}

fn finish(is_word: bool, text: String) -> Tok {
    if is_word {
        Tok::Word(text)
    } else {
        Tok::Sep(text)
    }
}

/// The same "word char" rule the keystroke buffer uses
/// (`hyprcorrect_core::buffer::is_word_char`): alphanumerics plus the
/// apostrophe, so contractions like `don't` stay one word but commas
/// and periods are separators. Duplicated here to keep this module
/// dependency-light; the two must agree on what a "word" is.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '\''
}

/// Indices into `b` of the words matched by an LCS of `a` and `b`.
/// Those corrected words (`b`) are "unchanged"; the rest are editable.
fn lcs_matched_b_indices(a: &[&str], b: &[&str]) -> HashSet<usize> {
    let (n, m) = (a.len(), b.len());
    // dp[i][j] = LCS length of a[i..] and b[j..].
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut matched = HashSet::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            matched.insert(j);
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    matched
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(segments: &[Segment]) -> Vec<&str> {
        segments
            .iter()
            .filter_map(|s| match s {
                Segment::Field(t) => Some(t.as_str()),
                Segment::Static(_) => None,
            })
            .collect()
    }

    #[test]
    fn single_word_substitution_is_one_field() {
        let segs = diff("recieve", "receive");
        assert_eq!(segs, vec![Segment::Field("receive".into())]);
        assert_eq!(reconstruct(&segs), "receive");
    }

    #[test]
    fn only_the_changed_word_is_editable() {
        let segs = diff("teh quick", "the quick");
        assert_eq!(fields(&segs), vec!["the"]);
        assert_eq!(
            segs,
            vec![
                Segment::Field("the".into()),
                Segment::Static(" quick".into())
            ]
        );
        assert_eq!(reconstruct(&segs), "the quick");
    }

    #[test]
    fn an_inserted_word_becomes_a_field() {
        let segs = diff("the fox", "the quick fox");
        assert_eq!(fields(&segs), vec!["quick"]);
        assert_eq!(reconstruct(&segs), "the quick fox");
    }

    #[test]
    fn a_deleted_word_leaves_no_field() {
        let segs = diff("the quick fox", "the fox");
        assert!(fields(&segs).is_empty());
        assert_eq!(reconstruct(&segs), "the fox");
    }

    #[test]
    fn case_only_change_is_editable() {
        let segs = diff("hello", "Hello");
        assert_eq!(fields(&segs), vec!["Hello"]);
    }

    #[test]
    fn separator_only_change_has_no_fields() {
        // Added a comma; both words are unchanged, so word-edit mode
        // has nothing to tab through (Ctrl+E / Apply still work).
        let segs = diff("hello world", "hello, world");
        assert!(fields(&segs).is_empty());
        assert_eq!(reconstruct(&segs), "hello, world");
    }

    #[test]
    fn multi_word_correction_marks_each_changed_word() {
        let segs = diff("i went too the stor", "I went to the store");
        assert_eq!(fields(&segs), vec!["I", "to", "store"]);
        assert_eq!(reconstruct(&segs), "I went to the store");
    }

    #[test]
    fn field_start_offset_points_at_each_field() {
        let segs = diff("i went too the stor", "I went to the store");
        // "I went to the store"
        //  0 2    7  10  14
        assert_eq!(field_start_offset(&segs, 0), Some(0)); // "I"
        assert_eq!(field_start_offset(&segs, 1), Some(7)); // "to"
        assert_eq!(field_start_offset(&segs, 2), Some(14)); // "store"
        assert_eq!(field_start_offset(&segs, 3), None);
    }

    #[test]
    fn reconstruct_round_trips_for_varied_pairs() {
        let pairs = [
            ("teh quick brown fox", "the quick brown fox"),
            ("recieve", "receive"),
            ("its a test", "it's a test"),
            ("hello world", "hello, world"),
            ("the fox", "the quick brown fox"),
            ("a b c d e", "a x c y e"),
        ];
        for (o, c) in pairs {
            assert_eq!(reconstruct(&diff(o, c)), c, "round-trip failed for {c:?}");
        }
    }

    #[test]
    fn multibyte_words_round_trip() {
        let segs = diff("cafe au lait", "café au lait");
        assert_eq!(fields(&segs), vec!["café"]);
        assert_eq!(reconstruct(&segs), "café au lait");
        assert_eq!(field_start_offset(&segs, 0), Some(0));
    }
}
