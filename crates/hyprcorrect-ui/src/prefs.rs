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
            .exact_width(160.0)
            .show(ctx, |ui| {
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    if let Some(handle) = &logo {
                        ui.add(egui::Image::new(handle).fit_to_exact_size(egui::vec2(28.0, 28.0)));
                        ui.add_space(8.0);
                    }
                    ui.heading("hyprcorrect");
                });
                ui.add_space(16.0);
                for section in Section::all() {
                    let selected = self.section == *section;
                    if ui.selectable_label(selected, section.label()).clicked() {
                        self.section = *section;
                        self.clear_status();
                    }
                }
            });

        let mut quit_requested = false;
        let mut relaunch_requested = false;

        egui::TopBottomPanel::bottom("actions")
            .resizable(false)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
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
                ui.add_space(6.0);
            });

        egui::CentralPanel::default().show(ctx, |ui| match self.section {
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
        ui.add_space(8.0);
        ui.label("The trigger chord is Super+Ctrl+Shift+Alt+ a letter.");
        ui.label("Pick the letter (single A–Z; case is ignored):");
        ui.add_space(4.0);

        let response = ui.text_edit_singleline(&mut self.config.hotkeys.trigger_letter);
        if response.changed() {
            self.clear_status();
        }
        ui.add_space(8.0);
        ui.small(
            "$HYPRCORRECT_TRIGGER overrides this for one-off dev runs. \
             The modifier set is fixed for now.",
        );
    }

    fn providers_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Providers");
        ui.add_space(8.0);

        let mut touched = false;

        ui.label("Default provider (fix-last-word):");
        touched |= provider_radio(ui, &mut self.config.providers.default);

        ui.add_space(12.0);
        ui.label("Smart provider (fix-last-sentence / review — M4):");
        touched |= provider_radio(ui, &mut self.config.providers.smart);

        ui.add_space(16.0);
        ui.separator();
        ui.add_space(8.0);
        ui.heading("LLM");
        ui.add_space(4.0);
        touched |= llm_section(ui, &mut self.config.providers.llm, &mut self.api_key_field);

        ui.add_space(16.0);
        ui.separator();
        ui.add_space(8.0);
        ui.heading("LanguageTool");
        ui.add_space(4.0);
        touched |= ui
            .checkbox(&mut self.config.providers.languagetool.enabled, "Enabled")
            .changed();
        ui.horizontal(|ui| {
            ui.label("URL:");
            touched |= ui
                .text_edit_singleline(&mut self.config.providers.languagetool.url)
                .changed();
        });

        if touched {
            self.clear_status();
        }
    }

    fn behavior_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Behavior");
        ui.add_space(8.0);
        ui.label("Inter-key delay applied to synthetic typing:");
        let response = ui.add(
            egui::Slider::new(&mut self.config.behavior.inter_key_delay_ms, 0..=50).suffix(" ms"),
        );
        if response.changed() {
            self.clear_status();
        }
        ui.add_space(8.0);
        ui.small(
            "0 ms is fastest but some apps drop characters under that speed; \
             2 ms is the safe default.",
        );
    }

    fn privacy_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Privacy");
        ui.add_space(8.0);
        ui.label("Windows whose class is on this list never have their keys buffered.");
        ui.small("Match is case-insensitive; use the class shown by `hyprctl activewindow`.");
        ui.add_space(8.0);

        let mut remove: Option<usize> = None;
        for (i, entry) in self.config.privacy.app_blocklist.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                if ui.text_edit_singleline(entry).changed() {
                    // Status cleared by setter below.
                }
                if ui.small_button("Remove").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            self.config.privacy.app_blocklist.remove(i);
            self.clear_status();
        }

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.text_edit_singleline(&mut self.blocklist_entry);
            if ui.button("Add").clicked() {
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
        ui.add_space(8.0);
        ui.label(format!("Version {}", hyprcorrect_core::version()));
        ui.add_space(4.0);
        ui.label("Keyboard-driven spelling and typo correction for the whole desktop.");
        ui.add_space(8.0);
        ui.label("Source: https://github.com/jondkinney/hyprcorrect");
        ui.label("License: MIT OR Apache-2.0");
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
    ui.horizontal(|ui| {
        ui.label("Backend:");
        changed |= ui.text_edit_singleline(&mut llm.backend).changed();
    });
    ui.horizontal(|ui| {
        ui.label("Model:");
        changed |= ui.text_edit_singleline(&mut llm.model).changed();
    });
    ui.horizontal(|ui| {
        ui.label("API key:");
        changed |= ui
            .add(egui::TextEdit::singleline(api_key).password(true))
            .changed();
    });
    ui.small("The API key is stored in your OS keychain, not in config.toml.");
    changed
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
