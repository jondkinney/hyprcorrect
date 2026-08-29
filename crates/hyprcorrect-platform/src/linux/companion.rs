//! Bounded native bridge for the Omarchy/Quickshell companion.
//!
//! A bar-widget instance runs `hyprcorrect shell watch`. That client connects
//! to this private Unix socket and receives one compact JSON snapshot every
//! 500 ms. The daemon also gets attach/detach events so its StatusNotifierItem
//! can become `Passive` while the native bar widget is present and return to
//! `Active` as soon as the widget goes away.

use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use hyprcorrect_core::{Config, ProviderId, runtime};
use serde_json::json;

const REQUEST_LIMIT: u64 = 32;
const SNAPSHOT_LIMIT: usize = 8 * 1024;
const HOTKEY_LIMIT: usize = 96;
const ERROR_LIMIT: usize = 180;

/// A lifecycle event for one live bar-widget bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompanionEvent {
    Connected,
    Disconnected,
}

/// Start the daemon side of the companion socket.
pub fn start_listener(paused: Arc<AtomicBool>) -> io::Result<Receiver<CompanionEvent>> {
    let path = runtime::companion_socket_path();
    remove_stale_socket(&path)?;
    let listener = UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;

    let (events_tx, events_rx) = mpsc::channel();
    thread::Builder::new()
        .name("hyprcorrect-companion".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let paused = paused.clone();
                        let events_tx = events_tx.clone();
                        let _ = thread::Builder::new()
                            .name("hyprcorrect-companion-client".into())
                            .spawn(move || serve_client(stream, paused, events_tx));
                    }
                    Err(error) => {
                        log::warn!("hyprcorrect companion accept failed: {error}");
                    }
                }
            }
        })?;

    Ok(events_rx)
}

/// Run the stdout-facing side used by Quickshell's bounded `SplitParser`.
pub fn watch() -> io::Result<()> {
    let mut stream = UnixStream::connect(runtime::companion_socket_path())?;
    stream.write_all(b"watch\n")?;
    let result = io::copy(&mut stream, &mut io::stdout());
    match result {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error),
    }
}

fn serve_client(
    mut stream: UnixStream,
    paused: Arc<AtomicBool>,
    events_tx: Sender<CompanionEvent>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));

    let request = {
        let mut reader = BufReader::new(&stream);
        let mut request = String::new();
        match reader.by_ref().take(REQUEST_LIMIT).read_line(&mut request) {
            Ok(_) => request,
            Err(_) => return,
        }
    };
    if request != "watch\n" {
        return;
    }
    let _ = stream.set_read_timeout(None);

    if events_tx.send(CompanionEvent::Connected).is_err() {
        return;
    }

    loop {
        let snapshot = snapshot_line(paused.load(Ordering::Relaxed));
        if snapshot.len() > SNAPSHOT_LIMIT
            || stream.write_all(snapshot.as_bytes()).is_err()
            || stream.write_all(b"\n").is_err()
            || stream.flush().is_err()
        {
            break;
        }
        thread::sleep(Duration::from_millis(500));
    }

    let _ = events_tx.send(CompanionEvent::Disconnected);
}

fn snapshot_line(paused: bool) -> String {
    let (config, error) = match Config::load() {
        Ok(config) => (config, String::new()),
        Err(error) => (Config::default(), bounded(&error.to_string(), ERROR_LIMIT)),
    };

    let payload = json!({
        "schema_version": 1,
        "paused": paused,
        "default_provider": provider_name(config.providers.default),
        "smart_provider": provider_name(config.providers.smart),
        "review_starts_in_vim": config.behavior.review_starts_in_vim,
        "languagetool_enabled": config.providers.languagetool.enabled,
        "llm_configured": !config.providers.llms.is_empty(),
        "hotkeys": {
            "fix_word": bounded(&config.hotkeys.fix_word, HOTKEY_LIMIT),
            "fix_sentence": bounded(&config.hotkeys.fix_sentence, HOTKEY_LIMIT),
            "review": bounded(&config.hotkeys.review, HOTKEY_LIMIT),
            "review_llm": bounded(&config.hotkeys.review_llm, HOTKEY_LIMIT),
        },
        "error": error,
    });

    serde_json::to_string(&payload).unwrap_or_else(|_| {
        r#"{"schema_version":1,"error":"Could not encode companion status"}"#.to_string()
    })
}

fn provider_name(provider: ProviderId) -> &'static str {
    match provider {
        ProviderId::Spellbook => "spellbook",
        ProviderId::Llm => "llm",
        ProviderId::LanguageTool => "languagetool",
    }
}

fn bounded(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .filter(|character| !is_unsafe_text_control(*character))
        .take(max_chars)
        .collect()
}

fn is_unsafe_text_control(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061c}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
        )
}

fn remove_stale_socket(path: &std::path::Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_socket() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("refusing to replace non-socket path {}", path.display()),
                ));
            }
            if UnixStream::connect(path).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("companion socket already in use: {}", path.display()),
                ));
            }
            fs::remove_file(path)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_removes_controls_and_caps_characters() {
        assert_eq!(bounded("ABC\nDEF🙂XYZ", 8), "ABCDEF🙂X");
        assert_eq!(bounded("safe\u{0085}<b>\u{202e}text", 32), "safe<b>text");
        assert_eq!(bounded("12345678", 8), "12345678");
        assert_eq!(bounded("123456789", 8), "12345678");
    }

    #[test]
    fn fallback_snapshot_is_a_bounded_single_json_line() {
        let line = snapshot_line(false);
        assert!(line.len() < SNAPSHOT_LIMIT);
        assert!(!line.contains('\n'));
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["paused"], false);
    }
}
