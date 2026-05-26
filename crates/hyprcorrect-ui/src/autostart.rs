//! "Start at login" toggle backed by an XDG autostart `.desktop` file.
//!
//! Linux convention is that anything in `~/.config/autostart/` whose
//! `.desktop` file has `X-GNOME-Autostart-enabled=true` (or no such
//! key) gets launched by the user session's autostart loader. The
//! presence of the file IS the on/off state — there's no separate
//! config flag in `config.toml`, so the prefs toggle just writes
//! the file on enable and removes it on disable.
//!
//! The file's `Exec=` line points at the running prefs/daemon
//! binary (resolved via `current_exe()`). That keeps `cargo run`
//! local-dev sessions working while also being correct once the
//! AUR package ships `/usr/bin/hyprcorrect`.

use std::fs;
use std::io;
use std::path::PathBuf;

/// Where to put the autostart file. Honors `$XDG_CONFIG_HOME` then
/// falls back to `$HOME/.config`. Returns `None` if neither
/// variable is set — extremely rare on a normal Linux session.
pub fn path() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(dir.join("autostart").join("hyprcorrect.desktop"))
}

/// `true` when an autostart file exists. Treats a missing path or
/// an unreadable file as "off" rather than erroring — the prefs
/// toggle wants a boolean answer.
pub fn is_enabled() -> bool {
    path().is_some_and(|p| p.exists())
}

/// Write an autostart `.desktop` whose `Exec=` is the path passed
/// in. Creates `~/.config/autostart/` if missing. Overwrites any
/// existing file so the path always reflects the currently-running
/// binary.
///
/// # Errors
///
/// I/O errors only — the caller surfaces them in the prefs status
/// banner.
pub fn enable(exec_path: &str) -> io::Result<()> {
    let path = path().ok_or_else(|| {
        io::Error::other(
            "couldn't resolve $XDG_CONFIG_HOME / $HOME — autostart needs one of them set",
        )
    })?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let contents = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=hyprcorrect\n\
         GenericName=Spelling corrector\n\
         Comment=Keyboard-driven desktop spelling and typo correction\n\
         Exec={exec_path}\n\
         Icon=tools-check-spelling\n\
         Terminal=false\n\
         Categories=Utility;TextTools;\n\
         StartupNotify=false\n\
         X-GNOME-Autostart-enabled=true\n"
    );
    fs::write(path, contents)
}

/// Remove the autostart file. Idempotent — a missing file is not
/// an error.
///
/// # Errors
///
/// I/O errors other than "not found".
pub fn disable() -> io::Result<()> {
    let Some(path) = path() else {
        return Ok(());
    };
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
