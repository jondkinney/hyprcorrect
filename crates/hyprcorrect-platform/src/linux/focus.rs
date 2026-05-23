//! Hyprland focus and window-close events.
//!
//! Subscribes to Hyprland's IPC event socket
//! (`$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket2.sock`)
//! and translates the line-based event stream into [`FocusEvent`]s. The
//! daemon uses these to keep a per-window keystroke buffer — typing in
//! one window does not poison the buffer of another, and switching back
//! to a window restores its prior buffer state — and to apply the
//! privacy app-blocklist (gating buffer accumulation by window class).
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
    /// A window gained focus. The address is lowercase hex without
    /// `0x`. The class is taken from the immediately-preceding
    /// `activewindow>>` line (or empty if Hyprland did not emit one).
    Focused { address: String, class: String },
    /// A window was closed. Its keystroke buffer can be dropped.
    Closed { address: String },
}

/// The currently focused window at startup, used to seed the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitialFocus {
    pub address: String,
    pub class: String,
}

/// An error subscribing to Hyprland's IPC event stream.
#[derive(Debug, thiserror::Error)]
pub enum FocusError {
    /// `XDG_RUNTIME_DIR` or `HYPRLAND_INSTANCE_SIGNATURE` is unset —
    /// not running under Hyprland.
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
/// Returns the currently-focused window (address + class) — if any —
/// plus a receiver of subsequent [`FocusEvent`]s.
///
/// # Errors
///
/// See [`FocusError`].
pub fn start() -> Result<(Option<InitialFocus>, Receiver<FocusEvent>), FocusError> {
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

/// Query `hyprctl activewindow -j` for the currently focused window.
/// Returns `None` if the call fails or no window is focused.
fn query_active_window() -> Option<InitialFocus> {
    let output = Command::new("hyprctl")
        .args(["activewindow", "-j"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = std::str::from_utf8(&output.stdout).ok()?;
    let address = extract_json_string(text, "address")?;
    let class = extract_json_string(text, "class").unwrap_or_default();
    let address = normalize_address(&address);
    if address.is_empty() {
        None
    } else {
        Some(InitialFocus { address, class })
    }
}

/// Crude extraction of a top-level string field from JSON. Beats
/// pulling in `serde_json` for two fields.
fn extract_json_string(text: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\"");
    let after_key = text.split(&needle).nth(1)?;
    // Step past the `:` and leading whitespace.
    let after_colon = after_key.split_once(':')?.1.trim_start();
    let value = after_colon.strip_prefix('"')?;
    let (s, _) = value.split_once('"')?;
    Some(s.to_string())
}

fn read_events(stream: UnixStream, tx: &Sender<FocusEvent>) {
    let reader = BufReader::new(stream);
    // The text-form `activewindow>>CLASS,TITLE` event always arrives
    // before its sibling `activewindowv2>>ADDR`; buffer the class and
    // attach it when the address lands.
    let mut last_class: Option<String> = None;
    for line in reader.lines() {
        let Ok(line) = line else { return };
        let Some((kind, payload)) = line.split_once(">>") else {
            continue;
        };
        match kind {
            "activewindow" => {
                last_class = Some(
                    payload
                        .split_once(',')
                        .map_or(payload, |(class, _)| class)
                        .to_string(),
                );
            }
            "activewindowv2" => {
                let address = normalize_address(payload);
                let class = last_class.clone().unwrap_or_default();
                if tx.send(FocusEvent::Focused { address, class }).is_err() {
                    return; // receiver dropped — daemon is shutting down
                }
            }
            "closewindow" => {
                let address = normalize_address(payload);
                if tx.send(FocusEvent::Closed { address }).is_err() {
                    return;
                }
            }
            _ => {}
        }
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
    use std::sync::mpsc;

    fn run(lines: &[&str]) -> Vec<FocusEvent> {
        // We can't easily spin up a UnixStream in a unit test, so
        // exercise the line-parsing arm directly.
        let (tx, rx) = mpsc::channel();
        let mut last_class: Option<String> = None;
        for line in lines {
            let Some((kind, payload)) = line.split_once(">>") else {
                continue;
            };
            match kind {
                "activewindow" => {
                    last_class = Some(
                        payload
                            .split_once(',')
                            .map_or(payload, |(c, _)| c)
                            .to_string(),
                    );
                }
                "activewindowv2" => {
                    let _ = tx.send(FocusEvent::Focused {
                        address: normalize_address(payload),
                        class: last_class.clone().unwrap_or_default(),
                    });
                }
                "closewindow" => {
                    let _ = tx.send(FocusEvent::Closed {
                        address: normalize_address(payload),
                    });
                }
                _ => {}
            }
        }
        drop(tx);
        rx.iter().collect()
    }

    #[test]
    fn pairs_text_class_with_v2_address() {
        let events = run(&[
            "workspace>>2",
            "activewindow>>kitty,fish",
            "activewindowv2>>563c9141fe00",
        ]);
        assert_eq!(
            events,
            vec![FocusEvent::Focused {
                address: "563c9141fe00".into(),
                class: "kitty".into(),
            }]
        );
    }

    #[test]
    fn close_emits_closed() {
        let events = run(&["closewindow>>0xAbCdEf"]);
        assert_eq!(
            events,
            vec![FocusEvent::Closed {
                address: "abcdef".into(),
            }]
        );
    }

    #[test]
    fn ignores_unknown_events() {
        let events = run(&[
            "workspace>>2",
            "openwindow>>abc,1,kitty,fish",
            "monitor>>DP-1",
        ]);
        assert!(events.is_empty());
    }

    #[test]
    fn extract_json_string_handles_typical_hyprctl_json() {
        let json = r#"{"address": "0x563c9141fe00", "class":"kitty"}"#;
        assert_eq!(
            extract_json_string(json, "address"),
            Some("0x563c9141fe00".into())
        );
        assert_eq!(extract_json_string(json, "class"), Some("kitty".into()));
    }

    #[test]
    fn normalize_strips_prefix_and_lowercases() {
        assert_eq!(normalize_address("0xAbCdEf"), "abcdef");
        assert_eq!(normalize_address("563c9141fe00"), "563c9141fe00");
        assert_eq!(normalize_address("  0x563C9141FE00  "), "563c9141fe00");
    }
}
