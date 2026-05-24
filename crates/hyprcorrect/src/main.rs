//! hyprcorrect — keyboard-driven desktop spelling and typo correction.
//!
//! Running `hyprcorrect` with no subcommand starts the daemon: it
//! registers the trigger chord with Hyprland, captures keystrokes into
//! a per-window keystroke buffer, subscribes to focus events so each
//! window owns its own buffer, publishes a system-tray icon, and
//! corrects the last typed word in place when the chord fires.
//! See `DESIGN.md` at the repository root.

use clap::{Parser, Subcommand};

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
}

fn main() {
    env_logger::init();

    match Cli::parse().command {
        None => run_daemon(),
        Some(Command::FixWord) => {
            eprintln!(
                "hyprcorrect: run `hyprcorrect` with no subcommand — the daemon \
                 corrects the last word when you press the trigger chord"
            );
        }
        Some(Command::FixSentence) => not_yet("fix-sentence as a CLI subcommand", "M5"),
        Some(Command::Review) => hyprcorrect_ui::run_review(),
        Some(Command::Prefs) => hyprcorrect_ui::run_preferences(),
    }
}

/// Run the background daemon: load config, register the trigger,
/// capture keystrokes into per-window buffers, subscribe to focus
/// events, publish the tray, and correct the focused window's last
/// word on the chord.
#[cfg(target_os = "linux")]
fn run_daemon() {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread;

    use hyprcorrect_core::{Buffer, Chord, Config, OfflineProvider};
    use hyprcorrect_platform::linux::{capture, focus, hotkey, tray};

    let initial_config = Config::load().unwrap_or_else(|e| {
        eprintln!("hyprcorrect: could not load config ({e}) — using defaults");
        Config::default()
    });
    let mut llm = build_llm(&initial_config);
    let mut languagetool = build_languagetool(&initial_config);
    let mut smart_provider_id = initial_config.providers.smart;
    let mut inter_key_delay_ms = initial_config.behavior.inter_key_delay_ms;
    let mut post_backspace_pause_ms = initial_config.behavior.post_backspace_pause_ms;
    let mut chord = match effective_chord(&initial_config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("hyprcorrect: invalid chord in config ({e}) — falling back to default");
            Chord::parse("SUPER+CTRL+SHIFT+ALT+F").expect("default chord parses")
        }
    };
    let mut sentence_chord = parse_optional_chord(&initial_config.hotkeys.fix_sentence);
    let mut review_chord = parse_optional_chord(&initial_config.hotkeys.review);
    let mut blocklist = build_blocklist(&initial_config);
    let paused = Arc::new(AtomicBool::new(false));

    if let Err(e) = hyprcorrect_core::runtime::write_self_pid() {
        eprintln!("hyprcorrect: could not write PID file ({e}) — prefs reload won't work");
    }

    let provider = match OfflineProvider::en_us() {
        Ok(provider) => provider,
        Err(e) => {
            eprintln!("hyprcorrect: {e}");
            return;
        }
    };
    let key_rx = match capture::start(&active_chords(&chord, &sentence_chord, &review_chord)) {
        Ok(rx) => rx,
        Err(e) => {
            eprintln!("hyprcorrect: {e}");
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
    let (tray_handle, tray_rx) = match tray::start(paused.clone()) {
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
            }
            DaemonEvent::Signal(hotkey::HotkeyEvent::Trigger) => {
                let action = hyprcorrect_core::runtime::read_action();
                match action.as_str() {
                    "review-apply" => {
                        apply_review(&mut buffers, inter_key_delay_ms, post_backspace_pause_ms)
                    }
                    "review-cancel" => {
                        hyprcorrect_core::runtime::clear_review();
                    }
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
                                ),
                                "sentence" => fix_last_sentence(
                                    buffer,
                                    &provider,
                                    smart_provider_id,
                                    llm.as_ref(),
                                    languagetool.as_ref(),
                                    inter_key_delay_ms,
                                    post_backspace_pause_ms,
                                ),
                                _ => fix_last_word(
                                    buffer,
                                    &provider,
                                    inter_key_delay_ms,
                                    post_backspace_pause_ms,
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

                            blocklist = build_blocklist(&new_config);
                            llm = build_llm(&new_config);
                            languagetool = build_languagetool(&new_config);
                            smart_provider_id = new_config.providers.smart;
                            inter_key_delay_ms = new_config.behavior.inter_key_delay_ms;
                            post_backspace_pause_ms = new_config.behavior.post_backspace_pause_ms;
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
    hyprcorrect_core::runtime::clear_pid();
    hyprcorrect_core::runtime::clear_review();
}

/// Resolve the trigger chord the daemon should bind. `$HYPRCORRECT_CHORD`
/// overrides the config so tests and ad-hoc dev runs don't have to edit
/// `config.toml`.
#[cfg(target_os = "linux")]
fn effective_chord(
    config: &hyprcorrect_core::Config,
) -> Result<hyprcorrect_core::Chord, hyprcorrect_core::ChordError> {
    let raw =
        std::env::var("HYPRCORRECT_CHORD").unwrap_or_else(|_| config.hotkeys.fix_word.clone());
    hyprcorrect_core::Chord::parse(&raw)
}

#[cfg(target_os = "linux")]
fn build_blocklist(config: &hyprcorrect_core::Config) -> std::collections::HashSet<String> {
    config
        .privacy
        .app_blocklist
        .iter()
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Launch `hyprcorrect prefs` as a detached subprocess (no stdio).
/// Fire-and-forget; if a prefs window is already running, the new
/// process short-circuits and focuses the existing one (the prefs
/// entry handles the singleton lock).
#[cfg(target_os = "linux")]
fn spawn_prefs_window() {
    use std::process::{Command, Stdio};
    let Ok(exe) = std::env::current_exe() else {
        eprintln!("hyprcorrect: cannot find own executable to launch prefs");
        return;
    };
    let result = Command::new(&exe)
        .arg("prefs")
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
#[cfg(target_os = "linux")]
fn active_chords(
    word: &hyprcorrect_core::Chord,
    sentence: &Option<hyprcorrect_core::Chord>,
    review: &Option<hyprcorrect_core::Chord>,
) -> Vec<hyprcorrect_core::Chord> {
    let mut out = vec![word.clone()];
    if let Some(c) = sentence {
        out.push(c.clone());
    }
    if let Some(c) = review {
        out.push(c.clone());
    }
    out
}

#[cfg(target_os = "linux")]
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

/// Correct the buffer's last word in place via the offline provider.
/// When the buffer has nothing to work with (focus moved, caret-
/// moving key, app autocorrect, paste, …) we fall back to the
/// clipboard path: simulate "select previous word", read it from
/// the clipboard, correct, and overwrite the still-active
/// selection. Per `DESIGN.md`'s secondary mode.
#[cfg(target_os = "linux")]
fn fix_last_word(
    buffer: &mut hyprcorrect_core::Buffer,
    provider: &hyprcorrect_core::OfflineProvider,
    inter_key_delay_ms: u32,
    post_backspace_pause_ms: u32,
) {
    use hyprcorrect_core::plan_word_replacement;
    use hyprcorrect_platform::linux::emit;

    if let Some(last) = buffer.last_word() {
        let Some(correction) = provider.check_text(&last.word).into_iter().next() else {
            return;
        };
        let Some(fix) = correction.suggestions.into_iter().next() else {
            return;
        };
        let Some(edit) = plan_word_replacement(&last, &fix) else {
            return;
        };
        match emit::replace_with_delay(
            edit.backspaces,
            &edit.insert,
            inter_key_delay_ms,
            post_backspace_pause_ms,
        ) {
            Ok(()) => buffer.apply(edit.backspaces, &edit.insert),
            Err(e) => eprintln!("hyprcorrect: {e}"),
        }
    } else {
        fix_via_clipboard(provider);
    }
}

/// Clipboard fallback for the empty-buffer case. Best-effort —
/// doesn't work in terminals, and only in apps where
/// `Ctrl+Shift+Left` selects the previous word. Failures are
/// logged but never fatal.
#[cfg(target_os = "linux")]
fn fix_via_clipboard(provider: &hyprcorrect_core::OfflineProvider) {
    use hyprcorrect_platform::linux::clipboard;
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
#[cfg(target_os = "linux")]
fn start_review(
    address: &str,
    buffer: &hyprcorrect_core::Buffer,
    provider: &hyprcorrect_core::OfflineProvider,
    smart: hyprcorrect_core::ProviderId,
    llm: Option<&hyprcorrect_core::LlmProvider>,
    languagetool: Option<&hyprcorrect_core::LanguageToolProvider>,
) {
    use hyprcorrect_core::runtime::{ReviewRequest, write_review_request};
    let Some(last) = buffer.last_sentence() else {
        return;
    };
    let corrected = correct_sentence(&last.sentence, smart, llm, languagetool, provider);
    if corrected == last.sentence {
        // Nothing to review — every provider said the sentence is fine.
        return;
    }
    let request = ReviewRequest {
        original: last.sentence,
        corrected,
        trailing: last.trailing,
        window_address: address.to_string(),
    };
    if let Err(e) = write_review_request(&request) {
        eprintln!("hyprcorrect: could not write review request: {e}");
        return;
    }
    spawn_review_window();
}

#[cfg(target_os = "linux")]
fn spawn_review_window() {
    use std::process::{Command, Stdio};
    let Ok(exe) = std::env::current_exe() else {
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
#[cfg(target_os = "linux")]
fn apply_review(
    buffers: &mut std::collections::HashMap<String, hyprcorrect_core::Buffer>,
    inter_key_delay_ms: u32,
    post_backspace_pause_ms: u32,
) {
    use hyprcorrect_core::runtime::{clear_review, read_review_request};
    use hyprcorrect_platform::linux::emit;

    let Ok(Some(req)) = read_review_request() else {
        return;
    };
    let backspaces = req.original.chars().count() + req.trailing.chars().count();
    let insert = format!("{}{}", req.corrected, req.trailing);
    eprintln!(
        "hyprcorrect: review-apply — {backspaces} backspaces + {:?}",
        insert
    );
    match emit::replace_with_delay(
        backspaces,
        &insert,
        inter_key_delay_ms,
        post_backspace_pause_ms,
    ) {
        Ok(()) => {
            if let Some(buf) = buffers.get_mut(&req.window_address) {
                buf.apply(backspaces, &insert);
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
#[cfg(target_os = "linux")]
fn build_llm(config: &hyprcorrect_core::Config) -> Option<hyprcorrect_core::LlmProvider> {
    use hyprcorrect_core::{LlmProvider, ProviderId};
    if config.providers.smart != ProviderId::Llm {
        return None;
    }
    match LlmProvider::from_config(&config.providers.llm) {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("hyprcorrect: LLM provider unavailable — {e}");
            None
        }
    }
}

/// Build the LanguageTool provider when the user has the smart path
/// set to it and the URL is non-empty. Same non-fatal contract as
/// `build_llm`.
#[cfg(target_os = "linux")]
fn build_languagetool(
    config: &hyprcorrect_core::Config,
) -> Option<hyprcorrect_core::LanguageToolProvider> {
    use hyprcorrect_core::{LanguageToolProvider, ProviderId};
    if config.providers.smart != ProviderId::LanguageTool {
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

/// Correct the buffer's last sentence in place. If the user routed
/// the "smart" path to the LLM and the provider initialized cleanly,
/// the sentence goes through the LLM; otherwise (or on LLM failure)
/// we fall back to the offline spellbook provider so the trigger
/// never silently no-ops.
#[cfg(target_os = "linux")]
fn fix_last_sentence(
    buffer: &mut hyprcorrect_core::Buffer,
    provider: &hyprcorrect_core::OfflineProvider,
    smart: hyprcorrect_core::ProviderId,
    llm: Option<&hyprcorrect_core::LlmProvider>,
    languagetool: Option<&hyprcorrect_core::LanguageToolProvider>,
    inter_key_delay_ms: u32,
    post_backspace_pause_ms: u32,
) {
    use hyprcorrect_platform::linux::emit;

    let Some(last) = buffer.last_sentence() else {
        eprintln!(
            "hyprcorrect: sentence-fix skipped — focused window's keystroke buffer holds no sentence (try typing the sentence inside this window first)"
        );
        return;
    };
    eprintln!(
        "hyprcorrect: sentence-fix on {:?} ({} chars; smart={smart:?}; llm={}; lt={})",
        truncate(&last.sentence, 60),
        last.sentence.chars().count(),
        llm.is_some(),
        languagetool.is_some(),
    );
    let corrected = correct_sentence(&last.sentence, smart, llm, languagetool, provider);
    if corrected == last.sentence {
        eprintln!("hyprcorrect: sentence-fix — provider returned the same text, nothing to emit");
        return;
    }
    eprintln!(
        "hyprcorrect: sentence-fix emitting → {:?}",
        truncate(&corrected, 60)
    );
    let backspaces = last.sentence.chars().count() + last.trailing.chars().count();
    let insert = format!("{corrected}{}", last.trailing);
    match emit::replace_with_delay(
        backspaces,
        &insert,
        inter_key_delay_ms,
        post_backspace_pause_ms,
    ) {
        Ok(()) => buffer.apply(backspaces, &insert),
        Err(e) => eprintln!("hyprcorrect: {e}"),
    }
}

#[cfg(target_os = "linux")]
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

/// Route the sentence to whichever provider the user configured for
/// the "smart" path. On any provider failure we drop back to the
/// offline spellbook path so the chord never silently no-ops, and
/// fire a desktop toast so the user knows what failed instead of
/// having to tail the daemon's stdout.
#[cfg(target_os = "linux")]
fn correct_sentence(
    text: &str,
    smart: hyprcorrect_core::ProviderId,
    llm: Option<&hyprcorrect_core::LlmProvider>,
    languagetool: Option<&hyprcorrect_core::LanguageToolProvider>,
    spell: &hyprcorrect_core::OfflineProvider,
) -> String {
    use hyprcorrect_core::ProviderId;
    match smart {
        ProviderId::Llm => match llm {
            Some(llm) => match llm.rewrite(text) {
                Ok(corrected) => corrected,
                Err(e) => {
                    let msg = format!("LLM call failed: {e}");
                    eprintln!("hyprcorrect: {msg} — falling back to spellbook");
                    notify_warning("LLM unavailable", &msg);
                    apply_corrections(text, spell)
                }
            },
            None => {
                let msg = "Smart provider is set to LLM, but no API key is configured. \
                           Open Preferences → Providers → LLM and paste your Anthropic key.";
                eprintln!("hyprcorrect: {msg} — falling back to spellbook");
                notify_warning("LLM key not set", msg);
                apply_corrections(text, spell)
            }
        },
        ProviderId::LanguageTool => match languagetool {
            Some(lt) => match lt.check_text(text) {
                Ok(corrections) => apply_correction_list(text, corrections),
                Err(e) => {
                    let msg = format!("LanguageTool call failed: {e}");
                    eprintln!("hyprcorrect: {msg} — falling back to spellbook");
                    notify_warning("LanguageTool unavailable", &msg);
                    apply_corrections(text, spell)
                }
            },
            None => {
                let msg = "Smart provider is set to LanguageTool, but it is disabled or has \
                           no URL configured. Open Preferences → Providers → LanguageTool.";
                eprintln!("hyprcorrect: {msg} — falling back to spellbook");
                notify_warning("LanguageTool not configured", msg);
                apply_corrections(text, spell)
            }
        },
        ProviderId::Spellbook => apply_corrections(text, spell),
    }
}

/// Fire a best-effort desktop notification via `notify-send` so the
/// user sees provider failures without tailing logs. Silently skips
/// when `notify-send` (libnotify) isn't installed.
#[cfg(target_os = "linux")]
fn notify_warning(title: &str, body: &str) {
    use std::process::{Command, Stdio};
    let _ = Command::new("notify-send")
        .args([
            "-a",
            "hyprcorrect",
            "-c",
            "im",
            "-u",
            "normal",
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

/// Apply a precomputed list of corrections (from LanguageTool) to
/// `text`. Sorted right-to-left so byte offsets stay valid through
/// later replacements.
#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
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

#[cfg(not(target_os = "linux"))]
fn run_daemon() {
    println!(
        "hyprcorrect {}: the background daemon is Linux-only so far — \
         macOS support is milestone M2.",
        hyprcorrect_core::version(),
    );
}

fn not_yet(what: &str, milestone: &str) {
    eprintln!("hyprcorrect: {what} is not implemented yet ({milestone}) — see DESIGN.md");
}
