//! hyprcorrect — keyboard-driven desktop spelling and typo correction.
//!
//! This is the M0 scaffold: the CLI surface and crate wiring exist, but
//! the background daemon, the capture/replace engine, and the GUI arrive
//! in later milestones. See `DESIGN.md` at the repository root.

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
        Some(Command::FixWord) => not_yet("fix-word", "M1"),
        Some(Command::FixSentence) => not_yet("fix-sentence", "M4"),
        Some(Command::Review) => not_yet("the review popup", "M4"),
        Some(Command::Prefs) => hyprcorrect_ui::run_preferences(),
    }
}

/// Run the background daemon: the tray, hotkey listener, and keystroke
/// buffer. Implemented from milestone M1.
fn run_daemon() {
    println!(
        "hyprcorrect {} ({} backend)",
        hyprcorrect_core::version(),
        hyprcorrect_platform::backend_name(),
    );
    println!("the daemon is not implemented yet (M1) — see DESIGN.md");
}

fn not_yet(what: &str, milestone: &str) {
    eprintln!("hyprcorrect: {what} is not implemented yet ({milestone}) — see DESIGN.md");
}
