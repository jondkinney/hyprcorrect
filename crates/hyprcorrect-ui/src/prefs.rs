//! The egui preferences window.
//!
//! A single window with a left sidebar (sections) and a right pane
//! (the focused section). On Save the config is written to disk and
//! the running daemon is signalled (`SIGHUP` on Linux, no-op for now
//! on other OSes) so it picks up the change without restart. Secrets
//! (LLM API keys) live in the OS keychain — never in config.toml.

use std::collections::BTreeMap;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
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

/// Which hotkey row's chord is being recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HotkeyTarget {
    FixWord,
    FixSentence,
    Review,
    ReviewLlm,
}

/// Which LLM-provider tab is selected in the Providers panel. Existing
/// providers are keyed by their backend (the unique tab identity, stable
/// across reorders); `Add` is the "+ Add Provider" tab.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LlmTab {
    Provider(String),
    Add,
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

    /// Map a `$HYPRCORRECT_PREFS_SECTION` value (case-insensitive) to a
    /// section, so the daemon can open prefs straight to e.g. Providers.
    fn from_name(name: &str) -> Option<Self> {
        Self::all()
            .iter()
            .copied()
            .find(|s| s.label().eq_ignore_ascii_case(name.trim()))
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
    /// Per-backend LLM API keys being edited (backend → key field). One
    /// entry per configured provider; the daemon reads each from the OS
    /// keychain at `llm.<backend>`.
    llm_keys: BTreeMap<String, String>,
    /// Snapshot of `llm_keys` as it sits in the keychain — gates Save and
    /// drives which entries get written on Save.
    saved_llm_keys: BTreeMap<String, String>,
    /// Which provider tab is selected in the Providers panel.
    llm_tab: LlmTab,
    /// Draft provider being filled in on the "+ Add Provider" tab.
    llm_draft: LlmConfig,
    /// Draft API key for the provider being added.
    llm_draft_key: String,
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
    /// Whether the managed container has the n-gram dataset mounted —
    /// tracked separately from its run state. `None` = no managed
    /// container (or not probed yet).
    lt_ngrams: Option<bool>,
    /// In-flight n-gram archive download (~8.4 GB), or `None`.
    ngram_download: Option<crate::ngrams::DownloadHandle>,
    /// In-flight native folder-picker for the n-gram folder field. The
    /// picker runs on a worker thread so the dialog doesn't freeze the UI;
    /// the chosen path (or `None` if cancelled) arrives on this channel.
    folder_pick: Option<Receiver<Option<String>>>,
    /// Whether a folder-picker tool (zenity/kdialog) is on PATH — gates the
    /// Browse button.
    folder_picker_available: bool,
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
    fn new(
        saved: Config,
        saved_llm_keys: BTreeMap<String, String>,
        shutdown_tx: Sender<()>,
        section: Section,
    ) -> Self {
        #[cfg(target_os = "linux")]
        let autostart_enabled = autostart::is_enabled();
        // Open on the active (first) provider's tab, or the Add tab when
        // none are configured.
        let llm_tab = saved
            .providers
            .llms
            .first()
            .map(|c| LlmTab::Provider(c.backend.clone()))
            .unwrap_or(LlmTab::Add);
        Self {
            config: saved.clone(),
            saved,
            llm_keys: saved_llm_keys.clone(),
            saved_llm_keys,
            llm_tab,
            llm_draft: LlmConfig {
                backend: String::new(),
                model: String::new(),
                base_url: None,
            },
            llm_draft_key: String::new(),
            #[cfg(target_os = "linux")]
            autostart_enabled,
            #[cfg(target_os = "linux")]
            saved_autostart_enabled: autostart_enabled,
            section,
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
            lt_ngrams: None,
            ngram_download: None,
            folder_pick: None,
            folder_picker_available: tool_in_path("zenity") || tool_in_path("kdialog"),
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
            && let Some(result) = handle.poll()
        {
            self.lt_status = Some(result.status);
            self.lt_ngrams = result.ngrams;
            // Heal a forgotten n-gram folder: the container is serving
            // n-grams but the config never recorded the path (enabled before
            // we persisted it, or by an older build). Recover it from the
            // mount so the field shows it again and it sticks on disk.
            let unrecorded = self
                .config
                .providers
                .languagetool
                .ngram_dir
                .as_deref()
                .unwrap_or("")
                .trim()
                .is_empty();
            if result.ngrams == Some(true)
                && unrecorded
                && let Some(mount) = result.ngram_mount.filter(|m| !m.trim().is_empty())
            {
                self.config.providers.languagetool.ngram_dir = Some(mount);
                let _ = self.persist_languagetool();
            }
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

    /// Advance an in-flight n-gram download: keep repainting for the
    /// progress bar, and on completion point LanguageTool at the data and
    /// recreate the container with it mounted.
    fn poll_ngram_download(&mut self, ctx: &egui::Context) {
        use crate::ngrams::DownloadPhase;
        let phase = match &self.ngram_download {
            Some(h) => h.phase(),
            None => return,
        };
        match phase {
            DownloadPhase::Downloading { .. } | DownloadPhase::Extracting => {
                ctx.request_repaint_after(Duration::from_millis(200));
            }
            DownloadPhase::Done(dir) => {
                self.ngram_download = None;
                let dir_str = dir.to_string_lossy().to_string();
                self.config.providers.languagetool.ngram_dir = Some(dir_str.clone());
                let url = self.config.providers.languagetool.url.clone();
                if let Some(port) = docker::host_port_from_url(&url) {
                    self.docker_op = Some(docker::enable_ngrams(port, &dir_str));
                    self.ok("n-grams downloaded — enabling (recreating container)…");
                } else {
                    self.ok("n-grams downloaded. Set a URL with a port, then Enable n-grams.");
                }
                // Re-probe so the n-gram status flips to Loaded once done.
                self.last_status_check = Instant::now() - Duration::from_secs(60);
            }
            DownloadPhase::Failed(msg) => {
                self.ngram_download = None;
                self.err(format!("n-gram download failed: {msg}"));
            }
            DownloadPhase::Cancelled => {
                self.ngram_download = None;
                self.ok("n-gram download cancelled.");
            }
        }
    }

    /// Pick up the result of a background folder-picker (Browse button): on
    /// a chosen path, drop it into the n-gram folder field.
    fn poll_folder_pick(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.folder_pick else {
            return;
        };
        match rx.try_recv() {
            Ok(result) => {
                self.folder_pick = None;
                if let Some(path) = result {
                    self.config.providers.languagetool.ngram_dir = Some(path);
                    self.clear_status();
                }
                ctx.request_repaint();
            }
            Err(mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(150));
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.folder_pick = None;
            }
        }
    }

    /// Mirror the LanguageTool config to disk after a Docker op that
    /// changed persistent container state. The container survives a restart,
    /// so its config must too — otherwise a reopened prefs (and any later
    /// "Save") forgets the n-gram folder the user just enabled, even while
    /// the container keeps serving from it. Read-modify-write only the
    /// LanguageTool section so unsaved edits in other panels stay pending.
    fn persist_languagetool(&mut self) -> Result<(), String> {
        let mut on_disk = hyprcorrect_core::Config::load().map_err(|e| e.to_string())?;
        on_disk.providers.languagetool = self.config.providers.languagetool.clone();
        on_disk.save().map_err(|e| e.to_string())?;
        // Keep dirty-tracking honest: these fields now match disk.
        self.saved.providers.languagetool = self.config.providers.languagetool.clone();
        Ok(())
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
                        OpKind::EnableNgrams => {
                            self.config.providers.languagetool.enabled = true;
                            "n-grams enabled — container recreated with the dataset."
                        }
                        OpKind::RemoveNgrams => {
                            self.config.providers.languagetool.ngram_dir = None;
                            "n-grams removed — container recreated and data deleted."
                        }
                    };
                    // These ops change persistent container state (the
                    // container survives a restart), so mirror the resulting
                    // LanguageTool config to disk. Without this, enabling
                    // n-grams recreates the container but never records the
                    // folder — so the reopened prefs forgets it.
                    if matches!(
                        kind,
                        OpKind::Install | OpKind::EnableNgrams | OpKind::RemoveNgrams
                    ) && let Err(e) = self.persist_languagetool()
                    {
                        self.err(format!("{msg} (but saving config to disk failed: {e})"));
                        return;
                    }
                    self.ok(msg);
                }
                Err(e) => {
                    let verb = match kind {
                        OpKind::Install => "install",
                        OpKind::Start => "start",
                        OpKind::Stop => "stop",
                        OpKind::Remove => "remove",
                        OpKind::EnableNgrams => "enable n-grams",
                        OpKind::RemoveNgrams => "remove n-grams",
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
        self.config != self.saved || self.llm_keys != self.saved_llm_keys || autostart_changed
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
        // Write any per-backend keys that changed. Collect first so the
        // error path can borrow `self` mutably without aliasing the maps.
        let key_writes: Vec<(String, String)> = self
            .llm_keys
            .iter()
            .filter(|(backend, key)| {
                self.saved_llm_keys
                    .get(*backend)
                    .map(String::as_str)
                    .unwrap_or("")
                    != key.as_str()
            })
            .map(|(b, k)| (b.clone(), k.clone()))
            .collect();
        for (backend, key) in key_writes {
            let name = hyprcorrect_core::llm::key_name(&backend);
            let result = if key.is_empty() {
                secrets::delete(&name)
            } else {
                secrets::set(&name, &key)
            };
            if let Err(e) = result {
                self.err(format!("keychain write failed: {e}"));
                return;
            }
        }
        self.saved_llm_keys = self.llm_keys.clone();
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
        self.llm_keys = self.saved_llm_keys.clone();
        self.llm_draft = LlmConfig {
            backend: String::new(),
            model: String::new(),
            base_url: None,
        };
        self.llm_draft_key.clear();
        self.llm_tab = self
            .config
            .providers
            .llms
            .first()
            .map(|c| LlmTab::Provider(c.backend.clone()))
            .unwrap_or(LlmTab::Add);
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
        self.poll_ngram_download(ctx);
        self.poll_folder_pick(ctx);
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
                            HotkeyTarget::ReviewLlm => self.config.hotkeys.review_llm = chord,
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
                kanso::widgets::sidebar_header(
                    ui,
                    logo.as_ref().map(egui::Image::new),
                    "hyprcorrect",
                );
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
            // 20px horizontal padding to line the action row up with the
            // content column above it.
            .frame(
                egui::Frame::side_top_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(20, 20)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    // 20px between buttons.
                    ui.spacing_mut().item_spacing.x = 20.0;
                    let quit_label =
                        egui::RichText::new("Quit hyprcorrect").color(kanso::palette::ERROR);
                    if ui.add(egui::Button::new(quit_label)).clicked() {
                        quit_requested = true;
                    }
                    if self.daemon_stale {
                        let relaunch_label = egui::RichText::new("Relaunch daemon (new build)")
                            .color(kanso::palette::WARN);
                        let resp = ui.add(egui::Button::new(relaunch_label)).on_hover_text(
                            "The on-disk binary is newer than the running daemon. \
                                 Click to quit the old daemon and spawn the new one.",
                        );
                        if resp.clicked() {
                            relaunch_requested = true;
                        }
                    }

                    if !self.status.text.is_empty() {
                        let color = if self.status.is_error {
                            ui.visuals().error_fg_color
                        } else {
                            ui.visuals().widgets.active.fg_stroke.color
                        };
                        ui.colored_label(color, &self.status.text);
                    }

                    // The shared design system's settings action bar. Our
                    // dirty state is multi-source (config + llm_keys +
                    // autostart), so feed the precomputed flag via
                    // `from_dirty`; kanso paints the unsaved dot + Cancel/Save.
                    match kanso::widgets::DirtyFooter::from_dirty(self.dirty())
                        .revert_label("Cancel")
                        .show(ui)
                    {
                        kanso::widgets::FooterAction::Save => self.save(),
                        kanso::widgets::FooterAction::Revert => self.cancel(),
                        kanso::widgets::FooterAction::None => {}
                    }
                });
            });

        egui::CentralPanel::default()
            .frame(
                // No right inner margin so the scroll area — and its
                // scrollbar — reach the window's right edge. The content
                // inside keeps its 20px right gap via `set_max_width` below.
                egui::Frame::central_panel(&ctx.style()).inner_margin(egui::Margin {
                    left: 20,
                    right: 0,
                    top: 18,
                    bottom: 18,
                }),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // Scrollbar sits flush at the edge. Reserve only 10px
                        // here: the solid scrollbar already takes ~10px
                        // (bar_inner_margin 4 + bar_width 6), so total right
                        // padding (gap + scrollbar) ≈ 20px, matching the left.
                        ui.set_max_width((ui.available_width() - 10.0).max(0.0));
                        match self.section {
                            Section::Hotkeys => self.hotkeys_panel(ui),
                            Section::Providers => self.providers_panel(ui),
                            Section::Behavior => self.behavior_panel(ui),
                            Section::Privacy => self.privacy_panel(ui),
                            Section::About => self.about_panel(ui),
                        }
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
        ui.add_space(6.0);
        caption(
            ui,
            "Shows the proposed correction in a small popup; press Enter \
             to apply or Esc to cancel. Useful for eyeballing LLM \
             suggestions before they land.",
        );

        ui.add_space(SETTING_BLOCK_SPACING);
        field_label(ui, "Escalate review to LLM");
        ui.add_space(4.0);
        let review_llm_value = self.config.hotkeys.review_llm.clone();
        if hotkey_chord_row(
            ui,
            HotkeyTarget::ReviewLlm,
            &review_llm_value,
            self.capturing_chord,
        ) {
            self.capturing_chord = Some(HotkeyTarget::ReviewLlm);
            self.clear_status();
            notify_daemon_release();
        }
        ui.add_space(6.0);
        caption(
            ui,
            "While the review popup is open, re-runs the original sentence \
             through the LLM and reloads with its suggestions — for when \
             LanguageTool's fix is wrong. Also a button in the popup.",
        );

        ui.add_space(SETTING_BLOCK_SPACING);
        caption_with_code(
            ui,
            "`$HYPRCORRECT_CHORD` overrides Fix last word for one-off dev runs.",
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
        field_label_with_info(ui, "Smart provider", SMART_PROVIDER_TOOLTIP);
        caption(ui, "Used for fix-last-sentence and the review popup.");
        ui.add_space(4.0);
        touched |= provider_radio(ui, &mut self.config.providers.smart, None);

        ui.add_space(SETTING_BLOCK_SPACING);
        ui.separator();
        ui.add_space(SETTING_BLOCK_SPACING);

        ui.label(egui::RichText::new("LLM").size(16.0).strong());
        ui.add_space(8.0);
        touched |= self.llm_providers_section(ui);

        ui.add_space(SETTING_BLOCK_SPACING);
        ui.separator();
        ui.add_space(SETTING_BLOCK_SPACING);

        ui.label(egui::RichText::new("LanguageTool").size(16.0).strong());
        ui.add_space(8.0);
        touched |= ui
            .checkbox(&mut self.config.providers.languagetool.enabled, "Enabled")
            .changed();
        ui.add_space(8.0);
        field_label_with_note(ui, "URL", "base URL — hyprcorrect appends /v2/check");
        ui.add_space(4.0);
        touched |= padded_text_edit(ui, &mut self.config.providers.languagetool.url).changed();

        ui.add_space(SETTING_BLOCK_SPACING);
        self.languagetool_docker_row(ui);
        // The manual "I already have the data" folder lives at the bottom,
        // below the n-grams status + Download button.
        touched |= self.ngram_folder_field(ui);

        if touched {
            self.clear_status();
        }
    }

    /// The n-gram data-folder row at the bottom of Providers. When the app
    /// has downloaded the data it shows that (read-only) with a Remove
    /// button, so the empty box never implies the user must supply their
    /// own. Otherwise it's an editable field for data they already have.
    /// Returns whether the config value changed this frame.
    fn ngram_folder_field(&mut self, ui: &mut egui::Ui) -> bool {
        ui.add_space(SETTING_BLOCK_SPACING);

        // App-downloaded data takes over the row: show where it lives
        // (grayed, read-only) with the Browse button disabled, plus a
        // Remove option — the user doesn't pick a folder in this case.
        let base = hyprcorrect_core::config::ngram_data_dir();
        if let Some(managed) = base
            .as_deref()
            .filter(|b| crate::ngrams::data_root(b).is_some())
            .map(std::path::Path::to_path_buf)
        {
            field_label_with_info(
                ui,
                "n-gram data folder",
                "Downloaded and installed by hyprcorrect — you don't need to set your own.",
            );
            ui.add_space(4.0);
            let mut shown = managed.to_string_lossy().to_string();
            ui.add_enabled_ui(false, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let browse_w = 88.0;
                    let field_w =
                        (ui.available_width() - browse_w - 6.0 - TEXT_EDIT_MARGIN_X).max(80.0);
                    bordered_text_edit(
                        ui,
                        egui::TextEdit::singleline(&mut shown)
                            .margin(egui::Margin::symmetric(8, 6))
                            .desired_width(field_w),
                    );
                    ui.add(egui::Button::new("Browse…")).on_disabled_hover_text(
                        "hyprcorrect manages this folder — use Remove downloaded data to change it.",
                    );
                });
            });
            ui.add_space(8.0);
            let port = docker::host_port_from_url(&self.config.providers.languagetool.url);
            if ui
                .add_enabled(
                    self.docker_op.is_none() && port.is_some(),
                    egui::Button::new("Remove downloaded data"),
                )
                .on_hover_text(
                    "Recreates the container without n-grams and deletes the downloaded \
                     folder (frees ~16 GB).",
                )
                .clicked()
                && let Some(port) = port
            {
                self.docker_op = Some(docker::remove_ngrams(port, managed));
                self.ok(OpKind::RemoveNgrams.label());
            }
            return false;
        }

        // No app download — editable field + enabled Browse for data the
        // user already has.
        field_label(ui, "n-gram data folder (optional)");
        ui.add_space(4.0);
        let mut ngram = self
            .config
            .providers
            .languagetool
            .ngram_dir
            .clone()
            .unwrap_or_default();
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let browse_w = 88.0;
            let field_w = (ui.available_width() - browse_w - 6.0 - TEXT_EDIT_MARGIN_X).max(80.0);
            changed |= bordered_text_edit(
                ui,
                egui::TextEdit::singleline(&mut ngram)
                    .hint_text("/path/to/ngrams (the folder containing en/)")
                    .margin(egui::Margin::symmetric(8, 6))
                    .desired_width(field_w),
            )
            .changed();
            if ui
                .add_enabled(
                    self.folder_pick.is_none() && self.folder_picker_available,
                    egui::Button::new("Browse…"),
                )
                .on_disabled_hover_text("Install zenity or kdialog to browse for a folder.")
                .on_hover_text("Pick the folder in a file dialog.")
                .clicked()
            {
                self.folder_pick = Some(spawn_folder_pick(
                    self.config.providers.languagetool.ngram_dir.clone(),
                ));
            }
        });
        if changed {
            self.config.providers.languagetool.ngram_dir =
                (!ngram.trim().is_empty()).then(|| ngram.trim().to_string());
        }
        ui.add_space(4.0);
        caption_with_code(
            ui,
            "Only needed if you already have the unzipped n-gram data — the folder that \
             contains an `en/` subfolder (e.g. `…/ngrams/`, with `en/2grams`, `en/3grams` \
             inside). Point here, then click \"Enable n-grams\" above. Otherwise use \
             \"Download n-grams\".",
        );
        changed
    }

    /// One-click LanguageTool-in-Docker row under the LanguageTool
    /// section. See `crate::docker` for the rationale — provider
    /// integration is still URL-based, this is a UX convenience for
    /// users who'd otherwise have to memorize a `docker run` invocation.
    fn languagetool_docker_row(&mut self, ui: &mut egui::Ui) {
        let url = self.config.providers.languagetool.url.clone();
        let op_in_flight = self.docker_op.is_some();
        let probe_in_flight = self.status_probe.is_some() && self.lt_status.is_none();
        let status = self.lt_status.clone();

        // When our managed container is up there's genuinely nothing to
        // do, so the heading carries that reassurance in an (i) rather
        // than a caption stacked under the Stop / Remove buttons.
        if matches!(
            status,
            Some(LanguageToolStatus::Reachable {
                managed_container_running: true,
            })
        ) {
            field_label_with_info(
                ui,
                "Local server (Docker)",
                "Running in the hyprcorrect-managed container. Nothing else to do.",
            );
        } else {
            field_label(ui, "Local server (Docker)");
        }
        ui.add_space(4.0);

        let Some(status) = status else {
            // First-ever probe still in flight — show a neutral
            // "checking…" message instead of flashing a wrong state.
            if probe_in_flight {
                ui.colored_label(
                    kanso::palette::TEXT_MUTED,
                    "Checking for a running LanguageTool server…",
                );
            }
            return;
        };

        match status {
            LanguageToolStatus::Reachable {
                managed_container_running,
            } => {
                ui.colored_label(kanso::palette::OK, format!("Reachable at {url}"));
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

        self.languagetool_ngram_row(ui, &url, op_in_flight);
    }

    /// n-gram status + controls — tracked independently of the container's
    /// run state, since the dataset is a `docker run` mount that can only
    /// be added by (re)creating the container.
    fn languagetool_ngram_row(&mut self, ui: &mut egui::Ui, url: &str, op_in_flight: bool) {
        ui.add_space(SETTING_BLOCK_SPACING);
        field_label_with_info(
            ui,
            "n-grams (wear/where)",
            "LanguageTool's statistical n-gram model catches real-word errors — words \
             spelled correctly but wrong for the context, which a plain spell-checker \
             can't flag. Examples: their/there/they're, its/it's, then/than, to/too, \
             your/you're, of/off, lose/loose. The dataset is LanguageTool's English \
             n-gram corpus (~8.4 GB download, ~16 GB unzipped to a folder containing en/).",
        );
        ui.add_space(4.0);

        // A download in flight owns the row: progress bar + Cancel.
        if let Some(handle) = &self.ngram_download {
            use crate::ngrams::DownloadPhase;
            const GB: f64 = 1_000_000_000.0;
            match handle.phase() {
                DownloadPhase::Downloading { done, total } => {
                    let frac = if total > 0 {
                        done as f32 / total as f32
                    } else {
                        0.0
                    };
                    let text = if total > 0 {
                        format!(
                            "Downloading {:.1} / {:.1} GB",
                            done as f64 / GB,
                            total as f64 / GB
                        )
                    } else {
                        format!("Downloading {:.1} GB…", done as f64 / GB)
                    };
                    kanso::widgets::progress(ui, frac, &text);
                }
                // Done/Failed/Cancelled are consumed by poll_ngram_download.
                _ => {
                    kanso::widgets::ProgressBar::indeterminate()
                        .text("Unzipping (~16 GB)…")
                        .show(ui);
                }
            }
            ui.add_space(4.0);
            if ui.button("Cancel").clicked() {
                handle.cancel();
            }
            return;
        }

        let port = docker::host_port_from_url(url);
        // What we'd mount: the app's download (filesystem truth, even if the
        // config field was never saved), else a folder the user set.
        let base = hyprcorrect_core::config::ngram_data_dir();
        let downloaded = base.as_deref().and_then(crate::ngrams::data_root);
        let user_dir = self
            .config
            .providers
            .languagetool
            .ngram_dir
            .clone()
            .filter(|d| !d.trim().is_empty());

        // Loaded: green confirmation. Reload only matters for a user's own
        // folder (the app's downloaded data is static) — when it's ours,
        // the "Remove downloaded data" control lives in the field below.
        if self.lt_ngrams == Some(true) {
            ui.colored_label(
                kanso::palette::OK,
                "Loaded — real-word confusions are caught (their/there, its/it's, then/than).",
            );
            if downloaded.is_none()
                && let (Some(dir), Some(port)) = (user_dir.as_deref(), port)
            {
                ui.add_space(4.0);
                if ui
                    .add_enabled(!op_in_flight, egui::Button::new("Reload n-grams"))
                    .on_hover_text(
                        "Optional — n-grams already work. Only needed if you swap the data \
                         at the folder below (LanguageTool reads n-grams only at startup). \
                         Recreates the container.",
                    )
                    .clicked()
                {
                    self.docker_op = Some(docker::enable_ngrams(port, dir));
                    self.ok(OpKind::EnableNgrams.label());
                }
            }
            return;
        }

        // Not loaded. If the app already has the data, just Enable it;
        // otherwise offer the one-click Download (+ Enable for a folder the
        // user supplied below).
        if let Some(mount) = &downloaded {
            let mount = mount.to_string_lossy().to_string();
            if let Some(port) = port
                && ui
                    .add_enabled(!op_in_flight, egui::Button::new("Enable n-grams"))
                    .on_hover_text(
                        "Mounts the already-downloaded data and recreates the container.",
                    )
                    .clicked()
            {
                self.docker_op = Some(docker::enable_ngrams(port, &mount));
                self.ok(OpKind::EnableNgrams.label());
            }
            ui.add_space(4.0);
            caption(ui, "Off — data is downloaded. Click Enable to turn it on.");
            return;
        }

        // A valid user-supplied folder (contains en/) → Enable as a real
        // button and hide Download. Clearing the field (no valid data) brings
        // Download back — a presence/validity check, not the act of editing.
        let user_valid = user_dir
            .as_deref()
            .and_then(|d| crate::ngrams::data_root(std::path::Path::new(d)));
        if let Some(valid_root) = user_valid {
            let mount = valid_root.to_string_lossy().to_string();
            if let Some(port) = port
                && ui
                    .add_enabled(!op_in_flight, egui::Button::new("Enable n-grams"))
                    .on_hover_text(
                        "Mounts the n-gram data at the folder below and recreates the \
                         container.",
                    )
                    .clicked()
            {
                self.docker_op = Some(docker::enable_ngrams(port, &mount));
                self.ok(OpKind::EnableNgrams.label());
            }
            ui.add_space(4.0);
            caption(
                ui,
                "Off — n-gram data found at the folder below. Click Enable to turn it on.",
            );
            return;
        }

        // No data anywhere → the one-click download.
        if ui
            .add_enabled_ui(!op_in_flight && base.is_some(), |ui| {
                kanso::widgets::primary_button(ui, "Download n-grams (~8.4 GB)").on_hover_text(
                    "Downloads LanguageTool's English n-gram data to the app's data \
                     folder and enables it. Needs ~24 GB free while unzipping.",
                )
            })
            .inner
            .clicked()
            && let Some(d) = base.clone()
        {
            self.ngram_download = Some(crate::ngrams::spawn_ngram_download(d));
            self.ok("Downloading n-grams…");
        }
        ui.add_space(4.0);
        caption(
            ui,
            "Off — real-word errors slip through (their/there, its/it's). Download the \
             data, or point the folder below at a copy you have.",
        );
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
                kanso::palette::TEXT_MUTED,
            ),
            DockerState::DockerUnavailable(msg) => {
                (format!("Docker unavailable: {msg}"), kanso::palette::WARN)
            }
            DockerState::AbsentContainer => {
                ("Not installed.".to_string(), kanso::palette::TEXT_MUTED)
            }
            DockerState::ContainerStopped => (
                format!("Our container exists but is stopped. Start it to reach {url}."),
                kanso::palette::WARN,
            ),
            DockerState::ContainerRunning => (
                format!(
                    "Our container is running but {url} doesn't answer — \
                     likely a port-mapping mismatch."
                ),
                kanso::palette::WARN,
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
                kanso::palette::WARN,
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
                         \n      -p {}:8010 {}\nFirst run downloads ~600 MB. Add n-grams \
                         separately below.",
                        docker::CONTAINER,
                        p,
                        docker::IMAGE,
                    ),
                    None => "URL needs an explicit port (e.g. http://localhost:8081) before \
                             hyprcorrect can map it to the container."
                        .to_string(),
                };
                if ui
                    .add_enabled_ui(enabled, |ui| {
                        kanso::widgets::primary_button(ui, "Install with Docker")
                            .on_hover_text(hover)
                    })
                    .inner
                    .clicked()
                    && let Some(port) = port
                {
                    self.docker_op = Some(docker::install(port, None));
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
            let muted = kanso::palette::TEXT_MUTED;
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
            if kanso::widgets::labeled_toggle(
                ui,
                "Start at login",
                &mut self.autostart_enabled,
                "Launch hyprcorrect when I log in",
                "Drops a `hyprcorrect.desktop` into `~/.config/autostart/` \
                 so the daemon starts with your session. Takes effect on save.",
            )
            .changed()
            {
                self.clear_status();
            }
            ui.add_space(SETTING_BLOCK_SPACING);
        }

        if kanso::widgets::labeled_toggle(
            ui,
            "Review popup",
            &mut self.config.behavior.review_starts_in_vim,
            "Open in vim mode",
            "Start the review popup in vim mode — modal editing of the whole \
             sentence — instead of word-edit (Tab) mode. `Ctrl+E` toggles between \
             the two either way, so with this on it flips to word-edit.",
        )
        .changed()
        {
            self.clear_status();
        }
        ui.add_space(SETTING_BLOCK_SPACING);

        if kanso::widgets::labeled_toggle(
            ui,
            "Provider fallback",
            &mut self.config.behavior.fallback_to_languagetool,
            "Try LanguageTool before Spellbook",
            "When a fix routed to the LLM can't run — no API key, an unsupported \
             backend, or the call fails — try your LanguageTool server before \
             dropping to the offline Spellbook. Only takes effect when LanguageTool \
             is enabled with a URL in Providers; otherwise fixes fall straight \
             through to Spellbook.",
        )
        .changed()
        {
            self.clear_status();
        }
        ui.add_space(SETTING_BLOCK_SPACING);

        field_label(ui, "Word definitions");
        ui.add_space(4.0);
        {
            use hyprcorrect_core::DefinitionSource as DS;
            let cur = &mut self.config.behavior.definitions;
            let changed = kanso::widgets::segmented(
                ui,
                cur,
                &[
                    (DS::Local, "Offline"),
                    (DS::Online, "Online"),
                    (DS::Off, "Off"),
                ],
            );
            if changed {
                self.clear_status();
            }
        }
        ui.add_space(6.0);
        caption(
            ui,
            "Show a word's definition under the review popup's suggestion \
             options, updating as you move between them. Offline uses a bundled \
             dictionary (WordNet); Online queries api.dictionaryapi.dev, which \
             sends the looked-up word to a third party.",
        );
        ui.add_space(SETTING_BLOCK_SPACING);

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
        let response =
            kanso::widgets::Slider::new(&mut self.config.behavior.pause_per_backspace_ms, 0..=30)
                .suffix(" ms")
                .show(ui);
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
                        placeholder_app_icon(ui);
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
        // picker so we aren't borrowing `self.app_registry` while the picker
        // borrows `self.selected_app` / `self.app_filter`.
        let candidates: Vec<crate::apps::AppMeta> = candidate_ids
            .iter()
            .map(|id| self.app_registry.lookup(ui.ctx(), id))
            .collect();

        if candidates.is_empty() {
            caption(ui, "(no running apps detected)");
        } else {
            // kanso owns the searchable icon list; hyprcorrect supplies the
            // candidates and their resolved `.desktop` icon textures.
            let entries: Vec<kanso::widgets::AppEntry> = candidates
                .iter()
                .map(|c| {
                    let entry =
                        kanso::widgets::AppEntry::new(c.identifier.clone(), c.display_name.clone());
                    match &c.icon {
                        Some(icon) => entry.with_icon(icon.id()),
                        None => entry,
                    }
                })
                .collect();
            kanso::widgets::app_picker(ui, &entries, &mut self.selected_app, &mut self.app_filter);
        }
        ui.add_space(8.0);
        let can_add = self
            .selected_app
            .as_ref()
            .is_some_and(|s| !s.is_empty() && !already_blocked.contains(&s.to_ascii_lowercase()));
        if ui.add_enabled(can_add, egui::Button::new("Add")).clicked()
            && let Some(class) = self.selected_app.take()
        {
            self.config.privacy.app_blocklist.push(class);
            self.app_filter.clear();
            self.clear_status();
        }

        ui.add_space(SETTING_BLOCK_SPACING);

        // -- Fallback: type-in for apps that aren't running right now ------
        field_label(ui, "Or add by class name");
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let w = (ui.available_width() - 80.0).max(80.0);
            let resp = bordered_text_edit(
                ui,
                egui::TextEdit::singleline(&mut self.blocklist_entry)
                    .margin(egui::Margin::symmetric(8, 6))
                    .desired_width(w),
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
        caption_with_code(
            ui,
            "Useful for apps that aren't open yet. The class is whatever \
             `hyprctl activewindow` shows for that app.",
        );
    }

    fn about_panel(&mut self, ui: &mut egui::Ui) {
        // The shared About hero: centered logo + name + version + blurb +
        // links. hyprcorrect's old left-aligned, logo-less pane folds into
        // it; Source/License become entries in the links column.
        let logo = self.logo_texture(ui.ctx()).cloned();
        let version = hyprcorrect_core::version();
        kanso::widgets::about_pane(
            ui,
            kanso::widgets::AboutInfo {
                logo: logo.as_ref().map(egui::Image::new),
                name: "hyprcorrect",
                version,
                blurb: Some("Keyboard-driven spelling and typo correction for the whole desktop."),
                links: &[
                    ("Repository", "https://github.com/jondkinney/hyprcorrect"),
                    (
                        "License (MIT OR Apache-2.0)",
                        "https://github.com/jondkinney/hyprcorrect#license",
                    ),
                ],
            },
        );
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

/// Tooltip for the *Smart provider* `(i)` — the smart path operates on a
/// whole sentence, so it warns about that explicitly.
const SMART_PROVIDER_TOOLTIP: &str = "\
Fix-last-sentence and the review popup send the whole sentence around \
the caret to the chosen provider — not just one word.

With the LLM, it returns a corrected version of the ENTIRE sentence, so \
any word in it can change to fix spelling, typos, and minor grammar \
(including homophones like their/there). It's told to preserve your \
wording, voice, and punctuation and to leave already-correct text \
unchanged — it won't freely rephrase. LanguageTool changes only the \
spans it flags; Spellbook only fixes individual misspelled words.

Privacy: the whole sentence leaves your machine when the provider is \
the LLM or a remote LanguageTool.";

/// Render a provider-id radio group; returns `true` if the user
/// changed the selection in this frame. When `llm_tooltip` is
/// `Some`, an info icon next to the LLM radio surfaces that
/// text on hover — only the Default-provider variant uses this.
fn provider_radio(
    ui: &mut egui::Ui,
    selection: &mut ProviderId,
    llm_tooltip: Option<&str>,
) -> bool {
    // Order: simplest → most complex, and offline → potentially-online.
    // Spellbook is always offline (bundled dictionary). LanguageTool is
    // offline when self-hosted at localhost; the URL field next door
    // is where its locality is configured. LLM is always a network call.
    ui.horizontal(|ui| {
        let changed = kanso::widgets::radio_group_horizontal(
            ui,
            selection,
            &[
                (ProviderId::Spellbook, "Spellbook (offline)"),
                (ProviderId::LanguageTool, "LanguageTool"),
                (ProviderId::Llm, "LLM"),
            ],
        );
        if let Some(tip) = llm_tooltip {
            kanso::widgets::info_icon(ui, tip);
        }
        changed
    })
    .inner
}

/// Hosted LLM backends offered in the Add-provider dropdown. The combo is
/// editable, so this is a convenience list, not a hard constraint. All of
/// these are wired (see [`hyprcorrect_core::llm::is_backend_wired`]);
/// `openai-compatible` is last because it's the catch-all custom/local
/// endpoint that needs a Base URL.
const LLM_BACKENDS: &[&str] = &[
    "anthropic",
    "openai",
    "gemini",
    "openrouter",
    "mistral",
    "groq",
    "deepseek",
    "xai",
    "openai-compatible",
];

/// The custom/local backend id: an OpenAI-compatible endpoint whose Base
/// URL the user supplies (Ollama, LM Studio, vLLM, or any other vendor).
const CUSTOM_BACKEND: &str = "openai-compatible";

/// Whether `backend` is the custom/local OpenAI-compatible endpoint that
/// needs a user-supplied Base URL.
fn is_custom_backend(backend: &str) -> bool {
    let b = backend.trim().to_ascii_lowercase();
    b == CUSTOM_BACKEND || b == "custom"
}

/// Suggested models for a backend, cheapest/fastest first → most
/// capable/expensive last. The model combo is editable, so these are
/// starting points. Anthropic/OpenAI/Gemini IDs are current; the rest
/// are best-effort.
fn models_for_backend(backend: &str) -> &'static [&'static str] {
    match backend.trim().to_ascii_lowercase().as_str() {
        "anthropic" => &["claude-haiku-4-5", "claude-sonnet-4-6", "claude-opus-4-8"],
        "openai" => &["gpt-4o-mini", "gpt-4o", "o4-mini", "o3", "gpt-4.1"],
        "gemini" => &["gemini-2.5-flash", "gemini-2.5-pro"],
        "openrouter" => &[
            "openai/gpt-4o-mini",
            "anthropic/claude-haiku-4-5",
            "google/gemini-2.5-flash",
        ],
        "groq" => &["llama-3.1-8b-instant", "llama-3.3-70b-versatile"],
        "mistral" => &["mistral-small-latest", "mistral-large-latest"],
        "deepseek" => &["deepseek-chat", "deepseek-reasoner"],
        "xai" => &["grok-3-mini", "grok-3"],
        // Local model tags vary by install; these are common Ollama names.
        "openai-compatible" | "custom" => &["llama3.1", "qwen2.5", "gemma2"],
        _ => &[],
    }
}

/// Vendor-cased display name for a backend id; custom values show as
/// typed.
fn backend_display(backend: &str) -> String {
    let t = backend.trim();
    match t.to_ascii_lowercase().as_str() {
        "anthropic" => "Anthropic".into(),
        "openai" => "OpenAI".into(),
        "gemini" => "Gemini".into(),
        "openrouter" => "OpenRouter".into(),
        "mistral" => "Mistral".into(),
        "groq" => "Groq".into(),
        "deepseek" => "DeepSeek".into(),
        "xai" => "xAI (Grok)".into(),
        "openai-compatible" | "custom" => "OpenAI-compatible".into(),
        _ => t.to_string(),
    }
}

/// Amber caption: `backend` is configurable but not yet functional —
/// selecting LLM falls back to the offline provider until it's wired.
fn not_wired_note(ui: &mut egui::Ui, backend: &str) {
    ui.label(
        egui::RichText::new(format!(
            "{} isn't wired up yet — until it's supported, selecting LLM falls back \
             to the offline Spellbook.",
            backend_display(backend)
        ))
        .size(CAPTION_SIZE)
        .line_height(Some(CAPTION_LINE_HEIGHT))
        .color(kanso::palette::WARN),
    );
}

/// The Base-URL row for the custom `openai-compatible` endpoint. Edits
/// `slot` in place — a blank field clears it to `None` so named cloud
/// backends never carry an empty URL. Returns whether it changed.
fn base_url_field(ui: &mut egui::Ui, slot: &mut Option<String>) -> bool {
    field_label_with_note(ui, "Base URL", "OpenAI-compatible endpoint");
    ui.add_space(4.0);
    let mut url = slot.clone().unwrap_or_default();
    let changed = padded_text_edit(ui, &mut url).changed();
    if changed {
        *slot = if url.trim().is_empty() {
            None
        } else {
            Some(url)
        };
    }
    ui.add_space(4.0);
    caption(
        ui,
        "Up to but not including /chat/completions — e.g. http://localhost:11434/v1 \
         for a local Ollama server, or your provider's OpenAI-compatible URL.",
    );
    changed
}

/// API-key caption. The custom/local endpoint notes the key is optional
/// (Ollama and friends need none); cloud backends just say where it's
/// stored.
fn api_key_caption(backend: &str) -> &'static str {
    if is_custom_backend(backend) {
        "Stored in your OS keychain, not in config.toml. Leave blank for local \
         servers (e.g. Ollama) that need no key."
    } else {
        "Stored in your OS keychain, not in config.toml."
    }
}

/// Whether `name` resolves to an executable on `$PATH`.
fn tool_in_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
}

/// Spawn a native folder picker on a worker thread (so the dialog doesn't
/// freeze the egui loop) and return the channel its result lands on:
/// `Some(path)` when the user picks one, `None` if they cancel.
fn spawn_folder_pick(initial: Option<String>) -> Receiver<Option<String>> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("hyprcorrect-folder-pick".into())
        .spawn(move || {
            let _ = tx.send(pick_folder(initial.as_deref()));
        })
        .ok();
    rx
}

/// Open a directory chooser via zenity (then kdialog), starting at
/// `initial` if given. Blocks the worker thread until the user responds.
fn pick_folder(initial: Option<&str>) -> Option<String> {
    let initial = initial.map(str::trim).filter(|d| !d.is_empty());
    if tool_in_path("zenity") {
        let mut cmd = std::process::Command::new("zenity");
        cmd.args([
            "--file-selection",
            "--directory",
            "--title=Select n-gram data folder",
        ]);
        if let Some(dir) = initial {
            cmd.arg(format!("--filename={}/", dir.trim_end_matches('/')));
        }
        if let Ok(out) = cmd.output() {
            let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
            return out
                .status
                .success()
                .then_some(path)
                .filter(|p| !p.is_empty());
        }
    }
    if tool_in_path("kdialog") {
        let mut cmd = std::process::Command::new("kdialog");
        cmd.arg("--getexistingdirectory");
        cmd.arg(initial.unwrap_or("."));
        if let Ok(out) = cmd.output() {
            let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
            return out
                .status
                .success()
                .then_some(path)
                .filter(|p| !p.is_empty());
        }
    }
    None
}

/// A neutral 20×20 placeholder where an app icon would go, for apps with
/// no discoverable `.desktop` icon — so the row still aligns and reads as
/// "an app" rather than a blank gap.
fn placeholder_app_icon(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::hover());
    ui.painter().rect_filled(
        rect.shrink(1.0),
        egui::CornerRadius::same(4),
        egui::Color32::from_gray(58),
    );
}

/// Paint a small filled dot — the "active provider" marker in the LLM
/// tab bar. Drawn, not a glyph, so it can't fall back to a tofu box (the
/// bundled fonts lack the Geometric-Shapes block, e.g. ●).
fn active_dot(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 14.0), egui::Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), 4.0, kanso::palette::OK);
}

impl PrefsApp {
    /// The tabbed multi-provider LLM editor under the Providers panel's
    /// "LLM" heading. The active provider (list index 0) is the leftmost
    /// tab, marked with a dot. Returns whether anything changed.
    fn llm_providers_section(&mut self, ui: &mut egui::Ui) -> bool {
        let mut touched = false;
        let backends: Vec<String> = self
            .config
            .providers
            .llms
            .iter()
            .map(|c| c.backend.clone())
            .collect();
        let can_add = backends.len() < hyprcorrect_core::config::MAX_LLM_PROVIDERS;

        // Keep the selected tab valid after reorders / removals.
        let valid = match &self.llm_tab {
            LlmTab::Provider(b) => backends.iter().any(|x| x == b),
            LlmTab::Add => can_add,
        };
        if !valid {
            self.llm_tab = backends
                .first()
                .map(|b| LlmTab::Provider(b.clone()))
                .unwrap_or(LlmTab::Add);
        }

        // --- Tab bar --- only once at least one provider is saved; with
        // none, the add form shows directly (no lone "+ Add Provider" chip).
        if !backends.is_empty() {
            let mut new_tab: Option<LlmTab> = None;
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                for (i, backend) in backends.iter().enumerate() {
                    let selected = matches!(&self.llm_tab, LlmTab::Provider(b) if b == backend);
                    // A green dot marks the active provider (always index 0).
                    if i == 0 {
                        active_dot(ui);
                    }
                    if ui
                        .selectable_label(selected, backend_display(backend))
                        .clicked()
                    {
                        new_tab = Some(LlmTab::Provider(backend.clone()));
                    }
                }
                if can_add
                    && ui
                        .selectable_label(matches!(self.llm_tab, LlmTab::Add), "+ Add Provider")
                        .clicked()
                {
                    new_tab = Some(LlmTab::Add);
                }
            });
            if let Some(t) = new_tab {
                self.llm_tab = t;
            }
            ui.add_space(12.0);
        }

        // --- Body ---
        match self.llm_tab.clone() {
            LlmTab::Provider(backend) => touched |= self.llm_provider_tab(ui, &backend),
            LlmTab::Add => touched |= self.llm_add_tab(ui),
        }
        touched
    }

    /// One configured provider's tab: Active toggle, (read-only) backend,
    /// editable model, per-backend API key, and Remove.
    fn llm_provider_tab(&mut self, ui: &mut egui::Ui, backend: &str) -> bool {
        let mut touched = false;
        let Some(idx) = self
            .config
            .providers
            .llms
            .iter()
            .position(|c| c.backend == backend)
        else {
            return false;
        };
        let is_active = idx == 0;
        let mut promote = false;
        let mut remove = false;

        let mut active = is_active;
        let resp = ui
            .add_enabled(!is_active, egui::Checkbox::new(&mut active, "Active"))
            .on_hover_text(if is_active {
                "This is the active provider — used whenever a provider is set to LLM."
            } else {
                "Make this the active provider (moves it to the front of the list)."
            });
        if resp.changed() && active && !is_active {
            promote = true;
        }
        ui.add_space(SETTING_BLOCK_SPACING);

        field_label(ui, "Provider");
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(backend_display(backend))
                .size(14.0)
                .color(egui::Color32::from_gray(200)),
        );

        ui.add_space(SETTING_BLOCK_SPACING);
        field_label(ui, "Model");
        ui.add_space(4.0);
        touched |= kanso::widgets::editable_combo(
            ui,
            format!("model_{backend}"),
            &mut self.config.providers.llms[idx].model,
            models_for_backend(backend),
            "Pick or type a model",
        );

        if is_custom_backend(backend) {
            ui.add_space(SETTING_BLOCK_SPACING);
            touched |= base_url_field(ui, &mut self.config.providers.llms[idx].base_url);
        }

        ui.add_space(SETTING_BLOCK_SPACING);
        field_label(ui, "API key");
        ui.add_space(4.0);
        let key = self.llm_keys.entry(backend.to_string()).or_default();
        touched |= padded_password_edit(ui, key).changed();
        ui.add_space(4.0);
        caption(ui, api_key_caption(backend));

        if !hyprcorrect_core::llm::is_backend_wired(backend) {
            ui.add_space(6.0);
            not_wired_note(ui, backend);
        }

        ui.add_space(SETTING_BLOCK_SPACING);
        if ui
            .add(egui::Button::new("Remove provider"))
            .on_hover_text("Delete this provider tab. Its saved API key is left in the keychain.")
            .clicked()
        {
            remove = true;
        }

        if promote {
            self.promote_llm(backend);
            self.llm_tab = LlmTab::Provider(backend.to_string());
            touched = true;
        }
        if remove {
            self.config.providers.llms.retain(|c| c.backend != backend);
            self.llm_keys.remove(backend);
            self.llm_tab = self
                .config
                .providers
                .llms
                .first()
                .map(|c| LlmTab::Provider(c.backend.clone()))
                .unwrap_or(LlmTab::Add);
            touched = true;
        }
        touched
    }

    /// The "+ Add Provider" tab: editable backend + model + key, and a
    /// Save button that appends the provider (vendor-unique, capped at 5).
    fn llm_add_tab(&mut self, ui: &mut egui::Ui) -> bool {
        let mut touched = false;
        caption(ui, "Add a hosted LLM (up to 5 providers).");
        ui.add_space(SETTING_BLOCK_SPACING);

        field_label(ui, "Provider");
        ui.add_space(4.0);
        let before = self.llm_draft.backend.clone();
        if kanso::widgets::editable_combo(
            ui,
            "add_backend",
            &mut self.llm_draft.backend,
            LLM_BACKENDS,
            "Pick or type a provider",
        ) {
            touched = true;
            // When the provider changes, default the model to that
            // provider's cheapest/fastest.
            if self.llm_draft.backend != before {
                self.llm_draft.model = models_for_backend(&self.llm_draft.backend)
                    .first()
                    .map(|s| (*s).to_string())
                    .unwrap_or_default();
            }
        }

        ui.add_space(SETTING_BLOCK_SPACING);
        field_label(ui, "Model");
        ui.add_space(4.0);
        let models = models_for_backend(&self.llm_draft.backend);
        touched |= kanso::widgets::editable_combo(
            ui,
            "add_model",
            &mut self.llm_draft.model,
            models,
            "Pick or type a model",
        );

        if is_custom_backend(&self.llm_draft.backend) {
            ui.add_space(SETTING_BLOCK_SPACING);
            touched |= base_url_field(ui, &mut self.llm_draft.base_url);
        }

        // Validate up front so the Save button on the API-key row can gate.
        let backend = self.llm_draft.backend.trim().to_string();
        let dup = self
            .config
            .providers
            .llms
            .iter()
            .any(|c| c.backend.eq_ignore_ascii_case(&backend));
        let full = self.config.providers.llms.len() >= hyprcorrect_core::config::MAX_LLM_PROVIDERS;
        let can_add = !backend.is_empty() && !dup && !full;

        ui.add_space(SETTING_BLOCK_SPACING);
        field_label(ui, "API key");
        ui.add_space(4.0);
        // Save sits on the API-key row (right), the key field fills the rest —
        // mirroring how the chevron sits beside the combo field. (A plain
        // horizontal, not with_layout, which grabs the full remaining height
        // and floated this row to the bottom.)
        let mut save_clicked = false;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let save_w = 118.0;
            let field_w = (ui.available_width() - save_w - 6.0 - TEXT_EDIT_MARGIN_X).max(80.0);
            touched |= bordered_text_edit(
                ui,
                egui::TextEdit::singleline(&mut self.llm_draft_key)
                    .password(true)
                    .margin(egui::Margin::symmetric(8, 6))
                    .desired_width(field_w),
            )
            .changed();
            save_clicked = ui
                .add_enabled_ui(can_add, |ui| {
                    kanso::widgets::primary_button(ui, "Save provider")
                })
                .inner
                .clicked();
        });
        ui.add_space(4.0);
        caption(ui, api_key_caption(&backend));

        if !backend.is_empty() && !hyprcorrect_core::llm::is_backend_wired(&backend) {
            ui.add_space(6.0);
            not_wired_note(ui, &backend);
        }
        if dup && !backend.is_empty() {
            ui.add_space(4.0);
            caption(
                ui,
                &format!("{} already has a tab.", backend_display(&backend)),
            );
        } else if full {
            ui.add_space(4.0);
            caption(
                ui,
                "Maximum of 5 providers reached — remove one to add another.",
            );
        }

        if save_clicked {
            let model = if self.llm_draft.model.trim().is_empty() {
                models_for_backend(&backend)
                    .first()
                    .map(|s| (*s).to_string())
                    .unwrap_or_default()
            } else {
                self.llm_draft.model.trim().to_string()
            };
            // Only the custom endpoint carries a base URL; drop a blank
            // one to None so named backends never persist an empty string.
            let base_url = self
                .llm_draft
                .base_url
                .clone()
                .filter(|_| is_custom_backend(&backend))
                .filter(|s| !s.trim().is_empty());
            self.config.providers.llms.push(LlmConfig {
                backend: backend.clone(),
                model,
                base_url,
            });
            self.llm_keys
                .insert(backend.clone(), self.llm_draft_key.clone());
            self.llm_tab = LlmTab::Provider(backend.clone());
            self.llm_draft = LlmConfig {
                backend: String::new(),
                model: String::new(),
                base_url: None,
            };
            self.llm_draft_key.clear();
            touched = true;
        }
        touched
    }

    /// Move the provider with `backend` to the front of the list (index
    /// 0 = active). MRU order: the previously-active provider slides to
    /// second.
    fn promote_llm(&mut self, backend: &str) {
        if let Some(i) = self
            .config
            .providers
            .llms
            .iter()
            .position(|c| c.backend == backend)
            && i > 0
        {
            let c = self.config.providers.llms.remove(i);
            self.config.providers.llms.insert(0, c);
        }
    }
}

/// Sidebar row — vernier-style. Egui's default `selectable_label`
/// puts a square, light selection backdrop behind whatever it draws;
/// we want a rounded, contained pill. Allocates a click-sized rect
/// and paints the selection backdrop + label ourselves.
fn sidebar_item(ui: &mut egui::Ui, selected: bool, label: &str) -> egui::Response {
    kanso::widgets::nav_item(ui, selected, label)
}

/// Add a [`egui::TextEdit`] with a visible border in its *non-focused*
/// state, so a field reads as the same height as the buttons beside it
/// instead of receding into the panel (egui draws no border by default —
/// its own source notes the field "doesn't pop"). Scoped to the field:
/// the inactive `bg_stroke` is restored afterward so sibling widgets
/// (e.g. the combo chevron) are unaffected. Hover/focus keep egui's
/// gray/accent strokes.
fn bordered_text_edit(ui: &mut egui::Ui, te: egui::TextEdit<'_>) -> egui::Response {
    // Force the field to CONTROL_HEIGHT (its natural height is a touch
    // shorter) so it matches the buttons/chevrons. The always-on border
    // comes from the global widget visuals (see apply_style).
    ui.add(te.min_size(egui::vec2(0.0, CONTROL_HEIGHT)))
}

/// Single-line text input with consistent inner padding so fields
/// don't collapse to ~16 px tall at the body font size.
fn padded_text_edit(ui: &mut egui::Ui, text: &mut String) -> egui::Response {
    kanso::widgets::padded_text_edit(ui, text)
}

/// Single-line *password* input with the same padding as
/// [`padded_text_edit`]. The contents render as bullets.
fn padded_password_edit(ui: &mut egui::Ui, text: &mut String) -> egui::Response {
    kanso::widgets::password_field(ui, text)
}

/// Bold-ish label introducing a setting. Slightly larger than the
/// caption text below the input.
fn field_label(ui: &mut egui::Ui, text: &str) {
    kanso::widgets::field_label(ui, text);
}

/// A [`field_label`] with a trailing `(i)` info icon whose tooltip
/// carries the detail that would otherwise sit in a caption line —
/// keeps the row compact while leaving the explanation one hover away.
/// Mirrors the LLM info icon in [`provider_radio`].
fn field_label_with_info(ui: &mut egui::Ui, label: &str, tip: &str) {
    ui.horizontal(|ui| {
        field_label(ui, label);
        kanso::widgets::info_icon(ui, tip);
    });
}

/// A [`field_label`] followed by a muted parenthetical on the same line —
/// for a short clarifier that reads better beside the label than in a
/// caption below the control.
fn field_label_with_note(ui: &mut egui::Ui, label: &str, note: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        field_label(ui, label);
        ui.label(
            egui::RichText::new(format!("({note})"))
                .size(CAPTION_SIZE)
                .color(kanso::palette::TEXT_MUTED),
        );
    });
}

/// Muted explainer text under inputs or checkboxes. Sized for
/// comfortable wrapped reading — larger than egui's default body
/// with extra line-height so multi-line captions don't feel cramped.
const CAPTION_SIZE: f32 = 13.5;
const CAPTION_LINE_HEIGHT: f32 = 20.0;

fn caption(ui: &mut egui::Ui, text: &str) {
    // kanso's caption renders plain muted text and parses `backtick` spans
    // into inline code pills — a superset of this one.
    kanso::widgets::caption(ui, text);
}

/// Like [`caption`] but renders backtick-delimited spans as inline code
/// pills (monospace on a subtle dark backdrop), GitHub-comment style.
/// Mirrors vernier's `caption`: the pill backdrops are painted by hand at
/// a tight y-range hugging the glyph metrics (not the full row height) so
/// they sit centered on the text rather than riding high.
fn caption_with_code(ui: &mut egui::Ui, text: &str) {
    // Was a ~90-line glyph-metric pill painter, byte-for-byte vernier's;
    // now the shared design system owns it.
    kanso::widgets::caption(ui, text);
}

const SETTING_BLOCK_SPACING: f32 = 22.0;

/// Horizontal margin egui's `TextEdit` adds on top of `desired_width`
/// (our `Margin::symmetric(8, …)` → 8px each side = 16px). Must be
/// reserved when a field shares a row with another widget, or the field's
/// true outer width overflows the row.
const TEXT_EDIT_MARGIN_X: f32 = 16.0;

/// Shared height for every interactive control (inputs, buttons, combo
/// chevrons). Forced via `interact_size.y` (button floor) and
/// `TextEdit::min_size` so they all match in every state.
const CONTROL_HEIGHT: f32 = 30.0;

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
    // The shared capture chip owns the glyph rendering (modifier symbols
    // + the Omarchy SUPER logo via kanso's SHORTCUT_FAMILY), the
    // "Press a shortcut…" / "Click to set" prompts, and the record/hover
    // states — so hyprcorrect no longer hand-paints its own chip.
    kanso::widgets::shortcut_capture_chip(ui, value, is_capturing_this).clicked()
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
    // Type scale, spacing, the solid scrollbar, and corner radius all come
    // from the shared design system now; control_visuals adds the
    // input/button border (color-matched at rest, colored on hover/press,
    // never expanding). Fonts are installed once at startup via
    // kanso::fonts::install (also done by the review popup), so this stays
    // font-free and cheap to call per frame.
    kanso::theme::apply_styles(ctx);
    ctx.style_mut(|style| kanso::theme::control_visuals(&mut style.visuals));
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
    // Load each configured provider's API key from the keychain
    // (`llm.<backend>`) so the per-tab key fields start populated.
    let mut saved_llm_keys: BTreeMap<String, String> = BTreeMap::new();
    for llm in &saved.providers.llms {
        let key = secrets::get(&hyprcorrect_core::llm::key_name(&llm.backend))
            .ok()
            .flatten()
            .unwrap_or_default();
        saved_llm_keys.insert(llm.backend.clone(), key);
    }
    // The daemon can open us straight to a section (e.g. Providers when
    // the user tries to escalate to the LLM without a key configured).
    let initial_section = std::env::var("HYPRCORRECT_PREFS_SECTION")
        .ok()
        .and_then(|s| Section::from_name(&s))
        .unwrap_or(Section::Hotkeys);

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
            kanso::fonts::install(
                &cc.egui_ctx,
                &kanso::fonts::FontOptions {
                    shortcut_family: true,
                    ..Default::default()
                },
            );
            Ok(Box::new(PrefsApp::new(
                saved,
                saved_llm_keys,
                shutdown_tx,
                initial_section,
            )))
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
