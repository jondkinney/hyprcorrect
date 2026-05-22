//! hyprcorrect — keyboard-driven desktop spelling and typo correction.
//!
//! Running `hyprcorrect` with no subcommand starts the daemon: it
//! captures keystrokes and, when the trigger key is pressed, corrects
//! the last typed word in place. See `DESIGN.md` at the repository root.

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
                 corrects the last word when you press the trigger key"
            );
        }
        Some(Command::FixSentence) => not_yet("fix-sentence", "M4"),
        Some(Command::Review) => not_yet("the review popup", "M4"),
        Some(Command::Prefs) => hyprcorrect_ui::run_preferences(),
    }
}

/// Run the background daemon: capture keystrokes, and on the trigger key
/// correct the buffer's last word in place.
#[cfg(target_os = "linux")]
fn run_daemon() {
    use hyprcorrect_core::{Buffer, OfflineProvider};
    use hyprcorrect_platform::linux::capture::{self, Event};

    let provider = match OfflineProvider::en_us() {
        Ok(provider) => provider,
        Err(e) => {
            eprintln!("hyprcorrect: {e}");
            return;
        }
    };
    let events = match capture::start() {
        Ok(events) => events,
        Err(e) => {
            eprintln!("hyprcorrect: {e}");
            return;
        }
    };

    let trigger = std::env::var("HYPRCORRECT_TRIGGER").unwrap_or_else(|_| "Pause".to_string());
    println!(
        "hyprcorrect {} — capturing. Type a word, then press {trigger} to \
         correct it; Ctrl+C to quit.",
        hyprcorrect_core::version(),
    );

    let mut buffer = Buffer::default();
    for event in events {
        match event {
            Event::Key(key) => buffer.push(key),
            Event::Trigger => fix_last_word(&mut buffer, &provider),
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
        return; // the word is spelled correctly
    };
    let Some(fix) = correction.suggestions.into_iter().next() else {
        return; // no suggestion available
    };
    let Some(edit) = plan_word_replacement(&last, &fix) else {
        return; // already correct
    };
    match emit::replace(edit.backspaces, &edit.insert) {
        Ok(()) => buffer.apply(edit.backspaces, &edit.insert),
        Err(e) => eprintln!("hyprcorrect: {e}"),
    }
}

/// The daemon is implemented for Linux first; macOS arrives in M2.
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
