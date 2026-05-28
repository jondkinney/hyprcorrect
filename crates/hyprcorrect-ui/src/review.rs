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
const WINDOW_WIDTH: f32 = 560.0;
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
}

impl ReviewApp {
    fn new(request: ReviewRequest) -> Self {
        let segments = worddiff::diff(&request.original, &request.corrected);
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

    /// Switch into vim mode, seeding the buffer with the current
    /// sentence and dropping the cursor on the focused word.
    fn enter_vim(&mut self) {
        let sentence = worddiff::reconstruct(&self.segments);
        let cursor = self
            .focused_field
            .and_then(|ord| worddiff::field_start_offset(&self.segments, ord))
            .unwrap_or(0);
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
    }

    // ---- word-edit mode -------------------------------------------

    fn input_word(&mut self, ctx: &egui::Context) {
        // Ctrl+E → vim mode (consume so the 'e' never lands in a field).
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::E)) {
            self.enter_vim();
            return;
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
        ui.add_space(10.0);
        section_label(ui, "Original");
        show_block(ui, &self.request.original, egui::Color32::from_gray(170));

        ui.add_space(12.0);
        if self.field_segments.is_empty() {
            section_label(ui, "Proposed  ·  Ctrl+E to edit in vim");
            show_block(ui, &self.request.corrected, egui::Color32::from_gray(230));
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

        egui::Frame::new()
            .fill(egui::Color32::from_gray(40))
            .corner_radius(egui::CornerRadius::same(6))
            .inner_margin(egui::Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let mut ordinal = 0usize;
                    for (seg_idx, seg) in segments.iter_mut().enumerate() {
                        match seg {
                            Segment::Static(t) => {
                                ui.label(
                                    egui::RichText::new(t.as_str())
                                        .size(15.0)
                                        .color(egui::Color32::from_gray(210)),
                                );
                            }
                            Segment::Field(t) => {
                                let this_ord = ordinal;
                                ordinal += 1;
                                let id = egui::Id::new(("hc_review_field", seg_idx));
                                let chars = t.chars().count();
                                let width = (chars.max(3) as f32) * 9.5 + 12.0;
                                let out = egui::TextEdit::singleline(t)
                                    .id(id)
                                    .desired_width(width)
                                    .margin(egui::Margin::symmetric(4, 2))
                                    .show(ui);

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
        let mut outcome = VimOutcome::None;
        if let Some(vim) = self.vim.as_mut() {
            for k in keys {
                let o = vim.handle(k);
                if o != VimOutcome::None {
                    outcome = o;
                }
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
        show_block(ui, &self.request.original, egui::Color32::from_gray(170));
        ui.add_space(12.0);

        let (text, cursor, mode, status) = match self.vim.as_ref() {
            Some(v) => (v.text().to_string(), v.cursor(), v.mode(), v.status_line()),
            None => return,
        };

        let font = egui::FontId::monospace(15.0);
        let fg = egui::Color32::from_gray(230);
        let accent = egui::Color32::from_rgb(120, 190, 255);
        let on_block = egui::Color32::from_gray(20);

        egui::Frame::new()
            .fill(egui::Color32::from_gray(40))
            .corner_radius(egui::CornerRadius::same(6))
            .inner_margin(egui::Margin::symmetric(10, 8))
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
                    .inner_margin(egui::Margin::symmetric(20, 18)),
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
    const CHARS_PER_LINE: usize = 65;
    const LINE_HEIGHT: f32 = 22.0;
    // heading + section labels + two block paddings + the hint lines +
    // the bottom action row + paint margins.
    const CHROME: f32 = 240.0;
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
    ui.label(egui::RichText::new(text).strong().size(14.0));
    ui.add_space(4.0);
}

fn show_block(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
    egui::Frame::new()
        .fill(egui::Color32::from_gray(40))
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(egui::RichText::new(text).color(color).size(14.0));
        });
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
