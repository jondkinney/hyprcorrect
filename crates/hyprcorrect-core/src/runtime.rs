//! Runtime coordination between the daemon and the prefs subprocess.
//!
//! Both write/read a PID file at the platform's runtime location
//! (`$XDG_RUNTIME_DIR/hyprcorrect.pid` on Linux, `$TMPDIR/...` on
//! macOS) so the prefs window can target SIGHUP at the daemon
//! specifically — `pkill -x hyprcorrect` would catch both processes
//! since they share a binary name.

use std::path::PathBuf;

use crate::secure_fs;

const MAX_PID_BYTES: usize = 32;
const MAX_ACTION_BYTES: usize = 64;
const MAX_REVIEW_BYTES: usize = 2 * 1024 * 1024;

/// An error reading or writing the daemon PID file.
#[derive(Debug, thiserror::Error)]
pub enum PidError {
    #[error("pid file I/O: {0}")]
    Io(String),
    #[error("pid file content is not a number: {0}")]
    Parse(String),
}

/// Path to the daemon PID file. Falls back to an owner-only subdirectory of
/// the OS temp dir when `$XDG_RUNTIME_DIR` is unset.
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

/// Path to the private native bridge used by the Omarchy bar companion.
pub fn companion_socket_path() -> PathBuf {
    runtime_dir().join("hyprcorrect-companion.sock")
}

/// Path to the preferences-window singleton socket.
pub fn prefs_socket_path() -> PathBuf {
    runtime_dir().join("hyprcorrect-prefs.sock")
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

/// Ranked alternative spellings for one corrected word, for the review
/// popup's per-field suggestion dropdown. `options` is best-first and
/// the first entry is normally the applied correction; the popup drops
/// whatever matches the field's current text and shows the rest.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WordSuggestions {
    /// The corrected word these options belong to.
    pub word: String,
    /// Candidate replacements, best first.
    pub options: Vec<String>,
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
    /// Ranked backup suggestions for each changed word, ordered by the
    /// word's position in `corrected` so it lines up with the popup's
    /// editable fields. Empty when no provider offered alternatives.
    #[serde(default)]
    pub suggestions: Vec<WordSuggestions>,
    /// `true` while the daemon is still computing the correction (e.g.
    /// an in-flight LLM call). The popup is spawned immediately in this
    /// state — showing the original text and a "Checking…" line — and
    /// re-reads the request until the daemon writes the finished one
    /// with `pending: false`.
    #[serde(default)]
    pub pending: bool,
    /// Logical width (points) of the monitor the source window sits on,
    /// so the popup can grow with the sentence up to half the screen.
    /// Zero when unknown — the popup then falls back to a fixed cap.
    #[serde(default)]
    pub screen_width: f32,
    /// Usable logical height (points) of that monitor — the full height
    /// minus any reserved areas (e.g. a top waybar). Lets the popup grow
    /// to fit its content up to the screen without ever sliding under the
    /// bar. Zero when unknown — the popup then falls back to a fixed cap.
    #[serde(default)]
    pub screen_height: f32,
    /// Whether the daemon has an LLM provider configured. The popup shows
    /// its "Ask LLM" escalation button only when this is true.
    #[serde(default)]
    pub llm_available: bool,
    /// Whether the LLM produced the `corrected` text shown. When `true`
    /// the popup hides the "Ask LLM" button — the result is already the
    /// LLM's, so there's nothing to escalate. Keyed on the provider that
    /// actually produced the correction, so an LLM miss that fell back to
    /// LanguageTool/Spellbook still offers the button.
    #[serde(default)]
    pub from_llm: bool,
}

/// Write a fresh review request to disk. Overwrites any pending one.
///
/// # Errors
///
/// I/O errors are surfaced; the daemon logs and skips the spawn if
/// this fails, so a half-written file doesn't trip up the popup.
pub fn write_review_request(req: &ReviewRequest) -> Result<(), PidError> {
    let json = serde_json::to_string(req).map_err(|e| PidError::Io(e.to_string()))?;
    if json.len() > MAX_REVIEW_BYTES {
        return Err(PidError::Io(format!(
            "review request exceeds the {MAX_REVIEW_BYTES}-byte limit"
        )));
    }
    ensure_runtime_dir()?;
    secure_fs::atomic_write(&review_path(), json.as_bytes(), 0o600)
        .map_err(|error| PidError::Io(error.to_string()))
}

/// Read the pending review request, or `None` if no file exists.
///
/// # Errors
///
/// See [`PidError`].
pub fn read_review_request() -> Result<Option<ReviewRequest>, PidError> {
    ensure_runtime_dir()?;
    match secure_fs::read_limited(&review_path(), MAX_REVIEW_BYTES)
        .map_err(|error| PidError::Io(error.to_string()))?
    {
        Some(snapshot) => serde_json::from_slice(&snapshot.bytes)
            .map(Some)
            .map_err(|e| PidError::Parse(e.to_string())),
        None => Ok(None),
    }
}

/// Remove the review-request file (idempotent).
pub fn clear_review() {
    if ensure_runtime_dir().is_ok() {
        let _ = secure_fs::remove_file(&review_path());
    }
}

/// Read the trigger-action file, returning the trimmed contents. An
/// empty string is returned if the file is missing or unreadable —
/// callers treat that as "default action" (fix-last-word).
pub fn read_action() -> String {
    ensure_runtime_dir()
        .and_then(|()| {
            secure_fs::read_limited(&action_path(), MAX_ACTION_BYTES)
                .map_err(|error| PidError::Io(error.to_string()))
        })
        .ok()
        .flatten()
        .and_then(|snapshot| String::from_utf8(snapshot.bytes).ok())
        .map(|value| value.trim().to_string())
        .unwrap_or_default()
}

/// Atomically publish a fixed daemon action before raising `SIGUSR1`.
pub fn write_action(action: &str) -> Result<(), PidError> {
    if action.is_empty()
        || action.len() > MAX_ACTION_BYTES
        || !action
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'-')
    {
        return Err(PidError::Io("invalid daemon action".into()));
    }
    ensure_runtime_dir()?;
    secure_fs::atomic_write(&action_path(), action.as_bytes(), 0o600)
        .map_err(|error| PidError::Io(error.to_string()))
}

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let temporary = std::env::temp_dir()
                .canonicalize()
                .unwrap_or_else(|_| std::env::temp_dir());
            #[cfg(unix)]
            let identity = unsafe { libc::geteuid() };
            #[cfg(not(unix))]
            let identity = std::process::id();
            temporary.join(format!("hyprcorrect-{identity}"))
        })
}

/// Validate the session runtime directory, or create an owner-only fallback
/// when the platform did not provide `XDG_RUNTIME_DIR`.
///
/// # Errors
///
/// Refuses symbolic links, unexpected owners, and group/world-accessible
/// directories.
pub fn ensure_runtime_dir() -> Result<(), PidError> {
    let supplied = std::env::var_os("XDG_RUNTIME_DIR").is_some();
    secure_fs::ensure_private_directory(&runtime_dir(), !supplied)
        .map_err(|error| PidError::Io(format!("unsafe runtime directory: {error}")))
}

/// Write the current process's PID to the daemon PID file.
///
/// # Errors
///
/// Returns [`PidError::Io`] if the file can't be written.
pub fn write_self_pid() -> Result<(), PidError> {
    ensure_runtime_dir()?;
    secure_fs::atomic_write(
        &pid_path(),
        std::process::id().to_string().as_bytes(),
        0o600,
    )
    .map_err(|error| PidError::Io(error.to_string()))
}

/// Remove the daemon PID file (idempotent — missing file is OK). The
/// action file is removed alongside it since the two have the same
/// lifecycle: both are owned by the running daemon.
pub fn clear_pid() {
    if ensure_runtime_dir().is_ok() {
        let _ = secure_fs::remove_file(&pid_path());
        let _ = secure_fs::remove_file(&action_path());
    }
}

/// Read the daemon's PID from the file. Returns `Ok(None)` if no file
/// exists (no daemon running).
///
/// # Errors
///
/// See [`PidError`].
pub fn read_daemon_pid() -> Result<Option<i32>, PidError> {
    ensure_runtime_dir()?;
    match secure_fs::read_limited(&pid_path(), MAX_PID_BYTES)
        .map_err(|error| PidError::Io(error.to_string()))?
    {
        Some(snapshot) => std::str::from_utf8(&snapshot.bytes)
            .map_err(|error| PidError::Parse(error.to_string()))?
            .trim()
            .parse::<i32>()
            .map(Some)
            .map_err(|e| PidError::Parse(e.to_string())),
        None => Ok(None),
    }
}

/// Signals accepted by the daemon's local control protocol.
#[derive(Debug, Clone, Copy)]
pub enum DaemonSignal {
    Reload,
    Trigger,
    ReleaseHotkeys,
    Terminate,
}

/// Signal the owned daemon named by the descriptor-safe PID file.
///
/// Linux additionally verifies `/proc/<pid>` ownership and process name so a
/// replaced or stale PID file cannot target an unrelated process.
pub fn signal_daemon(signal: DaemonSignal) -> Result<bool, PidError> {
    let Some(pid) = read_daemon_pid()? else {
        return Ok(false);
    };
    #[cfg(unix)]
    {
        let number = match signal {
            DaemonSignal::Reload => libc::SIGHUP,
            DaemonSignal::Trigger => libc::SIGUSR1,
            DaemonSignal::ReleaseHotkeys => libc::SIGUSR2,
            DaemonSignal::Terminate => libc::SIGTERM,
        };
        #[cfg(target_os = "linux")]
        return signal_verified_linux_daemon(pid, number).map(|()| true);

        #[cfg(not(target_os = "linux"))]
        verify_daemon_process(pid)?;
        #[cfg(not(target_os = "linux"))]
        let result = unsafe { libc::kill(pid, number) };
        #[cfg(not(target_os = "linux"))]
        if result == 0 {
            Ok(true)
        } else {
            Err(PidError::Io(format!(
                "could not signal daemon PID {pid}: {}",
                std::io::Error::last_os_error()
            )))
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, signal);
        Err(PidError::Io(
            "daemon signaling is not implemented on this platform".into(),
        ))
    }
}

#[cfg(target_os = "linux")]
fn signal_verified_linux_daemon(pid: i32, signal: i32) -> Result<(), PidError> {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::fs::MetadataExt;

    if pid <= 1 {
        return Err(PidError::Parse("refusing an invalid daemon PID".into()));
    }
    let raw_pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if raw_pidfd < 0 {
        return Err(PidError::Io(format!(
            "cannot pin daemon PID {pid}: {}",
            std::io::Error::last_os_error()
        )));
    }
    let pidfd = unsafe { OwnedFd::from_raw_fd(raw_pidfd as i32) };
    let process = PathBuf::from(format!("/proc/{pid}"));
    let metadata = std::fs::metadata(&process)
        .map_err(|error| PidError::Io(format!("cannot inspect daemon PID {pid}: {error}")))?;
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(PidError::Io(format!(
            "daemon PID {pid} is not owned by the current user"
        )));
    }
    let comm = std::fs::read_to_string(process.join("comm"))
        .map_err(|error| PidError::Io(format!("cannot identify daemon PID {pid}: {error}")))?;
    if comm.trim() != "hyprcorrect" {
        return Err(PidError::Io(format!(
            "PID {pid} is not a hyprcorrect process"
        )));
    }
    let process_executable = std::fs::metadata(process.join("exe"))
        .map_err(|error| PidError::Io(format!("cannot inspect daemon executable: {error}")))?;
    let own_executable = std::env::current_exe()
        .and_then(std::fs::metadata)
        .map_err(|error| PidError::Io(format!("cannot inspect current executable: {error}")))?;
    if process_executable.dev() != own_executable.dev()
        || process_executable.ino() != own_executable.ino()
    {
        return Err(PidError::Io(format!(
            "PID {pid} is not running this hyprcorrect executable"
        )));
    }
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            std::os::fd::AsRawFd::as_raw_fd(&pidfd),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(PidError::Io(format!(
            "could not signal daemon PID {pid}: {}",
            std::io::Error::last_os_error()
        )))
    }
}

#[cfg(not(target_os = "linux"))]
fn verify_daemon_process(pid: i32) -> Result<(), PidError> {
    if pid <= 1 {
        Err(PidError::Parse("refusing an invalid daemon PID".into()))
    } else {
        Ok(())
    }
}
