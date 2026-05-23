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
        Some(Command::FixSentence) => not_yet("fix-sentence", "M4"),
        Some(Command::Review) => not_yet("the review popup", "M4"),
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
    let mut chord = match effective_chord(&initial_config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("hyprcorrect: invalid chord in config ({e}) — falling back to default");
            Chord::parse("SUPER+CTRL+SHIFT+ALT+F").expect("default chord parses")
        }
    };
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
    let key_rx = match capture::start(&chord) {
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
    if let Err(e) = hotkey::install_bind(&chord) {
        eprintln!("hyprcorrect: {e}");
        return;
    }
    let (initial_window, focus_rx) = match focus::start() {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("hyprcorrect: {e}");
            let _ = hotkey::uninstall_bind(&chord);
            return;
        }
    };
    let (tray_handle, tray_rx) = match tray::start(paused.clone()) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("hyprcorrect: {e}");
            let _ = hotkey::uninstall_bind(&chord);
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
                if !paused.load(Ordering::Relaxed)
                    && !current_blocked
                    && let Some(addr) = current_address.as_deref()
                    && let Some(buffer) = buffers.get_mut(addr)
                {
                    fix_last_word(buffer, &provider);
                }
            }
            DaemonEvent::Signal(hotkey::HotkeyEvent::Reload) => {
                match Config::load() {
                    Ok(new_config) => match effective_chord(&new_config) {
                        Ok(new_chord) => {
                            if new_chord != chord
                                && let Err(e) = rebind_trigger(&chord, &new_chord)
                            {
                                eprintln!("hyprcorrect: rebind failed: {e}");
                            } else {
                                if new_chord != chord {
                                    eprintln!(
                                        "hyprcorrect: trigger chord changed: {chord} → {new_chord}"
                                    );
                                }
                                chord = new_chord;
                            }
                            blocklist = build_blocklist(&new_config);
                            eprintln!("hyprcorrect: config reloaded");
                        }
                        Err(e) => {
                            eprintln!("hyprcorrect: bad chord in new config ({e}) — kept old")
                        }
                    },
                    Err(e) => eprintln!("hyprcorrect: reload failed: {e}"),
                }
                // Capture's stale TriggerSpec doesn't matter — Hyprland
                // intercepts the chord and capture never sees the new
                // key under the chord. A full restart is only needed
                // if other capture-time settings change later.
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

    // Clean up so the bind and PID file don't outlive the daemon.
    let _ = hotkey::uninstall_bind(&chord);
    hyprcorrect_core::runtime::clear_pid();
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

/// Swap the Hyprland keybind from `old` to `new`. If installing the
/// new bind fails, restore the old one so the trigger keeps working.
#[cfg(target_os = "linux")]
fn rebind_trigger(
    old: &hyprcorrect_core::Chord,
    new: &hyprcorrect_core::Chord,
) -> Result<(), hyprcorrect_platform::linux::hotkey::HotkeyError> {
    use hyprcorrect_platform::linux::hotkey;
    let _ = hotkey::uninstall_bind(old);
    if let Err(e) = hotkey::install_bind(new) {
        let _ = hotkey::install_bind(old);
        return Err(e);
    }
    Ok(())
}

/// Correct the buffer's last word in place via the offline provider.
#[cfg(target_os = "linux")]
fn fix_last_word(
    buffer: &mut hyprcorrect_core::Buffer,
    provider: &hyprcorrect_core::OfflineProvider,
) {
    use hyprcorrect_core::plan_word_replacement;
    use hyprcorrect_platform::linux::emit;

    let Some(last) = buffer.last_word() else {
        return;
    };
    let Some(correction) = provider.check_text(&last.word).into_iter().next() else {
        return;
    };
    let Some(fix) = correction.suggestions.into_iter().next() else {
        return;
    };
    let Some(edit) = plan_word_replacement(&last, &fix) else {
        return;
    };
    match emit::replace(edit.backspaces, &edit.insert) {
        Ok(()) => buffer.apply(edit.backspaces, &edit.insert),
        Err(e) => eprintln!("hyprcorrect: {e}"),
    }
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
