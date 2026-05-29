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

/// Byte ranges, within `text`, of the words that differ from `other`
/// (those not in the two strings' longest common subsequence of words).
/// Used to underline misspellings in the original
/// (`changed_word_ranges(original, corrected)`, red) and the
/// corrections in the proposed text (`changed_word_ranges(corrected,
/// original)`, blue).
pub fn changed_word_ranges(text: &str, other: &str) -> Vec<(usize, usize)> {
    let text_spans = word_spans(text);
    let other_words: Vec<&str> = word_spans(other).into_iter().map(|s| s.0).collect();
    let text_words: Vec<&str> = text_spans.iter().map(|s| s.0).collect();
    // `lcs_matched_b_indices` returns matched indices of its second
    // argument, so pass `text_words` there to get matched text words.
    let matched = lcs_matched_b_indices(&other_words, &text_words);
    text_spans
        .iter()
        .enumerate()
        .filter(|(i, _)| !matched.contains(i))
        .map(|(_, &(_, s, e))| (s, e))
        .collect()
}

/// A shared column grid that lets each corrected word sit directly under
/// the original word it replaces — even when the correction added or
/// removed words. Built from the LCS word pairing: matched words share a
/// column, substituted words pair 1:1, and an inserted or deleted word
/// gets a column to itself (a blank gap on the other row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlignLayout {
    /// Display width (char count) of each column — the wider of the two
    /// words occupying it (a one-sided column uses just its word).
    pub col_widths: Vec<usize>,
    /// Column index of each original word, in order.
    pub orig_cols: Vec<usize>,
    /// Column index of each corrected word, in order.
    pub corr_cols: Vec<usize>,
}

/// Lay the original and corrected words out in shared columns so the
/// popup can render one directly above the other. `None` only when
/// neither sentence has a word (nothing to align).
pub fn align(original: &str, corrected: &str) -> Option<AlignLayout> {
    let orig_owned = word_list(original);
    let corr_owned = word_list(corrected);
    if orig_owned.is_empty() && corr_owned.is_empty() {
        return None;
    }
    let orig: Vec<&str> = orig_owned.iter().map(String::as_str).collect();
    let corr: Vec<&str> = corr_owned.iter().map(String::as_str).collect();
    // Column widths use each word's *extent* — its char count plus any
    // punctuation bound to it — so a trailing comma is folded into that
    // word's column instead of shoving every later column sideways. The
    // LCS still matches on the bare words.
    let oe = word_extents(original);
    let ce = word_extents(corrected);

    let mut layout = AlignLayout {
        col_widths: Vec::new(),
        orig_cols: vec![0; orig.len()],
        corr_cols: vec![0; corr.len()],
    };
    let (mut i, mut j) = (0usize, 0usize);
    for (mi, mj) in lcs_matched_pairs(&orig, &corr) {
        emit_run(&mut layout, &oe, &ce, &mut i, &mut j, mi, mj);
        // The matched word: shared column, advance past it on both sides.
        let col = layout.col_widths.len();
        layout.orig_cols[i] = col;
        layout.corr_cols[j] = col;
        layout.col_widths.push(oe[i].max(ce[j]));
        i += 1;
        j += 1;
    }
    // Trailing unmatched words after the last LCS anchor.
    emit_run(
        &mut layout,
        &oe,
        &ce,
        &mut i,
        &mut j,
        orig.len(),
        corr.len(),
    );
    Some(layout)
}

/// Assign columns to the unmatched run `orig[i..ai]` / `corr[j..aj]`,
/// pulling column widths from the per-word extents `oe`/`ce`: pair words
/// 1:1 as substitutions up to the shorter side, then give any leftover
/// original (deletion) or corrected (insertion) word its own column.
/// Advances `i`/`j` to `ai`/`aj`.
fn emit_run(
    layout: &mut AlignLayout,
    oe: &[usize],
    ce: &[usize],
    i: &mut usize,
    j: &mut usize,
    ai: usize,
    aj: usize,
) {
    let (da, db) = (ai - *i, aj - *j);
    let sub = da.min(db);
    for k in 0..sub {
        let col = layout.col_widths.len();
        layout.orig_cols[*i + k] = col;
        layout.corr_cols[*j + k] = col;
        layout.col_widths.push(oe[*i + k].max(ce[*j + k]));
    }
    for k in sub..da {
        let col = layout.col_widths.len();
        layout.orig_cols[*i + k] = col;
        layout.col_widths.push(oe[*i + k]);
    }
    for k in sub..db {
        let col = layout.col_widths.len();
        layout.corr_cols[*j + k] = col;
        layout.col_widths.push(ce[*j + k]);
    }
    *i = ai;
    *j = aj;
}

/// Split a separator run into the punctuation bound to the *preceding*
/// word — its leading run of non-whitespace — and the whitespace gap that
/// follows: `", "` → `(",", " ")`, `" "` → `("", " ")`, `"?"` → `("?", "")`.
/// Lets the popup fold a word's trailing punctuation into its column.
pub fn split_separator(sep: &str) -> (&str, &str) {
    let end = sep.find(char::is_whitespace).unwrap_or(sep.len());
    sep.split_at(end)
}

/// Each word's display extent (char count) in order: the word plus any
/// punctuation bound to it via [`split_separator`].
fn word_extents(s: &str) -> Vec<usize> {
    let toks = split_tokens(s);
    let mut out = Vec::new();
    for (idx, (is_word, text)) in toks.iter().enumerate() {
        if *is_word {
            let punct = toks
                .get(idx + 1)
                .filter(|(next_is_word, _)| !next_is_word)
                .map(|(_, sep)| split_separator(sep).0.chars().count())
                .unwrap_or(0);
            out.push(text.chars().count() + punct);
        }
    }
    out
}

/// The whitespace/punctuation-delimited words of `s`, in order.
pub fn word_list(s: &str) -> Vec<String> {
    word_spans(s)
        .into_iter()
        .map(|(w, _, _)| w.to_string())
        .collect()
}

/// Split `s` into ordered `(is_word, text)` tokens — words vs separator
/// runs — for column-aligned rendering of a static run.
pub fn split_tokens(s: &str) -> Vec<(bool, String)> {
    tokenize(s)
        .into_iter()
        .map(|t| match t {
            Tok::Word(w) => (true, w),
            Tok::Sep(s) => (false, s),
        })
        .collect()
}

/// Each word in `s` paired with its `[start, end)` byte range.
fn word_spans(s: &str) -> Vec<(&str, usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut in_word = false;
    for (i, c) in s.char_indices() {
        let w = is_word_char(c);
        if w && !in_word {
            start = i;
            in_word = true;
        } else if !w && in_word {
            out.push((&s[start..i], start, i));
            in_word = false;
        }
    }
    if in_word {
        out.push((&s[start..], start, s.len()));
    }
    out
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
    lcs_matched_pairs(a, b)
        .into_iter()
        .map(|(_, j)| j)
        .collect()
}

/// The `(a_index, b_index)` pairs matched by an LCS of `a` and `b`, in
/// increasing order on both sides. [`lcs_matched_b_indices`] keeps just
/// the `b` side; [`align`] needs both to map original words to corrected.
fn lcs_matched_pairs(a: &[&str], b: &[&str]) -> Vec<(usize, usize)> {
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
    let mut pairs = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            pairs.push((i, j));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    pairs
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

    #[test]
    fn align_substitutions_share_columns() {
        // Same word count: every word pairs into its own column, width =
        // the wider of the two.
        let a = align("teh quick browne fox jumpd", "the quick brown fox jumped").unwrap();
        assert_eq!(a.col_widths, vec![3, 5, 6, 3, 6]);
        assert_eq!(a.orig_cols, vec![0, 1, 2, 3, 4]);
        assert_eq!(a.corr_cols, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn align_inserted_word_gets_its_own_column() {
        // "quick" inserted: the original has no word in that column.
        let a = align("the fox", "the quick fox").unwrap();
        assert_eq!(a.col_widths, vec![3, 5, 3]);
        assert_eq!(a.orig_cols, vec![0, 2]); // the→col0, fox→col2
        assert_eq!(a.corr_cols, vec![0, 1, 2]); // the, quick(col1), fox
    }

    #[test]
    fn align_deleted_word_gets_its_own_column() {
        // "quick" deleted: the corrected has no word in that column.
        let a = align("the quick fox", "the fox").unwrap();
        assert_eq!(a.col_widths, vec![3, 5, 3]);
        assert_eq!(a.orig_cols, vec![0, 1, 2]); // the, quick(col1), fox
        assert_eq!(a.corr_cols, vec![0, 2]); // the→col0, fox→col2
    }

    #[test]
    fn align_split_word_substitutes_then_inserts() {
        // "alot"→"a lot": "a" substitutes alot (col2, width 4), "lot" is
        // an insertion in col3 (a gap below it in the original); the
        // shared "eat"/"of"/"food" keep both rows lined up.
        let a = align("i eat alot of food", "I eat a lot of food").unwrap();
        assert_eq!(a.col_widths, vec![1, 3, 4, 3, 2, 4]);
        assert_eq!(a.orig_cols, vec![0, 1, 2, 4, 5]); // i,eat,alot,of,food
        assert_eq!(a.corr_cols, vec![0, 1, 2, 3, 4, 5]); // I,eat,a,lot,of,food
    }

    #[test]
    fn align_folds_trailing_punctuation_into_the_column() {
        // The only word change is i→I, but the correction adds a comma
        // after "well". Folding it into column 0 (width = max("well"=4,
        // "well,"=5) = 5) keeps the later columns aligned instead of
        // shoving them right by the comma.
        let a = align("well i think", "well, I think").unwrap();
        assert_eq!(a.col_widths, vec![5, 1, 5]);
        assert_eq!(a.orig_cols, vec![0, 1, 2]);
        assert_eq!(a.corr_cols, vec![0, 1, 2]);
    }

    #[test]
    fn split_separator_peels_leading_punctuation() {
        assert_eq!(split_separator(", "), (",", " "));
        assert_eq!(split_separator(" "), ("", " "));
        assert_eq!(split_separator("?"), ("?", ""));
        assert_eq!(split_separator(". "), (".", " "));
    }

    #[test]
    fn align_none_only_when_wordless() {
        assert!(align("", "").is_none());
        assert!(align("hi", "").is_some());
        assert!(align("", "hi").is_some());
    }

    #[test]
    fn changed_word_ranges_marks_differing_words() {
        // misspellings in the original (red squiggle targets).
        let r = changed_word_ranges("teh quick browne fox", "the quick brown fox");
        assert_eq!(r, vec![(0, 3), (10, 16)]); // "teh", "browne"
        // corrections in the corrected text (blue squiggle targets).
        let r2 = changed_word_ranges("the quick brown fox", "teh quick browne fox");
        assert_eq!(r2, vec![(0, 3), (10, 15)]); // "the", "brown"
    }
}
