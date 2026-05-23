//! hyprcorrect — keyboard-driven desktop spelling and typo correction.
//!
//! Running `hyprcorrect` with no subcommand starts the daemon: it
//! registers the trigger chord with the GlobalShortcuts portal,
//! captures keystrokes for the buffer, publishes a system-tray icon,
//! and corrects the last typed word in place when the chord fires.
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

/// Run the background daemon: register the portal trigger, capture
/// keystrokes, publish the tray, and correct on the chord.
#[cfg(target_os = "linux")]
fn run_daemon() {
    use std::sync::mpsc;
    use std::thread;

    use hyprcorrect_core::{Buffer, OfflineProvider};
    use hyprcorrect_platform::linux::{capture, hotkey, tray};

    let provider = match OfflineProvider::en_us() {
        Ok(provider) => provider,
        Err(e) => {
            eprintln!("hyprcorrect: {e}");
            return;
        }
    };
    let key_rx = match capture::start() {
        Ok(rx) => rx,
        Err(e) => {
            eprintln!("hyprcorrect: {e}");
            return;
        }
    };
    let trigger_rx = match hotkey::start() {
        Ok(rx) => rx,
        Err(e) => {
            eprintln!("hyprcorrect: {e}");
            return;
        }
    };
    let tray_rx = match tray::start() {
        Ok(rx) => rx,
        Err(e) => {
            eprintln!("hyprcorrect: {e}");
            return;
        }
    };

    let letter = std::env::var("HYPRCORRECT_TRIGGER").unwrap_or_else(|_| "F".to_string());
    println!(
        "hyprcorrect {} — running. Press Super+Ctrl+Shift+Alt+{letter} to correct \
         the last word; quit from the tray menu.",
        hyprcorrect_core::version(),
    );

    enum DaemonEvent {
        Key(hyprcorrect_core::Key),
        Trigger,
        Quit,
    }

    // Merge the three sources into one channel so the main loop can
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
            while trigger_rx.recv().is_ok() {
                if tx.send(DaemonEvent::Trigger).is_err() {
                    break;
                }
            }
        });
    }
    {
        let tx = tx.clone();
        thread::spawn(move || {
            // The tray currently emits only Quit; M3 adds more.
            if matches!(tray_rx.recv(), Ok(tray::TrayEvent::Quit)) {
                let _ = tx.send(DaemonEvent::Quit);
            }
        });
    }
    drop(tx); // the forwarder threads now own all senders

    let mut buffer = Buffer::default();
    for event in rx {
        match event {
            DaemonEvent::Key(key) => buffer.push(key),
            DaemonEvent::Trigger => fix_last_word(&mut buffer, &provider),
            DaemonEvent::Quit => break,
        }
    }
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
