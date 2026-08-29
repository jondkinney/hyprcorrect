//! Global trigger via a Hyprland inline keybind + signals.
//!
//! At startup the daemon adds an inline Hyprland keybind whose `exec`
//! invokes the binary's hidden, validated `shell trigger` command. That
//! command publishes the action through the descriptor-safe runtime handoff
//! and signals only an owned process identified as hyprcorrect.
//! Lua-configured Hyprland sessions use `hyprctl eval` with `hl.bind`;
//! legacy hyprlang sessions use `hyprctl keyword bind`. Hyprland
//! intercepts the chord — terminals and other focused apps never see
//! it — and the daemon catches the signal as [`HotkeyEvent::Trigger`].
//!
//! The PID-file-based targeting is deliberate: `pkill -x hyprcorrect`
//! would match the prefs subprocess too (it shares the daemon's
//! binary name and therefore its `/proc/PID/comm`) and silently
//! terminate the prefs window when the user pressed the chord. The
//! file is written by the daemon at startup and removed on shutdown
//! — see [`hyprcorrect_core::runtime`].
//!
//! `SIGHUP` arrives as [`HotkeyEvent::Reload`] and is the prefs
//! window's signal to the running daemon that the config has
//! changed.
//!
//! Hyprland-specific. The cross-compositor route is the
//! `GlobalShortcuts` portal (DESIGN.md); that has its own auto-bind
//! limitation on `xdg-desktop-portal-hyprland` today, so we'll revisit
//! it together with M3's portable backends.

use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};

use hyprcorrect_core::Chord;
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGUSR1, SIGUSR2};
use signal_hook::iterator::Signals;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigProvider {
    Lua,
    Legacy,
}

/// A daemon-level event driven by the operating-system signal stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    /// `SIGUSR1` — the trigger chord fired. Run `fix-last-word`.
    Trigger,
    /// `SIGHUP` — the user saved the config. Reload it and rebind the
    /// trigger if the chord changed.
    Reload,
    /// `SIGUSR2` — the prefs window entered chord-capture mode and
    /// wants Hyprland to stop intercepting the chord so the prefs
    /// window can see the key press. The daemon temporarily
    /// uninstalls its bind; `Reload` reinstalls it after capture.
    Release,
    /// `SIGTERM` / `SIGINT` — the daemon should shut down cleanly so
    /// the Hyprland bind and PID file are removed.
    Shutdown,
}

/// An error registering the Hyprland keybind or signal handler.
#[derive(Debug, thiserror::Error)]
pub enum HotkeyError {
    /// Could not resolve the daemon executable for the compositor command.
    #[error("could not resolve the Hyprcorrect executable: {0}")]
    Executable(String),
    /// `hyprctl` could not bind the trigger chord.
    #[error("hyprctl could not bind the trigger chord: {0}")]
    Hyprctl(String),
    /// `hyprctl` could not unbind the trigger chord.
    #[error("hyprctl could not unbind the trigger chord: {0}")]
    HyprctlUnbind(String),
    /// Could not install the signal handler.
    #[error("could not install signal handler: {0}")]
    Signal(String),
    /// Could not spawn the signal-listener thread.
    #[error("could not spawn the signal-listener thread: {0}")]
    Thread(String),
}

/// Install the Hyprland inline keybind for the given chord, tagged
/// with an `action` label ("word", "sentence", "review", …).
///
/// The bind's `exec` invokes this exact executable's validated
/// `shell trigger` command. That command publishes the fixed action
/// through the private runtime protocol and signals the verified daemon.
///
/// Idempotent: first unbinds the same chord through the active config
/// provider so a previous (uncleanly-shut-down) daemon's bind doesn't
/// leave duplicates behind.
///
/// # Errors
///
/// See [`HotkeyError`].
pub fn install_bind(chord: &Chord, action: &str) -> Result<(), HotkeyError> {
    let provider = config_provider().map_err(HotkeyError::Hyprctl)?;
    let _ = uninstall_bind_with_provider(chord, provider);
    let command = trigger_command(action).map_err(HotkeyError::Executable)?;

    match provider {
        ConfigProvider::Lua => {
            let expression = format!(
                "hl.bind({}, hl.dsp.exec_cmd({}), {{ description = {} }})",
                lua_quote(&lua_chord(chord)),
                lua_quote(&command),
                lua_quote(&format!("Hyprcorrect: {}", action_description(action))),
            );
            run_hyprctl(&["eval", &expression]).map_err(HotkeyError::Hyprctl)
        }
        ConfigProvider::Legacy => {
            let bind_value = format!(
                "{mods}, {key}, exec, {command}",
                mods = chord.hyprland_modifiers(),
                key = chord.hyprland_key(),
            );
            run_hyprctl(&["keyword", "bind", &bind_value]).map_err(HotkeyError::Hyprctl)
        }
    }
}

/// Remove the Hyprland inline keybind for the given chord. Calling
/// this for an unbound chord is silently fine.
///
/// # Errors
///
/// Returns [`HotkeyError::HyprctlUnbind`] only on `hyprctl` invocation
/// failure (not on "nothing to unbind").
pub fn uninstall_bind(chord: &Chord) -> Result<(), HotkeyError> {
    let provider = config_provider().map_err(HotkeyError::HyprctlUnbind)?;
    uninstall_bind_with_provider(chord, provider).map_err(HotkeyError::HyprctlUnbind)
}

fn uninstall_bind_with_provider(chord: &Chord, provider: ConfigProvider) -> Result<(), String> {
    match provider {
        ConfigProvider::Lua => {
            let expression = format!("hl.unbind({})", lua_quote(&lua_chord(chord)));
            run_hyprctl(&["eval", &expression])
        }
        ConfigProvider::Legacy => {
            let unbind_value = format!(
                "{mods}, {key}",
                mods = chord.hyprland_modifiers(),
                key = chord.hyprland_key(),
            );
            run_hyprctl(&["keyword", "unbind", &unbind_value])
        }
    }
}

fn config_provider() -> Result<ConfigProvider, String> {
    let output = bounded_hyprctl(&["status", "-j"])
        .map_err(|e| format!("invoke `hyprctl status -j`: {e}"))?;
    if !output.status.success() {
        return Err(command_failure("hyprctl status -j", &output));
    }
    parse_config_provider(&output.stdout)
}

fn parse_config_provider(stdout: &[u8]) -> Result<ConfigProvider, String> {
    let status: serde_json::Value =
        serde_json::from_slice(stdout).map_err(|e| format!("parse `hyprctl status -j`: {e}"))?;
    match status
        .get("configProvider")
        .and_then(|value| value.as_str())
    {
        Some("lua") => Ok(ConfigProvider::Lua),
        Some(_) => Ok(ConfigProvider::Legacy),
        None => Err("`hyprctl status -j` omitted configProvider".to_string()),
    }
}

fn run_hyprctl(args: &[&str]) -> Result<(), String> {
    let output = bounded_hyprctl(args).map_err(|e| format!("invoke hyprctl: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || stdout.trim() != "ok" {
        return Err(command_failure(
            &format!("hyprctl {}", args.join(" ")),
            &output,
        ));
    }
    Ok(())
}

fn bounded_hyprctl(args: &[&str]) -> std::io::Result<hyprcorrect_core::bounded_process::Output> {
    hyprcorrect_core::bounded_process::output(
        Command::new("hyprctl").args(args),
        std::time::Duration::from_secs(3),
        64 * 1024,
        64 * 1024,
    )
}

fn command_failure(command: &str, output: &hyprcorrect_core::bounded_process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    format!(
        "{command} failed with {} — stdout: {stdout:?} stderr: {stderr:?}",
        output.status
    )
}

fn lua_chord(chord: &Chord) -> String {
    let modifiers = chord.hyprland_modifiers().replace(' ', " + ");
    if modifiers.is_empty() {
        chord.hyprland_key().to_string()
    } else {
        format!("{modifiers} + {}", chord.hyprland_key())
    }
}

fn trigger_command(action: &str) -> Result<String, String> {
    let executable = std::env::current_exe()
        .map_err(|error| error.to_string())?
        .to_string_lossy()
        .into_owned();
    Ok(format!(
        "{} shell trigger {}",
        shell_quote(&executable),
        shell_quote(action),
    ))
}

fn action_description(action: &str) -> &str {
    match action {
        "word" => "Fix last word",
        "sentence" => "Fix last sentence",
        "review" => "Review correction",
        "review-llm" => "Escalate review to LLM",
        _ => action,
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn lua_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            character if character.is_control() => {
                use std::fmt::Write;
                let _ = write!(quoted, "\\{:03}", character as u32);
            }
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

/// Start the signal listener.
///
/// Installs handlers for `SIGUSR1` (trigger), `SIGHUP` (reload), and
/// `SIGTERM` / `SIGINT` (shutdown) and returns a receiver of
/// [`HotkeyEvent`]s. The shutdown signals let the daemon clean up its
/// Hyprland bind and PID file even when killed via `pkill` or Ctrl-C.
///
/// # Errors
///
/// See [`HotkeyError`].
pub fn signal_channel() -> Result<Receiver<HotkeyEvent>, HotkeyError> {
    let mut signals = Signals::new([SIGUSR1, SIGUSR2, SIGHUP, SIGTERM, SIGINT])
        .map_err(|e| HotkeyError::Signal(e.to_string()))?;
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("hyprcorrect-signal".into())
        .spawn(move || forward_signals(&mut signals, &tx))
        .map_err(|e| HotkeyError::Thread(e.to_string()))?;
    Ok(rx)
}

fn forward_signals(signals: &mut Signals, tx: &Sender<HotkeyEvent>) {
    for signal in signals.forever() {
        let event = match signal {
            SIGUSR1 => HotkeyEvent::Trigger,
            SIGUSR2 => HotkeyEvent::Release,
            SIGHUP => HotkeyEvent::Reload,
            SIGTERM | SIGINT => HotkeyEvent::Shutdown,
            _ => continue,
        };
        if tx.send(event).is_err() {
            break; // receiver dropped — daemon is shutting down
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hyprland_config_provider() {
        assert_eq!(
            parse_config_provider(br#"{"configProvider":"lua","backend":"drm"}"#).unwrap(),
            ConfigProvider::Lua
        );
        assert_eq!(
            parse_config_provider(br#"{"configProvider":"hyprlang"}"#).unwrap(),
            ConfigProvider::Legacy
        );
        assert!(parse_config_provider(br#"{"backend":"drm"}"#).is_err());
    }

    #[test]
    fn renders_lua_chord_syntax() {
        let chord = Chord::parse("SUPER+CTRL+SHIFT+F").unwrap();
        assert_eq!(lua_chord(&chord), "CTRL + SHIFT + SUPER + F");
        let chord = Chord::parse("ENTER").unwrap();
        assert_eq!(lua_chord(&chord), "Return");
    }

    #[test]
    fn quotes_shell_and_lua_values() {
        assert_eq!(shell_quote("it's"), r#"'it'"'"'s'"#);
        assert_eq!(lua_quote("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
        let command = trigger_command("word").unwrap();
        assert!(command.ends_with(" shell trigger 'word'"));
        assert!(!command.starts_with("'hyprcorrect'"));
    }
}
