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
    // Drop the bundled SVG into the hicolor scalable-apps theme path
    // and reference it by *icon name* (`Icon=hyprcorrect`). Absolute-
    // path `Icon=` values are spec-legal but inconsistently honored
    // by launchers (Walker shows a blank slot for them in 2.16); the
    // theme-name lookup with a properly-placed SVG works everywhere.
    let _ = ensure_user_icon()?;
    let contents = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=hyprcorrect\n\
         GenericName=Spelling corrector\n\
         Comment=Keyboard-driven desktop spelling and typo correction\n\
         Exec={exec_path}\n\
         Icon=hyprcorrect\n\
         Terminal=false\n\
         Categories=Utility;TextTools;\n\
         StartupNotify=false\n\
         X-GNOME-Autostart-enabled=true\n"
    );
    fs::write(path, contents)
}

/// Best-effort write of the bundled SVG to the hicolor scalable-apps
/// theme path:
/// `$XDG_DATA_HOME/icons/hicolor/scalable/apps/hyprcorrect.svg` (or
/// `$HOME/.local/share/icons/hicolor/scalable/apps/hyprcorrect.svg`).
/// That's the freedesktop-spec-correct location an `Icon=hyprcorrect`
/// theme-name lookup resolves against — works in Walker / fuzzel /
/// rofi / KDE Krunner / GNOME Shell without each launcher needing
/// to honor absolute paths in `Icon=` (Walker 2.16 doesn't).
///
/// Returns the path on success, `Ok(None)` if neither XDG_DATA_HOME
/// nor HOME is set (rare) or a write fails.
///
/// Public so the daemon can call this on every startup to keep
/// Walker / other XDG launchers in sync with whatever SVG the
/// currently-installed binary has compiled in.
pub fn ensure_user_icon() -> io::Result<Option<PathBuf>> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    let Some(base) = base else {
        return Ok(None);
    };
    let dir = base.join("icons/hicolor/scalable/apps");
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!(
            "hyprcorrect: could not create {} ({e}) — launcher will fall back to icon-theme",
            dir.display()
        );
        return Ok(None);
    }
    let path = dir.join("hyprcorrect.svg");
    if let Err(e) = fs::write(&path, crate::icon::app_icon_svg_bytes()) {
        eprintln!(
            "hyprcorrect: could not write {} ({e}) — launcher will fall back to icon-theme",
            path.display()
        );
        return Ok(None);
    }
    Ok(Some(path))
}

/// Write/refresh the XDG application-catalog entry at
/// `$XDG_DATA_HOME/applications/hyprcorrect.desktop` so launchers
/// (Walker, fuzzel, rofi, KDE Krunner, GNOME Shell) find an entry
/// pointing at the *currently-running* binary + the freshly-refreshed
/// SVG. Without this, dev-build users see whichever stale entry was
/// laid down by an earlier install / `cargo install` run; AUR users
/// see the system-wide entry from `/usr/share/applications/` and the
/// user-level file just shadows it transparently.
///
/// `exec_path` is the path to the running binary (caller passes
/// `std::env::current_exe()`). On any I/O hiccup we fall through
/// silently rather than crashing the daemon.
///
/// # Errors
///
/// I/O errors only.
pub fn ensure_apps_catalog_entry(exec_path: &str) -> io::Result<Option<PathBuf>> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    let Some(base) = base else {
        return Ok(None);
    };
    let dir = base.join("applications");
    fs::create_dir_all(&dir)?;
    // Side-effect: ensure_user_icon places our SVG at the hicolor
    // scalable-apps path that `Icon=hyprcorrect` resolves against.
    let _ = ensure_user_icon()?;
    let path = dir.join("hyprcorrect.desktop");
    let contents = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=hyprcorrect\n\
         GenericName=Spelling corrector\n\
         Comment=Keyboard-driven desktop spelling and typo correction\n\
         Exec={exec_path} prefs\n\
         Icon=hyprcorrect\n\
         Terminal=false\n\
         Categories=Utility;TextTools;\n\
         Keywords=spell;spellcheck;autocorrect;typo;correction;keyboard;\n\
         StartupNotify=false\n"
    );
    fs::write(&path, contents)?;
    Ok(Some(path))
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
