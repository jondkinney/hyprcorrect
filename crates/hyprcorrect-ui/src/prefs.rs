//! The egui preferences window.
//!
//! A single window with a left sidebar (sections) and a right pane
//! (the focused section). On Save the config is written to disk and
//! the running daemon is signalled (`SIGHUP` on Linux, no-op for now
//! on other OSes) so it picks up the change without restart. Secrets
//! (LLM API keys) live in the OS keychain — never in config.toml.

use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant, SystemTime};

use eframe::egui;
use hyprcorrect_core::{Config, LlmConfig, ProviderId, runtime, secrets};

use crate::apps::AppRegistry;
use crate::icon;

const APP_ID: &str = "hyprcorrect-prefs";
const LLM_ANTHROPIC_KEY: &str = "llm.anthropic";

/// Which hotkey row's chord is being recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HotkeyTarget {
    FixWord,
    FixSentence,
    Review,
}

/// Sections in the left sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Hotkeys,
    Providers,
    Behavior,
    Privacy,
    About,
}

impl Section {
    fn label(self) -> &'static str {
        match self {
            Self::Hotkeys => "Hotkeys",
            Self::Providers => "Providers",
            Self::Behavior => "Behavior",
            Self::Privacy => "Privacy",
            Self::About => "About",
        }
    }

    fn all() -> &'static [Self] {
        &[
            Self::Hotkeys,
            Self::Providers,
            Self::Behavior,
            Self::Privacy,
            Self::About,
        ]
    }
}

/// Status banner under the title — feedback after Save / Cancel /
/// keyring errors. Cleared when the user touches a field.
#[derive(Debug, Clone, Default)]
struct Status {
    text: String,
    is_error: bool,
}

struct PrefsApp {
    /// Working copy of the config. The user edits this; Save writes
    /// it to disk.
    config: Config,
    /// Snapshot of the config as it sits on disk — used to gate the
    /// Save button (no-op saves are blocked).
    saved: Config,
    /// LLM API key in the keyring at startup, used to detect changes.
    saved_api_key: String,
    /// Current LLM API key field.
    api_key_field: String,
    section: Section,
    status: Status,
    /// New-entry text for the privacy blocklist's "Add" row.
    blocklist_entry: String,
    /// Signal the singleton holder thread to shut down on close.
    shutdown_tx: Option<Sender<()>>,
    /// Lazy-loaded app icon for the sidebar (256×256 raster from the
    /// bundled SVG).
    logo: Option<egui::TextureHandle>,
    /// Last time we recomputed `daemon_stale`. Rechecked at most once
    /// a second so a recompile shows up promptly without thrashing the
    /// filesystem on every repaint.
    last_stale_check: Instant,
    /// Cached "binary is newer than the running daemon" flag — drives
    /// the "Relaunch daemon (new build)" button's visibility.
    daemon_stale: bool,
    /// Which hotkey row, if any, is recording. The next non-modifier
    /// key press becomes the new chord for that target.
    capturing_chord: Option<HotkeyTarget>,
    /// Window classes detected on the desktop right now, sorted and
    /// deduplicated. Populated lazily and refreshed when the privacy
    /// panel is opened so the picker reflects whatever's running.
    running_apps: Vec<String>,
    /// Last time we refreshed `running_apps` — re-checked every few
    /// seconds while the Privacy panel is visible so newly-launched
    /// apps show up.
    last_apps_refresh: Instant,
    /// Currently-selected entry in the Privacy "Add app" dropdown.
    selected_app: Option<String>,
    /// Search filter for the Privacy app dropdown.
    app_filter: String,
    /// Cached `.desktop` registry — display names + icons.
    app_registry: AppRegistry,
}

impl PrefsApp {
    fn new(saved: Config, saved_api_key: String, shutdown_tx: Sender<()>) -> Self {
        Self {
            config: saved.clone(),
            saved,
            api_key_field: saved_api_key.clone(),
            saved_api_key,
            section: Section::Hotkeys,
            status: Status::default(),
            blocklist_entry: String::new(),
            shutdown_tx: Some(shutdown_tx),
            logo: None,
            last_stale_check: Instant::now() - Duration::from_secs(60),
            daemon_stale: false,
            capturing_chord: None,
            running_apps: Vec::new(),
            last_apps_refresh: Instant::now() - Duration::from_secs(60),
            selected_app: None,
            app_filter: String::new(),
            app_registry: AppRegistry::discover(),
        }
    }

    fn refresh_running_apps(&mut self) {
        if self.last_apps_refresh.elapsed() < Duration::from_secs(3) {
            return;
        }
        self.last_apps_refresh = Instant::now();
        self.running_apps = list_running_classes();
    }

    fn logo_texture(&mut self, ctx: &egui::Context) -> Option<&egui::TextureHandle> {
        if self.logo.is_none() {
            let size = 256u32;
            let rgba = icon::render_app_icon_rgba(size);
            if rgba.len() == (size as usize) * (size as usize) * 4 {
                let image =
                    egui::ColorImage::from_rgba_unmultiplied([size as usize, size as usize], &rgba);
                self.logo =
                    Some(ctx.load_texture("hyprcorrect_logo", image, egui::TextureOptions::LINEAR));
            }
        }
        self.logo.as_ref()
    }

    fn refresh_stale_check(&mut self) {
        if self.last_stale_check.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_stale_check = Instant::now();
        self.daemon_stale = daemon_is_stale();
    }

    fn dirty(&self) -> bool {
        self.config != self.saved || self.api_key_field != self.saved_api_key
    }

    fn ok(&mut self, text: impl Into<String>) {
        self.status = Status {
            text: text.into(),
            is_error: false,
        };
    }

    fn err(&mut self, text: impl Into<String>) {
        self.status = Status {
            text: text.into(),
            is_error: true,
        };
    }

    fn clear_status(&mut self) {
        self.status = Status::default();
    }

    fn save(&mut self) {
        if let Err(msg) = validate(&self.config) {
            self.err(msg);
            return;
        }
        if let Err(e) = self.config.save() {
            self.err(format!("save failed: {e}"));
            return;
        }
        if self.api_key_field != self.saved_api_key {
            let result = if self.api_key_field.is_empty() {
                secrets::delete(LLM_ANTHROPIC_KEY)
            } else {
                secrets::set(LLM_ANTHROPIC_KEY, &self.api_key_field)
            };
            if let Err(e) = result {
                self.err(format!("keychain write failed: {e}"));
                return;
            }
            self.saved_api_key = self.api_key_field.clone();
        }
        self.saved = self.config.clone();
        notify_daemon_reload();
        self.ok("Saved.");
    }

    fn cancel(&mut self) {
        self.config = self.saved.clone();
        self.api_key_field = self.saved_api_key.clone();
        self.clear_status();
    }
}

impl eframe::App for PrefsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        apply_style(ctx);
        self.refresh_stale_check();

        // While recording a chord, drain key events from egui's input
        // queue (so other widgets don't act on them) and commit the
        // first non-modifier key press with whatever modifiers are
        // currently held. Esc cancels.
        if let Some(target) = self.capturing_chord
            && let Some(outcome) = ctx.input_mut(capture_outcome)
        {
            match outcome {
                CaptureOutcome::Cancel => {
                    self.capturing_chord = None;
                    notify_daemon_reload();
                }
                CaptureOutcome::Commit(s) => {
                    match target {
                        HotkeyTarget::FixWord => self.config.hotkeys.fix_word = s,
                        HotkeyTarget::FixSentence => self.config.hotkeys.fix_sentence = s,
                        HotkeyTarget::Review => self.config.hotkeys.review = s,
                    }
                    self.capturing_chord = None;
                    self.clear_status();
                    notify_daemon_reload();
                }
            }
        }

        // Materialize the logo handle before borrowing self.* for the
        // sidebar closure.
        let logo = self.logo_texture(ctx).cloned();

        egui::SidePanel::left("sections")
            .resizable(false)
            .default_width(200.0)
            .show(ctx, |ui| {
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    if let Some(handle) = &logo {
                        ui.add(egui::Image::new(handle).fit_to_exact_size(egui::vec2(28.0, 28.0)));
                        ui.add_space(8.0);
                    }
                    ui.heading("hyprcorrect");
                });
                ui.add_space(14.0);
                ui.separator();
                ui.add_space(8.0);
                for section in Section::all() {
                    let selected = self.section == *section;
                    if sidebar_item(ui, selected, section.label()).clicked() {
                        self.section = *section;
                        self.clear_status();
                    }
                }
            });

        let mut quit_requested = false;
        let mut relaunch_requested = false;

        egui::TopBottomPanel::bottom("actions")
            .resizable(false)
            .min_height(54.0)
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.add_space(4.0);
                    let quit_label = egui::RichText::new("Quit hyprcorrect")
                        .color(egui::Color32::from_rgb(220, 90, 90));
                    if ui.add(egui::Button::new(quit_label)).clicked() {
                        quit_requested = true;
                    }
                    if self.daemon_stale {
                        let relaunch_label = egui::RichText::new("Relaunch daemon (new build)")
                            .color(egui::Color32::from_rgb(220, 160, 50));
                        let resp = ui.add(egui::Button::new(relaunch_label)).on_hover_text(
                            "The on-disk binary is newer than the running daemon. \
                                 Click to quit the old daemon and spawn the new one.",
                        );
                        if resp.clicked() {
                            relaunch_requested = true;
                        }
                    }

                    if !self.status.text.is_empty() {
                        ui.add_space(8.0);
                        let color = if self.status.is_error {
                            ui.visuals().error_fg_color
                        } else {
                            ui.visuals().widgets.active.fg_stroke.color
                        };
                        ui.colored_label(color, &self.status.text);
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(4.0);
                        if ui
                            .add_enabled(self.dirty(), egui::Button::new("Save"))
                            .clicked()
                        {
                            self.save();
                        }
                        if ui
                            .add_enabled(self.dirty(), egui::Button::new("Cancel"))
                            .clicked()
                        {
                            self.cancel();
                        }
                    });
                });
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::central_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(20, 18)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| match self.section {
                        Section::Hotkeys => self.hotkeys_panel(ui),
                        Section::Providers => self.providers_panel(ui),
                        Section::Behavior => self.behavior_panel(ui),
                        Section::Privacy => self.privacy_panel(ui),
                        Section::About => self.about_panel(ui),
                    });
            });

        if quit_requested {
            quit_daemon();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if relaunch_requested {
            relaunch_daemon_now();
            // Force the next stale check to fire immediately so the
            // button hides as soon as the new daemon is up.
            self.last_stale_check = Instant::now() - Duration::from_secs(60);
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        // If the window is closing while we were still recording,
        // restore the daemon's bind so the user isn't left with a
        // dead trigger.
        if self.capturing_chord.is_some() {
            notify_daemon_reload();
        }
    }
}

impl PrefsApp {
    fn hotkeys_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Hotkeys");
        ui.add_space(14.0);

        // -- Fix last word --------------------------------------------------
        field_label(ui, "Fix last word");
        ui.add_space(4.0);
        let fix_word_value = self.config.hotkeys.fix_word.clone();
        if hotkey_chord_row(
            ui,
            HotkeyTarget::FixWord,
            &fix_word_value,
            self.capturing_chord,
        ) {
            self.capturing_chord = Some(HotkeyTarget::FixWord);
            self.clear_status();
            notify_daemon_release();
        }
        ui.add_space(6.0);
        caption(
            ui,
            "Click the chip and press the chord you want. Esc cancels. \
             Hyprland will eat the chord so terminals and other focused \
             apps never see it.",
        );

        ui.add_space(SETTING_BLOCK_SPACING);

        // -- Fix last sentence (M4 — UI ready, daemon not yet wired) -------
        field_label(ui, "Fix last sentence");
        ui.add_space(4.0);
        let fix_sentence_value = self.config.hotkeys.fix_sentence.clone();
        if hotkey_chord_row(
            ui,
            HotkeyTarget::FixSentence,
            &fix_sentence_value,
            self.capturing_chord,
        ) {
            self.capturing_chord = Some(HotkeyTarget::FixSentence);
            self.clear_status();
            notify_daemon_release();
        }
        if !self.config.hotkeys.fix_sentence.is_empty()
            && ui
                .add(egui::Button::new("Clear").frame(false))
                .on_hover_text("Unbind this chord")
                .clicked()
        {
            self.config.hotkeys.fix_sentence.clear();
            self.clear_status();
        }
        ui.add_space(6.0);
        caption(
            ui,
            "Corrects the previous sentence in one keypress. Routes to \
             whichever provider you picked as `Smart` in Providers.",
        );

        ui.add_space(SETTING_BLOCK_SPACING);

        // -- Review popup -------------------------------------------------
        field_label(ui, "Review correction");
        ui.add_space(4.0);
        let review_value = self.config.hotkeys.review.clone();
        if hotkey_chord_row(
            ui,
            HotkeyTarget::Review,
            &review_value,
            self.capturing_chord,
        ) {
            self.capturing_chord = Some(HotkeyTarget::Review);
            self.clear_status();
            notify_daemon_release();
        }
        if !self.config.hotkeys.review.is_empty()
            && ui
                .add(egui::Button::new("Clear").frame(false))
                .on_hover_text("Unbind this chord")
                .clicked()
        {
            self.config.hotkeys.review.clear();
            self.clear_status();
        }
        ui.add_space(6.0);
        caption(
            ui,
            "Shows the proposed correction in a small popup; press Enter \
             to apply or Esc to cancel. Useful for eyeballing LLM \
             suggestions before they land.",
        );

        ui.add_space(SETTING_BLOCK_SPACING);
        caption(
            ui,
            "$HYPRCORRECT_CHORD overrides Fix last word for one-off dev runs.",
        );
    }

    fn providers_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Providers");
        ui.add_space(14.0);

        let mut touched = false;

        field_label(ui, "Default provider");
        caption(ui, "Used for fix-last-word — instant, ideally local.");
        ui.add_space(4.0);
        touched |= provider_radio(ui, &mut self.config.providers.default);

        ui.add_space(SETTING_BLOCK_SPACING);
        field_label(ui, "Smart provider");
        caption(ui, "Used for fix-last-sentence and the review popup (M4).");
        ui.add_space(4.0);
        touched |= provider_radio(ui, &mut self.config.providers.smart);

        ui.add_space(SETTING_BLOCK_SPACING);
        ui.separator();
        ui.add_space(SETTING_BLOCK_SPACING);

        ui.label(egui::RichText::new("LLM").size(16.0).strong());
        ui.add_space(8.0);
        touched |= llm_section(ui, &mut self.config.providers.llm, &mut self.api_key_field);

        ui.add_space(SETTING_BLOCK_SPACING);
        ui.separator();
        ui.add_space(SETTING_BLOCK_SPACING);

        ui.label(egui::RichText::new("LanguageTool").size(16.0).strong());
        ui.add_space(8.0);
        touched |= ui
            .checkbox(&mut self.config.providers.languagetool.enabled, "Enabled")
            .changed();
        ui.add_space(8.0);
        field_label(ui, "URL");
        ui.add_space(4.0);
        touched |= padded_text_edit(ui, &mut self.config.providers.languagetool.url).changed();
        ui.add_space(4.0);
        caption(ui, "POST endpoint of your self-hosted LanguageTool server.");

        if touched {
            self.clear_status();
        }
    }

    fn behavior_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Behavior");
        ui.add_space(14.0);

        field_label(ui, "Inter-key delay");
        caption(ui, "Applied to synthetic typing.");
        ui.add_space(6.0);
        let response = ui.add(
            egui::Slider::new(&mut self.config.behavior.inter_key_delay_ms, 0..=50).suffix(" ms"),
        );
        if response.changed() {
            self.clear_status();
        }
        ui.add_space(6.0);
        caption(
            ui,
            "0 ms is fastest but some apps drop characters under that speed; \
             2 ms is the safe default.",
        );
    }

    fn privacy_panel(&mut self, ui: &mut egui::Ui) {
        self.refresh_running_apps();

        ui.heading("Privacy");
        ui.add_space(14.0);

        field_label(ui, "App blocklist");
        caption(
            ui,
            "Apps in this list never have their keys buffered. Match is \
             case-insensitive against the window class.",
        );
        ui.add_space(12.0);

        // -- Currently blocked entries: render with icon + display name ---
        let blocked_ids: Vec<String> = self.config.privacy.app_blocklist.clone();
        let mut remove: Option<usize> = None;
        if blocked_ids.is_empty() {
            caption(ui, "(none yet — pick a running app below)");
            ui.add_space(8.0);
        } else {
            for (i, identifier) in blocked_ids.iter().enumerate() {
                let meta = self.app_registry.lookup(ui.ctx(), identifier);
                ui.horizontal(|ui| {
                    if let Some(handle) = &meta.icon {
                        ui.add(egui::Image::new(handle).fit_to_exact_size(egui::vec2(20.0, 20.0)));
                    } else {
                        ui.add_space(20.0);
                    }
                    ui.add_space(6.0);
                    ui.label(&meta.display_name);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(egui::Button::new("Remove").frame(false))
                            .on_hover_text(format!("Remove {} from the blocklist", meta.identifier))
                            .clicked()
                        {
                            remove = Some(i);
                        }
                    });
                });
                ui.add_space(2.0);
            }
        }
        if let Some(i) = remove {
            let removed = self.config.privacy.app_blocklist.remove(i);
            if self.selected_app.as_deref() == Some(removed.as_str()) {
                self.selected_app = None;
            }
            self.clear_status();
        }

        ui.add_space(SETTING_BLOCK_SPACING);
        ui.separator();
        ui.add_space(SETTING_BLOCK_SPACING);

        // -- Picker: running apps not already on the blocklist ------------
        field_label(ui, "Add a running app");
        ui.add_space(4.0);

        let already_blocked: std::collections::HashSet<String> = self
            .config
            .privacy
            .app_blocklist
            .iter()
            .map(|s| s.to_ascii_lowercase())
            .collect();
        let candidate_ids: Vec<String> = self
            .running_apps
            .iter()
            .filter(|c| !already_blocked.contains(&c.to_ascii_lowercase()))
            .cloned()
            .collect();

        // Resolve each candidate to its display name + icon ahead of the
        // closure so we don't borrow `self` twice.
        let candidates: Vec<crate::apps::AppMeta> = candidate_ids
            .iter()
            .map(|id| self.app_registry.lookup(ui.ctx(), id))
            .collect();

        ui.horizontal(|ui| {
            let selected_display = self
                .selected_app
                .as_deref()
                .and_then(|id| candidates.iter().find(|c| c.identifier == id))
                .map(|c| c.display_name.clone())
                .unwrap_or_else(|| {
                    if candidates.is_empty() {
                        "(no running apps detected)".to_string()
                    } else {
                        "Choose an app…".to_string()
                    }
                });
            let selected_ref = &mut self.selected_app;
            let filter = &mut self.app_filter;
            egui::ComboBox::from_id_salt("blocklist_app_picker")
                .selected_text(selected_display)
                .width(ui.available_width() - 80.0)
                .show_ui(ui, |ui| {
                    ui.add(
                        egui::TextEdit::singleline(filter)
                            .hint_text("Search")
                            .margin(egui::Margin::symmetric(8, 4))
                            .desired_width(f32::INFINITY),
                    );
                    ui.separator();
                    let needle = filter.to_ascii_lowercase();
                    egui::ScrollArea::vertical()
                        .max_height(260.0)
                        .show(ui, |ui| {
                            for c in &candidates {
                                if !needle.is_empty()
                                    && !c.display_name.to_ascii_lowercase().contains(&needle)
                                    && !c.identifier.to_ascii_lowercase().contains(&needle)
                                {
                                    continue;
                                }
                                let is_selected =
                                    selected_ref.as_deref() == Some(c.identifier.as_str());
                                let row = ui
                                    .horizontal(|ui| {
                                        if let Some(handle) = &c.icon {
                                            ui.add(
                                                egui::Image::new(handle)
                                                    .fit_to_exact_size(egui::vec2(20.0, 20.0)),
                                            );
                                        } else {
                                            ui.add_space(20.0);
                                        }
                                        ui.add_space(6.0);
                                        ui.selectable_label(is_selected, &c.display_name)
                                    })
                                    .inner;
                                if row.clicked() {
                                    *selected_ref = Some(c.identifier.clone());
                                }
                            }
                        });
                });
            let can_add = self.selected_app.as_ref().is_some_and(|s| {
                !s.is_empty() && !already_blocked.contains(&s.to_ascii_lowercase())
            });
            if ui.add_enabled(can_add, egui::Button::new("Add")).clicked()
                && let Some(class) = self.selected_app.take()
            {
                self.config.privacy.app_blocklist.push(class);
                self.app_filter.clear();
                self.clear_status();
            }
        });

        ui.add_space(SETTING_BLOCK_SPACING);

        // -- Fallback: type-in for apps that aren't running right now ------
        field_label(ui, "Or add by class name");
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.blocklist_entry)
                    .margin(egui::Margin::symmetric(8, 6))
                    .desired_width(ui.available_width() - 80.0),
            );
            let add_clicked = ui.button("Add").clicked()
                || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
            if add_clicked {
                let entry = self.blocklist_entry.trim().to_string();
                if !entry.is_empty() && !already_blocked.contains(&entry.to_ascii_lowercase()) {
                    self.config.privacy.app_blocklist.push(entry);
                    self.blocklist_entry.clear();
                    self.clear_status();
                }
            }
        });
        ui.add_space(4.0);
        caption(
            ui,
            "Useful for apps that aren't open yet. The class is whatever \
             `hyprctl activewindow` shows for that app.",
        );
    }

    fn about_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("About hyprcorrect");
        ui.add_space(14.0);

        ui.label(
            egui::RichText::new(format!("Version {}", hyprcorrect_core::version()))
                .size(15.0)
                .strong(),
        );
        ui.add_space(8.0);
        ui.label("Keyboard-driven spelling and typo correction for the whole desktop.");
        ui.add_space(SETTING_BLOCK_SPACING);

        field_label(ui, "Source");
        caption(ui, "https://github.com/jondkinney/hyprcorrect");
        ui.add_space(SETTING_BLOCK_SPACING);

        field_label(ui, "License");
        caption(ui, "MIT OR Apache-2.0");
    }
}

/// Render a provider-id radio group; returns `true` if the user
/// changed the selection in this frame.
fn provider_radio(ui: &mut egui::Ui, selection: &mut ProviderId) -> bool {
    let before = *selection;
    ui.horizontal(|ui| {
        ui.radio_value(selection, ProviderId::Spellbook, "Spellbook (offline)");
        ui.radio_value(selection, ProviderId::Llm, "LLM");
        ui.radio_value(selection, ProviderId::LanguageTool, "LanguageTool");
    });
    *selection != before
}

/// Render LLM-specific fields. Returns `true` if anything changed.
fn llm_section(ui: &mut egui::Ui, llm: &mut LlmConfig, api_key: &mut String) -> bool {
    let mut changed = false;

    field_label(ui, "Backend");
    ui.add_space(4.0);
    changed |= padded_text_edit(ui, &mut llm.backend).changed();

    ui.add_space(SETTING_BLOCK_SPACING);
    field_label(ui, "Model");
    ui.add_space(4.0);
    changed |= padded_text_edit(ui, &mut llm.model).changed();

    ui.add_space(SETTING_BLOCK_SPACING);
    field_label(ui, "API key");
    ui.add_space(4.0);
    changed |= padded_password_edit(ui, api_key).changed();
    ui.add_space(4.0);
    caption(ui, "Stored in your OS keychain, not in config.toml.");

    changed
}

/// Sidebar row — vernier-style. Egui's default `selectable_label`
/// puts a square, light selection backdrop behind whatever it draws;
/// we want a rounded, contained pill. Allocates a click-sized rect
/// and paints the selection backdrop + label ourselves.
fn sidebar_item(ui: &mut egui::Ui, selected: bool, label: &str) -> egui::Response {
    let height = 32.0;
    let response = ui.allocate_response(
        egui::vec2(ui.available_width(), height),
        egui::Sense::click(),
    );
    let visuals = ui.style().interact_selectable(&response, selected);
    if selected || response.hovered() {
        ui.painter().rect_filled(
            response.rect.expand(-2.0),
            egui::CornerRadius::same(6),
            visuals.bg_fill,
        );
    }
    let text_pos = response.rect.left_center() + egui::vec2(12.0, 0.0);
    ui.painter().text(
        text_pos,
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(14.0),
        visuals.text_color(),
    );
    response
}

/// Single-line text input with consistent inner padding so fields
/// don't collapse to ~16 px tall at the body font size.
fn padded_text_edit(ui: &mut egui::Ui, text: &mut String) -> egui::Response {
    ui.add(
        egui::TextEdit::singleline(text)
            .margin(egui::Margin::symmetric(8, 6))
            .desired_width(f32::INFINITY),
    )
}

/// Single-line *password* input with the same padding as
/// [`padded_text_edit`]. The contents render as bullets.
fn padded_password_edit(ui: &mut egui::Ui, text: &mut String) -> egui::Response {
    ui.add(
        egui::TextEdit::singleline(text)
            .password(true)
            .margin(egui::Margin::symmetric(8, 6))
            .desired_width(f32::INFINITY),
    )
}

/// Bold-ish label introducing a setting. Slightly larger than the
/// caption text below the input.
fn field_label(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).strong().size(14.0));
}

/// Muted explainer text under inputs or checkboxes.
fn caption(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .size(12.0)
            .color(egui::Color32::from_gray(170)),
    );
}

const SETTING_BLOCK_SPACING: f32 = 22.0;

/// The Hyprland/Omarchy logo glyph in the bundled `omarchy.ttf`.
/// Renders as a blank tofu box if the font isn't installed — we
/// guard with [`OMARCHY_FONT_AVAILABLE`] before using it.
const OMARCHY_LOGO: char = '\u{e900}';

static OMARCHY_FONT_AVAILABLE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Register the fonts the chord chip needs into a dedicated
/// `shortcut` font family:
///
///   shortcut = [ omarchy.ttf ? , sans-with-modifier-glyphs ? ,
///                default Proportional ]
///
/// The chain order means egui draws each character with the first
/// font that has it. Omarchy gives us the Hyprland logo at
/// `\u{e900}`; a system sans font with `⌃ ⇧ ⌥ ⌘` covers the
/// standard modifier glyphs; the default proportional font handles
/// plain letters. If no symbol font is found we fall back to the
/// default, and `chord_glyphs` will show ASCII names through
/// [`OMARCHY_FONT_AVAILABLE`] — but the symbol font search rarely
/// fails on a desktop Linux system.
///
/// Mirrors `vernier`'s `install_glyph_fonts` pattern.
pub(crate) fn install_glyph_fonts(ctx: &egui::Context) {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    let mut fonts = egui::FontDefinitions::default();
    let mut shortcut_chain: Vec<String> = Vec::new();

    // -- Highest priority: a sans font with the modifier glyphs -------
    // Adwaita Sans and DejaVu Sans both ship `⌃ ⇧ ⌥ ⌘`; Noto Sans is
    // a guaranteed fallback. Take the first that loads. This font
    // also covers ASCII; coming first prevents a partial-coverage
    // font like Omarchy (which has cmap entries for ASCII letters
    // pointing at empty glyphs) from eating characters from the
    // chip's text.
    let symbol_paths = [
        // Linux
        "/usr/share/fonts/Adwaita/AdwaitaSans-Regular.ttf",
        "/usr/share/fonts/dejavu/DejaVuSans-Bold.ttf",
        "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
        "/usr/share/fonts/noto/NotoSans-Bold.ttf",
        "/usr/share/fonts/noto/NotoSansSymbols-Black.ttf",
        "/usr/share/fonts/liberation/LiberationSans-Bold.ttf",
        // macOS
        "/System/Library/Fonts/SFNS.ttf",
        "/System/Library/Fonts/Helvetica.ttc",
        "/Library/Fonts/Arial Bold.ttf",
    ];
    for path in symbol_paths {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert(
                "shortcut_symbols".into(),
                Arc::new(egui::FontData::from_owned(bytes)),
            );
            shortcut_chain.push("shortcut_symbols".into());
            break;
        }
    }

    // -- Then the egui defaults ---------------------------------------
    if let Some(default_chain) = fonts.families.get(&egui::FontFamily::Proportional) {
        shortcut_chain.extend(default_chain.iter().cloned());
    }

    // -- Last: Omarchy, used only for codepoints no earlier font has
    // — that's the Hyprland logo at U+E900. We push it last because
    // Omarchy's cmap claims most ASCII codepoints with empty glyphs,
    // so putting it earlier would silently drop letters like 'c' / 'o'
    // from the chip's text.
    let mut omarchy_candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let mut p = std::path::PathBuf::from(home);
        p.push(".local/share/fonts/omarchy.ttf");
        omarchy_candidates.push(p);
    }
    omarchy_candidates.push("/usr/share/fonts/omarchy.ttf".into());
    for path in omarchy_candidates {
        if let Ok(bytes) = std::fs::read(&path) {
            fonts.font_data.insert(
                "omarchy".into(),
                Arc::new(egui::FontData::from_owned(bytes)),
            );
            shortcut_chain.push("omarchy".into());
            OMARCHY_FONT_AVAILABLE.store(true, Ordering::Relaxed);
            break;
        }
    }

    fonts
        .families
        .insert(egui::FontFamily::Name("shortcut".into()), shortcut_chain);
    ctx.set_fonts(fonts);
}

/// Replace `+`-separated modifier tokens with Unicode glyphs the
/// reader recognizes from native menus. Used to display the stored
/// accelerator string on the chord chip.
fn chord_glyphs(stored: &str) -> String {
    use std::sync::atomic::Ordering;
    let omarchy = OMARCHY_FONT_AVAILABLE.load(Ordering::Relaxed);
    stored
        .split('+')
        .filter(|t| !t.trim().is_empty())
        .map(|tok| match tok.trim().to_ascii_uppercase().as_str() {
            "SUPER" | "META" | "CMD" | "COMMAND" | "WIN" | "WINDOWS" => {
                if omarchy {
                    OMARCHY_LOGO.to_string()
                } else {
                    "⌘".to_string()
                }
            }
            "CTRL" | "CONTROL" => "⌃".to_string(),
            "SHIFT" => "⇧".to_string(),
            "ALT" | "OPTION" => "⌥".to_string(),
            "RETURN" | "ENTER" => "↵".to_string(),
            "TAB" => "⇥".to_string(),
            "ESCAPE" | "ESC" => "⎋".to_string(),
            "BACKSPACE" => "⌫".to_string(),
            "DELETE" => "⌦".to_string(),
            "SPACE" => "␣".to_string(),
            "UP" => "↑".to_string(),
            "DOWN" => "↓".to_string(),
            "LEFT" => "←".to_string(),
            "RIGHT" => "→".to_string(),
            "PRIOR" => "PgUp".to_string(),
            "NEXT" => "PgDn".to_string(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Render one row of the Hotkeys panel: a chord-capture chip whose
/// label reflects `value` (or a "Press…" / "Click to set" prompt
/// depending on capture state). Returns `true` when the row was
/// clicked and should enter capture mode.
fn hotkey_chord_row(
    ui: &mut egui::Ui,
    target: HotkeyTarget,
    value: &str,
    capturing: Option<HotkeyTarget>,
) -> bool {
    let is_capturing_this = capturing == Some(target);
    let display = if is_capturing_this {
        "Press a shortcut…".to_string()
    } else if value.is_empty() {
        "Click to set".to_string()
    } else {
        chord_glyphs(value)
    };
    chord_chip(ui, &display, is_capturing_this).clicked()
}

/// Render the chord-capture chip — a wide, click-to-record button
/// that displays the current accelerator or a prompt while recording.
/// Uses the `shortcut` font family registered by
/// [`install_glyph_fonts`] so modifier glyphs (⌃ ⇧ ⌥ ⌘ / Omarchy
/// logo) render even when egui's default proportional font lacks
/// them.
fn chord_chip(ui: &mut egui::Ui, display: &str, capturing: bool) -> egui::Response {
    let chip_size = egui::vec2(280.0, 32.0);
    let resp = ui.allocate_response(chip_size, egui::Sense::click());
    let bg = if capturing {
        egui::Color32::from_rgb(50, 90, 140)
    } else if resp.hovered() {
        egui::Color32::from_gray(74)
    } else {
        egui::Color32::from_gray(56)
    };
    ui.painter()
        .rect_filled(resp.rect, egui::CornerRadius::same(6), bg);
    ui.painter().text(
        resp.rect.center(),
        egui::Align2::CENTER_CENTER,
        display,
        shortcut_font(14.0),
        egui::Color32::WHITE,
    );
    resp
}

fn shortcut_font(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Name("shortcut".into()))
}

/// Outcome of one frame of chord capture.
enum CaptureOutcome {
    /// User pressed Escape — exit capture without changing anything.
    Cancel,
    /// User pressed a non-modifier key — `String` is the accelerator
    /// (e.g. `"SUPER+CTRL+SHIFT+ALT+F"`).
    Commit(String),
}

/// Drain the input queue for one frame of chord capture. Eats any
/// key events so they don't reach other widgets.
fn capture_outcome(i: &mut egui::InputState) -> Option<CaptureOutcome> {
    // Esc with no modifiers cancels.
    let escaped = i.events.iter().any(|ev| {
        matches!(
            ev,
            egui::Event::Key {
                key: egui::Key::Escape,
                pressed: true,
                modifiers,
                ..
            } if !modifiers.shift && !modifiers.ctrl && !modifiers.alt
                && !modifiers.command && !modifiers.mac_cmd
        )
    });
    if escaped {
        i.events.retain(|ev| !matches!(ev, egui::Event::Key { .. }));
        return Some(CaptureOutcome::Cancel);
    }
    let result = i.events.iter().find_map(|ev| match ev {
        egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } => Some(format_accelerator(*key, *modifiers)),
        _ => None,
    });
    i.events.retain(|ev| !matches!(ev, egui::Event::Key { .. }));
    result.map(CaptureOutcome::Commit)
}

/// Format an egui (key, modifiers) pair as an UPPERCASE
/// `+`-separated accelerator that round-trips through
/// [`hyprcorrect_core::Chord::parse`].
fn format_accelerator(key: egui::Key, modifiers: egui::Modifiers) -> String {
    let mut parts: Vec<&'static str> = Vec::new();
    if modifiers.command || modifiers.mac_cmd {
        parts.push("SUPER");
    }
    if modifiers.ctrl {
        parts.push("CTRL");
    }
    if modifiers.shift {
        parts.push("SHIFT");
    }
    if modifiers.alt {
        parts.push("ALT");
    }
    let key_str = match key {
        egui::Key::Space => "space".to_string(),
        egui::Key::Enter => "Return".to_string(),
        egui::Key::Escape => "Escape".to_string(),
        egui::Key::Tab => "Tab".to_string(),
        egui::Key::Backspace => "BackSpace".to_string(),
        egui::Key::Delete => "Delete".to_string(),
        egui::Key::Insert => "Insert".to_string(),
        egui::Key::Home => "Home".to_string(),
        egui::Key::End => "End".to_string(),
        egui::Key::PageUp => "Prior".to_string(),
        egui::Key::PageDown => "Next".to_string(),
        egui::Key::ArrowUp => "Up".to_string(),
        egui::Key::ArrowDown => "Down".to_string(),
        egui::Key::ArrowLeft => "Left".to_string(),
        egui::Key::ArrowRight => "Right".to_string(),
        // Punctuation: spell out so the saved string can't collide
        // with the `+` modifier separator.
        egui::Key::Plus => "PLUS".to_string(),
        egui::Key::Minus => "MINUS".to_string(),
        egui::Key::Equals => "EQUAL".to_string(),
        egui::Key::Comma => "COMMA".to_string(),
        egui::Key::Period => "PERIOD".to_string(),
        egui::Key::Slash => "SLASH".to_string(),
        egui::Key::Backslash => "BACKSLASH".to_string(),
        egui::Key::Semicolon => "SEMICOLON".to_string(),
        other => other.name().to_uppercase(),
    };
    if parts.is_empty() {
        key_str
    } else {
        format!("{}+{}", parts.join("+"), key_str)
    }
}

/// Apply hyprcorrect's egui style — larger fonts and more generous
/// spacing than egui's defaults, mirroring `vernier`'s prefs window.
fn apply_style(ctx: &egui::Context) {
    use egui::FontFamily::Proportional;
    use egui::TextStyle::{Body, Button, Heading, Monospace, Small};
    ctx.style_mut(|style| {
        style.text_styles = [
            (Heading, egui::FontId::new(21.0, Proportional)),
            (Body, egui::FontId::new(14.0, Proportional)),
            (
                Monospace,
                egui::FontId::new(13.0, egui::FontFamily::Monospace),
            ),
            (Button, egui::FontId::new(14.0, Proportional)),
            (Small, egui::FontId::new(12.0, Proportional)),
        ]
        .into();
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 6.0);
        style.spacing.indent = 14.0;
        style.spacing.interact_size = egui::vec2(40.0, 28.0);
        style.spacing.icon_width = 18.0;
        style.spacing.icon_spacing = 6.0;
        style.visuals.widgets.inactive.expansion = 0.0;
    });
}

/// `true` when the daemon's on-disk binary has been replaced since the
/// daemon was started — i.e. the PID file (written at daemon startup)
/// is older than the executable file. Drives the
/// "Relaunch daemon (new build)" button's visibility.
fn daemon_is_stale() -> bool {
    let Ok(Some(_pid)) = runtime::read_daemon_pid() else {
        return false;
    };
    let pid_meta = match std::fs::metadata(runtime::pid_path()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return false,
    };
    let exe_meta = match std::fs::metadata(&exe) {
        Ok(m) => m,
        Err(_) => return false,
    };
    let pid_t = pid_meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let exe_t = exe_meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    exe_t > pid_t
}

/// SIGTERM the running daemon. The daemon's shutdown handler cleans
/// up its Hyprland bind, tray, and PID file.
fn quit_daemon() {
    let Ok(Some(pid)) = runtime::read_daemon_pid() else {
        return;
    };
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .output();
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}

/// Quit the running daemon and immediately spawn a fresh one from the
/// current executable's on-disk path. Best-effort: errors are logged
/// to stderr (the prefs window is on its way out anyway, or staying
/// open with the in-memory status banner already shown by the caller).
fn relaunch_daemon_now() {
    let was_running = matches!(runtime::read_daemon_pid(), Ok(Some(_)));
    if was_running {
        quit_daemon();
        // Wait up to ~1s for the daemon's PID file to disappear,
        // which is its last cleanup step before exit.
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            if matches!(runtime::read_daemon_pid(), Ok(None)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("hyprcorrect: cannot find own executable to relaunch: {e}");
            return;
        }
    };
    let result = std::process::Command::new(&exe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if let Err(e) = result {
        eprintln!("hyprcorrect: could not spawn fresh daemon: {e}");
    }
}

/// Enumerate currently-running windows' classes (Hyprland-specific
/// for now — calls `hyprctl clients -j` and parses out unique
/// `class` strings). Sorted alphabetically; case-insensitive dedup.
/// Returns an empty Vec on non-Hyprland systems or if hyprctl fails.
fn list_running_classes() -> Vec<String> {
    #[cfg(target_os = "linux")]
    {
        let Ok(output) = std::process::Command::new("hyprctl")
            .args(["clients", "-j"])
            .output()
        else {
            return Vec::new();
        };
        if !output.status.success() {
            return Vec::new();
        }
        let Ok(text) = std::str::from_utf8(&output.stdout) else {
            return Vec::new();
        };
        // Crude scan for `"class": "..."`. The full hyprctl JSON is
        // large, but every entry has exactly one top-level `class`
        // string per object — we can collect them without a JSON dep.
        let mut classes: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        let needle = "\"class\"";
        for chunk in text.split(needle).skip(1) {
            let after = chunk
                .split_once(':')
                .map(|p| p.1)
                .unwrap_or(chunk)
                .trim_start();
            let Some(rest) = after.strip_prefix('"') else {
                continue;
            };
            let Some((value, _)) = rest.split_once('"') else {
                continue;
            };
            if value.is_empty() {
                continue;
            }
            classes
                .entry(value.to_ascii_lowercase())
                .or_insert_with(|| value.to_string());
        }
        let mut out: Vec<String> = classes.into_values().collect();
        out.sort_by_key(|a| a.to_ascii_lowercase());
        out
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

fn validate(config: &Config) -> Result<(), String> {
    hyprcorrect_core::Chord::parse(&config.hotkeys.fix_word).map_err(|e| {
        format!("Fix-last-word chord is invalid ({e}). Click the chip and re-record it.")
    })?;
    if !config.hotkeys.fix_sentence.is_empty() {
        hyprcorrect_core::Chord::parse(&config.hotkeys.fix_sentence)
            .map_err(|e| format!("Fix-last-sentence chord is invalid ({e})."))?;
    }
    if !config.hotkeys.review.is_empty() {
        hyprcorrect_core::Chord::parse(&config.hotkeys.review)
            .map_err(|e| format!("Review chord is invalid ({e})."))?;
    }
    Ok(())
}

/// Send a Unix signal to the running daemon (if any).
///
/// Reads the daemon's PID from the runtime PID file and sends the
/// signal to that PID. Targeting by PID avoids the trap of
/// `pkill -x hyprcorrect` — the prefs subprocess shares the daemon's
/// binary name and would receive the signal too.
fn signal_daemon(signal: &str) {
    let pid = match runtime::read_daemon_pid() {
        Ok(Some(pid)) => pid,
        Ok(None) => return, // no daemon running
        Err(e) => {
            eprintln!("hyprcorrect: could not read daemon PID file: {e}");
            return;
        }
    };
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .args([signal, &pid.to_string()])
            .output();
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, signal); // Windows: M-later
    }
}

/// Ask the daemon to reload its config — re-installs the bind, applies
/// new blocklist + chord.
fn notify_daemon_reload() {
    signal_daemon("-HUP");
}

/// Ask the daemon to temporarily release its Hyprland keybind so the
/// prefs window can capture the chord's keypress. Reload restores it.
fn notify_daemon_release() {
    signal_daemon("-USR2");
}

/// Acquire the singleton lock and return a listener that holds it for
/// the life of the process. If another prefs window is already
/// running, best-effort ask it to focus and return `None`.
fn acquire_singleton() -> Option<UnixListener> {
    let path = singleton_path();
    if let Ok(listener) = UnixListener::bind(&path) {
        return Some(listener);
    }
    // The socket exists. If we can connect, prefs is already running.
    if UnixStream::connect(&path).is_ok() {
        focus_existing_prefs();
        return None;
    }
    // Stale socket file — remove and try again.
    let _ = std::fs::remove_file(&path);
    UnixListener::bind(&path).ok()
}

fn singleton_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("hyprcorrect-prefs.sock")
}

fn focus_existing_prefs() {
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("hyprctl")
            .args(["dispatch", "focuswindow", &format!("class:{APP_ID}")])
            .output();
    }
}

/// Run the prefs window. Acquires the singleton lock, loads config +
/// secrets, then runs eframe to completion.
pub(crate) fn run() {
    let Some(listener) = acquire_singleton() else {
        eprintln!("hyprcorrect: preferences are already open");
        return;
    };
    // The listener owns the socket file; keep it alive for the life
    // of this process. A separate thread holds it and exits when prefs
    // closes.
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();
    let listener_thread = std::thread::Builder::new()
        .name("hyprcorrect-prefs-lock".into())
        .spawn(move || {
            // We don't actually accept connections — the bind alone is
            // what makes the path appear "in use" to other instances.
            let _ = listener;
            let _ = shutdown_rx.recv();
        })
        .ok();

    let saved = Config::load().unwrap_or_else(|e| {
        eprintln!("hyprcorrect: could not load config ({e}) — using defaults");
        Config::default()
    });
    let saved_api_key = secrets::get(LLM_ANTHROPIC_KEY)
        .ok()
        .flatten()
        .unwrap_or_default();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id(APP_ID)
            .with_title("hyprcorrect — Preferences")
            .with_inner_size([640.0, 480.0])
            .with_min_inner_size([520.0, 360.0]),
        vsync: false, // matches vernier — better Wayland responsiveness
        ..Default::default()
    };
    let _ = eframe::run_native(
        "hyprcorrect — Preferences",
        options,
        Box::new(move |cc| {
            install_glyph_fonts(&cc.egui_ctx);
            Ok(Box::new(PrefsApp::new(saved, saved_api_key, shutdown_tx)))
        }),
    );

    // Best-effort cleanup of the socket file.
    let _ = std::fs::remove_file(singleton_path());
    if let Some(handle) = listener_thread {
        let _ = handle.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_well_formed_chords() {
        let mut cfg = Config::default();
        for good in ["F", "CTRL+F", "SUPER+CTRL+SHIFT+ALT+F1", "ALT+space"] {
            cfg.hotkeys.fix_word = good.into();
            assert!(validate(&cfg).is_ok(), "should accept {good:?}");
        }
    }

    #[test]
    fn validate_rejects_empty_or_garbage() {
        let mut cfg = Config::default();
        for bad in ["", "  ", "+", "FOO+F", "CTRL+"] {
            cfg.hotkeys.fix_word = bad.into();
            assert!(validate(&cfg).is_err(), "should reject {bad:?}");
        }
    }
}
