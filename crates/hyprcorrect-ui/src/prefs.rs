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

use crate::icon;

const APP_ID: &str = "hyprcorrect-prefs";
const LLM_ANTHROPIC_KEY: &str = "llm.anthropic";

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
        }
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

        // Materialize the logo handle before borrowing self.* for the
        // sidebar closure.
        let logo = self.logo_texture(ctx).cloned();

        egui::SidePanel::left("sections")
            .resizable(false)
            .default_width(200.0)
            .show_separator_line(false)
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
                    if ui.add(egui::Button::new(quit_label).frame(false)).clicked() {
                        quit_requested = true;
                    }
                    if self.daemon_stale {
                        let relaunch_label = egui::RichText::new("Relaunch daemon (new build)")
                            .color(egui::Color32::from_rgb(220, 160, 50));
                        let resp = ui
                            .add(egui::Button::new(relaunch_label).frame(false))
                            .on_hover_text(
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
                            .add_enabled(self.dirty(), egui::Button::new("Save").frame(false))
                            .clicked()
                        {
                            self.save();
                        }
                        if ui
                            .add_enabled(self.dirty(), egui::Button::new("Cancel").frame(false))
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
            .show(ctx, |ui| match self.section {
                Section::Hotkeys => self.hotkeys_panel(ui),
                Section::Providers => self.providers_panel(ui),
                Section::Behavior => self.behavior_panel(ui),
                Section::Privacy => self.privacy_panel(ui),
                Section::About => self.about_panel(ui),
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
    }
}

impl PrefsApp {
    fn hotkeys_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Hotkeys");
        ui.add_space(14.0);

        field_label(ui, "Trigger letter");
        ui.add_space(4.0);
        let response = ui.add(
            egui::TextEdit::singleline(&mut self.config.hotkeys.trigger_letter)
                .margin(egui::Margin::symmetric(8, 6))
                .desired_width(72.0),
        );
        if response.changed() {
            self.clear_status();
        }
        ui.add_space(6.0);
        caption(
            ui,
            "Pressed alongside Super+Ctrl+Shift+Alt to fix the last word. \
             Single A–Z; case is ignored. The modifier set is fixed for now.",
        );
        ui.add_space(4.0);
        caption(
            ui,
            "$HYPRCORRECT_TRIGGER overrides this for one-off dev runs.",
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
        ui.heading("Privacy");
        ui.add_space(14.0);

        field_label(ui, "App blocklist");
        caption(
            ui,
            "Windows whose class matches one of these never have their keys buffered. \
             Match is case-insensitive; use the class shown by `hyprctl activewindow`.",
        );
        ui.add_space(8.0);

        let mut remove: Option<usize> = None;
        let mut touched_any = false;
        for (i, entry) in self.config.privacy.app_blocklist.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                let resp = ui.add(
                    egui::TextEdit::singleline(entry)
                        .margin(egui::Margin::symmetric(8, 6))
                        .desired_width(ui.available_width() - 100.0),
                );
                if resp.changed() {
                    touched_any = true;
                }
                if ui.add(egui::Button::new("Remove").frame(false)).clicked() {
                    remove = Some(i);
                }
            });
            ui.add_space(4.0);
        }
        if let Some(i) = remove {
            self.config.privacy.app_blocklist.remove(i);
            touched_any = true;
        }
        if touched_any {
            self.clear_status();
        }

        ui.add_space(SETTING_BLOCK_SPACING);
        field_label(ui, "Add entry");
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
                if !entry.is_empty() {
                    self.config.privacy.app_blocklist.push(entry);
                    self.blocklist_entry.clear();
                    self.clear_status();
                }
            }
        });
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

fn validate(config: &Config) -> Result<(), String> {
    let letter = config.hotkeys.trigger_letter.trim();
    let mut chars = letter.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphabetic() => Ok(()),
        _ => Err("Trigger letter must be a single A–Z character".into()),
    }
}

/// Ask the running daemon (if any) to reload its config.
///
/// Reads the daemon's PID from the runtime PID file and sends it
/// `SIGHUP`. Targeting by PID avoids the trap of `pkill -x hyprcorrect`
/// — the prefs subprocess shares the daemon's binary name and would
/// receive the signal too, exiting immediately.
fn notify_daemon_reload() {
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
            .args(["-HUP", &pid.to_string()])
            .output();
    }
    #[cfg(not(unix))]
    {
        let _ = pid; // Windows: M-later
    }
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
        Box::new(move |_cc| Ok(Box::new(PrefsApp::new(saved, saved_api_key, shutdown_tx)))),
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
    fn validate_accepts_single_letter() {
        let mut cfg = Config::default();
        cfg.hotkeys.trigger_letter = "k".into();
        assert!(validate(&cfg).is_ok());
        cfg.hotkeys.trigger_letter = "J".into();
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn validate_rejects_empty_or_multichar_or_nonalpha() {
        let mut cfg = Config::default();
        for bad in ["", "ff", "1", " ", "F1", "ée"] {
            cfg.hotkeys.trigger_letter = bad.into();
            assert!(validate(&cfg).is_err(), "should reject {bad:?}");
        }
    }
}
