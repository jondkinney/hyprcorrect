//! Hyprland focus and window-close events.
//!
//! Subscribes to Hyprland's IPC event socket
//! (`$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket2.sock`)
//! and translates the line-based event stream into [`FocusEvent`]s. The
//! daemon uses these to keep a per-window keystroke buffer: typing in
//! one window does not poison the buffer of another, and switching back
//! to a window restores its prior buffer state.
//!
//! See `DESIGN.md` — "The keystroke buffer".

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};

/// A focus or window-lifecycle event from Hyprland.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusEvent {
    /// A window gained focus. The payload is the window's IPC address
    /// (lowercase hex, no `0x` prefix).
    Focused(String),
    /// A window was closed. Its keystroke buffer can be dropped.
    Closed(String),
}

/// An error subscribing to Hyprland's IPC event stream.
#[derive(Debug, thiserror::Error)]
pub enum FocusError {
    /// `XDG_RUNTIME_DIR` or `HYPRLAND_INSTANCE_SIGNATURE` is unset — not
    /// running under Hyprland.
    #[error(
        "Hyprland IPC is unavailable: $XDG_RUNTIME_DIR or $HYPRLAND_INSTANCE_SIGNATURE is unset"
    )]
    Env,
    /// The IPC socket could not be opened.
    #[error("could not connect to Hyprland IPC socket: {0}")]
    Connect(String),
    /// The reader thread could not be spawned.
    #[error("could not spawn IPC reader thread: {0}")]
    Thread(String),
}

/// Start the Hyprland focus subscription.
///
/// Returns the address of the currently focused window (if any) plus a
/// receiver of subsequent [`FocusEvent`]s. Knowing the initial focus
/// lets the daemon route the very first keystroke to the right buffer.
///
/// # Errors
///
/// See [`FocusError`].
pub fn start() -> Result<(Option<String>, Receiver<FocusEvent>), FocusError> {
    let socket_path = socket2_path()?;
    let stream = UnixStream::connect(&socket_path)
        .map_err(|e| FocusError::Connect(format!("{}: {e}", socket_path.display())))?;
    let initial = query_active_window();

    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("hyprcorrect-focus".into())
        .spawn(move || read_events(stream, &tx))
        .map_err(|e| FocusError::Thread(e.to_string()))?;

    Ok((initial, rx))
}

fn socket2_path() -> Result<PathBuf, FocusError> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or(FocusError::Env)?;
    let instance = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").ok_or(FocusError::Env)?;
    Ok(PathBuf::from(runtime)
        .join("hypr")
        .join(instance)
        .join(".socket2.sock"))
}

/// Query `hyprctl activewindow -j` for the currently focused window's
/// address. Returns `None` if the call fails or no window is focused —
/// the daemon then waits for the first IPC focus event.
fn query_active_window() -> Option<String> {
    let output = Command::new("hyprctl")
        .args(["activewindow", "-j"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = std::str::from_utf8(&output.stdout).ok()?;
    // Crude extraction beats pulling in a JSON dep for one string field.
    let after_key = text.split("\"address\"").nth(1)?;
    let value = after_key.split_once('"')?.1;
    let addr = value.split_once('"')?.0;
    if addr.is_empty() {
        None
    } else {
        Some(normalize_address(addr))
    }
}

fn read_events(stream: UnixStream, tx: &Sender<FocusEvent>) {
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let Ok(line) = line else { return };
        let Some(event) = parse_event(&line) else {
            continue;
        };
        if tx.send(event).is_err() {
            return; // receiver dropped — daemon is shutting down
        }
    }
}

fn parse_event(line: &str) -> Option<FocusEvent> {
    let (kind, payload) = line.split_once(">>")?;
    match kind {
        "activewindowv2" => Some(FocusEvent::Focused(normalize_address(payload))),
        "closewindow" => Some(FocusEvent::Closed(normalize_address(payload))),
        _ => None,
    }
}

/// Strip a leading `0x` and lowercase the rest, so addresses from
/// `hyprctl activewindow -j` (with prefix) and from `.socket2.sock`
/// events (without prefix) compare equal.
fn normalize_address(addr: &str) -> String {
    addr.trim()
        .strip_prefix("0x")
        .unwrap_or_else(|| addr.trim())
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_active_window_event() {
        assert_eq!(
            parse_event("activewindowv2>>563c9141fe00"),
            Some(FocusEvent::Focused("563c9141fe00".into()))
        );
    }

    #[test]
    fn parses_close_window_event() {
        assert_eq!(
            parse_event("closewindow>>563c9141fe00"),
            Some(FocusEvent::Closed("563c9141fe00".into()))
        );
    }

    #[test]
    fn ignores_other_events() {
        assert!(parse_event("workspace>>2").is_none());
        assert!(parse_event("activewindow>>kitty,fish").is_none());
        assert!(parse_event("openwindow>>abc,1,kitty,fish").is_none());
    }

    #[test]
    fn ignores_malformed_lines() {
        assert!(parse_event("").is_none());
        assert!(parse_event("no separator here").is_none());
    }

    #[test]
    fn normalize_strips_prefix_and_lowercases() {
        assert_eq!(normalize_address("0xAbCdEf"), "abcdef");
        assert_eq!(normalize_address("563c9141fe00"), "563c9141fe00");
        assert_eq!(normalize_address("  0x563C9141FE00  "), "563c9141fe00");
    }
}
