//! hyprcorrect — keyboard-driven desktop spelling and typo correction.
//!
//! Running `hyprcorrect` with no subcommand starts the daemon: it
//! registers the trigger chord with Hyprland, captures keystrokes into
//! a per-window keystroke buffer, subscribes to focus events so each
//! window owns its own buffer, publishes a system-tray icon, and
//! corrects the last typed word in place when the chord fires.
//! See `DESIGN.md` at the repository root.

use clap::{Parser, Subcommand};

/// The per-OS platform backend. `crate::backend::{capture, emit, hotkey,
/// focus, tray, chord_capture, clipboard}` resolve to the Linux or the
/// macOS implementation; the shared daemon code below is identical across
/// both — the whole point of milestone M2.
#[cfg(target_os = "linux")]
use hyprcorrect_platform::linux as backend;
#[cfg(target_os = "macos")]
use hyprcorrect_platform::macos as backend;

/// Keyboard-driven spelling correction for the whole desktop.
#[derive(Parser)]
#[command(name = "hyprcorrect", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Correct the last typed word in place, with no UI.
    FixWord,
    /// Correct the last typed sentence in place, with no UI.
    FixSentence,
    /// Open the suggestion popup for the recently typed text.
    Review,
    /// Open the preferences window.
    Prefs,
    /// Install the desktop entry and app icon into the XDG data
    /// directory, then exit — without starting the daemon. The
    /// daemon does this on every start anyway, so this is only
    /// needed to register a freshly `cargo install`ed binary with
    /// app launchers before its first run.
    InstallDesktop,
}

fn main() {
    env_logger::init();

    match Cli::parse().command {
        None => {
            // macOS needs AppKit's run loop on the main thread (for the
            // Carbon hotkey, NSStatusItem tray, and NSWorkspace focus
            // observer), so the daemon body runs on a worker thread that
            // `bootstrap_main` spawns; `bootstrap_main` never returns.
            #[cfg(target_os = "macos")]
            hyprcorrect_platform::macos::bootstrap_main(run_daemon);
            #[cfg(not(target_os = "macos"))]
            run_daemon();
        }
        Some(Command::FixWord) => {
            eprintln!(
                "hyprcorrect: run `hyprcorrect` with no subcommand — the daemon \
                 corrects the last word when you press the trigger chord"
            );
        }
        Some(Command::FixSentence) => not_yet("fix-sentence as a CLI subcommand", "M5"),
        Some(Command::Review) => hyprcorrect_ui::run_review(),
        Some(Command::Prefs) => hyprcorrect_ui::run_preferences(),
        Some(Command::InstallDesktop) => run_install_desktop(),
    }
}

/// `install-desktop`: write the app icon + application-catalog entry
/// into the user's XDG data dir and report what landed.
///
/// `cargo install hyprcorrect` places only the executable in
/// `~/.cargo/bin` — no icon, no `.desktop` entry — so a crates.io
/// install wouldn't surface in launchers or file managers. This is
/// the loud, explicit path to register it; the daemon performs the
/// same write on first launch, so AUR / autostart users never need
/// to run it.
///
/// XDG desktop entries are a Linux concept; the `autostart` module
/// that backs this is Linux-only, so the non-Linux build gets a stub.
#[cfg(target_os = "linux")]
fn run_install_desktop() {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("hyprcorrect: could not locate the running binary: {e}");
            std::process::exit(1);
        }
    };

    let icon = match hyprcorrect_ui::autostart::ensure_user_icon() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("hyprcorrect: could not install app icon: {e}");
            std::process::exit(1);
        }
    };
    let entry = match hyprcorrect_ui::autostart::ensure_apps_catalog_entry(&exe.to_string_lossy()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("hyprcorrect: could not install desktop entry: {e}");
            std::process::exit(1);
        }
    };

    // Mark the one-shot first-launch step done so the daemon doesn't
    // redo it on the next start.
    hyprcorrect_ui::autostart::mark_install_done();

    println!("Installed hyprcorrect desktop integration:");
    if let Some(icon) = icon {
        println!("  icon           {}", icon.display());
    }
    if let Some(entry) = entry {
        println!("  desktop entry  {}", entry.display());
    }
    println!();
    println!("hyprcorrect should now appear in your application launcher.");
}

#[cfg(not(target_os = "linux"))]
fn run_install_desktop() {
    eprintln!(
        "hyprcorrect: install-desktop writes XDG `.desktop` + icon files \
         and is Linux-only so far — macOS support is milestone M2."
    );
}

/// Run the background daemon: load config, register the trigger,
/// capture keystrokes into per-window buffers, subscribe to focus
/// events, publish the tray, and correct the focused window's last
/// word on the chord.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_daemon() {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread;

    use crate::backend::{capture, chord_capture, focus, hotkey, tray};
    use hyprcorrect_core::{Buffer, Chord, Config, OfflineProvider};

    // Daemon singleton: the chord-capture socket is bound by the
    // running daemon. If we can connect, another daemon owns it —
    // exit cleanly so a stray `hyprcorrect` from the launcher /
    // autostart / a debug rerun doesn't spin up a duplicate that
    // would race for evdev events and for hyprctl bind ownership.
    if std::os::unix::net::UnixStream::connect(hyprcorrect_core::runtime::chord_socket_path())
        .is_ok()
    {
        eprintln!(
            "hyprcorrect: another daemon instance is already running — exiting. \
             Open Preferences with `hyprcorrect prefs`."
        );
        return;
    }

    let initial_config = Config::load().unwrap_or_else(|e| {
        eprintln!("hyprcorrect: could not load config ({e}) — using defaults");
        Config::default()
    });
    let mut llm = build_llm(&initial_config);
    let mut languagetool = build_languagetool(&initial_config);
    let mut default_provider_id = initial_config.providers.default;
    let mut smart_provider_id = initial_config.providers.smart;
    let mut pause_per_backspace_ms = initial_config.behavior.pause_per_backspace_ms;
    let mut pause_per_char_ms = initial_config.behavior.pause_per_char_ms;
    let mut lt_fallback = initial_config.behavior.fallback_to_languagetool;
    capture::set_reset_keys(reset_key_config(&initial_config.behavior.reset_keys));
    let mut chord = match effective_chord(&initial_config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("hyprcorrect: invalid chord in config ({e}) — falling back to default");
            Chord::parse("SUPER+CTRL+SHIFT+ALT+F").expect("default chord parses")
        }
    };
    let mut sentence_chord = parse_optional_chord(&initial_config.hotkeys.fix_sentence);
    let mut review_chord = parse_optional_chord(&initial_config.hotkeys.review);
    let mut review_llm_chord = parse_optional_chord(&initial_config.hotkeys.review_llm);
    let mut blocklist = build_blocklist(&initial_config);
    let paused = Arc::new(AtomicBool::new(false));

    if let Err(e) = hyprcorrect_core::runtime::write_self_pid() {
        eprintln!("hyprcorrect: could not write PID file ({e}) — prefs reload won't work");
    }

    // On the first launch only, register the icon + applications-
    // catalog entry so a `cargo install`ed binary shows up in
    // launchers (Walker / fuzzel / rofi index
    // `~/.local/share/applications/`, NOT the autostart dir). One-shot
    // via a marker in the XDG state dir; skipped inside Flatpak and
    // when an AUR / distro package already provides the entry. Run
    // `hyprcorrect install-desktop` to force a refresh — e.g. after a
    // dev rebuild changes the icon. (XDG desktop entries are Linux-only;
    // the macOS app-bundle install path is M6 packaging, not the daemon.)
    #[cfg(target_os = "linux")]
    if let Ok(exe) = std::env::current_exe() {
        hyprcorrect_ui::autostart::ensure_first_launch(&exe.to_string_lossy());
    }

    // Register the review-popup's Wayland class as a floating window
    // in Hyprland. Tiled, the popup would push the source window
    // around mid-edit and the user has nowhere to put it; floating
    // (+ centered) keeps the prefs/review experience inline with how
    // most native correction overlays behave. Best-effort — if
    // hyprctl isn't available we still work, just tiled by default.
    install_window_rules();

    let provider = match OfflineProvider::en_us() {
        Ok(provider) => provider,
        Err(e) => {
            eprintln!("hyprcorrect: {e}");
            return;
        }
    };
    let chord_slot = chord_capture::ChordCaptureSlot::new();
    if let Err(e) = chord_capture::start_listener(chord_slot.clone()) {
        eprintln!(
            "hyprcorrect: could not start chord-capture listener ({e}) — prefs won't be able to record SUPER chords"
        );
    }
    let key_rx = match capture::start(
        &active_chords(&chord, &sentence_chord, &review_chord, &review_llm_chord),
        chord_slot.clone(),
    ) {
        Ok(rx) => rx,
        Err(e) => {
            eprintln!("hyprcorrect: {e}");
            // macOS: instead of forcing a manual restart after the user
            // grants the capture permission, park here watching for it and
            // relaunch automatically the moment it lands. The CGEventTap is
            // latched at process start, so a live grant can't re-arm the
            // already-running process — only a fresh one can. Never returns.
            #[cfg(target_os = "macos")]
            if matches!(e, capture::CaptureError::Permission) {
                await_capture_permission_then_relaunch();
            }
            return;
        }
    };
    let signal_rx = match hotkey::signal_channel() {
        Ok(rx) => rx,
        Err(e) => {
            eprintln!("hyprcorrect: {e}");
            return;
        }
    };
    if let Err(e) = hotkey::install_bind(&chord, "word") {
        eprintln!("hyprcorrect: {e}");
        return;
    }
    if let Some(ref sc) = sentence_chord
        && let Err(e) = hotkey::install_bind(sc, "sentence")
    {
        eprintln!("hyprcorrect: sentence bind failed: {e}");
        // Non-fatal — fall through with the word chord still bound.
        sentence_chord = None;
    }
    if let Some(ref rc) = review_chord
        && let Err(e) = hotkey::install_bind(rc, "review")
    {
        eprintln!("hyprcorrect: review bind failed: {e}");
        review_chord = None;
    }
    if let Some(ref lc) = review_llm_chord
        && let Err(e) = hotkey::install_bind(lc, "review-llm")
    {
        eprintln!("hyprcorrect: review-llm bind failed: {e}");
        review_llm_chord = None;
    }
    let (initial_window, focus_rx) = match focus::start() {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("hyprcorrect: {e}");
            let _ = hotkey::uninstall_bind(&chord);
            if let Some(ref sc) = sentence_chord {
                let _ = hotkey::uninstall_bind(sc);
            }
            return;
        }
    };
    let (tray_handle, tray_rx) = match tray::start(
        paused.clone(),
        build_tray_pixmaps(false),
        build_tray_pixmaps(true),
    ) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("hyprcorrect: {e}");
            let _ = hotkey::uninstall_bind(&chord);
            if let Some(ref sc) = sentence_chord {
                let _ = hotkey::uninstall_bind(sc);
            }
            return;
        }
    };

    println!(
        "hyprcorrect {} — running. Press {chord} to correct the last word; \
         quit from the tray menu.",
        hyprcorrect_core::version(),
    );

    enum DaemonEvent {
        Key(hyprcorrect_core::Key),
        Signal(hotkey::HotkeyEvent),
        Focus(focus::FocusEvent),
        Tray(tray::TrayEvent),
    }

    // Merge all four sources into one channel so the main loop can
    // process them in arrival order.
    let (tx, rx) = mpsc::channel::<DaemonEvent>();
    {
        let tx = tx.clone();
        thread::spawn(move || {
            while let Ok(key) = key_rx.recv() {
                if tx.send(DaemonEvent::Key(key)).is_err() {
                    break;
                }
            }
        });
    }
    {
        let tx = tx.clone();
        thread::spawn(move || {
            while let Ok(event) = signal_rx.recv() {
                if tx.send(DaemonEvent::Signal(event)).is_err() {
                    break;
                }
            }
        });
    }
    {
        let tx = tx.clone();
        thread::spawn(move || {
            while let Ok(event) = focus_rx.recv() {
                if tx.send(DaemonEvent::Focus(event)).is_err() {
                    break;
                }
            }
        });
    }
    {
        let tx = tx.clone();
        thread::spawn(move || {
            while let Ok(event) = tray_rx.recv() {
                if tx.send(DaemonEvent::Tray(event)).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx); // the forwarder threads now own all senders

    let mut buffers: HashMap<String, Buffer> = HashMap::new();
    let mut current_address: Option<String> = initial_window.as_ref().map(|f| f.address.clone());
    let mut current_blocked = initial_window
        .as_ref()
        .is_some_and(|f| blocklist.contains(&f.class.to_ascii_lowercase()));

    for event in rx {
        match event {
            DaemonEvent::Key(key) => {
                if !paused.load(Ordering::Relaxed)
                    && !current_blocked
                    && let Some(addr) = current_address.as_deref()
                {
                    buffers.entry(addr.to_string()).or_default().push(key);
                }
                // Any keystroke that resets the buffer (Enter, Tab,
                // Esc, …) also restores caret-buffer agreement, so
                // the "buffer caret is suspect, scan the whole
                // buffer" mode mouse clicks opted us into is no
                // longer needed.
                if matches!(key, hyprcorrect_core::Key::Reset) {
                    capture::caret_suspect_flag().store(false, Ordering::Relaxed);
                }
            }
            DaemonEvent::Signal(hotkey::HotkeyEvent::Trigger) => {
                let action = hyprcorrect_core::runtime::read_action();
                match action.as_str() {
                    "review-apply" => {
                        apply_review(&mut buffers, pause_per_backspace_ms, pause_per_char_ms)
                    }
                    "review-cancel" => {
                        hyprcorrect_core::runtime::clear_review();
                    }
                    // Re-run the open review through the LLM (or, with no
                    // LLM configured, open Preferences so the user can add
                    // one). Operates on the review file, not the buffer —
                    // the focused window is the popup, not the source.
                    "review-llm" => reprocess_review_with_llm(
                        llm.as_ref(),
                        languagetool.as_ref(),
                        &provider,
                        lt_fallback,
                    ),
                    _ => {
                        if !paused.load(Ordering::Relaxed)
                            && !current_blocked
                            && let Some(addr) = current_address.as_deref()
                            && let Some(buffer) = buffers.get_mut(addr)
                        {
                            match action.as_str() {
                                "review" => start_review(
                                    addr,
                                    buffer,
                                    &provider,
                                    smart_provider_id,
                                    llm.as_ref(),
                                    languagetool.as_ref(),
                                    lt_fallback,
                                ),
                                "sentence" => fix_last_sentence(
                                    buffer,
                                    &provider,
                                    smart_provider_id,
                                    llm.as_ref(),
                                    languagetool.as_ref(),
                                    pause_per_backspace_ms,
                                    pause_per_char_ms,
                                    lt_fallback,
                                ),
                                _ => fix_last_word(
                                    buffer,
                                    default_provider_id,
                                    llm.as_ref(),
                                    languagetool.as_ref(),
                                    &provider,
                                    pause_per_backspace_ms,
                                    pause_per_char_ms,
                                    lt_fallback,
                                ),
                            }
                        }
                    }
                }
            }
            DaemonEvent::Signal(hotkey::HotkeyEvent::Reload) => {
                match Config::load() {
                    Ok(new_config) => match effective_chord(&new_config) {
                        Ok(new_chord) => {
                            if new_chord != chord {
                                let _ = hotkey::uninstall_bind(&chord);
                                eprintln!(
                                    "hyprcorrect: trigger chord changed: {chord} → {new_chord}"
                                );
                                chord = new_chord;
                            }
                            if let Err(e) = hotkey::install_bind(&chord, "word") {
                                eprintln!("hyprcorrect: rebind failed: {e}");
                            }
                            let new_sentence_chord =
                                parse_optional_chord(&new_config.hotkeys.fix_sentence);
                            if let Some(ref old) = sentence_chord
                                && new_sentence_chord.as_ref() != Some(old)
                            {
                                let _ = hotkey::uninstall_bind(old);
                            }
                            if let Some(ref sc) = new_sentence_chord
                                && let Err(e) = hotkey::install_bind(sc, "sentence")
                            {
                                eprintln!("hyprcorrect: sentence rebind failed: {e}");
                            }
                            sentence_chord = new_sentence_chord;

                            let new_review_chord = parse_optional_chord(&new_config.hotkeys.review);
                            if let Some(ref old) = review_chord
                                && new_review_chord.as_ref() != Some(old)
                            {
                                let _ = hotkey::uninstall_bind(old);
                            }
                            if let Some(ref rc) = new_review_chord
                                && let Err(e) = hotkey::install_bind(rc, "review")
                            {
                                eprintln!("hyprcorrect: review rebind failed: {e}");
                            }
                            review_chord = new_review_chord;

                            let new_review_llm_chord =
                                parse_optional_chord(&new_config.hotkeys.review_llm);
                            if let Some(ref old) = review_llm_chord
                                && new_review_llm_chord.as_ref() != Some(old)
                            {
                                let _ = hotkey::uninstall_bind(old);
                            }
                            if let Some(ref lc) = new_review_llm_chord
                                && let Err(e) = hotkey::install_bind(lc, "review-llm")
                            {
                                eprintln!("hyprcorrect: review-llm rebind failed: {e}");
                            }
                            review_llm_chord = new_review_llm_chord;

                            blocklist = build_blocklist(&new_config);
                            llm = build_llm(&new_config);
                            languagetool = build_languagetool(&new_config);
                            default_provider_id = new_config.providers.default;
                            smart_provider_id = new_config.providers.smart;
                            pause_per_backspace_ms = new_config.behavior.pause_per_backspace_ms;
                            pause_per_char_ms = new_config.behavior.pause_per_char_ms;
                            lt_fallback = new_config.behavior.fallback_to_languagetool;
                            capture::set_reset_keys(reset_key_config(
                                &new_config.behavior.reset_keys,
                            ));
                            eprintln!("hyprcorrect: config reloaded");
                        }
                        Err(e) => {
                            eprintln!("hyprcorrect: bad chord in new config ({e}) — kept old");
                        }
                    },
                    Err(e) => eprintln!("hyprcorrect: reload failed: {e}"),
                }
                // Capture's stale TriggerSpec doesn't matter — Hyprland
                // intercepts the chord and capture never sees the new
                // key under the chord. A full restart is only needed
                // if other capture-time settings change later.
            }
            DaemonEvent::Signal(hotkey::HotkeyEvent::Release) => {
                // Prefs is recording — let Hyprland deliver the chord
                // to the prefs window. We re-install on Reload.
                let _ = hotkey::uninstall_bind(&chord);
                if let Some(ref sc) = sentence_chord {
                    let _ = hotkey::uninstall_bind(sc);
                }
                if let Some(ref rc) = review_chord {
                    let _ = hotkey::uninstall_bind(rc);
                }
                if let Some(ref lc) = review_llm_chord {
                    let _ = hotkey::uninstall_bind(lc);
                }
                eprintln!("hyprcorrect: trigger released for capture");
            }
            DaemonEvent::Signal(hotkey::HotkeyEvent::Shutdown) => break,
            DaemonEvent::Focus(focus::FocusEvent::Focused { address, class }) => {
                current_blocked = blocklist.contains(&class.to_ascii_lowercase());
                current_address = Some(address);
            }
            DaemonEvent::Focus(focus::FocusEvent::Closed { address }) => {
                buffers.remove(&address);
                if current_address.as_deref() == Some(address.as_str()) {
                    current_address = None;
                    current_blocked = false;
                }
            }
            DaemonEvent::Tray(tray::TrayEvent::TogglePause) => {
                let was_paused = paused.fetch_xor(true, Ordering::Relaxed);
                tray_handle.refresh();
                eprintln!(
                    "hyprcorrect: {}",
                    if was_paused { "resumed" } else { "paused" }
                );
            }
            DaemonEvent::Tray(tray::TrayEvent::OpenPrefs) => {
                spawn_prefs_window();
            }
            DaemonEvent::Tray(tray::TrayEvent::Quit) => break,
        }
    }
    drop(tray_handle); // tear down the SNI service on exit

    // Clean up so the binds and PID file don't outlive the daemon.
    let _ = hotkey::uninstall_bind(&chord);
    if let Some(ref sc) = sentence_chord {
        let _ = hotkey::uninstall_bind(sc);
    }
    if let Some(ref rc) = review_chord {
        let _ = hotkey::uninstall_bind(rc);
    }
    if let Some(ref lc) = review_llm_chord {
        let _ = hotkey::uninstall_bind(lc);
    }
    hyprcorrect_core::runtime::clear_pid();
    hyprcorrect_core::runtime::clear_review();
}

/// Resolve the trigger chord the daemon should bind. `$HYPRCORRECT_CHORD`
/// overrides the config so tests and ad-hoc dev runs don't have to edit
/// `config.toml`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn effective_chord(
    config: &hyprcorrect_core::Config,
) -> Result<hyprcorrect_core::Chord, hyprcorrect_core::ChordError> {
    let raw =
        std::env::var("HYPRCORRECT_CHORD").unwrap_or_else(|_| config.hotkeys.fix_word.clone());
    hyprcorrect_core::Chord::parse(&raw)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn build_blocklist(config: &hyprcorrect_core::Config) -> std::collections::HashSet<String> {
    config
        .privacy
        .app_blocklist
        .iter()
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Convert the core's [`hyprcorrect_core::ResetKeys`] config struct
/// into the platform's
/// [`crate::backend::capture::ResetKeyConfig`]. They're
/// intentionally separate types — the config one is serializable /
/// versioned for TOML; the capture one is the runtime view the
/// classifier reads on every keystroke.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn reset_key_config(rk: &hyprcorrect_core::ResetKeys) -> crate::backend::capture::ResetKeyConfig {
    crate::backend::capture::ResetKeyConfig {
        enter: rk.enter,
        tab: rk.tab,
        escape: rk.escape,
        up: rk.up,
        down: rk.down,
        page_up: rk.page_up,
        page_down: rk.page_down,
        delete: rk.delete,
        insert: rk.insert,
    }
}

/// Launch `hyprcorrect prefs` as a detached subprocess (no stdio).
/// Fire-and-forget; if a prefs window is already running, the new
/// process short-circuits and focuses the existing one (the prefs
/// entry handles the singleton lock).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn spawn_prefs_window() {
    spawn_prefs_window_section(None);
}

/// Launch the prefs window, optionally opening it straight to a named
/// section (e.g. `"providers"` so the user can add an LLM key). The
/// section is passed via `$HYPRCORRECT_PREFS_SECTION` rather than a CLI
/// flag to keep the subcommand surface unchanged.
// Linux-only: both callers (spawn_prefs_window, reprocess_review_with_llm)
// are Linux-gated, so this would be dead code on other targets.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn spawn_prefs_window_section(section: Option<&str>) {
    use std::process::{Command, Stdio};
    let Ok(exe) = std::env::current_exe() else {
        eprintln!("hyprcorrect: cannot find own executable to launch prefs");
        return;
    };
    let mut cmd = Command::new(&exe);
    cmd.arg("prefs");
    if let Some(section) = section {
        cmd.env("HYPRCORRECT_PREFS_SECTION", section);
    }
    let result = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Err(e) = result {
        eprintln!("hyprcorrect: could not launch prefs window: {e}");
    }
}

/// Parse an optional chord string. Empty input → `None` (unbound);
/// a non-empty string that fails to parse is reported and treated
/// as unbound rather than killing the daemon.
/// Collect every action chord into a single slice for `capture::start`'s
/// suppression list. Chords that aren't bound (sentence/review unbound)
/// don't show up.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn active_chords(
    word: &hyprcorrect_core::Chord,
    sentence: &Option<hyprcorrect_core::Chord>,
    review: &Option<hyprcorrect_core::Chord>,
    review_llm: &Option<hyprcorrect_core::Chord>,
) -> Vec<hyprcorrect_core::Chord> {
    let mut out = vec![word.clone()];
    for c in [sentence, review, review_llm].into_iter().flatten() {
        out.push(c.clone());
    }
    out
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn parse_optional_chord(raw: &str) -> Option<hyprcorrect_core::Chord> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match hyprcorrect_core::Chord::parse(trimmed) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("hyprcorrect: ignoring invalid chord '{trimmed}': {e}");
            None
        }
    }
}

/// Correct the buffer's last word in place. Routes through the
/// configured default provider: Spellbook for fast offline lookups,
/// LLM when the user wants context-aware fixes (homophones, typos
/// that depend on the surrounding sentence). When the buffer has
/// nothing to work with (focus moved, caret-moving key, paste, …)
/// we fall back to the clipboard path. Per `DESIGN.md`'s secondary
/// mode.
///
/// If the picked word (`word_at_caret`) comes back fine from the
/// provider, the search widens to nearby words within ~30 chars
/// of the caret — this covers the common case where a held arrow
/// or a mouse click has drifted the buffer caret a few chars from
/// the visible cursor.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(clippy::too_many_arguments)] // provider-routing + emit-timing context
fn fix_last_word(
    buffer: &mut hyprcorrect_core::Buffer,
    default: hyprcorrect_core::ProviderId,
    llm: Option<&hyprcorrect_core::LlmProvider>,
    languagetool: Option<&hyprcorrect_core::LanguageToolProvider>,
    provider: &hyprcorrect_core::OfflineProvider,
    pause_per_backspace_ms: u32,
    pause_per_char_ms: u32,
    lt_fallback: bool,
) {
    use std::sync::atomic::Ordering;

    use crate::backend::emit;

    let Some(at) = buffer.word_at_caret() else {
        eprintln!("hyprcorrect: word-fix — buffer has no word at caret, trying clipboard fallback");
        fix_via_clipboard(provider);
        return;
    };
    eprintln!(
        "hyprcorrect: word-fix on {:?} (before={}, after={}; default={default:?})",
        at.word, at.chars_before_caret, at.chars_after_caret,
    );
    let bracketed = format_word_with_caret(&at.word, at.chars_before_caret, at.chars_after_caret);
    let Some(plan) = pick_word_fix(
        buffer,
        &at,
        &bracketed,
        default,
        llm,
        languagetool,
        provider,
        lt_fallback,
    ) else {
        return;
    };
    // Guard against a provider (in practice the LLM) returning a
    // conversational reply instead of the corrected word — emitting that
    // verbatim would dump a sentence into the user's text.
    if !is_plausible_word_fix(&plan.original, &plan.fix) {
        eprintln!(
            "hyprcorrect: word-fix — {:?} produced an implausible result {:?}; not emitting",
            plan.provider, plan.fix
        );
        notify_warning(
            "Unexpected reply",
            "The provider returned a message instead of a word — nothing was changed.",
        );
        return;
    }
    let word_chars = plan.original.chars().count();
    // Compute chars from end-of-line to end of target word. End-
    // anchored emit dodges the held-arrow caret-drift trap the
    // direct-offset path falls into.
    let chars_from_end = buffer.text()[plan.byte_end..].chars().count();
    eprintln!(
        "hyprcorrect: word-fix emit — chars_from_end={chars_from_end}, backspace {word_chars} chars, insert {:?}",
        plan.fix
    );
    // Lock focus to the (already-focused) target window for the emit so a mouse
    // move can't divert keystrokes mid-retype. Restored when `_lock` drops.
    let _lock = FocusLock::engage();
    match emit::anchored_replace_with_delay(
        chars_from_end,
        word_chars,
        &plan.fix,
        pause_per_backspace_ms,
        pause_per_char_ms,
    ) {
        Ok(()) => {
            buffer.apply_at_word(plan.byte_start, plan.byte_end, &plan.fix);
            // The fix landed; the buffer's caret now agrees with
            // the on-screen cursor (both at end of `plan.fix`),
            // so a click-driven "scan the whole buffer" flag is
            // no longer warranted.
            crate::backend::capture::caret_suspect_flag().store(false, Ordering::Relaxed);
            notify_info(
                &format!("Corrected ({})", provider_label(plan.provider)),
                &format!("{} → \"{}\"", plan.label, plan.fix),
            );
        }
        Err(e) => eprintln!("hyprcorrect: {e}"),
    }
}

/// Where in the buffer to land an edit and what to type. Built by
/// [`pick_word_fix`]; consumed by [`fix_last_word`] to drive the
/// emit + buffer-apply pair.
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct WordFixPlan {
    /// The original word being replaced (for the toast and so the
    /// emit knows how many BackSpaces to fire).
    original: String,
    /// The corrected text to type.
    fix: String,
    /// Byte start of the original word in the buffer's text.
    byte_start: usize,
    /// Byte end (exclusive) of the original word in the buffer —
    /// the emit converts this to "chars from end-of-line" and
    /// uses `End`+`Left` to land the cursor reliably.
    byte_end: usize,
    /// Toast label — for the primary pick this is the
    /// caret-bracketed original (e.g., `"spa[g]heti"`); for a
    /// nearby pick it's the plain word.
    label: String,
    /// Which provider actually produced `fix`. Set at the call site
    /// inside [`pick_word_fix`] so fallback paths (e.g. LLM error →
    /// spellbook) report the provider that *succeeded*, not the
    /// configured default. Surfaced in the success toast.
    provider: hyprcorrect_core::ProviderId,
}

/// How far (in chars) from the caret to consider "nearby" when
/// the primary word comes back fine. Tuned so a held arrow that
/// over-or-undershoots by a word or two still lands a fix, but
/// we don't go fishing for typos halfway across the buffer.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const NEARBY_WORD_MAX_CHARS: i32 = 30;

/// Build a [`WordFixPlan`] for the word the user wants fixed.
/// First tries the primary pick (`word_at_caret`) through the
/// configured provider. If that comes back fine, widens to
/// nearby words around the caret — within
/// [`NEARBY_WORD_MAX_CHARS`] by default, or the entire buffer
/// when [`capture::caret_suspect_flag`] is set (a recent mouse
/// click means the buffer caret may be far from the visible
/// cursor). On an LLM miss, falls back to LanguageTool when
/// `lt_fallback` is set and a server is configured, otherwise
/// straight to Spellbook — so the chord never silently no-ops.
/// Returns `None` after firing the "nothing to do" toast and
/// logging — callers exit without emitting.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(clippy::too_many_arguments)] // provider-routing context; see also capture.rs
fn pick_word_fix(
    buffer: &hyprcorrect_core::Buffer,
    at: &hyprcorrect_core::WordAtCaret,
    bracketed: &str,
    default: hyprcorrect_core::ProviderId,
    llm: Option<&hyprcorrect_core::LlmProvider>,
    languagetool: Option<&hyprcorrect_core::LanguageToolProvider>,
    spell: &hyprcorrect_core::OfflineProvider,
    lt_fallback: bool,
) -> Option<WordFixPlan> {
    use std::sync::atomic::Ordering;

    use crate::backend::capture;
    use hyprcorrect_core::ProviderId;

    let sentence = buffer
        .sentence_at_caret()
        .map(|s| s.sentence)
        .unwrap_or_else(|| at.word.clone());

    let use_llm = default == ProviderId::Llm && llm.is_some();
    let primary = primary_target(buffer, at);
    // A recent mouse click leaves the buffer caret stale; scan
    // every word in the buffer rather than just the ±30-char
    // window around the (probably wrong) caret position.
    let caret_suspect = capture::caret_suspect_flag().load(Ordering::Relaxed);
    let max_chars = if caret_suspect {
        i32::MAX
    } else {
        NEARBY_WORD_MAX_CHARS
    };
    if caret_suspect {
        eprintln!(
            "hyprcorrect: word-fix — caret suspect (recent click); scan widened to whole buffer"
        );
    }

    // Primary pass: the picked word at the caret. Each branch either
    // returns a finished plan / "nothing to do", or sets `run_lt` to
    // route through LanguageTool next — as the selected provider, or as
    // the LLM-miss fallback when the user enabled it. Anything not
    // handled here drops to the Spellbook tail below.
    let mut run_lt = false;
    if use_llm {
        let llm = llm.expect("checked above");
        match llm.fix_word_in_context(&sentence, &at.word) {
            Ok(corrected) => {
                let corrected = corrected.trim().to_string();
                if !corrected.is_empty() && corrected != at.word {
                    eprintln!(
                        "hyprcorrect: word-fix — LLM picked {:?} for {:?}",
                        corrected, at.word
                    );
                    return Some(plan_for(&primary, corrected, bracketed, ProviderId::Llm));
                }
                eprintln!(
                    "hyprcorrect: word-fix — LLM left {:?} unchanged, scanning nearby",
                    at.word
                );
                // Fall through to nearby scan via LLM.
                if let Some(plan) = scan_nearby_llm(buffer, &sentence, &at.word, llm, max_chars) {
                    return Some(plan);
                }
                notify_info(
                    "Nothing to correct",
                    &format!("LLM thinks {bracketed} (and nearby) are fine in context."),
                );
                return None;
            }
            Err(e) => {
                // Prefer LanguageTool over Spellbook when the user
                // enabled the fallback and a server is configured.
                if lt_fallback && languagetool.is_some() {
                    eprintln!(
                        "hyprcorrect: word-fix LLM failed ({e}) — trying LanguageTool fallback"
                    );
                    run_lt = true;
                } else {
                    eprintln!("hyprcorrect: word-fix LLM failed ({e}) — falling back to spellbook");
                    notify_warning(
                        "LLM unavailable",
                        &format!("Falling back to Spellbook for {bracketed}."),
                    );
                }
            }
        }
    } else if default == ProviderId::Llm {
        if lt_fallback && languagetool.is_some() {
            eprintln!(
                "hyprcorrect: word-fix — default provider is LLM but no key configured; trying LanguageTool fallback"
            );
            run_lt = true;
        } else {
            eprintln!(
                "hyprcorrect: word-fix — default provider is LLM but no key configured; falling back to spellbook"
            );
            notify_warning(
                "LLM API key not set",
                "Open Preferences → Providers → LLM and add your API key.",
            );
        }
    } else if default == ProviderId::LanguageTool {
        if languagetool.is_some() {
            run_lt = true;
        } else {
            eprintln!(
                "hyprcorrect: word-fix — default provider is LanguageTool but it isn't configured; falling back to spellbook"
            );
            notify_warning(
                "LanguageTool not configured",
                "Open Preferences → Providers → LanguageTool, enable it, and set the URL.",
            );
        }
    }

    if run_lt && let Some(lt) = languagetool {
        match try_languagetool_word_fix(buffer, at, &sentence, &primary, bracketed, lt, max_chars) {
            LtWordOutcome::Fixed(plan) => return Some(plan),
            LtWordOutcome::NothingFound => {
                notify_info(
                    "Nothing to correct",
                    &format!("LanguageTool thinks {bracketed} (and nearby) are fine."),
                );
                return None;
            }
            LtWordOutcome::Failed => {
                notify_warning(
                    "LanguageTool unavailable",
                    &format!("Falling back to Spellbook for {bracketed}."),
                );
                // Fall through to spellbook.
            }
        }
    }

    if let Some(fix) = spellbook_pick(spell, &at.word) {
        return Some(plan_for(&primary, fix, bracketed, ProviderId::Spellbook));
    }
    eprintln!(
        "hyprcorrect: word-fix — spellbook found no error in {:?}, scanning nearby",
        at.word
    );
    if let Some(plan) = scan_nearby_spellbook(buffer, &at.word, spell, max_chars) {
        return Some(plan);
    }
    notify_warning(
        "Nothing to correct",
        &format!("Spellbook didn't find an error in {bracketed} or nearby."),
    );
    None
}

/// Result of a LanguageTool word-fix attempt — see
/// [`try_languagetool_word_fix`]. The caller turns this into the user
/// toasts so the wording can reflect whether LanguageTool was the
/// selected provider or the LLM-miss fallback.
#[cfg(any(target_os = "linux", target_os = "macos"))]
enum LtWordOutcome {
    /// LanguageTool produced a fix — here's the emit plan.
    Fixed(WordFixPlan),
    /// LanguageTool ran cleanly but flagged nothing to change.
    NothingFound,
    /// The LanguageTool call itself failed (network, server, …).
    Failed,
}

/// Try to fix the caret word — and, failing that, a nearby word —
/// through LanguageTool, sending the whole `sentence` as context so
/// confusable rules (their/there/they're, your/you're) can fire. Picks
/// the first match whose span overlaps the target's position in the
/// sentence. Logs its own decisions but leaves user-facing toasts to the
/// caller. Shared by the `default == LanguageTool` path and the
/// LLM-miss fallback.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn try_languagetool_word_fix(
    buffer: &hyprcorrect_core::Buffer,
    at: &hyprcorrect_core::WordAtCaret,
    sentence: &str,
    primary: &PrimaryTarget,
    bracketed: &str,
    lt: &hyprcorrect_core::LanguageToolProvider,
    max_chars: i32,
) -> LtWordOutcome {
    use hyprcorrect_core::ProviderId;
    // `primary_sentence` carries the buffer-byte range, so the target's
    // in-sentence position is a subtraction — `PrimaryTarget` already
    // holds buffer-byte positions.
    let primary_sentence = buffer.sentence_containing(buffer.caret());
    let target_in_sentence = primary_sentence
        .as_ref()
        .and_then(|s| word_in_sentence_bytes(s, primary.byte_start, primary.byte_end));
    // Per-sentence corrections cache shared with the nearby scan: any
    // nearby word whose containing sentence we've already checked reuses
    // the result.
    let mut sentence_cache = SentenceCache::new();
    match lt.check_text(sentence) {
        Ok(corrections) => {
            if let Some(s) = primary_sentence.as_ref() {
                sentence_cache.seed(s.buffer_byte_start, corrections.clone());
            }
            let pick = match &target_in_sentence {
                Some(range) => first_overlap_suggestion(&corrections, range),
                // No sentence context (very short buffer, etc.): take
                // whatever LT found first.
                None => corrections
                    .iter()
                    .find_map(|c| c.suggestions.first().cloned()),
            };
            if let Some(fix) = pick
                && fix != at.word
            {
                eprintln!(
                    "hyprcorrect: word-fix — LT picked {:?} for {:?} (with sentence context)",
                    fix, at.word
                );
                return LtWordOutcome::Fixed(plan_for(
                    primary,
                    fix,
                    bracketed,
                    ProviderId::LanguageTool,
                ));
            }
            eprintln!(
                "hyprcorrect: word-fix — LT left {:?} unchanged, scanning nearby",
                at.word
            );
            match scan_nearby_lt(buffer, &at.word, lt, max_chars, &mut sentence_cache) {
                Some(plan) => LtWordOutcome::Fixed(plan),
                None => LtWordOutcome::NothingFound,
            }
        }
        Err(e) => {
            eprintln!("hyprcorrect: word-fix LT failed ({e})");
            LtWordOutcome::Failed
        }
    }
}

/// First spellbook suggestion for `word`, or `None` if the word
/// isn't flagged.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn spellbook_pick(spell: &hyprcorrect_core::OfflineProvider, word: &str) -> Option<String> {
    let correction = spell.check_text(word).into_iter().next()?;
    correction.suggestions.into_iter().next()
}

/// Walk words near the buffer caret and return a plan for the
/// first one Spellbook flags. Skips the primary word (already
/// tried) and any word beyond `max_chars` from the caret.
/// Caller passes `i32::MAX` to scan the entire buffer (used
/// when a recent mouse click made the caret position unreliable).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn scan_nearby_spellbook(
    buffer: &hyprcorrect_core::Buffer,
    primary_word: &str,
    spell: &hyprcorrect_core::OfflineProvider,
    max_chars: i32,
) -> Option<WordFixPlan> {
    for nw in buffer.words_near_caret() {
        if nw.word == primary_word && nw.caret_offset_chars.abs() <= 1 {
            // Skip the primary pick — `word_at_caret`'s entry is
            // always at distance 0 or 1 (caret at end / inside).
            continue;
        }
        if nw.caret_offset_chars.abs() > max_chars {
            break; // sorted by distance, nothing closer follows
        }
        if let Some(fix) = spellbook_pick(spell, &nw.word) {
            eprintln!(
                "hyprcorrect: word-fix — nearby spellbook hit {:?} → {:?} (offset {})",
                nw.word, fix, nw.caret_offset_chars
            );
            return Some(WordFixPlan {
                original: nw.word.clone(),
                fix,
                byte_start: nw.byte_start,
                byte_end: nw.byte_end,
                label: nw.word,
                provider: hyprcorrect_core::ProviderId::Spellbook,
            });
        }
    }
    None
}

/// Walk words near the buffer caret, ask the LLM once per word
/// to fix-in-context, and return a plan for the first one the
/// LLM rewrites. Caps the per-chord LLM cost at a few extra
/// round-trips — same rate-limit / latency profile as a manual
/// re-trigger by the user. `max_chars` is `i32::MAX` for the
/// post-mouse-click "scan everything" mode, otherwise the normal
/// ±30-char window around the caret.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn scan_nearby_llm(
    buffer: &hyprcorrect_core::Buffer,
    sentence: &str,
    primary_word: &str,
    llm: &hyprcorrect_core::LlmProvider,
    max_chars: i32,
) -> Option<WordFixPlan> {
    const MAX_NEARBY_LLM_CALLS: usize = 4;
    let mut calls = 0;
    for nw in buffer.words_near_caret() {
        if nw.word == primary_word && nw.caret_offset_chars.abs() <= 1 {
            continue;
        }
        if nw.caret_offset_chars.abs() > max_chars {
            break;
        }
        if calls >= MAX_NEARBY_LLM_CALLS {
            break;
        }
        calls += 1;
        match llm.fix_word_in_context(sentence, &nw.word) {
            Ok(corrected) => {
                let corrected = corrected.trim().to_string();
                if corrected.is_empty() || corrected == nw.word {
                    continue;
                }
                eprintln!(
                    "hyprcorrect: word-fix — nearby LLM hit {:?} → {:?} (offset {})",
                    nw.word, corrected, nw.caret_offset_chars
                );
                return Some(WordFixPlan {
                    original: nw.word.clone(),
                    fix: corrected,
                    byte_start: nw.byte_start,
                    byte_end: nw.byte_end,
                    label: nw.word,
                    provider: hyprcorrect_core::ProviderId::Llm,
                });
            }
            Err(e) => {
                eprintln!("hyprcorrect: word-fix nearby LLM call failed ({e}) — stopping scan");
                return None;
            }
        }
    }
    None
}

/// Walk words near the buffer caret and return a plan for the
/// first one LanguageTool flags, with the *containing sentence* sent
/// as context — same machinery as the primary path so homonym /
/// confusable rules can fire for nearby words too.
///
/// `cache` is a per-sentence corrections cache seeded by the caller
/// with the caret-sentence result. Multiple nearby words that share
/// a sentence (the common case for ±30-char scans) reuse one LT
/// round-trip. Capped by [`MAX_NEARBY_LT_SENTENCES`] *unique*
/// sentences per chord — sentences are bigger payloads than single
/// words, so the cap is lower than the old per-word one.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn scan_nearby_lt(
    buffer: &hyprcorrect_core::Buffer,
    primary_word: &str,
    lt: &hyprcorrect_core::LanguageToolProvider,
    max_chars: i32,
    cache: &mut SentenceCache,
) -> Option<WordFixPlan> {
    const MAX_NEARBY_LT_SENTENCES: usize = 2;
    for nw in buffer.words_near_caret() {
        if nw.word == primary_word && nw.caret_offset_chars.abs() <= 1 {
            continue;
        }
        if nw.caret_offset_chars.abs() > max_chars {
            break;
        }
        let Some(sentence) = buffer.sentence_containing(nw.byte_start) else {
            continue;
        };
        // Bail out only when we'd need to call LT for a *new*
        // sentence we haven't seen yet and we've already used our
        // budget. Already-cached sentences are free to consult.
        if !cache.has(sentence.buffer_byte_start)
            && cache.sentences_fetched() >= MAX_NEARBY_LT_SENTENCES
        {
            break;
        }
        let corrections = match cache.get_or_fetch(&sentence, lt) {
            Ok(cs) => cs,
            Err(e) => {
                eprintln!("hyprcorrect: word-fix nearby LT call failed ({e}) — stopping scan");
                return None;
            }
        };
        let Some(target) = word_in_sentence_bytes(&sentence, nw.byte_start, nw.byte_end) else {
            continue;
        };
        if let Some(fix) = first_overlap_suggestion(corrections, &target)
            && fix != nw.word
        {
            eprintln!(
                "hyprcorrect: word-fix — nearby LT hit {:?} → {:?} (offset {}, with context)",
                nw.word, fix, nw.caret_offset_chars
            );
            return Some(WordFixPlan {
                original: nw.word.clone(),
                fix,
                byte_start: nw.byte_start,
                byte_end: nw.byte_end,
                label: nw.word,
                provider: hyprcorrect_core::ProviderId::LanguageTool,
            });
        }
    }
    None
}

/// Per-sentence corrections cache shared between the primary LT
/// pass and [`scan_nearby_lt`]. Keyed by the sentence's
/// `buffer_byte_start` so the same buffer region never gets two
/// LT round-trips per chord.
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct SentenceCache {
    entries: std::collections::HashMap<usize, Vec<hyprcorrect_core::Correction>>,
    fetched: usize,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl SentenceCache {
    fn new() -> Self {
        Self {
            entries: std::collections::HashMap::new(),
            fetched: 0,
        }
    }

    /// Pre-populate with a sentence we've already checked (e.g. the
    /// caret-sentence the primary path queried). Counts toward the
    /// fetched total so the nearby-scan cap accounts for it.
    fn seed(&mut self, buffer_byte_start: usize, corrections: Vec<hyprcorrect_core::Correction>) {
        if self
            .entries
            .insert(buffer_byte_start, corrections)
            .is_none()
        {
            self.fetched += 1;
        }
    }

    fn has(&self, buffer_byte_start: usize) -> bool {
        self.entries.contains_key(&buffer_byte_start)
    }

    fn sentences_fetched(&self) -> usize {
        self.fetched
    }

    /// Returns a shared reference to the cached corrections for
    /// `sentence`, fetching via `lt` (and caching the result) if
    /// the sentence isn't already known.
    fn get_or_fetch(
        &mut self,
        sentence: &hyprcorrect_core::Sentence,
        lt: &hyprcorrect_core::LanguageToolProvider,
    ) -> Result<&Vec<hyprcorrect_core::Correction>, hyprcorrect_core::LanguageToolError> {
        use std::collections::hash_map::Entry;
        let key = sentence.buffer_byte_start;
        match self.entries.entry(key) {
            Entry::Occupied(e) => Ok(e.into_mut()),
            Entry::Vacant(e) => {
                let cs = lt.check_text(&sentence.sentence)?;
                self.fetched += 1;
                Ok(e.insert(cs))
            }
        }
    }
}

/// Byte range of `[buffer_start, buffer_end)` translated into
/// `sentence`-relative bytes. Returns `None` when the buffer range
/// doesn't lie entirely within the sentence — defensive, in practice
/// nearby words always fall fully inside their containing sentence.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn word_in_sentence_bytes(
    sentence: &hyprcorrect_core::Sentence,
    buffer_start: usize,
    buffer_end: usize,
) -> Option<std::ops::Range<usize>> {
    if buffer_start < sentence.buffer_byte_start || buffer_end > sentence.buffer_byte_end {
        return None;
    }
    Some((buffer_start - sentence.buffer_byte_start)..(buffer_end - sentence.buffer_byte_start))
}

/// The primary edit target derived from `word_at_caret`: the
/// word's byte range in the buffer. Used to build the plan for
/// the caret's-actual-word case.
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct PrimaryTarget {
    original: String,
    byte_start: usize,
    byte_end: usize,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn primary_target(
    buffer: &hyprcorrect_core::Buffer,
    at: &hyprcorrect_core::WordAtCaret,
) -> PrimaryTarget {
    // `WordAtCaret::chars_before_caret` counts only word chars,
    // never the `trailing` whitespace/punctuation between the
    // word's end and the caret. We have to step past `trailing`
    // explicitly when walking back from the caret to find the
    // word's start — otherwise a chord fired after typing
    // "disambiguat " (note the trailing space) lands byte_start
    // one position into the word, leaves the first letter behind
    // on emit, and the caret-end backspace burst strips the space
    // instead of the leading letter. Same bug shape for any
    // captured trailing punctuation.
    let caret = buffer.caret();
    let text = buffer.text();
    let trailing_chars = at.trailing.chars().count();
    let byte_start = char_step_left(text, caret, at.chars_before_caret + trailing_chars);
    let byte_end = byte_start + at.word.len();
    PrimaryTarget {
        original: at.word.clone(),
        byte_start,
        byte_end,
    }
}

/// First LanguageTool suggestion whose match span overlaps
/// `target` (byte range inside the sentence we sent). Half-open
/// overlap: `a.start < b.end && b.start < a.end`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn first_overlap_suggestion(
    corrections: &[hyprcorrect_core::Correction],
    target: &std::ops::Range<usize>,
) -> Option<String> {
    corrections.iter().find_map(|c| {
        let overlaps = c.span.start < target.end && target.start < c.span.end;
        if overlaps {
            c.suggestions.first().cloned()
        } else {
            None
        }
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn plan_for(
    primary: &PrimaryTarget,
    fix: String,
    bracketed: &str,
    provider: hyprcorrect_core::ProviderId,
) -> WordFixPlan {
    WordFixPlan {
        original: primary.original.clone(),
        fix,
        byte_start: primary.byte_start,
        byte_end: primary.byte_end,
        label: bracketed.to_string(),
        provider,
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn char_step_left(text: &str, from: usize, steps: usize) -> usize {
    let mut pos = from;
    for _ in 0..steps {
        if pos == 0 {
            break;
        }
        pos = text[..pos].char_indices().next_back().map_or(0, |(i, _)| i);
    }
    pos
}

/// Render a word with the daemon's caret position bracketed,
/// e.g. `spagheti` + caret after the 3rd char → `"spa[g]heti"`.
/// Caret at the very end falls back to `"word|"`; at the very
/// start to `"|word"`. Used in the fix-word success toast so the
/// user can confirm exactly which character the daemon thinks
/// the caret is on, since the TUI's visible cursor block can sit
/// a position over.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn format_word_with_caret(word: &str, chars_before: usize, chars_after: usize) -> String {
    let chars: Vec<char> = word.chars().collect();
    if chars_after == 0 {
        return format!("{word}|");
    }
    if chars_before >= chars.len() {
        return format!("{word}|");
    }
    let before: String = chars[..chars_before].iter().collect();
    let on: char = chars[chars_before];
    let after: String = chars[chars_before + 1..].iter().collect();
    if chars_before == 0 {
        format!("|[{on}]{after}")
    } else {
        format!("{before}[{on}]{after}")
    }
}

/// Clipboard fallback for the empty-buffer case. Best-effort —
/// doesn't work in terminals, and only in apps where
/// `Ctrl+Shift+Left` selects the previous word. Failures are
/// logged but never fatal.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn fix_via_clipboard(provider: &hyprcorrect_core::OfflineProvider) {
    use crate::backend::clipboard;
    let word = match clipboard::copy_previous_word() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("hyprcorrect: clipboard fallback skipped — {e}");
            return;
        }
    };
    let trimmed = word.trim();
    if trimmed.is_empty() {
        return;
    }
    let Some(correction) = provider.check_text(trimmed).into_iter().next() else {
        return;
    };
    let Some(fix) = correction.suggestions.into_iter().next() else {
        return;
    };
    // The selection is still live — typing replaces it in place.
    // We restore any leading/trailing whitespace from the original
    // wl-paste payload so we don't trim the user's spacing.
    let leading_ws_len = word.len() - word.trim_start().len();
    let trailing_ws_len = word.len() - word.trim_end().len();
    let mut replacement = String::with_capacity(word.len());
    replacement.push_str(&word[..leading_ws_len]);
    replacement.push_str(&fix);
    replacement.push_str(&word[word.len() - trailing_ws_len..]);
    if let Err(e) = clipboard::type_replacement(&replacement) {
        eprintln!("hyprcorrect: clipboard fallback type-back failed: {e}");
    }
}

/// Compute the smart provider's suggestion for the focused window's
/// last sentence, write a review request, and spawn the popup. The
/// daemon does no emit here — the popup's exit signals back with a
/// `review-apply` / `review-cancel` action and the apply path below
/// finishes the job.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn start_review(
    address: &str,
    buffer: &hyprcorrect_core::Buffer,
    provider: &hyprcorrect_core::OfflineProvider,
    smart: hyprcorrect_core::ProviderId,
    llm: Option<&hyprcorrect_core::LlmProvider>,
    languagetool: Option<&hyprcorrect_core::LanguageToolProvider>,
    lt_fallback: bool,
) {
    use hyprcorrect_core::runtime::{ReviewRequest, write_review_request};
    let Some(at) = buffer.sentence_at_caret() else {
        return;
    };
    eprintln!(
        "hyprcorrect: review-build — original ({} chars): {:?}",
        at.sentence.chars().count(),
        at.sentence
    );

    // Show the popup immediately in a "Checking…" state (it displays the
    // original and polls for the finished request) so a slow provider —
    // an LLM round-trip especially — doesn't leave the chord feeling
    // dead. Then correct and write the finished request.
    let (screen_width, screen_height) = focused_monitor_size();
    let llm_available = llm.is_some();

    // Build the finished (pending: false) request for a correction result.
    // Shared by the fast path (resolved before any popup opens) and the slow
    // path (resolved after "Checking…"), so both construct it identically.
    let finished =
        |corrected: String,
         used_provider: hyprcorrect_core::ProviderId,
         suggestions: Vec<hyprcorrect_core::runtime::WordSuggestions>| {
            ReviewRequest {
                original: at.sentence.clone(),
                corrected,
                trailing: at.trailing.clone(),
                chars_before_caret: at.chars_before_caret,
                chars_after_caret: at.chars_after_caret,
                window_address: address.to_string(),
                suggestions,
                pending: false,
                screen_width,
                screen_height,
                llm_available,
                // Hide "Ask LLM" when the LLM itself produced this — but not when
                // an LLM miss fell back to LanguageTool/Spellbook (still escalatable).
                from_llm: used_provider == hyprcorrect_core::ProviderId::Llm,
            }
        };
    // Toast shown when there's nothing to correct — mirrors the word/sentence
    // chords so a no-op never just flashes (or silently swallows) the popup.
    let toast_nothing = |used_provider| {
        notify_info(
            "Nothing to correct",
            &format!(
                "{} thinks the sentence is fine.",
                provider_label(used_provider)
            ),
        );
    };

    // Correct on a worker thread so the "Checking…" popup opens *only* when the
    // provider is slow. A fast/offline result that needs no change then shows a
    // toast with no window flash; a slow LLM still gets the instant "Checking…"
    // so the chord never feels dead. Scoped thread → it can borrow the
    // providers without cloning them.
    let sentence = at.sentence.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let _ = tx.send(correct_sentence_with_suggestions(
                &sentence,
                smart,
                llm,
                languagetool,
                provider,
                lt_fallback,
            ));
        });

        // Grace window for a fast provider to answer before we fall back to the
        // eager popup. Long enough for the offline spellbook / a snappy
        // LanguageTool; short enough that an LLM round-trip still pops
        // "Checking…" almost immediately.
        const FAST_GRACE: std::time::Duration = std::time::Duration::from_millis(150);

        match rx.recv_timeout(FAST_GRACE) {
            Ok((corrected, used_provider, suggestions)) => {
                // Resolved fast — decide before opening anything.
                if corrected == at.sentence {
                    eprintln!("hyprcorrect: review-build — no changes; toast, no popup");
                    toast_nothing(used_provider);
                    return;
                }
                eprintln!(
                    "hyprcorrect: review-build — corrected ({} chars): {:?}, {} suggestion set(s)",
                    corrected.chars().count(),
                    corrected,
                    suggestions.len(),
                );
                if let Err(e) =
                    write_review_request(&finished(corrected, used_provider, suggestions))
                {
                    eprintln!("hyprcorrect: could not write finished review request: {e}");
                    return;
                }
                spawn_review_window();
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Slow provider (LLM): show "Checking…" now so the chord doesn't
                // feel dead, then finish when the worker returns.
                let pending = ReviewRequest {
                    original: at.sentence.clone(),
                    corrected: at.sentence.clone(),
                    trailing: at.trailing.clone(),
                    chars_before_caret: at.chars_before_caret,
                    chars_after_caret: at.chars_after_caret,
                    window_address: address.to_string(),
                    suggestions: Vec::new(),
                    pending: true,
                    screen_width,
                    screen_height,
                    llm_available,
                    // Unknown until the correction lands; the button stays
                    // available through the brief "Checking…" state.
                    from_llm: false,
                };
                if let Err(e) = write_review_request(&pending) {
                    eprintln!("hyprcorrect: could not write pending review request: {e}");
                    return;
                }
                spawn_review_window();

                let (corrected, used_provider, suggestions) = match rx.recv() {
                    Ok(result) => result,
                    Err(_) => {
                        eprintln!("hyprcorrect: review worker vanished before returning");
                        return;
                    }
                };
                if corrected == at.sentence {
                    // The popup is already up; the finished no-op request below
                    // closes it, and the toast explains why.
                    eprintln!("hyprcorrect: review-build — no changes; popup will close");
                    toast_nothing(used_provider);
                } else {
                    eprintln!(
                        "hyprcorrect: review-build — corrected ({} chars): {:?}, {} suggestion set(s)",
                        corrected.chars().count(),
                        corrected,
                        suggestions.len(),
                    );
                }
                // Always write the finished request (pending: false) — even on a
                // no-op, so the popup stops "Checking…" and closes itself.
                if let Err(e) =
                    write_review_request(&finished(corrected, used_provider, suggestions))
                {
                    eprintln!("hyprcorrect: could not write finished review request: {e}");
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                eprintln!("hyprcorrect: review worker disconnected before sending");
            }
        }
    });
}

/// Handle the `review-llm` action: re-run the *open* review's original
/// sentence through the LLM and rewrite the request so the popup reloads
/// with the LLM's correction + suggestions. With no LLM configured,
/// open Preferences at the Providers section instead so the user can add
/// one (the popup keeps its "Ask LLM" button — progressive setup).
// Linux-only: invoked solely from the (Linux-gated) daemon dispatch in
// `run_daemon`, and calls Linux-only helpers (notify_warning,
// correct_sentence_with_suggestions).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn reprocess_review_with_llm(
    llm: Option<&hyprcorrect_core::LlmProvider>,
    languagetool: Option<&hyprcorrect_core::LanguageToolProvider>,
    provider: &hyprcorrect_core::OfflineProvider,
    lt_fallback: bool,
) {
    use hyprcorrect_core::ProviderId;
    use hyprcorrect_core::runtime::{read_review_request, write_review_request};

    let Ok(Some(mut req)) = read_review_request() else {
        return;
    };
    if llm.is_none() {
        eprintln!("hyprcorrect: review-llm — no LLM configured; opening Preferences");
        notify_warning(
            "LLM not configured",
            "Add an LLM API key in Preferences → Providers to escalate corrections.",
        );
        spawn_prefs_window_section(Some("providers"));
        return;
    }

    // Flip to pending so the popup shows "Checking…" while the LLM runs.
    req.pending = true;
    req.llm_available = true;
    if let Err(e) = write_review_request(&req) {
        eprintln!("hyprcorrect: review-llm — could not write pending request: {e}");
        return;
    }

    let (corrected, used, suggestions) = correct_sentence_with_suggestions(
        &req.original,
        ProviderId::Llm,
        llm,
        languagetool,
        provider,
        lt_fallback,
    );
    eprintln!(
        "hyprcorrect: review-llm — LLM corrected ({} chars): {:?}, {} suggestion set(s)",
        corrected.chars().count(),
        corrected,
        suggestions.len(),
    );
    req.corrected = corrected;
    req.suggestions = suggestions;
    req.pending = false;
    // The escalation succeeded only if the LLM actually produced it; if it
    // missed and fell back, keep "Ask LLM" available for another try.
    req.from_llm = used == ProviderId::Llm;
    if let Err(e) = write_review_request(&req) {
        eprintln!("hyprcorrect: review-llm — could not write finished request: {e}");
    }
}

/// Logical width (points) of the focused Hyprland monitor — its pixel
/// width and *usable* height in logical points — physical size divided by
/// scale, with the height minus the monitor's reserved areas (e.g. a top
/// waybar). The review popup sizes itself up to half the screen wide and up
/// to the usable height tall, so it never slides under the bar. Returns
/// `(0.0, 0.0)` when hyprctl is unavailable or the output can't be parsed;
/// the popup then uses fixed fallback caps.
#[cfg(target_os = "linux")]
fn focused_monitor_size() -> (f32, f32) {
    use std::process::Command;
    let Ok(out) = Command::new("hyprctl").args(["monitors", "-j"]).output() else {
        return (0.0, 0.0);
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return (0.0, 0.0);
    };
    let Some(monitors) = json.as_array() else {
        return (0.0, 0.0);
    };
    let monitor = monitors
        .iter()
        .find(|m| m["focused"].as_bool() == Some(true))
        .or_else(|| monitors.first());
    let Some(monitor) = monitor else {
        return (0.0, 0.0);
    };
    let width = monitor["width"].as_f64().unwrap_or(0.0) as f32;
    let height = monitor["height"].as_f64().unwrap_or(0.0) as f32;
    let scale = monitor["scale"].as_f64().unwrap_or(1.0) as f32;
    // `reserved` is [left, top, right, bottom] in *logical* points (it matches
    // the waybar's configured logical height), unlike width/height which are
    // physical — so divide the size by scale but subtract reserved as-is.
    let reserved = monitor["reserved"].as_array();
    let reserved_at = |i: usize| {
        reserved
            .and_then(|r| r.get(i))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0) as f32
    };
    let scale = if scale > 0.0 { scale } else { 1.0 };
    let logical_width = width / scale;
    let usable_height = (height / scale - reserved_at(1) - reserved_at(3)).max(0.0);
    (logical_width, usable_height)
}

/// Read the current `input:follow_mouse` value via hyprctl, as a string (e.g.
/// `"1"`). `None` if hyprctl is unavailable or the value can't be parsed.
#[cfg(target_os = "linux")]
fn read_follow_mouse() -> Option<String> {
    use std::process::Command;
    let out = Command::new("hyprctl")
        .args(["getoption", "input:follow_mouse", "-j"])
        .output()
        .ok()?;
    let json = serde_json::from_slice::<serde_json::Value>(&out.stdout).ok()?;
    json["int"].as_i64().map(|n| n.to_string())
}

/// Set `input:follow_mouse` via hyprctl. Returns whether it succeeded.
#[cfg(target_os = "linux")]
fn set_follow_mouse(value: &str) -> bool {
    use std::process::Command;
    Command::new("hyprctl")
        .args(["keyword", "input:follow_mouse", value])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Give keyboard focus to the window with this Hyprland address. Addresses are
/// stored normalized — lowercase hex without the `0x` prefix — and `focuswindow`
/// wants it back. Best-effort; the natural post-popup refocus is the fallback.
#[cfg(target_os = "linux")]
fn focus_window_by_address(address: &str) {
    use std::process::Command;
    let _ = Command::new("hyprctl")
        .args(["dispatch", "focuswindow", &format!("address:0x{address}")])
        .output();
}

/// Pins keyboard focus for the duration of a multi-keystroke emit so a stray
/// mouse move can't hand half the retyped correction to another window.
/// Disables focus-follows-mouse (`input:follow_mouse 0`) on creation and
/// restores the user's previous value on drop — covering early returns and
/// panics alike. A no-op when follow-mouse is already off (no race to guard) or
/// hyprctl is unavailable, so it never changes behavior on non-Hyprland setups.
///
/// Note: a hard kill (SIGKILL) *during* an emit would leave follow_mouse at 0
/// until the next Hyprland config reload; the RAII restore covers every normal
/// exit path, and an emit is short, so this is a negligible edge.
#[cfg(target_os = "linux")]
struct FocusLock {
    /// The value to restore on drop — `None` when we changed nothing.
    restore: Option<String>,
}

#[cfg(target_os = "linux")]
impl FocusLock {
    fn engage() -> Self {
        let restore = match read_follow_mouse() {
            // Already off → no race to guard, and nothing to restore.
            Some(v) if v == "0" => None,
            Some(v) => set_follow_mouse("0").then_some(v),
            None => None,
        };
        Self { restore }
    }
}

#[cfg(target_os = "linux")]
impl Drop for FocusLock {
    fn drop(&mut self) {
        if let Some(v) = self.restore.take() {
            set_follow_mouse(&v);
        }
    }
}

/// Install per-class Hyprland windowrules so the review popup
/// always opens floating (and centered). Uses `hyprctl keyword
/// windowrule` — the rules persist for the running Hyprland
/// session, just like our `hyprctl keyword bind`. Idempotent:
/// re-registering the same rule on a daemon restart is a no-op.
#[cfg(target_os = "linux")]
fn install_window_rules() {
    use std::process::Command;
    // Only the transient review popup is floated/centered by us. The prefs
    // window opens *tiled* (no rule), obeying Hyprland's normal tiling; its
    // floating width is capped from inside the app instead — a `maxsize`
    // windowrule doesn't clamp a window's live floating resize, and a Wayland
    // max-size hint would force-float it (see prefs.rs `update`).
    const REVIEW_CLASS: &str = "hyprcorrect-review";
    // The prefs "Browse…" file picker is a GTK portal dialog; without a
    // rule Hyprland tiles it and it opens *behind* the prefs window. Float
    // + center it so it pops over the top. (Generic portal class, but
    // floating file-choosers is the conventional behavior anyway.)
    const PORTAL_CLASS: &str = "xdg-desktop-portal-gtk";
    // Hyprland's current syntax (post-deprecation of windowrulev2):
    // `windowrule = <rule>, match:class <CLASS>`. State-bearing rules
    // require the `on` suffix (`float on`, not bare `float`).
    for rule in [
        format!("float on, match:class {REVIEW_CLASS}"),
        format!("center on, match:class {REVIEW_CLASS}"),
        format!("float on, match:class {PORTAL_CLASS}"),
        format!("center on, match:class {PORTAL_CLASS}"),
    ] {
        let result = Command::new("hyprctl")
            .args(["keyword", "windowrule", &rule])
            .output();
        match result {
            Ok(output) if !output.status.success() => {
                eprintln!(
                    "hyprcorrect: windowrule install failed for {rule:?}: {}",
                    String::from_utf8_lossy(&output.stderr).trim(),
                );
            }
            Err(e) => eprintln!("hyprcorrect: hyprctl not available for windowrules: {e}"),
            _ => {}
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn spawn_review_window() {
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    // Prefer /proc/self/exe — the kernel-maintained symlink to
    // the running binary's inode. `current_exe()` returns the
    // resolved on-disk path, which can be marked `(deleted)`
    // after a `cargo build` overwrites it; spawning that path
    // fails with ENOENT. /proc/self/exe always resolves to the
    // still-running binary so `cargo build && SIGUSR1 the
    // running daemon` keeps working without a daemon restart.
    let exe_proc = PathBuf::from("/proc/self/exe");
    let exe = if exe_proc.exists() {
        exe_proc
    } else if let Ok(p) = std::env::current_exe() {
        p
    } else {
        eprintln!("hyprcorrect: cannot find own executable to launch review");
        return;
    };
    let result = Command::new(&exe)
        .arg("review")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Err(e) = result {
        eprintln!("hyprcorrect: could not launch review window: {e}");
    }
}

/// Honor a `review-apply` signal: emit the proposed correction to the
/// originating window (the popup already slept ~150 ms after closing
/// itself, so Hyprland has had a chance to refocus the source).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn apply_review(
    buffers: &mut std::collections::HashMap<String, hyprcorrect_core::Buffer>,
    pause_per_backspace_ms: u32,
    pause_per_char_ms: u32,
) {
    use crate::backend::emit;
    use hyprcorrect_core::runtime::{clear_review, read_review_request};

    let Ok(Some(req)) = read_review_request() else {
        return;
    };
    // Pin keyboard focus to the originating window for the whole emit: disable
    // focus-follows-mouse first (so a mouse move mid-retype can't split the
    // correction across windows), then refocus the source window by address —
    // the popup just closed and Hyprland normally refocuses it, but make it
    // explicit and deterministic. `_lock` restores follow_mouse when this
    // function returns.
    let _lock = FocusLock::engage();
    focus_window_by_address(&req.window_address);
    // Extra settle pause before the emit. The popup just closed and
    // Hyprland is delivering the focus event to the source window;
    // the receiving app (terminal TUIs especially) needs a beat to
    // drain its first input batch before the backspace burst lands.
    // The popup-side `REFOCUS_DELAY_MS` covers the focus-handoff
    // proper; this covers the app's post-focus settling.
    std::thread::sleep(std::time::Duration::from_millis(100));
    // Backspaces (left of caret) = original's left half + trailing
    //   whitespace that sits between sentence and caret.
    // Deletes (right of caret) = original's right half.
    // Insert at caret = corrected text + the same trailing whitespace.
    // For end-of-text reviews (the common case) `chars_after_caret`
    // is 0 and this collapses to "backspace everything, retype".
    let backspaces = req.chars_before_caret + req.trailing.chars().count();
    let deletes = req.chars_after_caret;
    let insert = format!("{}{}", req.corrected, req.trailing);
    eprintln!(
        "hyprcorrect: review-apply — {backspaces} backspaces + {deletes} deletes + {:?}",
        insert
    );
    match emit::replace_around_caret_with_delay(
        backspaces,
        deletes,
        &insert,
        pause_per_backspace_ms,
        pause_per_char_ms,
    ) {
        Ok(()) => {
            if let Some(buf) = buffers.get_mut(&req.window_address) {
                buf.apply_around_caret(backspaces, deletes, &insert);
            }
        }
        Err(e) => eprintln!("hyprcorrect: review emit failed: {e}"),
    }
    clear_review();
}

/// Try to build the LLM provider from the current config. Returns
/// `None` if the user hasn't picked the LLM provider, hasn't set an
/// API key, or has configured an unsupported backend — all
/// non-fatal: the daemon just falls back to the offline provider.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn build_llm(config: &hyprcorrect_core::Config) -> Option<hyprcorrect_core::LlmProvider> {
    use hyprcorrect_core::{LlmError, LlmProvider};
    // Build the provider whenever an API key is configured — not only
    // when the LLM is the smart/default provider. That keeps on-demand
    // "Ask LLM" escalation working when the user runs LanguageTool by
    // default but wants the LLM as a fallback. The auto-correct paths
    // still only *call* it when their ProviderId selects it, so merely
    // building it triggers no network use.
    //
    // The *active* provider is the first in the list (the prefs UI keeps
    // it there via the Active checkbox / MRU reorder). We honor that
    // choice strictly: if the active backend isn't wired yet or has no
    // key, we fall back to the offline provider rather than silently
    // using a different tab the user didn't pick.
    let active = config.providers.llms.first()?;
    match LlmProvider::from_config(active) {
        Ok(p) => Some(p),
        // No key set up yet, or the chosen backend isn't wired —
        // expected; the daemon falls back to the offline provider.
        Err(LlmError::NoApiKey) | Err(LlmError::UnsupportedBackend(_)) => None,
        Err(e) => {
            eprintln!(
                "hyprcorrect: active LLM provider '{}' unavailable — {e}",
                active.backend
            );
            None
        }
    }
}

/// Build the LanguageTool provider when the user either routes a path to
/// it (smart/default) or wants it as the LLM-miss fallback
/// (`behavior.fallback_to_languagetool`). Nothing to build unless it's
/// enabled, so a disabled LanguageTool short-circuits without noise even
/// when the fallback toggle is on (the common case). Same non-fatal
/// contract as `build_llm`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn build_languagetool(
    config: &hyprcorrect_core::Config,
) -> Option<hyprcorrect_core::LanguageToolProvider> {
    use hyprcorrect_core::{LanguageToolProvider, ProviderId};
    if !config.providers.languagetool.enabled {
        return None;
    }
    let selected = config.providers.smart == ProviderId::LanguageTool
        || config.providers.default == ProviderId::LanguageTool;
    if !selected && !config.behavior.fallback_to_languagetool {
        return None;
    }
    match LanguageToolProvider::from_config(&config.providers.languagetool) {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("hyprcorrect: LanguageTool provider unavailable — {e}");
            None
        }
    }
}

/// Pre-rasterized tray icons (1× + 2×). The SNI host picks the
/// closest match for whatever bar height it draws. `paused=true`
/// returns a half-alpha variant so the tray dims without needing a
/// second SVG asset.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn build_tray_pixmaps(paused: bool) -> Vec<crate::backend::tray::IconPixmap> {
    use crate::backend::tray::IconPixmap;
    // 22 px is the canonical SNI tray height on most bars; 44 px
    // covers HiDPI / Waybar at 2×.
    const SIZES: &[u32] = &[22, 44];
    hyprcorrect_ui::icon::tray_pixmaps(SIZES, paused)
        .into_iter()
        .map(|p| IconPixmap {
            width: p.size as i32,
            height: p.size as i32,
            argb: p.argb,
        })
        .collect()
}

/// Correct the buffer's last sentence in place. If the user routed
/// the "smart" path to the LLM and the provider initialized cleanly,
/// the sentence goes through the LLM; otherwise (or on LLM failure)
/// we fall back to the offline spellbook provider so the trigger
/// never silently no-ops.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(clippy::too_many_arguments)] // provider-routing + emit-timing context
fn fix_last_sentence(
    buffer: &mut hyprcorrect_core::Buffer,
    provider: &hyprcorrect_core::OfflineProvider,
    smart: hyprcorrect_core::ProviderId,
    llm: Option<&hyprcorrect_core::LlmProvider>,
    languagetool: Option<&hyprcorrect_core::LanguageToolProvider>,
    pause_per_backspace_ms: u32,
    pause_per_char_ms: u32,
    lt_fallback: bool,
) {
    use crate::backend::emit;

    let Some(at) = buffer.sentence_at_caret() else {
        eprintln!(
            "hyprcorrect: sentence-fix skipped — focused window's keystroke buffer holds no sentence (try typing the sentence inside this window first)"
        );
        return;
    };
    eprintln!(
        "hyprcorrect: sentence-fix on {:?} ({} chars; smart={smart:?}; llm={}; lt={})",
        truncate(&at.sentence, 60),
        at.sentence.chars().count(),
        llm.is_some(),
        languagetool.is_some(),
    );
    let (corrected, used_provider) = correct_sentence(
        &at.sentence,
        smart,
        llm,
        languagetool,
        provider,
        lt_fallback,
    );
    // Guard against the LLM returning a conversational reply instead of
    // the corrected sentence — emitting it would type a paragraph in.
    if !is_plausible_correction(&at.sentence, &corrected) {
        eprintln!(
            "hyprcorrect: sentence-fix — {used_provider:?} returned an implausible result; not emitting"
        );
        notify_warning(
            "Unexpected reply",
            "The provider returned a message instead of a correction — nothing was changed.",
        );
        return;
    }
    if corrected == at.sentence {
        eprintln!("hyprcorrect: sentence-fix — provider returned the same text, nothing to emit");
        notify_info(
            "Nothing to correct",
            &format!(
                "{} thinks the sentence is fine.",
                provider_label(used_provider)
            ),
        );
        return;
    }
    eprintln!(
        "hyprcorrect: sentence-fix emitting → {:?}",
        truncate(&corrected, 60)
    );
    let backspaces = at.chars_before_caret + at.trailing.chars().count();
    let deletes = at.chars_after_caret;
    let insert = format!("{corrected}{}", at.trailing);
    // Lock focus to the (already-focused) target window for the emit so a mouse
    // move can't divert the retype mid-stream. Restored when `_lock` drops.
    let _lock = FocusLock::engage();
    match emit::replace_around_caret_with_delay(
        backspaces,
        deletes,
        &insert,
        pause_per_backspace_ms,
        pause_per_char_ms,
    ) {
        Ok(()) => {
            buffer.apply_around_caret(backspaces, deletes, &insert);
            notify_info(
                &format!("Corrected ({})", provider_label(used_provider)),
                &format!(
                    "{} → {}",
                    truncate(&at.sentence, 40),
                    truncate(&corrected, 40)
                ),
            );
        }
        Err(e) => eprintln!("hyprcorrect: {e}"),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

/// Heuristic: does `s` read like a chat reply rather than corrected text?
/// LLMs sometimes refuse/clarify ("I'd be happy to help…", "Could you
/// share the sentence?") instead of returning the fix; emitting that
/// verbatim dumps a paragraph into the user's input.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn looks_conversational(s: &str) -> bool {
    let lc = s.to_lowercase();
    const MARKERS: &[&str] = &[
        "i'd be happy",
        "i would be happy",
        "happy to help",
        "could you",
        "can you please",
        "please provide",
        "please share",
        "you haven't provided",
        "you have not provided",
        "i don't see",
        "i do not see",
        "there is no",
        "it looks like",
        "i'm sorry",
        "i am sorry",
        "as an ai",
        "i can't",
        "i cannot",
        "let me know",
        "the sentence you",
        "for me to correct",
    ];
    MARKERS.iter().any(|m| lc.contains(m))
}

/// Whether `candidate` is a plausible *sentence* correction of `original`
/// — close in length and not a conversational reply. Rejects the
/// LLM-went-chatty case before it's emitted.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn is_plausible_correction(original: &str, candidate: &str) -> bool {
    let o = original.trim().chars().count();
    let c = candidate.trim().chars().count();
    // Generous on real growth (added words / punctuation), but a chat
    // reply balloons well past this.
    c <= o.max(8) * 3 + 60 && !looks_conversational(candidate)
}

/// Whether `candidate` is a plausible *single-word* fix of `original` —
/// one token, not conversational, not wildly longer.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn is_plausible_word_fix(original: &str, candidate: &str) -> bool {
    let c = candidate.trim();
    !c.is_empty()
        && !c.chars().any(char::is_whitespace)
        && c.chars().count() <= original.chars().count().max(4) * 3 + 12
        && !looks_conversational(c)
}

/// The LLM smart path is unavailable (call failed, no key, or an unwired
/// backend). Prefer LanguageTool when it's configured; only drop to the
/// offline spellbook when LanguageTool isn't configured (or also fails).
/// Keeps the smart path useful for users who run a local LanguageTool but
/// haven't readied an LLM provider.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn llm_unavailable_fallback(
    text: &str,
    languagetool: Option<&hyprcorrect_core::LanguageToolProvider>,
    spell: &hyprcorrect_core::OfflineProvider,
) -> (String, hyprcorrect_core::ProviderId) {
    use hyprcorrect_core::ProviderId;
    if let Some(lt) = languagetool {
        match lt.check_text(text) {
            Ok(corrections) => {
                return (
                    apply_correction_list(text, corrections),
                    ProviderId::LanguageTool,
                );
            }
            Err(e) => {
                eprintln!("hyprcorrect: LanguageTool fallback also failed: {e} — using spellbook");
            }
        }
    }
    (apply_corrections(text, spell), ProviderId::Spellbook)
}

/// [`llm_unavailable_fallback`] for the review path: also returns ranked
/// per-word suggestions for the dropdown.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn llm_unavailable_fallback_with_suggestions(
    text: &str,
    languagetool: Option<&hyprcorrect_core::LanguageToolProvider>,
    spell: &hyprcorrect_core::OfflineProvider,
) -> (
    String,
    hyprcorrect_core::ProviderId,
    Vec<hyprcorrect_core::runtime::WordSuggestions>,
) {
    use hyprcorrect_core::ProviderId;
    if let Some(lt) = languagetool {
        match lt.check_text(text) {
            Ok(corrections) => {
                let (corrected, suggestions) = apply_with_suggestions(text, corrections);
                return (corrected, ProviderId::LanguageTool, suggestions);
            }
            Err(e) => {
                eprintln!("hyprcorrect: LanguageTool fallback also failed: {e} — using spellbook");
            }
        }
    }
    let (corrected, suggestions) = apply_with_suggestions(text, spell.check_text(text));
    (corrected, ProviderId::Spellbook, suggestions)
}

/// Route the sentence to whichever provider the user configured for
/// the "smart" path. On any provider failure we drop back to the
/// offline spellbook path so the chord never silently no-ops, and
/// fire a desktop toast so the user knows what failed instead of
/// having to tail the daemon's stdout.
///
/// Returns the corrected text together with the provider that
/// actually produced it — fallback paths (e.g. LLM error → spellbook)
/// report `Spellbook`, not the configured default, so the success
/// toast can't claim a provider that didn't run.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn correct_sentence(
    text: &str,
    smart: hyprcorrect_core::ProviderId,
    llm: Option<&hyprcorrect_core::LlmProvider>,
    languagetool: Option<&hyprcorrect_core::LanguageToolProvider>,
    spell: &hyprcorrect_core::OfflineProvider,
    lt_fallback: bool,
) -> (String, hyprcorrect_core::ProviderId) {
    use hyprcorrect_core::ProviderId;
    // LanguageTool is only consulted as an LLM-miss fallback when the
    // user enabled it; otherwise an LLM miss drops straight to Spellbook.
    let lt_after_llm = languagetool.filter(|_| lt_fallback);
    match smart {
        ProviderId::Llm => match llm {
            Some(llm) => match llm.rewrite(text) {
                Ok(corrected) => (corrected, ProviderId::Llm),
                Err(e) => {
                    let msg = format!("LLM call failed: {e}");
                    eprintln!("hyprcorrect: {msg} — falling back");
                    notify_warning("LLM unavailable", &msg);
                    llm_unavailable_fallback(text, lt_after_llm, spell)
                }
            },
            None => {
                let msg = "Smart provider is set to LLM, but the active provider has no API \
                           key (or its backend isn't supported yet). Open Preferences → \
                           Providers → LLM.";
                eprintln!("hyprcorrect: {msg} — falling back");
                notify_warning("LLM API key not set", msg);
                llm_unavailable_fallback(text, lt_after_llm, spell)
            }
        },
        ProviderId::LanguageTool => match languagetool {
            Some(lt) => match lt.check_text(text) {
                Ok(corrections) => (
                    apply_correction_list(text, corrections),
                    ProviderId::LanguageTool,
                ),
                Err(e) => {
                    let msg = format!("LanguageTool call failed: {e}");
                    eprintln!("hyprcorrect: {msg} — falling back to spellbook");
                    notify_warning("LanguageTool unavailable", &msg);
                    (apply_corrections(text, spell), ProviderId::Spellbook)
                }
            },
            None => {
                let msg = "Smart provider is set to LanguageTool, but it is disabled or has \
                           no URL configured. Open Preferences → Providers → LanguageTool.";
                eprintln!("hyprcorrect: {msg} — falling back to spellbook");
                notify_warning("LanguageTool not configured", msg);
                (apply_corrections(text, spell), ProviderId::Spellbook)
            }
        },
        ProviderId::Spellbook => (apply_corrections(text, spell), ProviderId::Spellbook),
    }
}

/// Like [`correct_sentence`] but also gathers ranked per-word backup
/// suggestions for the review popup's dropdown. Used only by the review
/// path: the LLM branch makes one structured call that returns the
/// corrected sentence plus alternatives; spellbook/LanguageTool reuse
/// the ranked lists they already produce. The no-UI quick paths keep
/// using [`correct_sentence`] (plain, no extra LLM cost).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn correct_sentence_with_suggestions(
    text: &str,
    smart: hyprcorrect_core::ProviderId,
    llm: Option<&hyprcorrect_core::LlmProvider>,
    languagetool: Option<&hyprcorrect_core::LanguageToolProvider>,
    spell: &hyprcorrect_core::OfflineProvider,
    lt_fallback: bool,
) -> (
    String,
    hyprcorrect_core::ProviderId,
    Vec<hyprcorrect_core::runtime::WordSuggestions>,
) {
    use hyprcorrect_core::ProviderId;
    // Offline fallback shared by every provider's error/none arm — the
    // dropdown still gets spellbook's ranked alternatives.
    let spellbook_fallback = || {
        let (corrected, suggestions) = apply_with_suggestions(text, spell.check_text(text));
        (corrected, ProviderId::Spellbook, suggestions)
    };
    // LanguageTool is only consulted as an LLM-miss fallback when the
    // user enabled it; otherwise an LLM miss drops straight to Spellbook.
    let lt_after_llm = languagetool.filter(|_| lt_fallback);
    match smart {
        ProviderId::Llm => match llm {
            Some(llm) => match llm.rewrite_with_alternatives(text) {
                Ok((corrected, alts)) if is_plausible_correction(text, &corrected) => {
                    let suggestions = order_alternatives_by_position(&corrected, alts);
                    (corrected, ProviderId::Llm, suggestions)
                }
                // A conversational reply, not a correction — don't surface it.
                Ok(_) => {
                    eprintln!(
                        "hyprcorrect: review LLM returned an implausible result — using offline"
                    );
                    notify_warning(
                        "Unexpected reply",
                        "The LLM returned a message instead of a correction; using the offline result.",
                    );
                    llm_unavailable_fallback_with_suggestions(text, lt_after_llm, spell)
                }
                Err(e) => {
                    let msg = format!("LLM call failed: {e}");
                    eprintln!("hyprcorrect: {msg} — falling back");
                    notify_warning("LLM unavailable", &msg);
                    llm_unavailable_fallback_with_suggestions(text, lt_after_llm, spell)
                }
            },
            None => {
                let msg = "Smart provider is set to LLM, but the active provider has no API \
                           key (or its backend isn't supported yet). Open Preferences → \
                           Providers → LLM.";
                eprintln!("hyprcorrect: {msg} — falling back");
                notify_warning("LLM API key not set", msg);
                llm_unavailable_fallback_with_suggestions(text, lt_after_llm, spell)
            }
        },
        ProviderId::LanguageTool => match languagetool {
            Some(lt) => match lt.check_text(text) {
                Ok(corrections) => {
                    let (corrected, suggestions) = apply_with_suggestions(text, corrections);
                    (corrected, ProviderId::LanguageTool, suggestions)
                }
                Err(e) => {
                    let msg = format!("LanguageTool call failed: {e}");
                    eprintln!("hyprcorrect: {msg} — falling back to spellbook");
                    notify_warning("LanguageTool unavailable", &msg);
                    spellbook_fallback()
                }
            },
            None => {
                let msg = "Smart provider is set to LanguageTool, but it is disabled or has \
                           no URL configured. Open Preferences → Providers → LanguageTool.";
                eprintln!("hyprcorrect: {msg} — falling back to spellbook");
                notify_warning("LanguageTool not configured", msg);
                spellbook_fallback()
            }
        },
        ProviderId::Spellbook => spellbook_fallback(),
    }
}

/// Fire a best-effort desktop notification via `notify-send` so the
/// user sees provider failures without tailing logs. Silently skips
/// when `notify-send` (libnotify) isn't installed.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn notify_warning(title: &str, body: &str) {
    notify_send("normal", title, body);
}

/// Low-urgency informational notification. Used to confirm which
/// word the daemon picked for fix-word, since the TUI's rendered
/// cursor block can read as sitting on a different character than
/// the buffer caret.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn notify_info(title: &str, body: &str) {
    notify_send("low", title, body);
}

/// User-facing name of the provider that produced a correction.
/// Source-of-truth lookup so the toast can't disagree with the code
/// path that ran — `WordFixPlan::provider` is set at the same return
/// site as the fix itself.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn provider_label(provider: hyprcorrect_core::ProviderId) -> &'static str {
    use hyprcorrect_core::ProviderId;
    match provider {
        ProviderId::Spellbook => "Spellbook",
        ProviderId::Llm => "LLM",
        ProviderId::LanguageTool => "LanguageTool",
    }
}

#[cfg(target_os = "linux")]
fn notify_send(urgency: &str, title: &str, body: &str) {
    use std::process::{Command, Stdio};
    let _ = Command::new("notify-send")
        .args([
            "-a",
            "hyprcorrect",
            "-c",
            "im",
            "-u",
            urgency,
            "-i",
            "tools-check-spelling",
            &format!("hyprcorrect — {title}"),
            body,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

// --- macOS platform glue ----------------------------------------------------
//
// The shared daemon calls a handful of helpers that are inherently
// compositor-specific on Linux (focus-follows-mouse pinning, `hyprctl`
// window rules, `notify-send`). macOS has no equivalent of those concepts,
// so these siblings provide the same call surface the daemon expects.

/// Desktop toast via `osascript` — dependency-free, no `notify-send`.
#[cfg(target_os = "macos")]
fn notify_send(_urgency: &str, title: &str, body: &str) {
    use std::process::{Command, Stdio};
    // Escape for the AppleScript string literals.
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        "display notification \"{}\" with title \"hyprcorrect\" subtitle \"{}\"",
        esc(body),
        esc(title),
    );
    let _ = Command::new("osascript")
        .args(["-e", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// On macOS there's no focus-follows-mouse to pin: synthetic keystrokes
/// already land in the frontmost app (the source window). The lock is a
/// no-op that keeps the shared emit paths' `let _lock = FocusLock::engage()`
/// guard compiling and meaningful across platforms.
#[cfg(target_os = "macos")]
struct FocusLock;

#[cfg(target_os = "macos")]
impl FocusLock {
    fn engage() -> Self {
        FocusLock
    }
}

/// No-op on macOS: when the review popup closes, the OS refocuses the
/// prior app on its own. An explicit activate would need Apple Events
/// (Automation) permission, deferred to the post-M2 NSPanel review popup.
#[cfg(target_os = "macos")]
fn focus_window_by_address(_address: &str) {}

/// No compositor window rules on macOS. The review popup is an ordinary
/// window for M2; a borderless `NSPanel` is a later refinement.
#[cfg(target_os = "macos")]
fn install_window_rules() {}

/// Logical size of the main display, for sizing the review popup.
#[cfg(target_os = "macos")]
fn focused_monitor_size() -> (f32, f32) {
    crate::backend::primary_screen_size()
}

/// macOS only: park here until the capture permission is granted, then
/// relaunch a fresh instance and exit. Called when `capture::start`
/// reports the Input Monitoring grant is missing. The CGEventTap is
/// latched at process start — a grant that lands while we're running
/// can't re-arm the already-running process — so the only way to begin
/// capturing without a manual restart is to relaunch once the grant
/// appears. Blocking here (rather than returning) keeps the process
/// alive: `bootstrap_main` stops the app the moment the worker returns,
/// and `NSApp.run` on the main thread holds the process up until we exit.
///
/// On macOS 13+ the gate flips as soon as the user enables hyprcorrect
/// under Accessibility (which transitively confers the listen-only tap
/// privilege), so the app usually never appears in the Input Monitoring
/// list — watching the actual capture gate, not a specific pane, is what
/// makes this correct across versions and loop-safe (we only relaunch
/// when a fresh `start` would actually succeed).
#[cfg(target_os = "macos")]
const RELAUNCH_MARKER: &str = "HYPRCORRECT_AUTORELAUNCHED";

#[cfg(target_os = "macos")]
fn await_capture_permission_then_relaunch() -> ! {
    use std::time::Duration;
    // Loop guard: if we are ALREADY a relaunched instance (the marker is set)
    // and capture STILL can't start, do not relaunch again — that would spin
    // forever, spamming notifications. Idle quietly with one clear notice.
    if std::env::var_os(RELAUNCH_MARKER).is_some() {
        notify_warning(
            "Permission needed",
            "hyprcorrect still can't capture after the grant. Make sure it's \
             enabled under Privacy & Security → Accessibility, then quit and \
             reopen it.",
        );
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
    // Fire the real Accessibility prompt up front: on macOS 13+ that single
    // grant confers both the capture tap and synthetic typing, so it's the
    // one permission to ask for (Input Monitoring would only cover capture).
    crate::backend::capture::fire_accessibility_prompt();
    notify_warning(
        "Permission needed",
        "Enable hyprcorrect under System Settings → Privacy & Security → \
         Accessibility. It starts working automatically once you do — no \
         restart needed.",
    );
    loop {
        // Gate on AXIsProcessTrusted ALONE. Two empirical findings drive this:
        //   1. CGPreflightPostEventAccess (emit) reads a FALSE true in this
        //      running process after only Input Monitoring was granted, so it
        //      can't gate the relaunch — AXIsProcessTrusted is the honest
        //      emit signal and flips only on the real Accessibility grant.
        //   2. CGPreflightListenEventAccess (capture) does NOT pick up the
        //      macOS 13+ "Accessibility confers Input Monitoring" conferral in
        //      an ALREADY-running process — it stays cached-false — so we
        //      can't wait on it either. A FRESH process does get the
        //      conferral, so once AXIsProcessTrusted is true the relaunched
        //      daemon's capture::start succeeds and emit works. (On 11–12,
        //      where the two grants are separate, the user grants Input
        //      Monitoring too; the relaunched process simply re-parks until
        //      both are in place.)
        let trusted = crate::backend::capture::accessibility_granted();
        eprintln!("hyprcorrect: waiting for Accessibility grant — trusted={trusted}");
        if trusted {
            notify_info("Permission granted", "Starting hyprcorrect…");
            relaunch_self();
            std::process::exit(0);
        }
        std::thread::sleep(Duration::from_millis(1500));
    }
}

/// macOS only: relaunch a fresh copy of ourselves. From inside a `.app`
/// bundle, ask Launch Services to open the bundle so the new process gets
/// its own clean TCC identity; from a bare binary (dev / `cargo run`),
/// respawn the executable directly. The one-second delay lets THIS process
/// exit first — otherwise Launch Services reactivates the existing instance
/// instead of starting a fresh one, and the still-held singleton socket
/// would reject the newcomer. The trailing `&` detaches the spawner so it
/// survives our exit (adopted by launchd).
#[cfg(target_os = "macos")]
fn relaunch_self() {
    use std::path::Path;
    use std::process::Command;
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    // <bundle>/Contents/MacOS/hyprcorrect → three parents up is the bundle.
    let bundle = exe
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .filter(|p| p.extension().is_some_and(|e| e == "app"));
    // Tag the relaunched process with a marker env var so its own watcher
    // (if capture STILL fails) won't relaunch again and spin. `open --env`
    // carries it into the bundle; the bare-binary path sets it inline.
    let cmd = match bundle {
        Some(b) => format!("(sleep 1 && open --env {RELAUNCH_MARKER}=1 {b:?}) &"),
        None => format!("(sleep 1 && {RELAUNCH_MARKER}=1 {exe:?}) &"),
    };
    let _ = Command::new("sh").arg("-c").arg(cmd).spawn();
}

/// Apply a precomputed list of corrections (from LanguageTool) to
/// `text`. Sorted right-to-left so byte offsets stay valid through
/// later replacements.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn apply_correction_list(text: &str, mut corrections: Vec<hyprcorrect_core::Correction>) -> String {
    if corrections.is_empty() {
        return text.to_string();
    }
    corrections.sort_by_key(|c| std::cmp::Reverse(c.span.start));
    let mut out = text.to_string();
    for c in corrections {
        if let Some(fix) = c.suggestions.first() {
            out.replace_range(c.span.clone(), fix);
        }
    }
    out
}

/// Run the provider over `text` and apply each correction's top
/// suggestion to produce a corrected string. Applies right-to-left
/// so earlier byte offsets stay valid through later replacements.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn apply_corrections(text: &str, provider: &hyprcorrect_core::OfflineProvider) -> String {
    let mut corrections = provider.check_text(text);
    if corrections.is_empty() {
        return text.to_string();
    }
    corrections.sort_by_key(|c| std::cmp::Reverse(c.span.start));
    let mut out = text.to_string();
    for c in corrections {
        if let Some(fix) = c.suggestions.first() {
            out.replace_range(c.span.clone(), fix);
        }
    }
    out
}

/// Apply each correction's top suggestion to `text` and, in the same
/// pass, collect the full ranked option list per corrected word
/// (left-to-right) for the review dropdown. The applied fix is
/// `options[0]`; the rest are backups. Works for spellbook and
/// LanguageTool corrections alike.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn apply_with_suggestions(
    text: &str,
    mut corrections: Vec<hyprcorrect_core::Correction>,
) -> (String, Vec<hyprcorrect_core::runtime::WordSuggestions>) {
    use hyprcorrect_core::runtime::WordSuggestions;
    if corrections.is_empty() {
        return (text.to_string(), Vec::new());
    }
    // Left-to-right for the dropdown ordering (matches the popup fields).
    corrections.sort_by_key(|c| c.span.start);
    let suggestions: Vec<WordSuggestions> = corrections
        .iter()
        .filter_map(|c| {
            c.suggestions.first().map(|applied| WordSuggestions {
                word: applied.clone(),
                options: c.suggestions.iter().take(6).cloned().collect(),
            })
        })
        .collect();
    // Apply right-to-left so earlier byte offsets stay valid.
    let mut out = text.to_string();
    corrections.sort_by_key(|c| std::cmp::Reverse(c.span.start));
    for c in &corrections {
        if let Some(fix) = c.suggestions.first() {
            out.replace_range(c.span.clone(), fix);
        }
    }
    (out, suggestions)
}

/// Order LLM word alternatives by where each word first appears in
/// `corrected`, so they line up with the popup's left-to-right fields.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn order_alternatives_by_position(
    corrected: &str,
    mut alts: Vec<hyprcorrect_core::runtime::WordSuggestions>,
) -> Vec<hyprcorrect_core::runtime::WordSuggestions> {
    alts.sort_by_key(|a| corrected.find(&a.word).unwrap_or(usize::MAX));
    alts
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn run_daemon() {
    println!(
        "hyprcorrect {}: the background daemon runs on Linux/Wayland and \
         macOS. This platform isn't supported yet (Windows is a stub).",
        hyprcorrect_core::version(),
    );
}

fn not_yet(what: &str, milestone: &str) {
    eprintln!("hyprcorrect: {what} is not implemented yet ({milestone}) — see DESIGN.md");
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use hyprcorrect_core::{Correction, Sentence};

    use super::{apply_with_suggestions, first_overlap_suggestion, word_in_sentence_bytes};

    #[test]
    fn apply_with_suggestions_orders_left_to_right_and_keeps_backups() {
        // Corrections deliberately out of order; the result must apply
        // both and emit suggestions left-to-right with full ranked lists.
        let corrections = vec![
            Correction {
                span: 10..16,
                original: "browne".into(),
                suggestions: vec!["brown".into(), "crown".into(), "browse".into()],
            },
            Correction {
                span: 0..3,
                original: "teh".into(),
                suggestions: vec!["the".into(), "then".into()],
            },
        ];
        let (corrected, sugg) = apply_with_suggestions("teh quick browne fox", corrections);
        assert_eq!(corrected, "the quick brown fox");
        assert_eq!(sugg.len(), 2);
        assert_eq!(sugg[0].word, "the"); // applied fix = options[0]
        assert_eq!(sugg[0].options, vec!["the", "then"]);
        assert_eq!(sugg[1].word, "brown");
        assert_eq!(sugg[1].options, vec!["brown", "crown", "browse"]);
    }

    fn sentence(s: &str, start: usize) -> Sentence {
        Sentence {
            sentence: s.into(),
            buffer_byte_start: start,
            buffer_byte_end: start + s.len(),
        }
    }

    #[test]
    fn word_in_sentence_bytes_subtracts_buffer_offset() {
        // Sentence starts at buffer byte 13; find "their" via the
        // sentence text itself to avoid hand-counted offsets.
        let buffer_offset = 13;
        let s = sentence("The cat ran their way.", buffer_offset);
        let in_sentence = s.sentence.find("their").unwrap();
        let buf_start = buffer_offset + in_sentence;
        let buf_end = buf_start + "their".len();
        let r = word_in_sentence_bytes(&s, buf_start, buf_end).unwrap();
        assert_eq!(&s.sentence[r], "their");
    }

    #[test]
    fn word_in_sentence_bytes_rejects_out_of_range() {
        let s = sentence("Hello.", 0);
        // buffer_end past sentence end → None
        assert!(word_in_sentence_bytes(&s, 0, 10).is_none());
        // buffer_start before sentence start → None
        let s2 = sentence("Hello.", 5);
        assert!(word_in_sentence_bytes(&s2, 0, 4).is_none());
    }

    #[test]
    fn first_overlap_picks_match_on_target_word() {
        let target = 9..14; // bytes of "their" in "They went their way."
        let corrections = vec![
            Correction {
                span: 0..4,
                original: "They".into(),
                suggestions: vec!["Them".into()],
            },
            Correction {
                span: 9..14,
                original: "their".into(),
                suggestions: vec!["there".into()],
            },
        ];
        assert_eq!(
            first_overlap_suggestion(&corrections, &target),
            Some("there".into())
        );
    }

    #[test]
    fn first_overlap_returns_none_when_nothing_touches_target() {
        let target = 9..14;
        let corrections = vec![Correction {
            span: 0..4,
            original: "They".into(),
            suggestions: vec!["Them".into()],
        }];
        assert!(first_overlap_suggestion(&corrections, &target).is_none());
    }
}
