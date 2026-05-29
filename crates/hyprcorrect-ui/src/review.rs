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

use std::time::Duration;

use eframe::egui;
use egui::text::{CCursor, CCursorRange, LayoutJob};
use hyprcorrect_core::runtime::{self, ReviewRequest};

use crate::vimedit::{self, VimEdit, VimKey, VimOutcome};
use crate::worddiff::{self, Segment};

const APP_ID: &str = "hyprcorrect-review";
const REFOCUS_DELAY_MS: u64 = 280;
const WINDOW_WIDTH: f32 = 600.0;
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

    let estimated_height = estimate_window_height(&request);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id(APP_ID)
            .with_title("hyprcorrect — Review")
            .with_inner_size([WINDOW_WIDTH, estimated_height])
            .with_min_inner_size([WINDOW_WIDTH, MIN_WINDOW_HEIGHT])
            .with_resizable(true),
        vsync: false,
        ..Default::default()
    };
    let _ = eframe::run_native(
        "hyprcorrect — Review",
        options,
        Box::new(move |cc| {
            crate::prefs::install_glyph_fonts(&cc.egui_ctx);
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
    /// Per-word column widths (char counts) when the original and
    /// corrected sentences have the same word count — used to render
    /// both in monospace with each correction directly under the word
    /// it replaces. `None` falls back to proportional, unaligned text.
    align: Option<Vec<usize>>,
}

impl ReviewApp {
    fn new(request: ReviewRequest) -> Self {
        let segments = worddiff::diff(&request.original, &request.corrected);
        let align = worddiff::align_widths(&request.original, &request.corrected);
        let field_segments: Vec<usize> = segments
            .iter()
            .enumerate()
            .filter_map(|(i, s)| matches!(s, Segment::Field(_)).then_some(i))
            .collect();
        Self {
            request,
            decision: None,
            mode: EditMode::Word,
            segments,
            field_segments,
            focused_field: None,
            pending_focus: None,
            focus_caret: None,
            initialized: false,
            vim: None,
            vim_marks: Vec::new(),
            dropdown_highlight: None,
            align,
        }
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

    /// Replace field `ordinal`'s text with `option`, re-focus it (so the
    /// new word is selected and typing replaces), and close the dropdown.
    fn insert_suggestion(&mut self, ordinal: usize, option: &str) {
        if let Some(&seg) = self.field_segments.get(ordinal) {
            if let Some(Segment::Field(t)) = self.segments.get_mut(seg) {
                *t = option.to_string();
            }
        }
        self.pending_focus = Some(ordinal);
        self.dropdown_highlight = None;
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
            let opts = self.options_for_field(ord, &current);
            if !opts.is_empty() {
                if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)) {
                    let next = self
                        .dropdown_highlight
                        .map_or(0, |h| (h + 1).min(opts.len() - 1));
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
                    for d in 1..=opts.len().min(5) {
                        if take_digit(ctx, d) {
                            self.insert_suggestion(ord, &opts[d - 1]);
                            return;
                        }
                    }
                }
                if let Some(h) = self.dropdown_highlight {
                    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)) {
                        let opt = opts[h].clone();
                        self.insert_suggestion(ord, &opt);
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

    fn render_word(&mut self, ui: &mut egui::Ui) {
        ui.heading("Review correction");
        ui.add_space(16.0);
        section_label(ui, "Original");
        original_card(
            ui,
            &self.request.original,
            &self.request.corrected,
            self.align.as_deref(),
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
        let mut focused_rect: Option<egui::Rect> = None;
        let mut consumed_pending = false;
        let segments = &mut self.segments;
        // When aligned, render in monospace and size each word to its
        // column so corrections sit directly under the originals.
        let widths = self.align.clone();
        let font = if widths.is_some() {
            mono_font()
        } else {
            prose_font()
        };

        card(ui, |ui| {
            let cw = char_width(ui, &font);
            // Column width in points for word column `col` given the
            // current text's char count (grows if the user types past it).
            let col_width = |col: usize, chars: usize| -> f32 {
                match &widths {
                    Some(w) => w.get(col).copied().unwrap_or(chars).max(chars) as f32 * cw,
                    None => 0.0,
                }
            };
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(0.0, 4.0);
                let mut ordinal = 0usize; // editable-field ordinal
                let mut col = 0usize; // word-column index (aligned mode)
                for (seg_idx, seg) in segments.iter_mut().enumerate() {
                    match seg {
                        Segment::Static(t) => {
                            if widths.is_some() {
                                // Split so each unchanged word can be padded
                                // to its column; separators render as-is.
                                for (is_word, tok) in worddiff::split_tokens(t) {
                                    let text = if is_word {
                                        let wlen = tok.chars().count();
                                        let pad = col_width(col, wlen) / cw;
                                        col += 1;
                                        format!("{tok:<width$}", width = pad.round() as usize)
                                    } else {
                                        tok
                                    };
                                    ui.label(
                                        egui::RichText::new(text)
                                            .font(font.clone())
                                            .color(egui::Color32::from_gray(215)),
                                    );
                                }
                            } else {
                                ui.label(
                                    egui::RichText::new(t.as_str())
                                        .font(font.clone())
                                        .color(egui::Color32::from_gray(215)),
                                );
                            }
                        }
                        Segment::Field(t) => {
                            let this_ord = ordinal;
                            ordinal += 1;
                            let this_col = col;
                            col += 1;
                            let id = egui::Id::new(("hc_review_field", seg_idx));
                            let chars = t.chars().count();
                            // Width: the column (aligned) or the exact text
                            // (unaligned), so the field grows/shrinks as the
                            // user types.
                            let w = if widths.is_some() {
                                // Exact column width — no caret padding, or
                                // the columns after this field would drift.
                                col_width(this_col, chars)
                            } else {
                                measure_width(ui, t, &font).max(7.0) + 3.0
                            };
                            let out = egui::TextEdit::singleline(t)
                                .id(id)
                                .frame(false)
                                .desired_width(w)
                                .margin(egui::Margin::ZERO)
                                .font(font.clone())
                                .text_color(egui::Color32::from_gray(238))
                                .show(ui);

                            // Blue squiggle marks the correction / tab target;
                            // span the word, not the padded column.
                            let rect = out.response.rect;
                            let focused = out.response.has_focus() || pending == Some(this_ord);
                            if focused {
                                focused_rect = Some(rect);
                            }
                            let sq_color = if focused {
                                SQUIGGLE_BLUE
                            } else {
                                SQUIGGLE_BLUE.gamma_multiply(0.65)
                            };
                            let sq_right = if widths.is_some() {
                                rect.left() + chars as f32 * cw
                            } else {
                                rect.right()
                            };
                            squiggle(ui.painter(), rect.left(), sq_right, rect.bottom(), sq_color);

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
                                        (r.primary.index, r.primary.index != r.secondary.index)
                                    })
                                    .unwrap_or((chars, false));
                                new_caret = Some((caret, chars, sel));
                            }
                        }
                    }
                }
            });
        });

        if consumed_pending {
            self.pending_focus = None;
        }
        self.focused_field = new_focused;
        self.focus_caret = new_caret;

        // Auto-expanded backup-suggestion dropdown under the focused field.
        if let (Some(ord), Some(rect)) = (self.focused_field, focused_rect) {
            let current = self.field_word(ord);
            let opts = self.options_for_field(ord, &current);
            if !opts.is_empty() {
                if let Some(pick) =
                    render_suggestion_dropdown(ui, rect, &opts, self.dropdown_highlight)
                {
                    self.insert_suggestion(ord, &opts[pick]);
                }
            }
        }
    }

    // ---- vim mode -------------------------------------------------

    fn input_vim(&mut self, ctx: &egui::Context) {
        // Vim doesn't use Tab; swallow it so egui doesn't move focus
        // onto the action buttons.
        ctx.input_mut(|i| {
            i.consume_key(egui::Modifiers::NONE, egui::Key::Tab);
            i.consume_key(egui::Modifiers::SHIFT, egui::Key::Tab);
        });

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
            VimOutcome::None => {}
        }
    }

    fn render_vim(&mut self, ui: &mut egui::Ui) {
        ui.heading("Edit sentence  ·  vim");
        ui.add_space(10.0);
        section_label(ui, "Original");
        // No column alignment in vim mode — the editor buffer below isn't
        // column-padded, so there's nothing to line up against.
        original_card(ui, &self.request.original, &self.request.corrected, None);
        ui.add_space(16.0);

        let (text, cursor, mode, status) = match self.vim.as_ref() {
            Some(v) => (v.text().to_string(), v.cursor(), v.mode(), v.status_line()),
            None => return,
        };

        let marks = self.vim_marks.clone();
        let font = egui::FontId::monospace(15.0);
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
                let row_h = ui.fonts(|f| f.row_height(&font));

                // Lay the text out plainly — the caret is painted on top
                // as an overlay so switching to/from INSERT never inserts
                // a glyph and never shifts the text.
                let mut job = LayoutJob::default();
                job.wrap.max_width = wrap_width;
                job.append(
                    &text,
                    0.0,
                    egui::TextFormat {
                        font_id: font.clone(),
                        color: fg,
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
                        origin.x + galley.size().x
                    };
                    squiggle(ui.painter(), r0.min.x, x1, r0.max.y, SQUIGGLE_BLUE);
                }

                let at = cursor.min(text.len());
                let char_idx = text[..at].chars().count();
                let caret = galley
                    .pos_from_cursor(CCursor::new(char_idx))
                    .translate(origin.to_vec2());
                match mode {
                    vimedit::Mode::Insert => {
                        // Thin i-beam between glyphs.
                        let ibeam =
                            egui::Rect::from_min_size(caret.min, egui::vec2(2.0, caret.height()));
                        ui.painter().rect_filled(ibeam, 0.0, accent);
                    }
                    _ => {
                        // Block over the character under the cursor.
                        let next = galley
                            .pos_from_cursor(CCursor::new(char_idx + 1))
                            .translate(origin.to_vec2());
                        let advance = next.min.x - caret.min.x;
                        let w = if advance > 1.0 && advance < 200.0 {
                            advance
                        } else {
                            ui.fonts(|f| f.glyph_width(&font, ' '))
                        };
                        let block =
                            egui::Rect::from_min_size(caret.min, egui::vec2(w, caret.height()));
                        ui.painter().rect_filled(block, 0.0, accent);
                        if let Some(ch) = text[at..].chars().next() {
                            if ch != '\n' {
                                ui.painter().text(
                                    caret.min,
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

        ui.add_space(8.0);
        ui.label(egui::RichText::new(status).monospace().color(accent));
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new(
                "ciw · dw · cw · x · r  ·  w b 0 $  ·  i a o  ·  :wq / Enter apply  ·  :q cancel",
            )
            .size(11.0)
            .color(egui::Color32::from_gray(130)),
        );
    }
}

impl eframe::App for ReviewApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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
        egui::TopBottomPanel::bottom("review_actions")
            .resizable(false)
            .show(ctx, |ui| {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel  (Esc)").clicked() {
                        do_cancel = true;
                    }
                    let apply_label = egui::RichText::new("Apply  (Enter)")
                        .color(egui::Color32::from_rgb(90, 200, 120));
                    if ui.button(apply_label).clicked() {
                        do_apply = true;
                    }
                });
                ui.add_space(8.0);
            });
        if do_apply {
            self.apply(ctx);
        } else if do_cancel {
            self.cancel(ctx);
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
                    key, pressed: true, ..
                } => {
                    let vk = match key {
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

/// Pick a window height that fits the original + proposed text
/// without truncation. Lightweight estimate; the surrounding
/// `ScrollArea` covers any miss.
fn estimate_window_height(request: &ReviewRequest) -> f32 {
    const CHARS_PER_LINE: usize = 60;
    const LINE_HEIGHT: f32 = 24.0;
    // heading + section labels + two card paddings + the hint lines +
    // the bottom action row + paint margins.
    const CHROME: f32 = 270.0;
    let lines = |s: &str| -> usize {
        s.lines()
            .map(|line| line.chars().count().max(1).div_ceil(CHARS_PER_LINE))
            .sum::<usize>()
            .max(1)
    };
    let total_lines = lines(&request.original) + lines(&request.corrected);
    let body_height = total_lines as f32 * LINE_HEIGHT;
    (CHROME + body_height).clamp(MIN_WINDOW_HEIGHT, MAX_WINDOW_HEIGHT)
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
const SQUIGGLE_BLUE: egui::Color32 = egui::Color32::from_rgb(96, 165, 250);
const CARD_BG: egui::Color32 = egui::Color32::from_gray(34);
const TEXT_FG: egui::Color32 = egui::Color32::from_gray(225);

/// The prose font for the Original / Proposed text, shared by the
/// static words and the editable fields so they keep one baseline.
fn prose_font() -> egui::FontId {
    egui::FontId::proportional(16.0)
}

/// A rounded, padded container for a block of review text.
fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(CARD_BG)
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// The "Original" card: the user's text with a red squiggle under each
/// word the corrector changed. With `align` widths it renders monospace
/// and pads each word to its column so the words above line up with the
/// corrections below.
fn original_card(ui: &mut egui::Ui, original: &str, corrected: &str, align: Option<&[usize]>) {
    card(ui, |ui| {
        if let Some(widths) = align {
            paint_aligned_original(ui, original, corrected, widths);
        } else {
            let ranges = worddiff::changed_word_ranges(original, corrected);
            paint_text_with_squiggles(
                ui,
                original,
                &ranges,
                egui::Color32::from_gray(170),
                SQUIGGLE_RED,
            );
        }
    });
}

/// Render `original` in monospace with each word padded to its column
/// width, and a red squiggle under each word that differs from the
/// matching corrected word — so the originals line up directly above
/// the corrections.
fn paint_aligned_original(ui: &mut egui::Ui, original: &str, corrected: &str, widths: &[usize]) {
    let font = mono_font();
    let fg = egui::Color32::from_gray(170);
    let cw = char_width(ui, &font);
    let corr_words = worddiff::word_list(corrected);

    // Render exactly like the Proposed card — a row of monospace labels,
    // each word padded to its column width — so the two cards share the
    // same per-widget layout and the columns line up pixel-for-pixel.
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(0.0, 4.0);
        let mut col = 0usize;
        for (is_word, tok) in worddiff::split_tokens(original) {
            if is_word {
                let wlen = tok.chars().count();
                let width = widths.get(col).copied().unwrap_or(wlen).max(wlen);
                let changed = corr_words.get(col).is_some_and(|c| *c != tok);
                col += 1;
                let padded = format!("{tok:<width$}");
                let resp = ui.label(egui::RichText::new(padded).font(font.clone()).color(fg));
                if changed {
                    let r = resp.rect;
                    squiggle(
                        ui.painter(),
                        r.left(),
                        r.left() + wlen as f32 * cw,
                        r.bottom(),
                        SQUIGGLE_RED,
                    );
                }
            } else {
                ui.label(egui::RichText::new(tok).font(font.clone()).color(fg));
            }
        }
    });
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

/// Width of `text` laid out in `font`, in points.
fn measure_width(ui: &egui::Ui, text: &str, font: &egui::FontId) -> f32 {
    ui.fonts(|f| {
        f.layout_no_wrap(text.to_owned(), font.clone(), egui::Color32::WHITE)
            .size()
            .x
    })
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

/// Render the auto-expanded suggestion list as a floating panel just
/// below `anchor` (the focused field). Returns the clicked option index.
fn render_suggestion_dropdown(
    ui: &egui::Ui,
    anchor: egui::Rect,
    options: &[String],
    highlight: Option<usize>,
) -> Option<usize> {
    let mut clicked = None;
    egui::Area::new(egui::Id::new("hc_suggestion_dropdown"))
        .order(egui::Order::Foreground)
        .fixed_pos(anchor.left_bottom() + egui::vec2(0.0, 6.0))
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style())
                .fill(CARD_BG)
                .corner_radius(egui::CornerRadius::same(6))
                .show(ui, |ui| {
                    ui.set_min_width(170.0);
                    ui.spacing_mut().item_spacing.y = 2.0;
                    for (i, opt) in options.iter().enumerate() {
                        let label = egui::RichText::new(format!("{}   {opt}", i + 1))
                            .font(prose_font())
                            .color(TEXT_FG);
                        if ui.selectable_label(highlight == Some(i), label).clicked() {
                            clicked = Some(i);
                        }
                    }
                });
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

#[cfg(test)]
mod tests {
    use super::*;

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
