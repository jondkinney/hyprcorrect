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
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("hyprcorrect.pid")
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

/// Remove the daemon PID file (idempotent — missing file is OK).
pub fn clear_pid() {
    let _ = fs::remove_file(pid_path());
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
