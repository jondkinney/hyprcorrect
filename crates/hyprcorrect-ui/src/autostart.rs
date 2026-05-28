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
/// Public so the one-shot first-launch hook, the explicit
/// `install-desktop` command, and the autostart toggle can all place
/// the icon.
pub fn ensure_user_icon() -> io::Result<Option<PathBuf>> {
    let Some(base) = data_home() else {
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

/// Write the XDG application-catalog entry at
/// `$XDG_DATA_HOME/applications/hyprcorrect.desktop` so launchers
/// (Walker, fuzzel, rofi, KDE Krunner, GNOME Shell) find an entry
/// pointing at the *currently-running* binary + the bundled SVG. This
/// is what makes a `cargo install`ed binary appear in launchers.
///
/// Always writes when called directly (the explicit `install-desktop`
/// path). The daemon reaches it through [`ensure_first_launch`], which
/// skips when an AUR / distro package already ships the entry — so it
/// never shadows a system install.
///
/// `exec_path` is the path to the running binary (caller passes
/// `std::env::current_exe()`). On any I/O hiccup we fall through
/// silently rather than crashing the daemon.
///
/// # Errors
///
/// I/O errors only.
pub fn ensure_apps_catalog_entry(exec_path: &str) -> io::Result<Option<PathBuf>> {
    let Some(base) = data_home() else {
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

/// `$XDG_DATA_HOME`, falling back to `$HOME/.local/share`. `None`
/// when neither is set (extremely rare on a normal Linux session).
fn data_home() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
}

/// The user-local application-catalog entry path
/// (`$XDG_DATA_HOME/applications/hyprcorrect.desktop`).
fn user_apps_entry() -> Option<PathBuf> {
    Some(
        data_home()?
            .join("applications")
            .join("hyprcorrect.desktop"),
    )
}

/// One-shot marker for the first-launch desktop install:
/// `$XDG_STATE_HOME/hyprcorrect/desktop-install-done` (or
/// `$HOME/.local/state/...`). Its presence means the daemon's
/// one-time first-launch integration has already run.
fn first_launch_marker() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("hyprcorrect").join("desktop-install-done"))
}

/// Does a system XDG data dir already provide `hyprcorrect.desktop`?
/// An AUR / distro / `make install` package drops it under
/// `/usr/share/applications` (or `/usr/local/share/...`), in which
/// case a user-local copy would only shadow it.
fn packaged_entry_exists() -> bool {
    let dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    std::env::split_paths(&dirs).any(|d| d.join("applications/hyprcorrect.desktop").is_file())
}

/// Install the icon + applications-catalog entry on the first daemon
/// launch only, so a `cargo install`ed hyprcorrect appears in
/// launchers without the user knowing to run `install-desktop`.
///
/// One-shot: a marker in the XDG state dir records that this ran, so
/// it never repeats — not even if the user later removes the entry on
/// purpose (a *first-launch* action happens once). Skipped inside
/// Flatpak (the runtime ships the entry and sandboxed XDG dirs make a
/// user-local copy pointless) and when a system package already
/// provides the entry (the user-local copy would only shadow it).
/// Best-effort: any failure is swallowed and retried next launch;
/// `install-desktop` is the loud, explicit refresh path.
///
/// `exec_path` is the running binary (caller passes `current_exe()`).
pub fn ensure_first_launch(exec_path: &str) {
    let _ = try_ensure_first_launch(exec_path);
}

fn try_ensure_first_launch(exec_path: &str) -> io::Result<()> {
    if std::env::var_os("FLATPAK_ID").is_some() {
        return Ok(());
    }
    let Some(marker) = first_launch_marker() else {
        return Ok(());
    };
    if marker.exists() {
        return Ok(());
    }

    // Install only if nothing already provides the entry — neither an
    // earlier `install-desktop` run nor a system package.
    let already = user_apps_entry().is_some_and(|p| p.exists()) || packaged_entry_exists();
    if !already {
        ensure_user_icon()?;
        ensure_apps_catalog_entry(exec_path)?;
    }

    // Record completion last, so a failed install above is retried on
    // the next launch rather than marked done.
    if let Some(parent) = marker.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        &marker,
        "hyprcorrect ran its one-time first-launch desktop integration.\n",
    )
}

/// Record that the desktop integration is installed so the daemon's
/// one-shot first-launch step won't redo it. Called after the explicit
/// `install-desktop` command. Best-effort.
pub fn mark_install_done() {
    let Some(marker) = first_launch_marker() else {
        return;
    };
    if let Some(parent) = marker.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(
        &marker,
        "hyprcorrect desktop integration installed via install-desktop.\n",
    );
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
