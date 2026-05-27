//! Runtime coordination between the daemon and the prefs subprocess.
//!
//! Both write/read a PID file at the platform's runtime location
//! (`$XDG_RUNTIME_DIR/hyprcorrect.pid` on Linux, `$TMPDIR/...` on
//! macOS) so the prefs window can target SIGHUP at the daemon
//! specifically — `pkill -x hyprcorrect` would catch both processes
//! since they share a binary name.

use std::fs;
use std::path::PathBuf;

/// An error reading or writing the daemon PID file.
#[derive(Debug, thiserror::Error)]
pub enum PidError {
    #[error("pid file I/O: {0}")]
    Io(String),
    #[error("pid file content is not a number: {0}")]
    Parse(String),
}

/// Path to the daemon PID file. Falls back to the OS temp dir when
/// `$XDG_RUNTIME_DIR` is unset (macOS, restricted environments).
pub fn pid_path() -> PathBuf {
    runtime_dir().join("hyprcorrect.pid")
}

/// Path to the trigger-action file. The hyprctl bind writes "word",
/// "sentence", or "review" here before signaling the daemon; the
/// daemon reads it on `SIGUSR1` to know which action fired. The
/// review subprocess also writes "review-apply" / "review-cancel"
/// here when it closes, so the daemon knows what to do with the
/// pending request file.
pub fn action_path() -> PathBuf {
    runtime_dir().join("hyprcorrect.action")
}

/// Path to the chord-capture Unix socket. The prefs window connects
/// here and writes `capture\n` to ask the daemon to deliver the
/// next non-modifier key press (with full modifier mask, including
/// Super) as a chord string. The socket exists because egui-winit
/// on Linux discards Super from `Modifiers`, so the prefs UI cannot
/// record SUPER-containing chords on its own.
pub fn chord_socket_path() -> PathBuf {
    runtime_dir().join("hyprcorrect-chord.sock")
}

/// Path to the review-request file. The daemon writes the original
/// sentence + the proposed correction + trailing whitespace + the
/// originating window's address here when the review chord fires;
/// the review subprocess reads it to populate the popup, then
/// updates the same path with its decision on exit so the daemon's
/// apply handler can finish the job.
pub fn review_path() -> PathBuf {
    runtime_dir().join("hyprcorrect.review")
}

/// A pending review request — what the user typed, what the smart
/// provider suggested, and where to emit the result.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReviewRequest {
    /// The sentence as it sits in the focused window's buffer.
    pub original: String,
    /// The smart provider's proposed correction.
    pub corrected: String,
    /// Whitespace between the sentence's right edge and the caret —
    /// preserved so the emit lands with the user's spacing intact.
    pub trailing: String,
    /// How many characters of `original` sit BEFORE the caret —
    /// determines the BackSpace count when the apply path emits.
    #[serde(default)]
    pub chars_before_caret: usize,
    /// How many characters of `original` sit AFTER the caret —
    /// determines the Delete count when the apply path emits.
    /// Zero for the common case where the caret is at the end of
    /// (or in trailing whitespace after) the sentence.
    #[serde(default)]
    pub chars_after_caret: usize,
    /// Hyprland address of the window the request originated from —
    /// the daemon uses it to update that window's buffer when the
    /// user accepts.
    pub window_address: String,
}

/// Write a fresh review request to disk. Overwrites any pending one.
///
/// # Errors
///
/// I/O errors are surfaced; the daemon logs and skips the spawn if
/// this fails, so a half-written file doesn't trip up the popup.
pub fn write_review_request(req: &ReviewRequest) -> Result<(), PidError> {
    let json = serde_json::to_string(req).map_err(|e| PidError::Io(e.to_string()))?;
    let path = review_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| PidError::Io(e.to_string()))?;
    }
    fs::write(&path, json).map_err(|e| PidError::Io(e.to_string()))
}

/// Read the pending review request, or `None` if no file exists.
///
/// # Errors
///
/// See [`PidError`].
pub fn read_review_request() -> Result<Option<ReviewRequest>, PidError> {
    match fs::read_to_string(review_path()) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| PidError::Parse(e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(PidError::Io(e.to_string())),
    }
}

/// Remove the review-request file (idempotent).
pub fn clear_review() {
    let _ = fs::remove_file(review_path());
}

/// Read the trigger-action file, returning the trimmed contents. An
/// empty string is returned if the file is missing or unreadable —
/// callers treat that as "default action" (fix-last-word).
pub fn read_action() -> String {
    std::fs::read_to_string(action_path())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// Write the current process's PID to the daemon PID file.
///
/// # Errors
///
/// Returns [`PidError::Io`] if the file can't be written.
pub fn write_self_pid() -> Result<(), PidError> {
    let path = pid_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| PidError::Io(e.to_string()))?;
    }
    fs::write(&path, std::process::id().to_string()).map_err(|e| PidError::Io(e.to_string()))
}

/// Remove the daemon PID file (idempotent — missing file is OK). The
/// action file is removed alongside it since the two have the same
/// lifecycle: both are owned by the running daemon.
pub fn clear_pid() {
    let _ = fs::remove_file(pid_path());
    let _ = fs::remove_file(action_path());
}

/// Read the daemon's PID from the file. Returns `Ok(None)` if no file
/// exists (no daemon running).
///
/// # Errors
///
/// See [`PidError`].
pub fn read_daemon_pid() -> Result<Option<i32>, PidError> {
    match fs::read_to_string(pid_path()) {
        Ok(text) => text
            .trim()
            .parse::<i32>()
            .map(Some)
            .map_err(|e| PidError::Parse(e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(PidError::Io(e.to_string())),
    }
}
