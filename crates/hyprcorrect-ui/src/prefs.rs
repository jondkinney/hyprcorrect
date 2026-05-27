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
#[cfg(target_os = "linux")]
use hyprcorrect_platform::linux::chord_capture::{self, ChordRecording, ClientError};

use crate::apps::AppRegistry;
#[cfg(target_os = "linux")]
use crate::autostart;
use crate::docker::{self, DockerState, LanguageToolStatus, OpHandle, OpKind, StatusHandle};
use crate::icon;

#[cfg(target_os = "linux")]
type ChordRecorder = ChordRecording;

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
    /// "Start at login" — true when a `~/.config/autostart/
    /// hyprcorrect.desktop` exists. Linux-only; on macOS the same
    /// box will eventually map to a LaunchAgent.
    #[cfg(target_os = "linux")]
    autostart_enabled: bool,
    /// Snapshot of `autostart_enabled` at load, for dirty-detection.
    #[cfg(target_os = "linux")]
    saved_autostart_enabled: bool,
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
    /// In-flight chord-capture IPC. egui-winit on Linux discards
    /// Super, so all chord recording goes through the daemon's
    /// evdev-based capture loop instead. `Some` while a recording
    /// is in flight; `None` otherwise. Linux-only — the daemon's
    /// chord-capture endpoint reads evdev; macOS will grow its own
    /// recorder when the M2 macOS platform work lands.
    #[cfg(target_os = "linux")]
    chord_recorder: Option<ChordRecorder>,
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
    /// Cached LanguageTool status — combined URL probe + docker
    /// inspection. Updated by the background [`StatusHandle`] worker;
    /// the UI just reads this. `None` until the first probe lands.
    lt_status: Option<LanguageToolStatus>,
    /// Last status refresh — re-probed every few seconds while the
    /// Providers panel is visible so a foreign container the user
    /// just started shows up.
    last_status_check: Instant,
    /// In-flight URL+docker probe.
    status_probe: Option<StatusHandle>,
    /// In-flight docker operation (install / start / stop / remove).
    /// `Some` while the background thread runs; cleared when the
    /// result is picked up and surfaced in [`Status`].
    docker_op: Option<OpHandle>,
}

impl PrefsApp {
    fn new(saved: Config, saved_api_key: String, shutdown_tx: Sender<()>) -> Self {
        #[cfg(target_os = "linux")]
        let autostart_enabled = autostart::is_enabled();
        Self {
            config: saved.clone(),
            saved,
            api_key_field: saved_api_key.clone(),
            saved_api_key,
            #[cfg(target_os = "linux")]
            autostart_enabled,
            #[cfg(target_os = "linux")]
            saved_autostart_enabled: autostart_enabled,
            section: Section::Hotkeys,
            status: Status::default(),
            blocklist_entry: String::new(),
            shutdown_tx: Some(shutdown_tx),
            logo: None,
            last_stale_check: Instant::now() - Duration::from_secs(60),
            daemon_stale: false,
            capturing_chord: None,
            #[cfg(target_os = "linux")]
            chord_recorder: None,
            running_apps: Vec::new(),
            last_apps_refresh: Instant::now() - Duration::from_secs(60),
            selected_app: None,
            app_filter: String::new(),
            app_registry: AppRegistry::discover(),
            lt_status: None,
            last_status_check: Instant::now() - Duration::from_secs(60),
            status_probe: None,
            docker_op: None,
        }
    }

    /// Kick off a background URL+docker probe if one isn't already
    /// running and enough time has passed since the last result.
    /// Cheap: we run the slow part (HTTP probe up to ~1.5 s + a few
    /// `docker` invocations) on a worker thread.
    fn refresh_lt_status(&mut self, ctx: &egui::Context) {
        if let Some(handle) = &self.status_probe
            && let Some(status) = handle.poll()
        {
            self.lt_status = Some(status);
            self.status_probe = None;
            ctx.request_repaint();
        }
        if self.status_probe.is_some() {
            // Still in flight; re-poll shortly without spawning
            // another worker.
            ctx.request_repaint_after(Duration::from_millis(200));
            return;
        }
        if self.docker_op.is_some() {
            // Docker op will trigger its own immediate refresh on
            // completion — no need to probe in parallel.
            return;
        }
        if self.last_status_check.elapsed() < Duration::from_secs(5) {
            return;
        }
        self.last_status_check = Instant::now();
        let url = self.config.providers.languagetool.url.clone();
        self.status_probe = Some(docker::spawn_status_probe(url));
        ctx.request_repaint_after(Duration::from_millis(200));
    }

    fn poll_docker_op(&mut self, ctx: &egui::Context) {
        let Some(handle) = &self.docker_op else {
            return;
        };
        if let Some(result) = handle.poll() {
            let kind = handle.kind();
            self.docker_op = None;
            // Force the next status probe to fire on the next frame so
            // the UI reflects the post-op state without a 5 s wait.
            self.last_status_check = Instant::now() - Duration::from_secs(60);
            match result {
                Ok(()) => {
                    let msg = match kind {
                        OpKind::Install => {
                            // First install: turn the provider on so the
                            // user doesn't have to remember the second
                            // step.
                            self.config.providers.languagetool.enabled = true;
                            "LanguageTool installed and started."
                        }
                        OpKind::Start => "LanguageTool started.",
                        OpKind::Stop => "LanguageTool stopped.",
                        OpKind::Remove => "LanguageTool container removed.",
                    };
                    self.ok(msg);
                }
                Err(e) => {
                    let verb = match kind {
                        OpKind::Install => "install",
                        OpKind::Start => "start",
                        OpKind::Stop => "stop",
                        OpKind::Remove => "remove",
                    };
                    self.err(format!("Docker {verb} failed: {e}"));
                }
            }
        } else {
            ctx.request_repaint_after(Duration::from_millis(500));
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
        #[cfg(target_os = "linux")]
        let autostart_changed = self.autostart_enabled != self.saved_autostart_enabled;
        #[cfg(not(target_os = "linux"))]
        let autostart_changed = false;
        self.config != self.saved || self.api_key_field != self.saved_api_key || autostart_changed
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
        #[cfg(target_os = "linux")]
        if self.autostart_enabled != self.saved_autostart_enabled {
            let result = if self.autostart_enabled {
                std::env::current_exe()
                    .map_err(|e| std::io::Error::other(format!("current_exe: {e}")))
                    .and_then(|exe| autostart::enable(&exe.to_string_lossy()))
            } else {
                autostart::disable()
            };
            if let Err(e) = result {
                self.err(format!("autostart write failed: {e}"));
                return;
            }
            self.saved_autostart_enabled = self.autostart_enabled;
        }
        self.saved = self.config.clone();
        notify_daemon_reload();
        self.ok("Saved.");
    }

    fn cancel(&mut self) {
        self.config = self.saved.clone();
        self.api_key_field = self.saved_api_key.clone();
        #[cfg(target_os = "linux")]
        {
            self.autostart_enabled = self.saved_autostart_enabled;
        }
        self.clear_status();
    }
}

impl eframe::App for PrefsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        apply_style(ctx);
        self.refresh_stale_check();
        self.poll_docker_op(ctx);
        if self.section == Section::Providers {
            self.refresh_lt_status(ctx);
        }

        // Chord recording happens through the daemon (see
        // `hyprcorrect-platform/src/linux/chord_capture.rs`): egui
        // on Linux discards Super out of its `Modifiers`, so we
        // can't honestly record SUPER-containing chords here.
        // Open the IPC the first frame after a row is clicked,
        // then poll non-blockingly until the user releases a key.
        // macOS gets its own recorder when the M2 platform work
        // lands; until then chord rows just stay in "capture mode"
        // until cancelled via Save / Cancel / Revert.
        #[cfg(target_os = "linux")]
        if let Some(target) = self.capturing_chord {
            if self.chord_recorder.is_none() {
                match chord_capture::record_chord() {
                    Ok(rec) => self.chord_recorder = Some(rec),
                    Err(e) => {
                        self.capturing_chord = None;
                        self.err(chord_record_error(&e));
                        notify_daemon_reload();
                    }
                }
            }
            // Esc cancels — read it from egui's input queue and
            // shutdown the IPC so the daemon's slot also clears.
            let esc_pressed = ctx.input(|i| i.key_pressed(egui::Key::Escape));
            if esc_pressed && let Some(rec) = &self.chord_recorder {
                rec.abort();
            }
            if let Some(rec) = &self.chord_recorder {
                match rec.try_recv() {
                    Ok(None) => {
                        // Still waiting — request a repaint soon so
                        // the next try_recv lands quickly when the
                        // user does press a key.
                        ctx.request_repaint_after(Duration::from_millis(50));
                    }
                    Ok(Some(chord)) => {
                        match target {
                            HotkeyTarget::FixWord => self.config.hotkeys.fix_word = chord,
                            HotkeyTarget::FixSentence => self.config.hotkeys.fix_sentence = chord,
                            HotkeyTarget::Review => self.config.hotkeys.review = chord,
                        }
                        self.capturing_chord = None;
                        self.chord_recorder = None;
                        self.clear_status();
                        notify_daemon_reload();
                    }
                    Err(ClientError::Cancelled) => {
                        self.capturing_chord = None;
                        self.chord_recorder = None;
                        notify_daemon_reload();
                    }
                    Err(e) => {
                        self.capturing_chord = None;
                        self.chord_recorder = None;
                        self.err(chord_record_error(&e));
                        notify_daemon_reload();
                    }
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
                        // The icon's SVG is cropped tight (so it
                        // fills the tray slot), which makes its
                        // content reach the very top of a flush
                        // 28×28 widget — visually higher than the
                        // heading's baseline. Allocate a slightly
                        // taller rect and paint the image into the
                        // lower 28 px so it lines up with the
                        // "hyprcorrect" cap height.
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(28.0, 34.0), egui::Sense::hover());
                        let icon_rect = egui::Rect::from_min_size(
                            rect.left_top() + egui::vec2(0.0, 6.0),
                            egui::vec2(28.0, 28.0),
                        );
                        egui::Image::new(handle).paint_at(ui, icon_rect);
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
        caption(ui, "Used for fix-last-word.");
        ui.add_space(4.0);
        touched |= provider_radio(
            ui,
            &mut self.config.providers.default,
            Some(LLM_DEFAULT_TOOLTIP),
        );

        ui.add_space(SETTING_BLOCK_SPACING);
        field_label(ui, "Smart provider");
        caption(ui, "Used for fix-last-sentence and the review popup.");
        ui.add_space(4.0);
        touched |= provider_radio(ui, &mut self.config.providers.smart, None);

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

        ui.add_space(SETTING_BLOCK_SPACING);
        self.languagetool_docker_row(ui);

        if touched {
            self.clear_status();
        }
    }

    /// One-click LanguageTool-in-Docker row under the LanguageTool
    /// section. See `crate::docker` for the rationale — provider
    /// integration is still URL-based, this is a UX convenience for
    /// users who'd otherwise have to memorize a `docker run` invocation.
    fn languagetool_docker_row(&mut self, ui: &mut egui::Ui) {
        field_label(ui, "Local server (Docker)");
        ui.add_space(4.0);

        let url = self.config.providers.languagetool.url.clone();
        let op_in_flight = self.docker_op.is_some();
        let probe_in_flight = self.status_probe.is_some() && self.lt_status.is_none();

        let Some(status) = self.lt_status.clone() else {
            // First-ever probe still in flight — show a neutral
            // "checking…" message instead of flashing a wrong state.
            if probe_in_flight {
                ui.colored_label(
                    egui::Color32::from_gray(170),
                    "Checking for a running LanguageTool server…",
                );
            }
            return;
        };

        match status {
            LanguageToolStatus::Reachable {
                managed_container_running,
            } => {
                ui.colored_label(
                    egui::Color32::from_rgb(110, 200, 130),
                    format!("Reachable at {url}"),
                );
                ui.add_space(8.0);
                if managed_container_running {
                    // This is our container — give the user the same
                    // Stop / Remove controls they had before.
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(!op_in_flight, egui::Button::new("Stop"))
                            .on_hover_text(format!(
                                "docker stop {}\nLeaves the container in place; \
                                 Start brings it back.",
                                docker::CONTAINER
                            ))
                            .clicked()
                        {
                            self.docker_op = Some(docker::stop());
                            self.ok(OpKind::Stop.label());
                        }
                        if ui
                            .add_enabled(!op_in_flight, egui::Button::new("Remove").frame(false))
                            .on_hover_text("Stop and delete the container. The image stays cached.")
                            .clicked()
                        {
                            self.docker_op = Some(docker::remove());
                            self.ok(OpKind::Remove.label());
                        }
                    });
                    ui.add_space(4.0);
                    caption(
                        ui,
                        "Running in the hyprcorrect-managed container. Nothing else \
                         to do.",
                    );
                } else {
                    ui.add_space(4.0);
                    caption(
                        ui,
                        "Detected an existing LanguageTool server — hyprcorrect will \
                         use it as-is. No Docker setup needed.",
                    );
                }
            }
            LanguageToolStatus::Unreachable(docker_state) => {
                self.docker_unreachable_row(ui, &docker_state, &url, op_in_flight);
            }
        }
    }

    /// The "URL didn't answer" branch — drives the install / start UI.
    /// Split out so the parent function isn't a five-screen match.
    fn docker_unreachable_row(
        &mut self,
        ui: &mut egui::Ui,
        state: &DockerState,
        url: &str,
        op_in_flight: bool,
    ) {
        let (status_text, status_color) = match state {
            DockerState::NotInstalled => (
                format!(
                    "Nothing answers at {url}, and Docker isn't installed — \
                     install Docker or point the URL at an existing server."
                ),
                egui::Color32::from_gray(170),
            ),
            DockerState::DockerUnavailable(msg) => (
                format!("Docker unavailable: {msg}"),
                egui::Color32::from_rgb(220, 160, 50),
            ),
            DockerState::AbsentContainer => {
                ("Not installed.".to_string(), egui::Color32::from_gray(170))
            }
            DockerState::ContainerStopped => (
                format!("Our container exists but is stopped. Start it to reach {url}."),
                egui::Color32::from_rgb(220, 160, 50),
            ),
            DockerState::ContainerRunning => (
                format!(
                    "Our container is running but {url} doesn't answer — \
                     likely a port-mapping mismatch."
                ),
                egui::Color32::from_rgb(220, 160, 50),
            ),
            DockerState::ForeignContainer { name, running } => (
                if *running {
                    format!(
                        "Found another LanguageTool container ({name}) running, but \
                         it doesn't answer at {url}. Update the URL to match its \
                         port, or stop it and install ours."
                    )
                } else {
                    format!(
                        "Found another LanguageTool container ({name}), stopped. \
                         Start it manually (`docker start {name}`) or install ours."
                    )
                },
                egui::Color32::from_rgb(220, 160, 50),
            ),
        };
        ui.colored_label(status_color, status_text);
        ui.add_space(8.0);

        ui.horizontal(|ui| match state {
            DockerState::NotInstalled => {
                let _ = ui
                    .add_enabled(false, egui::Button::new("Install with Docker"))
                    .on_disabled_hover_text(
                        "Install Docker first: https://docs.docker.com/engine/install/",
                    );
            }
            DockerState::DockerUnavailable(_) => {
                if ui
                    .add_enabled(!op_in_flight, egui::Button::new("Retry"))
                    .on_hover_text("Recheck whether the Docker daemon is reachable.")
                    .clicked()
                {
                    self.last_status_check = Instant::now() - Duration::from_secs(60);
                }
            }
            DockerState::AbsentContainer | DockerState::ForeignContainer { .. } => {
                let port = docker::host_port_from_url(url);
                let enabled = !op_in_flight && port.is_some();
                let hover = match port {
                    Some(p) => format!(
                        "Runs:\n  docker run -d --name {} --restart=unless-stopped \\\
                         \n      -p {}:8010 {}\nFirst run downloads ~600 MB.",
                        docker::CONTAINER,
                        p,
                        docker::IMAGE,
                    ),
                    None => "URL needs an explicit port (e.g. http://localhost:8081) before \
                             hyprcorrect can map it to the container."
                        .to_string(),
                };
                if ui
                    .add_enabled(enabled, egui::Button::new("Install with Docker"))
                    .on_hover_text(hover)
                    .clicked()
                    && let Some(port) = port
                {
                    self.docker_op = Some(docker::install(port));
                    self.ok(OpKind::Install.label());
                }
            }
            DockerState::ContainerStopped => {
                if ui
                    .add_enabled(!op_in_flight, egui::Button::new("Start"))
                    .clicked()
                {
                    self.docker_op = Some(docker::start());
                    self.ok(OpKind::Start.label());
                }
                if ui
                    .add_enabled(!op_in_flight, egui::Button::new("Remove").frame(false))
                    .on_hover_text("Delete the container. The image stays cached locally.")
                    .clicked()
                {
                    self.docker_op = Some(docker::remove());
                    self.ok(OpKind::Remove.label());
                }
            }
            DockerState::ContainerRunning => {
                // Misconfiguration path: container up but URL wrong.
                // We can stop our container in case the user wants to
                // rebind, but we can't fix the URL for them.
                if ui
                    .add_enabled(!op_in_flight, egui::Button::new("Stop"))
                    .on_hover_text(
                        "Stop the container so you can adjust the URL or re-install \
                         with a matching port.",
                    )
                    .clicked()
                {
                    self.docker_op = Some(docker::stop());
                    self.ok(OpKind::Stop.label());
                }
                if ui
                    .add_enabled(!op_in_flight, egui::Button::new("Remove").frame(false))
                    .clicked()
                {
                    self.docker_op = Some(docker::remove());
                    self.ok(OpKind::Remove.label());
                }
            }
        });
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            // Tighten inline spacing so "Pulls the <link> image" reads as
            // one sentence rather than three widgets with default gaps.
            ui.spacing_mut().item_spacing.x = 0.0;
            let muted = egui::Color32::from_gray(170);
            ui.label(
                egui::RichText::new("Pulls the ")
                    .size(CAPTION_SIZE)
                    .line_height(Some(CAPTION_LINE_HEIGHT))
                    .color(muted),
            );
            ui.hyperlink_to(
                egui::RichText::new("erikvl87/languagetool")
                    .size(CAPTION_SIZE)
                    .line_height(Some(CAPTION_LINE_HEIGHT)),
                "https://hub.docker.com/r/erikvl87/languagetool",
            );
            ui.label(
                egui::RichText::new(
                    " image and runs it locally, mapped to the port in your \
                     URL above. Use this if you don't already self-host \
                     LanguageTool elsewhere.",
                )
                .size(CAPTION_SIZE)
                .line_height(Some(CAPTION_LINE_HEIGHT))
                .color(muted),
            );
        });
    }

    fn behavior_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Behavior");
        ui.add_space(14.0);

        #[cfg(target_os = "linux")]
        {
            field_label(ui, "Start at login");
            ui.add_space(4.0);
            let resp = ui.checkbox(
                &mut self.autostart_enabled,
                "Launch hyprcorrect when I log in",
            );
            if resp.changed() {
                self.clear_status();
            }
            ui.add_space(6.0);
            caption(
                ui,
                "Drops a `hyprcorrect.desktop` into `~/.config/\
                 autostart/` so the daemon starts with your session. \
                 Takes effect on save.",
            );
            ui.add_space(SETTING_BLOCK_SPACING);
        }

        field_label(ui, "Pause per backspace");
        caption(
            ui,
            "After hyprcorrect dispatches the backspaces, it waits \
             this long per backspace before typing the replacement. \
             That pause gives the focused app time to actually apply \
             the deletes through its own event loop — without it, \
             the typing burst can race ahead and leave a prefix of \
             the original on screen. Raise it if you see leftover \
             characters after a fix lands.",
        );
        ui.add_space(6.0);
        let response = ui.add(
            egui::Slider::new(&mut self.config.behavior.pause_per_backspace_ms, 0..=30)
                .suffix(" ms"),
        );
        if response.changed() {
            self.clear_status();
        }
        ui.add_space(6.0);
        caption(
            ui,
            "8 ms is the default and works for most apps. Raise to \
             12–15 ms for slow apps like LibreOffice Writer that \
             need longer to drain a big backspace burst. Lower to \
             4 ms if your apps keep up cleanly — corrections will \
             feel snappier.",
        );
        ui.add_space(SETTING_BLOCK_SPACING);

        field_label(ui, "Buffer reset keys");
        caption(
            ui,
            "When you press one of these keys, hyprcorrect clears the \
             per-window typing buffer — necessary for keys that \
             change the typing context (Enter submits, arrows \
             scroll history, Delete edits text the daemon can't \
             see). Disable a key to let the buffer survive across \
             it, so a follow-up fix-word can still operate on \
             already-typed text. Tab and Esc are off by default \
             because they rarely change content.",
        );
        ui.add_space(8.0);
        let mut any_changed = false;
        let rk = &mut self.config.behavior.reset_keys;
        for (label, slot) in [
            ("Enter / Return", &mut rk.enter),
            ("Tab", &mut rk.tab),
            ("Escape", &mut rk.escape),
            ("Up arrow", &mut rk.up),
            ("Down arrow", &mut rk.down),
            ("Page Up", &mut rk.page_up),
            ("Page Down", &mut rk.page_down),
            ("Delete (forward)", &mut rk.delete),
            ("Insert", &mut rk.insert),
        ] {
            if ui.checkbox(slot, label).changed() {
                any_changed = true;
            }
        }
        if any_changed {
            self.clear_status();
        }
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
        ui.hyperlink("https://github.com/jondkinney/hyprcorrect");
        ui.add_space(SETTING_BLOCK_SPACING);

        field_label(ui, "License");
        caption(ui, "MIT OR Apache-2.0");
    }
}

/// Tooltip shown next to the LLM radio in the *Default provider*
/// section. The smart provider sends the whole sentence anyway,
/// so the surprises this tooltip warns about (every chord =
/// outbound API call, sentence context attached) aren't news
/// there — only the default-provider users need this.
const LLM_DEFAULT_TOOLTIP: &str = "\
Each fix-word chord sends the sentence around the caret plus the \
word at the caret to your configured LLM (default: Anthropic \
Claude). The LLM returns only the corrected word; sentence \
context lets it disambiguate homophones like their/there.

If the picked word looks fine, hyprcorrect tries up to 4 nearby \
words in the same buffer — covers held-arrow caret drift and \
the click-then-trigger case. On any LLM failure (no key, \
timeout, network) we fall back to the offline Spellbook so the \
chord never silently no-ops.

Privacy: your typed text leaves your machine on every chord. \
Pick Spellbook if that's a concern.";

/// Render a provider-id radio group; returns `true` if the user
/// changed the selection in this frame. When `llm_tooltip` is
/// `Some`, an info icon next to the LLM radio surfaces that
/// text on hover — only the Default-provider variant uses this.
fn provider_radio(
    ui: &mut egui::Ui,
    selection: &mut ProviderId,
    llm_tooltip: Option<&str>,
) -> bool {
    let before = *selection;
    // Order: simplest → most complex, and offline → potentially-online.
    // Spellbook is always offline (bundled dictionary). LanguageTool is
    // offline when self-hosted at localhost; the URL field next door
    // is where its locality is configured. LLM is always a network call.
    ui.horizontal(|ui| {
        ui.radio_value(selection, ProviderId::Spellbook, "Spellbook (offline)");
        ui.radio_value(selection, ProviderId::LanguageTool, "LanguageTool");
        ui.radio_value(selection, ProviderId::Llm, "LLM");
        if let Some(tip) = llm_tooltip {
            info_icon(ui).on_hover_text(tip);
        }
    });
    *selection != before
}

/// Paint a small circle-with-`i` info icon at the current cursor
/// in `ui`, sized to the row height. Drawn with the egui painter
/// directly so we don't have to bundle an icon font or SVG just
/// for one glyph — the bundled Adwaita Sans doesn't include the
/// Unicode ⓘ codepoint and falls back to a tofu box otherwise.
/// Caller chains `.on_hover_text(...)` on the returned response
/// to attach a tooltip.
fn info_icon(ui: &mut egui::Ui) -> egui::Response {
    // Size the icon to the body text height so it sits flush
    // with the "LLM" label next to it, not the larger radio
    // hit-box.
    let font_size = egui::TextStyle::Body.resolve(ui.style()).size;
    let size = font_size;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let visuals = ui.visuals();
    let stroke_color = if response.hovered() {
        visuals.strong_text_color()
    } else {
        visuals.weak_text_color()
    };
    let painter = ui.painter();
    let center = rect.center();
    let radius = (size * 0.5) - 1.0;
    painter.circle_stroke(center, radius, egui::Stroke::new(1.0, stroke_color));
    // The "i" — drawn slightly above center because the glyph
    // baseline sits low in most fonts.
    painter.text(
        center + egui::vec2(0.0, -0.5),
        egui::Align2::CENTER_CENTER,
        "i",
        egui::FontId::proportional(size * 0.75),
        stroke_color,
    );
    response
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
    ui.label(egui::RichText::new(text).strong().size(15.0));
}

/// Muted explainer text under inputs or checkboxes. Sized for
/// comfortable wrapped reading — larger than egui's default body
/// with extra line-height so multi-line captions don't feel cramped.
const CAPTION_SIZE: f32 = 13.5;
const CAPTION_LINE_HEIGHT: f32 = 20.0;

fn caption(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .size(CAPTION_SIZE)
            .line_height(Some(CAPTION_LINE_HEIGHT))
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

    // -- Highest priority: the bundled sans with the modifier glyphs --
    // Adwaita Sans Regular ships with the app under `crates/
    // hyprcorrect-ui/assets/`. It covers ASCII plus the macOS-style
    // key glyphs (⌃ ⇧ ⌥ ⌘ ⎋ ↵ ⇥ ⌫ ⌦ ␣ ↑↓←→), so the chip renders
    // identically on any system without depending on whatever
    // fonts happen to be installed.
    const ADWAITA_SANS: &[u8] = include_bytes!("../assets/AdwaitaSans-Regular.ttf");
    fonts.font_data.insert(
        "shortcut_symbols".into(),
        Arc::new(egui::FontData::from_static(ADWAITA_SANS)),
    );
    shortcut_chain.push("shortcut_symbols".into());

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
            let mut data = egui::FontData::from_owned(bytes);
            // The omarchy glyph fills its full em square; at scale=1
            // it towers over the Adwaita letters / modifier symbols
            // at the same point size. 0.85 lands the logo at the
            // letters' visual cap height; positive y_offset_factor
            // nudges it DOWN so it sits on the same baseline as the F.
            // Mirrors vernier's tweak.
            data.tweak = egui::FontTweak {
                scale: 0.75,
                y_offset_factor: 0.09,
                ..Default::default()
            };
            fonts.font_data.insert("omarchy".into(), Arc::new(data));
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
            // Punctuation that chord-capture spells out so the
            // saved string can't collide with the `+` modifier
            // separator. Render them as the ASCII characters
            // they represent.
            "PLUS" => "+".to_string(),
            "MINUS" => "-".to_string(),
            "EQUAL" => "=".to_string(),
            "UNDERSCORE" => "_".to_string(),
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
        shortcut_font(17.0),
        egui::Color32::WHITE,
    );
    resp
}

fn shortcut_font(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Name("shortcut".into()))
}

/// Build a user-facing message for a chord-capture IPC failure.
/// Translates the rare-but-possible "daemon not running" case into
/// a clear hint instead of a raw error string.
#[cfg(target_os = "linux")]
fn chord_record_error(err: &ClientError) -> String {
    match err {
        ClientError::DaemonOffline => {
            "Daemon not running — start hyprcorrect, then try recording again.".to_string()
        }
        ClientError::Cancelled => "Recording cancelled.".to_string(),
        ClientError::Daemon(msg) => format!("Daemon error: {msg}"),
        ClientError::Io(msg) => format!("Chord-capture IPC failed: {msg}"),
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

/// Linux: if the daemon's chord-capture socket isn't responding,
/// spawn the daemon detached. The daemon's own singleton check
/// (in `run_daemon`) prevents two daemons from racing; we just
/// want one to be alive so prefs hotkey-record IPC works.
#[cfg(target_os = "linux")]
fn ensure_daemon_running() {
    use hyprcorrect_core::runtime::chord_socket_path;
    if std::os::unix::net::UnixStream::connect(chord_socket_path()).is_ok() {
        return; // daemon up
    }
    // Use `/proc/self/exe` first so a `cargo build`-replaced
    // binary still works (matches the review-popup spawn fix).
    let exe = if std::path::PathBuf::from("/proc/self/exe").exists() {
        std::path::PathBuf::from("/proc/self/exe")
    } else if let Ok(p) = std::env::current_exe() {
        p
    } else {
        eprintln!("hyprcorrect: can't find own executable to spawn daemon");
        return;
    };
    let result = std::process::Command::new(&exe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if let Err(e) = result {
        eprintln!("hyprcorrect: failed to spawn daemon: {e}");
    }
}

/// Run the prefs window. Acquires the singleton lock, loads config +
/// secrets, then runs eframe to completion.
pub(crate) fn run() {
    let Some(listener) = acquire_singleton() else {
        eprintln!("hyprcorrect: preferences are already open");
        return;
    };
    // Best-effort: if the daemon isn't already running, spawn it
    // detached so opening prefs from a launcher (walker / menus /
    // the AUR-installed `.desktop`) brings up a fully functional
    // app rather than just the prefs window with dead chords.
    // Matches vernier's `run_prefs_window` behavior.
    #[cfg(target_os = "linux")]
    ensure_daemon_running();
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
