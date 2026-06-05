//! The review popup — opens on the configured `review` chord and
//! shows the smart provider's proposed correction for the focused
//! window's last sentence. The user can:
//!
//! - **Word-edit mode** (default): each word the corrector *changed*
//!   is an inline editable field; unchanged words are static text.
//!   The first changed word opens focused and selected, so typing
//!   replaces it. Tab/Shift+Tab and ←/→ move between fields, Enter
//!   applies, Esc cancels.
//! - **Vim mode** (`Ctrl+E`): the whole sentence becomes a small
//!   modal editor ([`crate::vimedit`]) for free-form fixing when the
//!   correction is wrong. `:wq` / normal-mode Enter applies, `:q`
//!   cancels.
//!
//! Either way the daemon does the actual emit: the popup writes the
//! (possibly edited) sentence back into the review-request file and
//! signals, so focus has time to return to the source window first.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use eframe::egui;
use egui::text::{CCursor, CCursorRange, LayoutJob};
use hyprcorrect_core::runtime::{self, ReviewRequest};

use crate::vimedit::{self, VimEdit, VimKey, VimOutcome};
use crate::worddiff::{self, Segment};

const APP_ID: &str = "hyprcorrect-review";
const REFOCUS_DELAY_MS: u64 = 280;
const MIN_WINDOW_WIDTH: f32 = 520.0;
/// Width cap when the daemon couldn't tell us the screen size.
const FALLBACK_MAX_WIDTH: f32 = 1000.0;
const MIN_WINDOW_HEIGHT: f32 = 240.0;
const MAX_WINDOW_HEIGHT: f32 = 900.0;

/// Run the review popup. Reads the pending review request from the
/// runtime file; if there isn't one, returns immediately (the
/// daemon spawns this binary blindly on the review chord and might
/// race against an empty buffer).
pub(crate) fn run() {
    let request = match runtime::read_review_request() {
        Ok(Some(req)) => req,
        Ok(None) => return,
        Err(e) => {
            eprintln!("hyprcorrect: could not read review request: {e}");
            return;
        }
    };

    // Size to fit the whole popup up front — heading, both cards, and the
    // inline suggestion dropdown (now known: with deferred spawn the daemon
    // resolves the correction before launching us on the fast path). Resizing
    // afterward isn't viable: the popup is a *centered* floating window and
    // Hyprland won't re-center it after a grow. Capped at the usable screen
    // height by the estimator.
    let definitions_on = !matches!(
        hyprcorrect_core::Config::load()
            .map(|c| c.behavior.definitions)
            .unwrap_or_default(),
        hyprcorrect_core::DefinitionSource::Off
    );
    let width = estimate_window_width(&request);
    let estimated_height = estimate_window_height(&request, definitions_on);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id(APP_ID)
            .with_title("hyprcorrect — Review")
            .with_inner_size([width, estimated_height])
            .with_min_inner_size([width.min(MIN_WINDOW_WIDTH), MIN_WINDOW_HEIGHT])
            .with_resizable(true),
        vsync: false,
        ..Default::default()
    };
    let _ = eframe::run_native(
        "hyprcorrect — Review",
        options,
        Box::new(move |cc| {
            kanso::fonts::install(
                &cc.egui_ctx,
                &kanso::fonts::FontOptions {
                    shortcut_family: true,
                    ..Default::default()
                },
            );
            Ok(Box::new(ReviewApp::new(request)))
        }),
    );
}

/// Which editing surface is currently active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditMode {
    Word,
    Vim,
}

/// An open `z=` spell-suggest dropdown in vim mode: the word it's for, its
/// byte range in the buffer, the ranked options, and the highlighted row.
struct VimSuggest {
    start: usize,
    end: usize,
    word: String,
    options: Vec<String>,
    highlight: Option<usize>,
}

struct ReviewApp {
    request: ReviewRequest,
    /// `"apply"` or `"cancel"` once the user decides. `None` until
    /// the window closes (X-button close → cancel).
    decision: Option<&'static str>,
    mode: EditMode,
    /// The corrected sentence split into static text + editable
    /// (changed) words; the source of truth for word-edit mode.
    segments: Vec<Segment>,
    /// Ordinals (0-based) → segment index, for each editable field in
    /// visual order.
    field_segments: Vec<usize>,
    /// The editable field (by ordinal) that currently has focus.
    focused_field: Option<usize>,
    /// A field (by ordinal) to focus + select-all on the next frame.
    pending_focus: Option<usize>,
    /// `(caret_char_index, field_len_chars, has_selection)` captured
    /// from the focused field last frame — drives ←/→ boundary nav.
    focus_caret: Option<(usize, usize, bool)>,
    /// Set once we've requested the initial focus.
    initialized: bool,
    /// The vim editor, built lazily on the first `Ctrl+E`.
    vim: Option<VimEdit>,
    /// Byte ranges of the corrected words in the vim buffer, each
    /// `Some` until the user edits within it (then `None`) — drives the
    /// blue squiggles in vim mode.
    vim_marks: Vec<Option<(usize, usize)>>,
    /// Index into the focused field's suggestion dropdown that is
    /// highlighted via Up/Down, or `None` when nothing is highlighted.
    dropdown_highlight: Option<usize>,
    /// Shared column grid (from a word-level diff) so each correction
    /// renders in monospace directly under the original word it replaces,
    /// even when the correction added or removed words. `None` only when
    /// there's nothing to align (an empty sentence).
    align: Option<worddiff::AlignLayout>,
    /// `false` while the daemon is still computing (the popup shows
    /// "Checking…" and polls); flips `true` once the finished request
    /// is loaded and the review state below is built.
    ready: bool,
    /// The original word each field replaced, by field ordinal — for the
    /// "revert to original" dropdown entry. Empty unless the change is a
    /// clean 1:1 substitution (changed-word counts match).
    field_originals: Vec<String>,
    /// An open `z=` spell-suggest dropdown in vim mode, or `None`.
    vim_suggest: Option<VimSuggest>,
    /// True between an "Ask LLM" escalation and its reloaded result, so a
    /// no-change LLM pass doesn't trigger the initial close-on-no-op.
    reprocessing: bool,
    /// When set (from `behavior.review_starts_in_vim`), the first
    /// successful `load_review` flips straight into vim mode. Cleared
    /// after, so flipping back to word mode (Ctrl+E) sticks.
    pending_initial_vim: bool,
    /// Definition source for the suggestion dropdown (Off / Local /
    /// Online), read from config at startup.
    def_source: hyprcorrect_core::DefinitionSource,
    /// Resolved definitions keyed by lowercased word; a `None` value means
    /// "looked up, none found". Online results arrive via `def_rx`.
    def_cache: HashMap<String, Option<String>>,
    /// Words with an online lookup in flight, so repeated frames don't
    /// spawn duplicate fetches.
    def_inflight: HashSet<String>,
    /// Channel carrying online definition results back from worker
    /// threads (`(lowercased word, result)`).
    def_tx: Sender<(String, Option<String>)>,
    def_rx: Receiver<(String, Option<String>)>,
}

/// What the dropdown should show on its definition line for the
/// currently-highlighted option.
enum DefView {
    /// Definitions disabled (or nothing to define) — no line.
    Off,
    /// Online lookup in flight.
    Loading,
    /// Looked up, no definition found.
    Missing,
    /// The definition text.
    Text(String),
}

impl ReviewApp {
    fn new(request: ReviewRequest) -> Self {
        let behavior = hyprcorrect_core::Config::load()
            .map(|c| c.behavior)
            .unwrap_or_default();
        let (def_tx, def_rx) = std::sync::mpsc::channel();
        let mut app = Self {
            request,
            decision: None,
            mode: EditMode::Word,
            segments: Vec::new(),
            field_segments: Vec::new(),
            focused_field: None,
            pending_focus: None,
            focus_caret: None,
            initialized: false,
            vim: None,
            vim_marks: Vec::new(),
            dropdown_highlight: None,
            align: None,
            ready: false,
            field_originals: Vec::new(),
            vim_suggest: None,
            reprocessing: false,
            pending_initial_vim: behavior.review_starts_in_vim,
            def_source: behavior.definitions,
            def_cache: HashMap::new(),
            def_inflight: HashSet::new(),
            def_tx,
            def_rx,
        };
        if !app.request.pending {
            app.load_review();
        }
        app
    }

    /// Drain any online definition results that worker threads have sent
    /// since the last frame into the cache. Called once per frame.
    fn drain_definitions(&mut self) {
        while let Ok((key, result)) = self.def_rx.try_recv() {
            self.def_inflight.remove(&key);
            self.def_cache.insert(key, result);
        }
    }

    /// Resolve the definition line for `word` under the current source.
    /// Local is a synchronous in-memory lookup; Online is fetched on a
    /// worker thread (cached, deduped) and returns `Loading` until it
    /// lands. The thread requests a repaint so the line fills in.
    fn definition_view(&mut self, ctx: &egui::Context, word: &str) -> DefView {
        use hyprcorrect_core::DefinitionSource;
        let key = word.trim().to_ascii_lowercase();
        if key.is_empty() {
            return DefView::Off;
        }
        match self.def_source {
            DefinitionSource::Off => DefView::Off,
            DefinitionSource::Local => {
                match hyprcorrect_core::define(word, DefinitionSource::Local) {
                    Some(d) => DefView::Text(d),
                    None => DefView::Missing,
                }
            }
            DefinitionSource::Online => {
                if let Some(cached) = self.def_cache.get(&key) {
                    return match cached {
                        Some(d) => DefView::Text(d.clone()),
                        None => DefView::Missing,
                    };
                }
                if self.def_inflight.insert(key.clone()) {
                    let tx = self.def_tx.clone();
                    let ctx = ctx.clone();
                    let word = word.to_string();
                    std::thread::spawn(move || {
                        let result = hyprcorrect_core::define_online(&word);
                        let _ = tx.send((key, result));
                        ctx.request_repaint();
                    });
                }
                DefView::Loading
            }
        }
    }

    /// Build the word-edit state from the (finished) request. Called
    /// once the daemon has written the correction.
    fn load_review(&mut self) {
        self.segments = worddiff::diff(&self.request.original, &self.request.corrected);
        self.align = worddiff::align(&self.request.original, &self.request.corrected);
        self.field_segments = self
            .segments
            .iter()
            .enumerate()
            .filter_map(|(i, s)| matches!(s, Segment::Field(_)).then_some(i))
            .collect();
        // The original word behind each field, for "revert to original" —
        // only when it's a clean 1:1 substitution (counts line up).
        let ranges = worddiff::changed_word_ranges(&self.request.original, &self.request.corrected);
        self.field_originals = if ranges.len() == self.field_segments.len() {
            ranges
                .iter()
                .map(|&(s, e)| self.request.original[s..e].to_string())
                .collect()
        } else {
            Vec::new()
        };
        self.initialized = false; // re-run focus init on the next frame
        self.ready = true;
        // Honor "open in vim mode" on the first load only — so a later
        // flip back to word mode (Ctrl+E) isn't undone on rebuild.
        if self.pending_initial_vim {
            self.pending_initial_vim = false;
            self.enter_vim();
        }
    }

    /// Dropdown entries for the focused field: the ranked alternatives,
    /// plus a "revert to original" entry when one is available. Each is
    /// `(label, value)` — the value is inserted, the label is shown.
    fn field_entries(&self, ordinal: usize, current: &str) -> Vec<(String, String)> {
        let mut entries: Vec<(String, String)> = self
            .options_for_field(ordinal, current)
            .into_iter()
            .map(|o| (o.clone(), o))
            .collect();
        if let Some(orig) = self.field_originals.get(ordinal) {
            if orig != current && !entries.iter().any(|(_, v)| v == orig) {
                entries.push((format!("↩  {orig}   (original)"), orig.clone()));
            }
        }
        entries
    }

    /// Ranked backup options to show for the focused field `ordinal`,
    /// minus whatever equals the field's current text, capped at 5.
    /// Pairs by position (daemon emits left-to-right, matching the
    /// fields); if the counts ever drift it matches by word instead.
    fn options_for_field(&self, ordinal: usize, current: &str) -> Vec<String> {
        let aligned = self.request.suggestions.len() == self.field_segments.len();
        let chosen = if aligned {
            self.request.suggestions.get(ordinal)
        } else {
            self.request
                .suggestions
                .iter()
                .find(|ws| ws.word == current)
                .or_else(|| self.request.suggestions.get(ordinal))
        };
        chosen
            .map(|ws| {
                ws.options
                    .iter()
                    .filter(|o| o.as_str() != current)
                    .take(5)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The current text of editable field `ordinal`.
    fn field_word(&self, ordinal: usize) -> String {
        self.field_segments
            .get(ordinal)
            .and_then(|&seg| self.segments.get(seg))
            .map(|s| s.text().to_string())
            .unwrap_or_default()
    }

    /// Replace field `ordinal`'s text with `option`, then advance to the
    /// next correction (focusing + selecting it) — or apply the dialog
    /// when this was the last one. Closes the dropdown.
    fn insert_suggestion(&mut self, ctx: &egui::Context, ordinal: usize, option: &str) {
        if let Some(&seg) = self.field_segments.get(ordinal) {
            if let Some(Segment::Field(t)) = self.segments.get_mut(seg) {
                *t = option.to_string();
            }
        }
        self.dropdown_highlight = None;
        if ordinal + 1 >= self.field_segments.len() {
            // Picked the last correction — apply.
            self.apply(ctx);
        } else {
            // Move on to the next correction.
            self.pending_focus = Some(ordinal + 1);
        }
    }

    /// Commit the (possibly edited) sentence and close. The actual
    /// emit happens in the daemon after [`on_exit`](Self::on_exit)
    /// writes the request back.
    fn apply(&mut self, ctx: &egui::Context) {
        self.request.corrected = match self.mode {
            EditMode::Word => worddiff::reconstruct(&self.segments),
            EditMode::Vim => self
                .vim
                .as_ref()
                .map(|v| v.text().to_string())
                .unwrap_or_else(|| self.request.corrected.clone()),
        };
        self.decision = Some("apply");
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn cancel(&mut self, ctx: &egui::Context) {
        self.decision = Some("cancel");
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// "Ask LLM": with an LLM key configured, tell the daemon to re-process
    /// the original sentence through the LLM (it reloads us via the request
    /// file) and flip to `pending` so "Checking…" shows at once. Without a
    /// key, open Preferences → Providers directly so the user can add one.
    fn escalate_llm(&mut self) {
        // No API key yet → take the user straight to where they add one.
        // Done here rather than via the daemon so it works even when the
        // daemon isn't around to handle the action.
        if !self.request.llm_available {
            open_prefs_providers();
            return;
        }
        let mut req = self.request.clone();
        req.pending = true;
        let _ = runtime::write_review_request(&req);
        self.ready = false;
        self.reprocessing = true;
        if let Err(e) = std::fs::write(runtime::action_path(), "review-llm") {
            eprintln!("hyprcorrect: could not write review-llm action: {e}");
            return;
        }
        notify_daemon();
    }

    /// Switch into vim mode, seeding the buffer with the current
    /// sentence and dropping the cursor on the focused word.
    fn enter_vim(&mut self) {
        let sentence = worddiff::reconstruct(&self.segments);
        let cursor = self
            .focused_field
            .and_then(|ord| worddiff::field_start_offset(&self.segments, ord))
            .unwrap_or(0);
        // Blue-squiggle the words that differ from the original; each
        // mark survives until the user edits within it.
        self.vim_marks = worddiff::changed_word_ranges(&sentence, &self.request.original)
            .into_iter()
            .map(Some)
            .collect();
        self.vim = Some(VimEdit::new(sentence, cursor));
        self.mode = EditMode::Vim;
    }

    /// Flip from vim back to word-edit mode, re-diffing the vim buffer
    /// against the original so any vim edits carry into the word fields.
    fn exit_vim(&mut self) {
        if let Some(text) = self.vim.as_ref().map(|v| v.text().to_string()) {
            self.request.corrected = text;
        }
        self.load_review();
        self.mode = EditMode::Word;
    }

    /// Move focus `delta` fields from the current one, wrapping.
    fn focus_relative(&mut self, delta: isize) {
        let len = self.field_segments.len();
        if len == 0 {
            return;
        }
        let cur = self.focused_field.unwrap_or(0) as isize;
        let next = (cur + delta).rem_euclid(len as isize) as usize;
        self.pending_focus = Some(next);
        self.dropdown_highlight = None;
    }

    // ---- word-edit mode -------------------------------------------

    fn input_word(&mut self, ctx: &egui::Context) {
        // Ctrl+E → vim mode (consume so the 'e' never lands in a field).
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::E)) {
            self.enter_vim();
            return;
        }

        // Suggestion dropdown for the focused field — handled before the
        // field-level Enter/Esc/arrows. Down/Up highlight, Enter inserts
        // the highlight, 1–5 insert directly (only while the field is
        // still fully selected, so digits don't hijack normal typing),
        // Esc closes the dropdown.
        if let Some(ord) = self.focused_field {
            let current = self.field_word(ord);
            let entries = self.field_entries(ord, &current);
            if !entries.is_empty() {
                if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)) {
                    let next = self
                        .dropdown_highlight
                        .map_or(0, |h| (h + 1).min(entries.len() - 1));
                    self.dropdown_highlight = Some(next);
                    return;
                }
                if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp)) {
                    self.dropdown_highlight = match self.dropdown_highlight {
                        Some(0) | None => None,
                        Some(h) => Some(h - 1),
                    };
                    return;
                }
                let pristine = self.focus_caret.map(|(_, _, sel)| sel).unwrap_or(false);
                if pristine {
                    for d in 1..=entries.len().min(5) {
                        if take_digit(ctx, d) {
                            let value = entries[d - 1].1.clone();
                            self.insert_suggestion(ctx, ord, &value);
                            return;
                        }
                    }
                }
                if let Some(h) = self.dropdown_highlight {
                    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)) {
                        let value = entries[h].1.clone();
                        self.insert_suggestion(ctx, ord, &value);
                        return;
                    }
                    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
                        self.dropdown_highlight = None;
                        return;
                    }
                }
            }
        }

        let (enter, esc) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::Escape),
            )
        });
        if enter {
            self.apply(ctx);
            return;
        }
        if esc {
            self.cancel(ctx);
            return;
        }

        // Tab / Shift+Tab cycle the editable fields. Consuming the key
        // stops egui's built-in focus cycle (which would also visit the
        // Cancel/Apply buttons).
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Tab)) {
            self.focus_relative(1);
            return;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::SHIFT, egui::Key::Tab)) {
            self.focus_relative(-1);
            return;
        }

        // ←/→ jump fields at the text boundary (or when the field is
        // freshly selected); otherwise they move within the word.
        if let Some((caret, n, sel)) = self.focus_caret {
            let left = ctx.input(|i| i.key_pressed(egui::Key::ArrowLeft));
            let right = ctx.input(|i| i.key_pressed(egui::Key::ArrowRight));
            if left && (sel || caret == 0) {
                ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft));
                self.focus_relative(-1);
            } else if right && (sel || caret == n) {
                ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight));
                self.focus_relative(1);
            }
        }
    }

    /// The "Checking…" state shown while the daemon computes the
    /// correction — the original text plus a spinner.
    fn render_checking(&self, ctx: &egui::Context) {
        egui::CentralPanel::default()
            .frame(
                egui::Frame::central_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(26, 22)),
            )
            .show(ctx, |ui| {
                ui.heading("Review correction");
                ui.add_space(16.0);
                section_label(ui, "Original");
                original_card(ui, &self.request.original, &self.request.original, None);
                ui.add_space(18.0);
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(16.0));
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new("Checking…  (Esc to cancel)")
                            .size(15.0)
                            .color(kanso::palette::TEXT_MUTED),
                    );
                });
            });
    }

    fn render_word(&mut self, ui: &mut egui::Ui) {
        // Precompute a column view of the corrected sentence: each word's
        // text + trailing separator, and which words are editable fields
        // (by their segment index). The field text stays owned by
        // `segments` so edits flow back through `reconstruct`.
        let (corr_words, corr_seps) = words_and_seps(&self.request.corrected);
        let corr_field = field_map(&self.segments);
        // Widen each column to fit the field's *current* text, so a field
        // the user has grown by typing re-wraps with everything else
        // instead of pushing the rest of the line off-screen.
        let layout = self.align.clone().map(|mut l| {
            for (k, &c) in l.corr_cols.iter().enumerate() {
                if c >= l.col_widths.len() {
                    continue;
                }
                let word = match corr_field.get(k).copied().flatten() {
                    Some(seg) => self.segments[seg].text().chars().count(),
                    None => corr_words.get(k).map_or(0, |w| w.chars().count()),
                };
                let punct = corr_seps
                    .get(k)
                    .map_or(0, |s| worddiff::split_separator(s).0.chars().count());
                l.col_widths[c] = l.col_widths[c].max(word + punct);
            }
            l
        });

        ui.heading("Review correction");
        ui.add_space(16.0);
        section_label(ui, "Original");
        original_card(
            ui,
            &self.request.original,
            &self.request.corrected,
            layout.as_ref(),
        );

        ui.add_space(18.0);
        if self.field_segments.is_empty() {
            section_label(ui, "Proposed  ·  Ctrl+E to edit in vim");
            let corrected = self.request.corrected.clone();
            card(ui, |ui| {
                ui.label(
                    egui::RichText::new(corrected)
                        .font(prose_font())
                        .color(TEXT_FG),
                );
            });
            return;
        }
        section_label(
            ui,
            "Proposed  ·  type to replace · Tab/arrows to move · Ctrl+E for vim",
        );

        let pending = self.pending_focus;
        let mut new_focused: Option<usize> = None;
        let mut new_caret: Option<(usize, usize, bool)> = None;
        let mut consumed_pending = false;
        let segments = &mut self.segments;

        card(ui, |ui| {
            let font = mono_font();
            let cw = char_width(ui, &font);
            let row_h = ui.fonts(|f| f.row_height(&font));
            let Some(l) = &layout else { return };
            let ncols = l.col_widths.len();
            // column index → corrected-word index sitting in it.
            let mut col_word: Vec<Option<usize>> = vec![None; ncols];
            for (k, &c) in l.corr_cols.iter().enumerate() {
                if c < ncols {
                    col_word[c] = Some(k);
                }
            }
            // Wrap by hand so this card breaks at exactly the same columns
            // as the Original card above — editable fields are a hair wider
            // than padded labels, so egui's auto-wrap would drift the two
            // rows apart over a long line.
            let rows = wrap_columns(&l.col_widths, ui.available_width(), cw);
            // Separators are explicit, so no horizontal item gap; the
            // vertical gap gives wrapped lines ~1.5 line-height.
            ui.spacing_mut().item_spacing = egui::vec2(0.0, row_h * 0.5);

            let mut field_ord = 0usize;
            for &(c0, c1) in &rows {
                let segments = &mut *segments;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    // `c` indexes several column arrays and is used as `c + 1`.
                    #[allow(clippy::needless_range_loop)]
                    for c in c0..c1 {
                        let width = l.col_widths[c];
                        let Some(k) = col_word[c] else {
                            // Deletion column — a blank gap on the corrected
                            // row so following words stay under their originals.
                            ui.add_space((width + 1) as f32 * cw);
                            continue;
                        };
                        let sep = corr_seps.get(k).cloned().unwrap_or_default();
                        // Punctuation bound to the word is folded into the
                        // column (so a trailing comma can't shove later
                        // columns sideways); only the whitespace separates
                        // columns.
                        let (punct, ws) = worddiff::split_separator(&sep);
                        let punct_chars = punct.chars().count();
                        match corr_field.get(k).copied().flatten() {
                            // Unchanged word: word + punctuation, padded to
                            // the column.
                            None => {
                                let cell = format!("{}{punct}", corr_words[k]);
                                let padded = format!("{cell:<width$}");
                                ui.label(
                                    egui::RichText::new(padded)
                                        .font(font.clone())
                                        .color(egui::Color32::from_gray(215)),
                                );
                            }
                            // Changed/inserted word: editable field filling the
                            // column minus its punctuation (growing only if
                            // typed past it), then the punctuation alongside.
                            Some(seg_idx) => {
                                let this_ord = field_ord;
                                field_ord += 1;
                                if let Segment::Field(t) = &mut segments[seg_idx] {
                                    let id = egui::Id::new(("hc_review_field", seg_idx));
                                    let chars = t.chars().count();
                                    let w =
                                        width.saturating_sub(punct_chars).max(chars) as f32 * cw;
                                    let out = egui::TextEdit::singleline(t)
                                        .id(id)
                                        .frame(false)
                                        // We drive Tab/Shift+Tab ourselves
                                        // (focus_relative). Locking tab to the
                                        // field stops egui latching its own
                                        // focus move in begin_pass — without
                                        // this, Shift+Tab ejects to the action
                                        // buttons instead of cycling fields
                                        // backward. (Singleline never inserts a
                                        // tab char; that path is multiline-only.)
                                        .lock_focus(true)
                                        .desired_width(w)
                                        .margin(egui::Margin::ZERO)
                                        .font(font.clone())
                                        .text_color(egui::Color32::from_gray(238))
                                        .show(ui);

                                    // Blue squiggle spans the word, not the column.
                                    let rect = out.response.rect;
                                    let focused =
                                        out.response.has_focus() || pending == Some(this_ord);
                                    let sq_color = if focused {
                                        SQUIGGLE_BLUE
                                    } else {
                                        SQUIGGLE_BLUE.gamma_multiply(0.65)
                                    };
                                    let sq_right = rect.left() + chars as f32 * cw;
                                    squiggle(
                                        ui.painter(),
                                        rect.left(),
                                        sq_right,
                                        rect.bottom(),
                                        sq_color,
                                    );

                                    if pending == Some(this_ord) {
                                        out.response.request_focus();
                                        let mut state = out.state;
                                        state.cursor.set_char_range(Some(CCursorRange::two(
                                            CCursor::new(0),
                                            CCursor::new(chars),
                                        )));
                                        state.store(ui.ctx(), id);
                                        consumed_pending = true;
                                        new_focused = Some(this_ord);
                                        new_caret = Some((0, chars, true));
                                    } else if out.response.has_focus() {
                                        new_focused = Some(this_ord);
                                        let (caret, sel) = out
                                            .cursor_range
                                            .map(|r| {
                                                (
                                                    r.primary.index,
                                                    r.primary.index != r.secondary.index,
                                                )
                                            })
                                            .unwrap_or((chars, false));
                                        new_caret = Some((caret, chars, sel));
                                    }
                                }
                                if punct_chars > 0 {
                                    ui.label(
                                        egui::RichText::new(punct.to_string())
                                            .font(font.clone())
                                            .color(egui::Color32::from_gray(215)),
                                    );
                                }
                            }
                        }
                        // Whitespace gap to the next column (its own text, or a
                        // synthesized space when more columns follow this row).
                        let ws_chars = ws.chars().count();
                        if ws_chars > 0 {
                            ui.add_space(ws_chars as f32 * cw);
                        } else if c + 1 < c1 {
                            ui.add_space(cw);
                        }
                    }
                });
            }
        });

        if consumed_pending {
            self.pending_focus = None;
        }
        self.focused_field = new_focused;
        self.focus_caret = new_caret;

        // Suggestion list, inline below the Proposed card so it never
        // covers the corrected sentence above it.
        if let Some(ord) = self.focused_field {
            let current = self.field_word(ord);
            let entries = self.field_entries(ord, &current);
            if !entries.is_empty() {
                let labels: Vec<&str> = entries.iter().map(|(l, _)| l.as_str()).collect();
                // Define the highlighted option's word, or the field's
                // current word when nothing is highlighted.
                let def_word = self
                    .dropdown_highlight
                    .and_then(|i| entries.get(i))
                    .map(|(_, v)| v.clone())
                    .unwrap_or_else(|| current.clone());
                let def = self.definition_view(ui.ctx(), &def_word);
                if let Some(pick) =
                    render_suggestion_dropdown(ui, &current, &labels, self.dropdown_highlight, def)
                {
                    let value = entries[pick].1.clone();
                    let ctx = ui.ctx().clone();
                    self.insert_suggestion(&ctx, ord, &value);
                }
            }
        }
    }

    // ---- vim mode -------------------------------------------------

    fn input_vim(&mut self, ctx: &egui::Context) {
        // Ctrl+E flips back to word-edit mode. Consume it first so the 'e'
        // never reaches the vim editor as a motion.
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::E)) {
            self.exit_vim();
            return;
        }
        // Vim doesn't use Tab; swallow it so egui doesn't move focus
        // onto the action buttons.
        ctx.input_mut(|i| {
            i.consume_key(egui::Modifiers::NONE, egui::Key::Tab);
            i.consume_key(egui::Modifiers::SHIFT, egui::Key::Tab);
        });

        // While the z= dropdown is open, keys drive it, not the editor.
        if self.vim_suggest.is_some() {
            self.input_vim_dropdown(ctx);
            return;
        }

        let keys = collect_vim_keys(ctx);
        let before = self.vim.as_ref().map(|v| v.text().to_string());
        let mut outcome = VimOutcome::None;
        if let Some(vim) = self.vim.as_mut() {
            for k in keys {
                let o = vim.handle(k);
                if o != VimOutcome::None {
                    outcome = o;
                }
            }
        }
        // If the text changed, drop the squiggle on any touched word and
        // shift the rest to track the edit.
        if let (Some(before), Some(vim)) = (before, self.vim.as_ref()) {
            let after = vim.text();
            if before != after {
                update_marks(&mut self.vim_marks, &before, after);
            }
        }
        match outcome {
            VimOutcome::Submit => self.apply(ctx),
            VimOutcome::Cancel => self.cancel(ctx),
            VimOutcome::SpellSuggest => self.open_vim_suggest(),
            VimOutcome::None => {}
        }
    }

    /// `z=` over a word: open the spell-suggest dropdown for the word the
    /// vim cursor is on, if the provider offered alternatives for it.
    fn open_vim_suggest(&mut self) {
        let Some(vim) = self.vim.as_ref() else { return };
        let text = vim.text();
        let Some((start, end)) = worddiff::word_at(text, vim.cursor()) else {
            return;
        };
        let word = text[start..end].to_string();
        let options = self.suggest_options(&word);
        if !options.is_empty() {
            self.vim_suggest = Some(VimSuggest {
                start,
                end,
                word,
                options,
                highlight: None,
            });
        }
    }

    /// Ranked alternatives the provider offered for `word` (best first),
    /// minus the word itself, capped at 5. Matched by word text since the
    /// vim cursor can be on any word.
    fn suggest_options(&self, word: &str) -> Vec<String> {
        self.request
            .suggestions
            .iter()
            .find(|ws| ws.word == word)
            .map(|ws| {
                ws.options
                    .iter()
                    .filter(|o| o.as_str() != word)
                    .take(5)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Handle keys while the vim z= dropdown is open: 1–5 / Enter pick,
    /// Up/Down and j/k move the highlight, Esc closes it.
    fn input_vim_dropdown(&mut self, ctx: &egui::Context) {
        let (options, highlight) = match self.vim_suggest.as_ref() {
            Some(s) => (s.options.clone(), s.highlight),
            None => return,
        };
        let n = options.len();
        for d in 1..=n.min(5) {
            if take_digit(ctx, d) {
                self.pick_vim_suggest(ctx, options[d - 1].as_str());
                return;
            }
        }
        let down = ctx.input_mut(|i| {
            i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)
                || i.consume_key(egui::Modifiers::NONE, egui::Key::J)
        });
        if down {
            if let Some(s) = self.vim_suggest.as_mut() {
                s.highlight = Some(highlight.map_or(0, |h| (h + 1).min(n - 1)));
            }
            return;
        }
        let up = ctx.input_mut(|i| {
            i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp)
                || i.consume_key(egui::Modifiers::NONE, egui::Key::K)
        });
        if up {
            if let Some(s) = self.vim_suggest.as_mut() {
                s.highlight = match highlight {
                    Some(0) | None => None,
                    Some(h) => Some(h - 1),
                };
            }
            return;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)) {
            self.pick_vim_suggest(ctx, options[highlight.unwrap_or(0)].as_str());
            return;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            self.vim_suggest = None;
        }
    }

    /// Apply `option` to the dropdown's word in the vim buffer, then
    /// advance to the next word with suggestions (opening its dropdown) —
    /// or apply the whole correction when there is no next one.
    fn pick_vim_suggest(&mut self, ctx: &egui::Context, option: &str) {
        let Some(sug) = self.vim_suggest.take() else {
            return;
        };
        let next_from = sug.start + option.len();
        let before = self.vim.as_ref().map(|v| v.text().to_string());
        if let Some(vim) = self.vim.as_mut() {
            vim.replace_range(sug.start, sug.end, option);
        }
        let Some(text) = self.vim.as_ref().map(|v| v.text().to_string()) else {
            return;
        };
        // Drop the squiggle on the word we just swapped, then look for the
        // next word the provider has alternatives for.
        if let Some(before) = before {
            update_marks(&mut self.vim_marks, &before, &text);
        }
        for (start, end) in worddiff::word_byte_ranges(&text) {
            if start < next_from {
                continue;
            }
            let word = text[start..end].to_string();
            let options = self.suggest_options(&word);
            if !options.is_empty() {
                if let Some(vim) = self.vim.as_mut() {
                    vim.set_cursor(start);
                }
                self.vim_suggest = Some(VimSuggest {
                    start,
                    end,
                    word,
                    options,
                    highlight: None,
                });
                return;
            }
        }
        // No further suggestible word — that was the last one: submit.
        self.apply(ctx);
    }

    fn render_vim(&mut self, ui: &mut egui::Ui) {
        ui.heading("Edit sentence  ·  vim");
        // Spacing below mirrors render_word exactly so toggling Ctrl+E
        // doesn't shift the Original / second-section boxes vertically.
        ui.add_space(16.0);
        section_label(ui, "Original");

        let (text, cursor, mode, status) = match self.vim.as_ref() {
            Some(v) => (v.text().to_string(), v.cursor(), v.mode(), v.status_line()),
            None => {
                original_card(ui, &self.request.original, &self.request.corrected, None);
                return;
            }
        };

        // Align the Original to the *live* buffer so each original word
        // sits above the word it became, the same column grid as word-edit
        // mode.
        let layout = worddiff::align(&self.request.original, &text);
        original_card(ui, &self.request.original, &text, layout.as_ref());
        ui.add_space(18.0);
        section_label(ui, "Corrected");

        let marks = self.vim_marks.clone();
        let font = mono_font();
        let fg = TEXT_FG;
        let accent = egui::Color32::from_rgb(120, 190, 255);
        let on_block = egui::Color32::from_gray(20);

        egui::Frame::new()
            .fill(CARD_BG)
            .corner_radius(egui::CornerRadius::same(8))
            .inner_margin(egui::Margin::symmetric(14, 12))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                let wrap_width = ui.available_width();
                let cw = char_width(ui, &font);
                let row_h = ui.fonts(|f| f.row_height(&font));

                // Column-pad the buffer for display so it lines up under the
                // Original, breaking at the same columns. `map` turns a raw
                // buffer char index into its index in the padded string, so
                // the vim caret and squiggles (which index the raw buffer)
                // still land correctly. The caret is painted as an overlay,
                // so switching INSERT/NORMAL never shifts a glyph.
                let (disp, map) = match &layout {
                    Some(l) if !text.is_empty() => {
                        let rows = wrap_columns(&l.col_widths, wrap_width, cw);
                        aligned_display(&text, l, &rows)
                    }
                    _ => (text.clone(), (0..=text.chars().count()).collect()),
                };
                let d = |raw: usize| -> usize { map.get(raw).copied().unwrap_or(0) };

                let mut job = LayoutJob::default();
                job.wrap.max_width = wrap_width;
                job.append(
                    &disp,
                    0.0,
                    egui::TextFormat {
                        font_id: font.clone(),
                        color: fg,
                        // Match the Original card's row pitch — it spaces its
                        // rows by row_h*0.5 via item_spacing, i.e. a 1.5×
                        // line height. The galley defaults to the font's tight
                        // row height, which made the Corrected box look more
                        // cramped than the Original. Because every glyph gets
                        // the same line_height, egui's valign term cancels
                        // (max_row_height == glyph.line_height) and the glyph is
                        // top-anchored in the taller row with the extra space
                        // *below* it — so the caret/squiggles below anchor from
                        // the row top (= glyph top), not the row bottom.
                        line_height: Some(row_h * 1.5),
                        ..Default::default()
                    },
                );
                let galley = ui.fonts(|f| f.layout_job(job));
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(wrap_width, galley.size().y.max(row_h)),
                    egui::Sense::hover(),
                );
                let origin = rect.min;
                ui.painter().galley(origin, galley.clone(), fg);

                // Blue squiggles under the corrections not yet touched.
                for &(bs, be) in marks.iter().flatten() {
                    if bs >= be || be > text.len() {
                        continue;
                    }
                    let cs = d(text[..bs].chars().count());
                    let ce = d(text[..be].chars().count());
                    let r0 = galley
                        .pos_from_cursor(CCursor::new(cs))
                        .translate(origin.to_vec2());
                    let r1 = galley
                        .pos_from_cursor(CCursor::new(ce))
                        .translate(origin.to_vec2());
                    let x1 = if (r0.min.y - r1.min.y).abs() < 1.0 {
                        r1.min.x
                    } else {
                        origin.x + galley.size().x
                    };
                    // Glyph is top-anchored in the 1.5× row, so its bottom is
                    // row_top + row_h, not the row's max.y (which includes the
                    // extra space below).
                    squiggle(ui.painter(), r0.min.x, x1, r0.min.y + row_h, SQUIGGLE_BLUE);
                }

                let at = cursor.min(text.len());
                let char_idx = d(text[..at].chars().count());
                let caret = galley
                    .pos_from_cursor(CCursor::new(char_idx))
                    .translate(origin.to_vec2());
                // The cursor rect spans the taller 1.5× line box; the glyph is
                // top-anchored within it, so the glyph's own cell starts at the
                // row top. Draw the caret one `row_h` tall from there instead of
                // filling the whole (taller) line.
                let glyph_top = egui::pos2(caret.min.x, caret.min.y);
                match mode {
                    vimedit::Mode::Insert => {
                        // Thin i-beam between glyphs.
                        let ibeam = egui::Rect::from_min_size(glyph_top, egui::vec2(2.0, row_h));
                        ui.painter().rect_filled(ibeam, 0.0, accent);
                    }
                    _ => {
                        // Block over the character under the cursor — one
                        // monospace cell wide (never the padding after it).
                        let block = egui::Rect::from_min_size(glyph_top, egui::vec2(cw, row_h));
                        ui.painter().rect_filled(block, 0.0, accent);
                        if let Some(ch) = text[at..].chars().next() {
                            if ch != '\n' {
                                ui.painter().text(
                                    glyph_top,
                                    egui::Align2::LEFT_TOP,
                                    ch,
                                    font.clone(),
                                    on_block,
                                );
                            }
                        }
                    }
                }
            });

        // z= spell-suggest dropdown, inline below the editor.
        let dropdown = self
            .vim_suggest
            .as_ref()
            .map(|s| (s.word.clone(), s.options.clone(), s.highlight));
        if let Some((word, options, highlight)) = dropdown {
            let opts: Vec<&str> = options.iter().map(String::as_str).collect();
            let def_word = highlight
                .and_then(|i| options.get(i))
                .cloned()
                .unwrap_or_else(|| word.clone());
            let def = self.definition_view(ui.ctx(), &def_word);
            if let Some(pick) = render_suggestion_dropdown(ui, &word, &opts, highlight, def) {
                let opt = options[pick].clone();
                let ctx = ui.ctx().clone();
                self.pick_vim_suggest(&ctx, &opt);
            }
        }

        ui.add_space(8.0);
        ui.label(egui::RichText::new(status).monospace().color(accent));
        ui.add_space(8.0);
        // One row per command group so a new user can get oriented at a
        // glance instead of parsing one dense line.
        egui::Grid::new("vim_help")
            .num_columns(2)
            .spacing(egui::vec2(16.0, 3.0))
            .show(ui, |ui| {
                for (cmd, desc) in [
                    ("ciw  dw  cw", "change or delete a word"),
                    ("x   r", "delete or replace a character"),
                    ("i   a   o", "insert, append, or open a line"),
                    ("w  b  0  $", "move by word, to line start / end"),
                    ("z=", "show spelling suggestions for the word"),
                    ("u  Ctrl+R  .", "undo, redo, repeat the last change"),
                    ("Enter  :wq", "apply the correction"),
                    ("Esc  :q", "cancel"),
                ] {
                    ui.label(
                        egui::RichText::new(cmd)
                            .monospace()
                            .size(11.0)
                            .color(kanso::palette::TEXT_MUTED),
                    );
                    ui.label(
                        egui::RichText::new(desc)
                            .size(11.0)
                            .color(egui::Color32::from_gray(125)),
                    );
                    ui.end_row();
                }
            });
    }
}

impl eframe::App for ReviewApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        kanso::scroll::scroll_momentum(ctx);
        // Bring the review popup under the shared kanso theme — type scale,
        // spacing, solid scrollbar, corner radius (apply_styles) plus the
        // input/button control border (control_visuals) — so it matches the
        // prefs window instead of riding egui's raw defaults. Fonts are
        // installed once at startup; this is font-free and cheap per frame.
        kanso::theme::apply_styles(ctx);
        ctx.style_mut(|style| kanso::theme::control_visuals(&mut style.visuals));
        // Fold in any online definition results that arrived since last frame.
        self.drain_definitions();
        // A daemon-initiated re-process (the review-llm chord) flips the
        // request file back to `pending`; notice it even while a finished
        // review is on screen, and drop into the "Checking…" view. Poll on
        // a timer so this is caught when the popup is otherwise idle.
        if self.ready {
            if let Ok(Some(req)) = runtime::read_review_request() {
                if req.pending {
                    self.ready = false;
                    self.reprocessing = true;
                }
            }
            ctx.request_repaint_after(Duration::from_millis(250));
        }

        // Still computing the correction: show "Checking…", poll for the
        // finished request, and bail out of the normal review UI.
        if !self.ready {
            if let Ok(Some(req)) = runtime::read_review_request() {
                if !req.pending {
                    // An LLM re-process that found nothing must NOT slam the
                    // popup shut mid-review — only the initial no-op closes.
                    if !self.reprocessing && req.corrected == req.original {
                        self.decision = Some("cancel");
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        return;
                    }
                    self.request = req;
                    self.load_review();
                    self.reprocessing = false;
                }
            }
            if !self.ready {
                if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                    self.cancel(ctx);
                    return;
                }
                self.render_checking(ctx);
                ctx.request_repaint_after(Duration::from_millis(120));
                return;
            }
        }

        if !self.initialized {
            self.initialized = true;
            self.pending_focus = (!self.field_segments.is_empty()).then_some(0);
        }

        // Input first — this may flip the mode (Ctrl+E) or decide.
        match self.mode {
            EditMode::Word => self.input_word(ctx),
            EditMode::Vim => self.input_vim(ctx),
        }

        // Action row, pinned to the bottom so it's always reachable.
        let mut do_apply = false;
        let mut do_cancel = false;
        let mut do_llm = false;
        egui::TopBottomPanel::bottom("review_actions")
            .resizable(false)
            .show(ctx, |ui| {
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    // Roomier hit targets.
                    ui.spacing_mut().button_padding = egui::vec2(18.0, 9.0);
                    ui.add_space(12.0); // inset from the left edge
                    if ui
                        .button(
                            // ⎋ (Esc) from the bundled symbol font — the
                            // default body font may lack the key glyph.
                            egui::RichText::new("Cancel  ⎋")
                                .family(egui::FontFamily::Name(
                                    kanso::fonts::SHORTCUT_FAMILY.into(),
                                ))
                                .size(15.0),
                        )
                        .clicked()
                    {
                        do_cancel = true;
                    }
                    // Escalate to the LLM — only when the shown correction
                    // did NOT come from the LLM (nothing to escalate if it
                    // did). Otherwise always offered (progressive
                    // discovery); the trailing ellipsis hints it opens
                    // setup first when no LLM key is configured yet.
                    if !self.request.from_llm {
                        ui.add_space(8.0);
                        let (llm_label, llm_hint) = if self.request.llm_available {
                            ("Ask LLM", "Re-run this sentence through the LLM")
                        } else {
                            (
                                "Ask LLM…",
                                "Opens Preferences → Providers to add an LLM API key",
                            )
                        };
                        if ui
                            .button(egui::RichText::new(llm_label).size(15.0))
                            .on_hover_text(llm_hint)
                            .clicked()
                        {
                            do_llm = true;
                        }
                    }
                    // Apply is the primary action — filled, pinned to the right.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(12.0); // inset from the right edge
                        // Apply is the kanso primary: filled with the same
                        // selection teal primary_button uses (so it matches
                        // the cohort's CTAs), keeping the ↵ glyph from the
                        // shortcut font.
                        let apply = egui::Button::new(
                            egui::RichText::new("Apply  ↵")
                                .family(egui::FontFamily::Name(
                                    kanso::fonts::SHORTCUT_FAMILY.into(),
                                ))
                                .size(15.0)
                                .strong()
                                .color(ui.visuals().selection.stroke.color),
                        )
                        .fill(ui.visuals().selection.bg_fill);
                        if ui.add(apply).clicked() {
                            do_apply = true;
                        }
                    });
                });
                ui.add_space(16.0);
            });
        if do_apply {
            self.apply(ctx);
        } else if do_cancel {
            self.cancel(ctx);
        } else if do_llm {
            self.escalate_llm();
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::central_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(26, 22)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| match self.mode {
                        EditMode::Word => self.render_word(ui),
                        EditMode::Vim => self.render_vim(ui),
                    });
            });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Default to cancel if the user closed via the X button or
        // window-close event without making a choice.
        let decision = self.decision.unwrap_or("cancel");
        // On apply, persist the (possibly edited) corrected sentence so
        // the daemon's apply handler emits what the user actually sees.
        // The backspace/delete counts in the request are keyed off the
        // *original* sentence, so overwriting `corrected` is safe.
        if decision == "apply" {
            if let Err(e) = runtime::write_review_request(&self.request) {
                eprintln!("hyprcorrect: could not write edited review request: {e}");
            }
        }
        // The daemon picks up the next step by reading the trigger-
        // action file and re-signaling. Writing the action *before*
        // signaling ensures the daemon sees the right routing.
        let action = match decision {
            "apply" => "review-apply",
            _ => "review-cancel",
        };
        if let Err(e) = std::fs::write(runtime::action_path(), action) {
            eprintln!("hyprcorrect: could not write review action: {e}");
            return;
        }
        // Give Hyprland a beat to refocus the window the user came
        // from before the daemon's emit lands.
        std::thread::sleep(Duration::from_millis(REFOCUS_DELAY_MS));
        notify_daemon();
    }
}

/// Translate this frame's egui key/text events into [`VimKey`]s, in
/// order. Printable characters come from `Text` events; special keys
/// from `Key` events — so each keystroke maps to exactly one
/// [`VimKey`].
fn collect_vim_keys(ctx: &egui::Context) -> Vec<VimKey> {
    ctx.input(|i| {
        let mut out = Vec::new();
        for ev in &i.events {
            match ev {
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    let vk = if *key == egui::Key::R && modifiers.ctrl {
                        Some(VimKey::Redo) // Ctrl+R
                    } else {
                        match key {
                            egui::Key::Escape => Some(VimKey::Esc),
                            egui::Key::Enter => Some(VimKey::Enter),
                            egui::Key::Backspace => Some(VimKey::Backspace),
                            egui::Key::ArrowLeft => Some(VimKey::Left),
                            egui::Key::ArrowRight => Some(VimKey::Right),
                            egui::Key::ArrowUp => Some(VimKey::Up),
                            egui::Key::ArrowDown => Some(VimKey::Down),
                            egui::Key::Home => Some(VimKey::Home),
                            egui::Key::End => Some(VimKey::End),
                            _ => None,
                        }
                    };
                    if let Some(vk) = vk {
                        out.push(vk);
                    }
                }
                egui::Event::Text(t) => {
                    // Newlines/tabs arrive as Key events; skip control
                    // chars here so they aren't double-counted.
                    out.extend(t.chars().filter(|c| !c.is_control()).map(VimKey::Char));
                }
                _ => {}
            }
        }
        out
    })
}

/// Pick a window width that fits the sentence on one line where it can —
/// growing with the text up to half the screen (a fixed cap when the
/// monitor size is unknown). Longer sentences wrap inside the cap. Sized
/// from the longer of original/corrected since the popup opens before the
/// correction is known.
fn estimate_window_width(request: &ReviewRequest) -> f32 {
    // Generous monospace glyph estimate (≥ the real width, so we never
    // size too narrow); panel + card margins + the vertical scrollbar that
    // appears once the suggestion list makes the content tall.
    const CW: f32 = 10.0;
    const CHROME: f32 = 96.0;
    // The aligned grid can be wider than either raw sentence when the two
    // take their widest words in different columns, so size from it.
    let aligned = worddiff::align(&request.original, &request.corrected)
        .map(|l| l.col_widths.iter().sum::<usize>() + l.col_widths.len().saturating_sub(1))
        .unwrap_or(0);
    let chars = aligned
        .max(request.original.chars().count())
        .max(request.corrected.chars().count());
    let content = chars as f32 * CW + CHROME;
    let cap = if request.screen_width > 1.0 {
        request.screen_width * 0.5
    } else {
        FALLBACK_MAX_WIDTH
    };
    content.clamp(MIN_WINDOW_WIDTH.min(cap), cap)
}

/// Pick a window height that fits the *whole* popup — heading, both cards,
/// and the inline suggestion dropdown (definition line + options) — so nothing
/// is cut off, capped at the monitor's usable height (below the waybar). We
/// size up front rather than resize live because the popup is a *centered*
/// floating window: Hyprland won't re-center it after a post-open resize, so a
/// live grow would slide off the bottom. The inner `ScrollArea` still covers
/// any residual overflow (e.g. an unusually long online definition).
///
/// `definitions_on` reserves room for the async definition line, which the
/// popup fetches after opening (so its length is unknown here).
fn estimate_window_height(request: &ReviewRequest, definitions_on: bool) -> f32 {
    // Per-element heights in logical points — generous (≥ the real rendered
    // size) so the window opens tall enough on the first frame.
    const TEXT_LINE: f32 = 24.0; // a wrapped line of sentence text
    const CARD_PAD: f32 = 44.0; // a card's border + inner padding (top + bottom)
    const HEADING: f32 = 56.0; // "Review correction" + its 16px gap
    const SECTION_LABEL: f32 = 34.0; // a section label line + its gap
    const SECTION_GAP: f32 = 18.0; // gap between the cards
    const SUGG_CHROME: f32 = 78.0; // dropdown frame + "Other options" header + hint + gaps
    const SUGG_ROW: f32 = 28.0; // one option row in the dropdown
    const DEF_LINE: f32 = 20.0; // a wrapped line of definition text (size 12.5)
    const DEF_RESERVE_LINES: f32 = 3.0; // room kept for the variable-length definition
    const FOOTER: f32 = 92.0; // Cancel/Ask/Apply row + its margins
    const PANEL_MARGIN: f32 = 44.0; // central panel inner_margin (22 top + 22 bottom)

    // ~chars per line at the popup's width (monospace ≈ 10pt/char, minus margins).
    let cols = (((estimate_window_width(request) - 96.0) / 10.0) as usize).max(20);
    let lines = |s: &str| -> f32 {
        s.lines()
            .map(|line| line.chars().count().max(1).div_ceil(cols))
            .sum::<usize>()
            .max(1) as f32
    };

    let orig_card = CARD_PAD + lines(&request.original) * TEXT_LINE;
    let prop_card = CARD_PAD + lines(&request.corrected) * TEXT_LINE;

    // Suggestion dropdown: size for the *most* options any one changed word
    // offers — Tab moves focus between words and the window can't resize, so it
    // must already fit the tallest list. When the correction is still pending
    // (slow LLM, result unknown), reserve for a typical dropdown so the result
    // fits when it lands.
    let max_options = if request.pending {
        5
    } else {
        request
            .suggestions
            .iter()
            .map(|s| s.options.len())
            .max()
            .unwrap_or(0)
    };
    let dropdown = if max_options > 0 {
        let def = if definitions_on {
            DEF_RESERVE_LINES * DEF_LINE
        } else {
            0.0
        };
        SUGG_CHROME + def + max_options as f32 * SUGG_ROW
    } else {
        0.0
    };

    let content =
        HEADING + SECTION_LABEL + orig_card + SECTION_GAP + SECTION_LABEL + prop_card + dropdown;
    let total = PANEL_MARGIN + content + FOOTER;

    // Cap at the monitor's usable height when known (so the popup can grow
    // right up to the waybar), else the fixed fallback.
    let cap = if request.screen_height > 1.0 {
        request.screen_height
    } else {
        MAX_WINDOW_HEIGHT
    };
    total.clamp(MIN_WINDOW_HEIGHT, cap)
}

fn section_label(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .strong()
            .size(13.0)
            .color(egui::Color32::from_gray(140)),
    );
    ui.add_space(6.0);
}

const SQUIGGLE_RED: egui::Color32 = egui::Color32::from_rgb(232, 92, 92);
// Byte-identical to the kanso tokens — alias them so the review popup and
// the design system can't drift. (SQUIGGLE_RED has no exact kanso match, so
// it stays a local literal.)
const SQUIGGLE_BLUE: egui::Color32 = kanso::palette::INFO;
const CARD_BG: egui::Color32 = kanso::palette::CARD;
const TEXT_FG: egui::Color32 = kanso::palette::TEXT;

/// The prose font for the Original / Proposed text, shared by the
/// static words and the editable fields so they keep one baseline.
fn prose_font() -> egui::FontId {
    egui::FontId::proportional(16.0)
}

/// A rounded, padded container for a block of review text — the shared
/// kanso card surface (fill, [`kanso::palette::CARD_STROKE`] border, 10px
/// rounding, symmetric padding).
fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    kanso::widgets::card(ui, add)
}

/// The "Original" card: the user's text with a red squiggle under each
/// word the corrector changed. With `align` widths it renders monospace
/// and pads each word to its column so the words above line up with the
/// corrections below.
fn original_card(
    ui: &mut egui::Ui,
    original: &str,
    corrected: &str,
    align: Option<&worddiff::AlignLayout>,
) {
    card(ui, |ui| {
        if let Some(layout) = align {
            paint_aligned_original(ui, original, corrected, layout);
        } else {
            let ranges = worddiff::changed_word_ranges(original, corrected);
            paint_text_with_squiggles(
                ui,
                original,
                &ranges,
                kanso::palette::TEXT_MUTED,
                SQUIGGLE_RED,
            );
        }
    });
}

/// Render `original` in monospace, walking the shared column grid so each
/// word sits directly above the correction in the Proposed card — a blank
/// gap where the correction inserted a word, a padded word elsewhere, and
/// a red squiggle under each word the correction changed or removed.
fn paint_aligned_original(
    ui: &mut egui::Ui,
    original: &str,
    corrected: &str,
    layout: &worddiff::AlignLayout,
) {
    let font = mono_font();
    let fg = kanso::palette::TEXT_MUTED;
    let (orig_words, orig_seps) = words_and_seps(original);
    let (corr_words, _) = words_and_seps(corrected);

    let ncols = layout.col_widths.len();
    let mut col_orig: Vec<Option<usize>> = vec![None; ncols];
    for (k, &c) in layout.orig_cols.iter().enumerate() {
        if c < ncols {
            col_orig[c] = Some(k);
        }
    }
    let mut col_corr: Vec<Option<usize>> = vec![None; ncols];
    for (k, &c) in layout.corr_cols.iter().enumerate() {
        if c < ncols {
            col_corr[c] = Some(k);
        }
    }

    let cw = char_width(ui, &font);
    let row_h = ui.fonts(|f| f.row_height(&font));
    // Same hand-wrap as the Proposed card so the two stay column-aligned
    // across line breaks.
    let rows = wrap_columns(&layout.col_widths, ui.available_width(), cw);
    ui.spacing_mut().item_spacing = egui::vec2(0.0, row_h * 0.5);
    for &(c0, c1) in &rows {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            // `c` indexes several column arrays and is used as `c + 1`.
            #[allow(clippy::needless_range_loop)]
            for c in c0..c1 {
                let width = layout.col_widths[c];
                let Some(k) = col_orig[c] else {
                    // Insertion column — blank gap below the inserted correction.
                    ui.add_space((width + 1) as f32 * cw);
                    continue;
                };
                let word = &orig_words[k];
                // Changed when the correction put a different word in this
                // column, or removed it (no corrected word here).
                let changed = match col_corr[c] {
                    Some(ck) => corr_words[ck] != *word,
                    None => true,
                };
                // Fold this word's punctuation into its column, matching the
                // Proposed card; only whitespace separates columns.
                let (punct, ws) = worddiff::split_separator(&orig_seps[k]);
                let cell = format!("{word}{punct}");
                let padded = format!("{cell:<width$}");
                let resp = ui.label(egui::RichText::new(padded).font(font.clone()).color(fg));
                if changed {
                    let r = resp.rect;
                    squiggle(
                        ui.painter(),
                        r.left(),
                        r.left() + word.chars().count() as f32 * cw,
                        r.bottom(),
                        SQUIGGLE_RED,
                    );
                }
                let ws_chars = ws.chars().count();
                if ws_chars > 0 {
                    ui.add_space(ws_chars as f32 * cw);
                } else if c + 1 < c1 {
                    ui.add_space(cw);
                }
            }
        });
    }
}

/// Greedily pack columns into visual rows that each fit within `avail`
/// (column width + one separator, in monospace `cw`). Both cards use the
/// same widths and width, so they wrap at identical columns and stay
/// aligned across the break. Returns half-open `[start, end)` ranges.
fn wrap_columns(col_widths: &[usize], avail: f32, cw: f32) -> Vec<(usize, usize)> {
    let mut rows = Vec::new();
    let mut start = 0usize;
    let mut width = 0.0f32; // px of the current row's content (no trailing sep)
    for (c, &w) in col_widths.iter().enumerate() {
        // A column adds its width, plus one separator *before* it unless
        // it's first on the row — the last column has no trailing sep, so
        // don't count one (that would wrap a line that actually fits).
        let add = (if c == start { w } else { w + 1 }) as f32 * cw;
        if c > start && width + add > avail {
            rows.push((start, c));
            start = c;
            width = w as f32 * cw;
        } else {
            width += add;
        }
    }
    rows.push((start, col_widths.len()));
    rows
}

/// Column-pad `buffer` (the corrected side of `layout`) into a monospace
/// display string that lines up under the Original card, breaking at the
/// same `rows`. Returns the padded string plus a map from each raw char
/// index (`0..=char_len`) to its char index in the padded string — so the
/// vim caret and squiggles, which index the raw buffer, land in the right
/// place. Padding is appended *after* each word's separator space (so the
/// caret on the space after a shortened word stays tight against the word),
/// with gaps/newlines inserted; every raw char keeps its identity and maps
/// cleanly.
fn aligned_display(
    buffer: &str,
    layout: &worddiff::AlignLayout,
    rows: &[(usize, usize)],
) -> (String, Vec<usize>) {
    let raw_len = buffer.chars().count();
    let mut map = vec![0usize; raw_len + 1];
    let mut disp = String::new();
    let mut di = 0usize; // display char count so far

    // Buffer words and the separator following each, with raw char offsets.
    let mut words: Vec<(usize, String)> = Vec::new();
    let mut seps: Vec<(usize, String)> = Vec::new();
    let mut leading: Option<(usize, String)> = None;
    let mut rc = 0usize;
    for (is_word, t) in worddiff::split_tokens(buffer) {
        let len = t.chars().count();
        if is_word {
            words.push((rc, t));
            seps.push((rc + len, String::new()));
        } else if let Some(last) = seps.last_mut() {
            *last = (rc, t);
        } else {
            leading = Some((rc, t));
        }
        rc += len;
    }

    let ncols = layout.col_widths.len();
    let mut col_word: Vec<Option<usize>> = vec![None; ncols];
    for (k, &c) in layout.corr_cols.iter().enumerate() {
        if c < ncols {
            col_word[c] = Some(k);
        }
    }
    let mut is_row_start = vec![false; ncols];
    for &(c0, _) in rows.iter().skip(1) {
        if c0 < ncols {
            is_row_start[c0] = true;
        }
    }

    let push = |disp: &mut String, di: &mut usize, ch: char| {
        disp.push(ch);
        *di += 1;
    };

    if let Some((start, sep)) = leading {
        for (ci, ch) in sep.chars().enumerate() {
            map[start + ci] = di;
            push(&mut disp, &mut di, ch);
        }
    }
    for c in 0..ncols {
        if is_row_start[c] {
            push(&mut disp, &mut di, '\n');
        }
        let width = layout.col_widths[c];
        let Some(k) = col_word[c] else {
            // Deletion column — blank gap (display-only) + a separator space.
            for _ in 0..width + 1 {
                push(&mut disp, &mut di, ' ');
            }
            continue;
        };
        let (wstart, word) = &words[k];
        let wlen = word.chars().count();
        for (ci, ch) in word.chars().enumerate() {
            map[wstart + ci] = di;
            push(&mut disp, &mut di, ch);
        }
        let (sep_start, sep) = &seps[k];
        let (punct, ws) = worddiff::split_separator(sep);
        let punct_len = punct.chars().count();
        for (ci, ch) in punct.chars().enumerate() {
            map[sep_start + ci] = di;
            push(&mut disp, &mut di, ch);
        }
        let pad = width.saturating_sub(wlen + punct_len);
        // Emit the real separator space *before* the alignment padding so it
        // maps to the slot right after the word. A `cw` that shortens a word
        // leaves the caret on this space; we want it tight against the word
        // with the column padding to its right, not jumped to the column's
        // far edge. Exception: a separator newline (a user INSERT-Enter) ends
        // the row, so any padding has to stay before it.
        let pad_first = ws.contains('\n');
        if pad_first {
            for _ in 0..pad {
                push(&mut disp, &mut di, ' ');
            }
        }
        for (ci, ch) in ws.chars().enumerate() {
            map[sep_start + punct_len + ci] = di;
            // Keep real newlines (INSERT-Enter line breaks) — collapsing
            // them would lose multi-line editing.
            push(&mut disp, &mut di, ch);
        }
        if !pad_first {
            for _ in 0..pad {
                push(&mut disp, &mut di, ' ');
            }
        }
    }
    map[raw_len] = di;
    (disp, map)
}

/// The words of `s` in order, each paired with the separator run that
/// follows it (empty for the last word). A leading separator before the
/// first word is dropped — sentences start with a word in practice.
fn words_and_seps(s: &str) -> (Vec<String>, Vec<String>) {
    let mut words = Vec::new();
    let mut seps = Vec::new();
    for (is_word, tok) in worddiff::split_tokens(s) {
        if is_word {
            words.push(tok);
            seps.push(String::new());
        } else if let Some(last) = seps.last_mut() {
            *last = tok;
        }
    }
    (words, seps)
}

/// Corrected-word index → `Some(segment index)` when that word is an
/// editable field, `None` when it's unchanged static text. Lets the
/// column walk bind each field's `TextEdit` back to its owning segment.
fn field_map(segments: &[Segment]) -> Vec<Option<usize>> {
    let mut out = Vec::new();
    for (i, seg) in segments.iter().enumerate() {
        match seg {
            Segment::Static(t) => {
                for (is_word, _) in worddiff::split_tokens(t) {
                    if is_word {
                        out.push(None);
                    }
                }
            }
            Segment::Field(_) => out.push(Some(i)),
        }
    }
    out
}

/// The monospace font for column-aligned review text.
fn mono_font() -> egui::FontId {
    egui::FontId::monospace(16.0)
}

/// Width of one monospace character (every glyph is the same width).
fn char_width(ui: &egui::Ui, font: &egui::FontId) -> f32 {
    ui.fonts(|f| f.glyph_width(font, ' '))
}

/// Paint wrapped `text` and draw a squiggle under each `[start, end)`
/// byte range — used for the read-only Original block.
fn paint_text_with_squiggles(
    ui: &mut egui::Ui,
    text: &str,
    ranges: &[(usize, usize)],
    text_color: egui::Color32,
    squiggle_color: egui::Color32,
) {
    let mut job = LayoutJob::default();
    job.wrap.max_width = ui.available_width();
    job.append(
        text,
        0.0,
        egui::TextFormat {
            font_id: prose_font(),
            color: text_color,
            ..Default::default()
        },
    );
    let galley = ui.fonts(|f| f.layout_job(job));
    let (rect, _) = ui.allocate_exact_size(galley.size(), egui::Sense::hover());
    let origin = rect.min;
    ui.painter().galley(origin, galley.clone(), text_color);
    for &(bs, be) in ranges {
        let cs = text[..bs].chars().count();
        let ce = text[..be].chars().count();
        let r0 = galley
            .pos_from_cursor(CCursor::new(cs))
            .translate(origin.to_vec2());
        let r1 = galley
            .pos_from_cursor(CCursor::new(ce))
            .translate(origin.to_vec2());
        let x1 = if (r0.min.y - r1.min.y).abs() < 1.0 {
            r1.min.x
        } else {
            // Word wrapped to a new row; underline its first row to the edge.
            origin.x + galley.size().x
        };
        squiggle(ui.painter(), r0.min.x, x1, r0.max.y, squiggle_color);
    }
}

/// Draw a spell-checker-style sine-wave underline from `x0` to `x1` at
/// baseline `y`.
fn squiggle(painter: &egui::Painter, x0: f32, x1: f32, y: f32, color: egui::Color32) {
    if x1 <= x0 {
        return;
    }
    const AMP: f32 = 1.4;
    const WAVELEN: f32 = 5.0;
    const STEP: f32 = 1.0;
    let mut pts = Vec::new();
    let mut x = x0;
    while x <= x1 {
        let phase = (x - x0) / WAVELEN * std::f32::consts::TAU;
        pts.push(egui::pos2(x, y + AMP * phase.sin()));
        x += STEP;
    }
    painter.add(egui::Shape::line(pts, egui::Stroke::new(1.4, color)));
}

/// Render the suggestion list inline, *below* the Proposed card, so the
/// corrected sentence (the word you're choosing for) stays fully
/// visible. Shows which word it's for, the numbered `options`, and a key
/// hint. Returns the clicked option index.
fn render_suggestion_dropdown(
    ui: &mut egui::Ui,
    current: &str,
    options: &[&str],
    highlight: Option<usize>,
    def: DefView,
) -> Option<usize> {
    let mut clicked = None;
    ui.add_space(10.0);
    egui::Frame::new()
        .fill(egui::Color32::from_gray(30))
        .corner_radius(egui::CornerRadius::same(6))
        .stroke(egui::Stroke::new(1.0, SQUIGGLE_BLUE.gamma_multiply(0.5)))
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 2.0;
            // Definition of the highlighted option (or the applied word when
            // nothing is highlighted) shown first, labeled, then a gap before
            // the options below.
            match def {
                DefView::Off => {}
                DefView::Loading => {
                    ui.label(
                        egui::RichText::new("Definition:  looking up…")
                            .size(12.5)
                            .italics()
                            .color(egui::Color32::from_gray(120)),
                    );
                    ui.add_space(10.0);
                }
                DefView::Missing => {
                    ui.label(
                        egui::RichText::new("Definition:  not found")
                            .size(12.5)
                            .italics()
                            .color(egui::Color32::from_gray(110)),
                    );
                    ui.add_space(10.0);
                }
                DefView::Text(d) => {
                    ui.label(
                        egui::RichText::new(format!("Definition:  {d}"))
                            .size(12.5)
                            .color(egui::Color32::from_gray(185)),
                    );
                    ui.add_space(10.0);
                }
            }
            ui.label(
                egui::RichText::new(format!("Other options for  {current}"))
                    .size(12.0)
                    .strong()
                    .color(kanso::palette::TEXT_FAINT),
            );
            ui.add_space(4.0);
            for (i, opt) in options.iter().enumerate() {
                let label = egui::RichText::new(format!("{}   {opt}", i + 1))
                    .font(prose_font())
                    .color(TEXT_FG);
                if ui.selectable_label(highlight == Some(i), label).clicked() {
                    clicked = Some(i);
                }
            }
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("Up/Down choose · 1-5 pick · Enter use")
                    .size(11.0)
                    .color(egui::Color32::from_gray(120)),
            );
        });
    clicked
}

/// If digit `d` (1–5) was pressed, consume both its key and text events
/// (so it never lands in the focused field) and return `true`. Lets a
/// bare number key pick a suggestion without hijacking normal typing —
/// the caller only invokes this while the field is freshly selected.
fn take_digit(ctx: &egui::Context, d: usize) -> bool {
    let key = match d {
        1 => egui::Key::Num1,
        2 => egui::Key::Num2,
        3 => egui::Key::Num3,
        4 => egui::Key::Num4,
        5 => egui::Key::Num5,
        _ => return false,
    };
    let digit = d.to_string();
    ctx.input_mut(|i| {
        if !i.key_pressed(key) {
            return false;
        }
        i.events.retain(|e| {
            !matches!(e, egui::Event::Key { key: k, pressed: true, .. } if *k == key)
                && !matches!(e, egui::Event::Text(t) if *t == digit)
        });
        true
    })
}

/// Adjust vim-mode squiggle marks after an edit turned `prev` into
/// `curr`: drop any mark the edit overlapped, and shift marks that sit
/// entirely after the edit so they keep tracking their word.
fn update_marks(marks: &mut [Option<(usize, usize)>], prev: &str, curr: &str) {
    let (s, pe, ce) = changed_region(prev, curr);
    let delta = ce as isize - pe as isize;
    for m in marks.iter_mut() {
        if let Some((ws, we)) = *m {
            if we <= s {
                // entirely before the edit — unchanged
            } else if ws >= pe {
                let nws = ws as isize + delta;
                let nwe = we as isize + delta;
                *m = (nws >= 0 && nwe >= 0).then_some((nws as usize, nwe as usize));
            } else {
                *m = None; // the edit landed inside this word
            }
        }
    }
}

/// The byte span that differs between `prev` and `curr`, as
/// `(start, prev_end, curr_end)`: `prev[start..prev_end]` became
/// `curr[start..curr_end]`.
fn changed_region(prev: &str, curr: &str) -> (usize, usize, usize) {
    let (pb, cb) = (prev.as_bytes(), curr.as_bytes());
    let max = pb.len().min(cb.len());
    let mut s = 0;
    while s < max && pb[s] == cb[s] {
        s += 1;
    }
    let (mut pe, mut ce) = (pb.len(), cb.len());
    while pe > s && ce > s && pb[pe - 1] == cb[ce - 1] {
        pe -= 1;
        ce -= 1;
    }
    (s, pe, ce)
}

fn notify_daemon() {
    let Ok(Some(pid)) = runtime::read_daemon_pid() else {
        return;
    };
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .args(["-USR1", &pid.to_string()])
            .output();
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// Open Preferences straight to the Providers tab — used when the user asks
/// to escalate but no LLM API key is configured yet. Spawns a detached
/// `hyprcorrect prefs` (the prefs window is a singleton, so a second spawn
/// just focuses the open one); the section is passed via
/// `$HYPRCORRECT_PREFS_SECTION`, mirroring the daemon's launcher.
fn open_prefs_providers() {
    use std::process::{Command, Stdio};
    let Ok(exe) = std::env::current_exe() else {
        eprintln!("hyprcorrect: cannot find own executable to open Preferences");
        return;
    };
    let _ = Command::new(exe)
        .arg("prefs")
        .env("HYPRCORRECT_PREFS_SECTION", "providers")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caret_after_shortened_word_maps_tight() {
        // After a `cw` shortens "investigte" → "investi", the caret sits on
        // the space that follows. In the column-aligned display it must map
        // to the slot right after the word, not past the alignment padding.
        let original = "investigte the";
        let buffer = "investi the"; // 'i' is char 6, the space is char 7
        let layout = worddiff::align(original, buffer).expect("layout");
        let rows = wrap_columns(&layout.col_widths, 10_000.0, 8.0); // one row
        let (_disp, map) = aligned_display(buffer, &layout, &rows);
        // The space (raw char 7) lands immediately after the last 'i'
        // (raw char 6) — no column padding is skipped before the caret.
        assert_eq!(map[7], map[6] + 1);
    }

    #[test]
    fn changed_region_finds_the_edit() {
        assert_eq!(changed_region("abc", "aXc"), (1, 2, 2)); // replace
        assert_eq!(changed_region("abc", "abXc"), (2, 2, 3)); // insert
        assert_eq!(changed_region("abc", "ac"), (1, 2, 1)); // delete
    }

    #[test]
    fn editing_a_word_drops_its_mark_and_shifts_later_ones() {
        // marks for "the"(0,3) and "brown"(10,15) in "the quick brown".
        let mut marks = vec![Some((0usize, 3usize)), Some((10usize, 15usize))];
        update_marks(&mut marks, "the quick brown", "tXe quick brown");
        assert_eq!(marks[0], None); // 'h' -> 'X' touched it
        assert_eq!(marks[1], Some((10, 15))); // same-length edit before it
    }

    #[test]
    fn an_insertion_before_a_word_shifts_its_mark() {
        let mut marks = vec![Some((0usize, 3usize)), Some((10usize, 15usize))];
        // insert "AB" at the start: the words are unchanged, just moved
        // right by 2, so both marks shift and neither drops.
        update_marks(&mut marks, "the quick brown", "ABthe quick brown");
        assert_eq!(marks[0], Some((2, 5)));
        assert_eq!(marks[1], Some((12, 17)));
    }

    #[test]
    fn edits_after_a_mark_leave_it_untouched() {
        let mut marks = vec![Some((0usize, 3usize))];
        update_marks(&mut marks, "the quick", "the quickly");
        assert_eq!(marks[0], Some((0, 3)));
    }
}
