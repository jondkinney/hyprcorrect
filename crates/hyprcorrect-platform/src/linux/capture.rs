//! Linux keystroke capture.
//!
//! Reads key events from every keyboard under `/dev/input` via `evdev`
//! and translates them — honoring the keyboard layout and modifiers via
//! `xkbcommon` — into [`Key`] values for the keystroke buffer.
//!
//! The trigger chord itself (Super+Ctrl+Shift+Alt+letter) is *not*
//! delivered here — Hyprland intercepts it (via the inline keybind set
//! up by `hotkey`) and signals the daemon over `SIGUSR1`. Capture only
//! suppresses the would-be [`Key::Reset`] that the chord's letter
//! press would otherwise emit, so the buffer survives the chord and
//! the trigger has a word to fix.
//!
//! One OS thread per keyboard device runs for the life of the process;
//! [`start`] returns the channel they feed.
//!
//! Layout note: the keymap is the system / `XKB_DEFAULT_*` default. A
//! layout configured only in the compositor (e.g. `hyprland.conf`) is
//! not yet read — that is M5 polish.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use evdev::{Device, EventSummary, KeyCode};
use hyprcorrect_core::{Chord, Key};
use xkbcommon::xkb;

use crate::linux::chord_capture::ChordCaptureSlot;

/// An error starting keystroke capture.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// No keyboard devices were found under `/dev/input`.
    #[error("no keyboard devices found under /dev/input")]
    NoKeyboards,
    /// `/dev/input` devices exist but could not be opened.
    #[error(
        "permission denied reading /dev/input — add your user to the 'input' group (`sudo usermod -aG input $USER`) and log back in"
    )]
    Permission,
    /// The keyboard layout could not be compiled by xkbcommon.
    #[error("could not compile the keyboard layout (xkbcommon)")]
    Keymap,
}

/// The trigger chord, expanded into the data capture needs: which
/// modifier flags must match, and the xkb keysyms (upper- and
/// lower-case) of the non-modifier key. Capture uses this only to
/// *suppress* the would-be Reset the chord's key press would
/// otherwise emit; the Hyprland keybind in `hotkey` is what actually
/// fires the trigger.
#[derive(Debug, Clone, Copy)]
struct TriggerSpec {
    sym: u32,
    alt_sym: u32,
    needs_ctrl: bool,
    needs_alt: bool,
    needs_shift: bool,
    needs_super: bool,
}

/// Start capturing keystrokes from every keyboard under `/dev/input`.
///
/// `chords` is the list of trigger chords the daemon has bound.
/// Capture uses them to suppress the would-be [`Key::Reset`] that
/// pressing one of those chords would otherwise emit — without this,
/// pressing e.g. `Super+Ctrl+Shift+Alt+S` would wipe the buffer
/// before the sentence-fix gets a chance to read it.
///
/// Returns a channel of [`Key`] events. One detached OS thread per
/// keyboard device feeds the channel for the life of the process;
/// dropping the [`Receiver`] makes those threads exit.
///
/// # Errors
///
/// See [`CaptureError`].
pub fn start(
    chords: &[Chord],
    chord_capture: Arc<ChordCaptureSlot>,
) -> Result<Receiver<Key>, CaptureError> {
    // Compile the keymap once, up front, so a broken layout fails fast
    // with a clear error rather than a silent no-events daemon.
    let keymap_text = {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or(CaptureError::Keymap)?;
        keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
    };

    let triggers: Vec<TriggerSpec> = chords.iter().map(resolve_trigger).collect();
    let keyboards = keyboard_devices()?;
    let dedupe = Arc::new(Mutex::new(Dedupe::new()));
    let hold = Arc::new(HoldTracker::new(query_compositor_repeat()));
    let mods = MODS_WATCH
        .get_or_init(|| Arc::new(ModsWatch::new()))
        .clone();
    let suspect = caret_suspect_flag();
    for device in mouse_devices() {
        let suspect = suspect.clone();
        thread::spawn(move || read_mouse(device, suspect));
    }
    let (tx, rx) = mpsc::channel();
    for (idx, device) in keyboards.into_iter().enumerate() {
        let tx = tx.clone();
        let keymap_text = keymap_text.clone();
        let triggers = triggers.clone();
        let chord_capture = chord_capture.clone();
        let dedupe = dedupe.clone();
        let hold = hold.clone();
        let mods = mods.clone();
        let device_id = idx as u32;
        thread::spawn(move || {
            read_device(
                device,
                device_id,
                &keymap_text,
                &triggers,
                &chord_capture,
                &dedupe,
                &hold,
                &mods,
                &tx,
            )
        });
    }
    Ok(rx)
}

/// Block up to `timeout` for every chord modifier (Ctrl/Shift/Alt/
/// Super) to be released across every keyboard device the daemon is
/// watching. Returns `true` if everything cleared in time, `false` on
/// timeout.
///
/// Called by [`crate::linux::emit`] before each wtype burst: many
/// Wayland compositors deliver wtype's synthetic keys ORed with the
/// user's physical modifier state, which would turn each `BackSpace`
/// into `Ctrl+BackSpace` (delete-word in many terminals) while the
/// chord is still being held. Waiting for release dodges that.
///
/// No-op (returns `true` immediately) before [`start`] has been
/// called — useful for unit tests of the emit path.
pub fn wait_mods_clear(timeout: Duration) -> bool {
    let Some(watch) = MODS_WATCH.get() else {
        return true;
    };
    watch.wait_clear(timeout)
}

/// Shared flag set by the mouse-listener threads whenever the user
/// clicks a mouse button, and cleared by the daemon after the next
/// fix-word emit or buffer reset. When `true`, the daemon's
/// word-fix path widens its nearby-word scan to the entire buffer
/// — the buffer's caret tracking can't follow a mouse click, but
/// the buffer's *text* is still accurate, so scanning all of it
/// for a typo is the best we can do without OS-level cursor
/// snooping. Returned as an `Arc` so the daemon can both read and
/// reset it.
pub fn caret_suspect_flag() -> Arc<AtomicBool> {
    CARET_SUSPECT
        .get_or_init(|| Arc::new(AtomicBool::new(false)))
        .clone()
}

/// User-configurable view of which "control" keys reset the
/// per-window buffer. The daemon passes the current settings via
/// [`set_reset_keys`] at startup and on every config reload;
/// `classify` reads it under a `RwLock::read` (essentially free)
/// to decide whether a given key is a [`Key::Reset`] or just
/// ignored. Defaults match the safest behavior — every
/// context-changing key resets except for Tab and Escape, which
/// rarely change typed text and would otherwise drop the buffer
/// for no gain.
#[derive(Debug, Clone, Copy)]
pub struct ResetKeyConfig {
    pub enter: bool,
    pub tab: bool,
    pub escape: bool,
    pub up: bool,
    pub down: bool,
    pub page_up: bool,
    pub page_down: bool,
    pub delete: bool,
    pub insert: bool,
}

impl Default for ResetKeyConfig {
    fn default() -> Self {
        Self {
            enter: true,
            tab: false,
            escape: false,
            up: true,
            down: true,
            page_up: true,
            page_down: true,
            delete: true,
            insert: true,
        }
    }
}

/// Replace the daemon-wide reset-key config. Cheap (one `RwLock`
/// write); call at startup and on every config reload.
pub fn set_reset_keys(cfg: ResetKeyConfig) {
    *reset_keys_lock().write().expect("reset-keys poisoned") = cfg;
}

fn reset_keys_lock() -> &'static std::sync::RwLock<ResetKeyConfig> {
    RESET_KEY_CONFIG.get_or_init(|| std::sync::RwLock::new(ResetKeyConfig::default()))
}

fn reset_keys() -> ResetKeyConfig {
    *reset_keys_lock().read().expect("reset-keys poisoned")
}

static MODS_WATCH: OnceLock<Arc<ModsWatch>> = OnceLock::new();
static CARET_SUSPECT: OnceLock<Arc<AtomicBool>> = OnceLock::new();
static RESET_KEY_CONFIG: OnceLock<std::sync::RwLock<ResetKeyConfig>> = OnceLock::new();

/// Shared tracker of which chord modifiers are currently held, keyed
/// by input-device index. The capture thread per device writes its
/// xkb modifier mask after every key event; [`wait_mods_clear`]
/// reads the union.
struct ModsWatch {
    inner: Mutex<HashMap<u32, u8>>,
    cv: Condvar,
}

const MOD_CTRL: u8 = 1 << 0;
const MOD_ALT: u8 = 1 << 1;
const MOD_SHIFT: u8 = 1 << 2;
const MOD_SUPER: u8 = 1 << 3;

impl ModsWatch {
    fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            cv: Condvar::new(),
        }
    }

    /// Update this device's modifier mask. Notifies the condvar so
    /// any `wait_clear` caller re-checks.
    fn update(&self, device_id: u32, mask: u8) {
        let mut guard = self.inner.lock().expect("mods poisoned");
        let entry = guard.entry(device_id).or_insert(0);
        if *entry != mask {
            *entry = mask;
            self.cv.notify_all();
        }
    }

    /// Returns `true` once every recorded device reports a zero
    /// mask, or `false` if `timeout` elapses first.
    fn wait_clear(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut guard = self.inner.lock().expect("mods poisoned");
        loop {
            if guard.values().all(|&m| m == 0) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (g, res) = self
                .cv
                .wait_timeout(guard, deadline - now)
                .expect("mods poisoned");
            guard = g;
            if res.timed_out() && !guard.values().all(|&m| m == 0) {
                return false;
            }
        }
    }
}

/// Read the chord-modifier mask from `state`.
fn mods_mask(state: &xkb::State) -> u8 {
    let active = |m: &str| state.mod_name_is_active(m, xkb::STATE_MODS_EFFECTIVE);
    let mut mask = 0;
    if active(xkb::MOD_NAME_CTRL) {
        mask |= MOD_CTRL;
    }
    if active(xkb::MOD_NAME_ALT) {
        mask |= MOD_ALT;
    }
    if active(xkb::MOD_NAME_SHIFT) {
        mask |= MOD_SHIFT;
    }
    if active(xkb::MOD_NAME_LOGO) {
        mask |= MOD_SUPER;
    }
    mask
}

/// Auto-repeat tuning the compositor uses to drive Wayland clients.
/// We mimic these inside the daemon so the buffer's caret tracks
/// what the TUI is doing, not the kernel's slower evdev repeats.
#[derive(Debug, Clone, Copy)]
struct RepeatConfig {
    /// Initial delay before auto-repeat kicks in. Hyprland default
    /// is ~600 ms; users often crank it down (the test rig has 225).
    delay: Duration,
    /// Repeat interval between synthetic emits.
    interval: Duration,
}

impl Default for RepeatConfig {
    fn default() -> Self {
        // Common Wayland-compositor defaults — close enough for
        // setups where `hyprctl getoption` isn't available or fails.
        Self {
            delay: Duration::from_millis(600),
            interval: Duration::from_millis(40), // 25 Hz
        }
    }
}

/// Best-effort: pull `input:repeat_delay` and `input:repeat_rate`
/// out of Hyprland via `hyprctl getoption -j`. Falls back to the
/// common Wayland defaults when the call fails or we're not on
/// Hyprland. Called once at daemon startup; the values are
/// captured for the lifetime of the process.
fn query_compositor_repeat() -> RepeatConfig {
    let mut cfg = RepeatConfig::default();
    let read = |key: &str| -> Option<i64> {
        let out = std::process::Command::new("hyprctl")
            .args(["getoption", "-j", key])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        // The "int" field is what we want; cheap textual extract.
        let text = String::from_utf8_lossy(&out.stdout);
        text.split("\"int\"")
            .nth(1)?
            .split(':')
            .nth(1)?
            .split(',')
            .next()?
            .trim()
            .parse::<i64>()
            .ok()
    };
    if let Some(d) = read("input:repeat_delay")
        && d > 0
    {
        cfg.delay = Duration::from_millis(d as u64);
    }
    if let Some(r) = read("input:repeat_rate")
        && r > 0
    {
        cfg.interval = Duration::from_millis(1000 / r as u64);
    }
    cfg
}

/// Synthesizes auto-repeats inside the daemon to match the
/// compositor's repeat rate, since evdev's `value=2` events arrive
/// at the kernel's slower rate (which the compositor often ignores
/// in favor of its own timer-driven repeats). For each held key
/// we spawn a thread that waits the initial delay, then emits
/// `Key` at the configured interval until the release cancels it.
///
/// Cancellation is plumbed through an mpsc channel so the thread
/// can `recv_timeout` on it — the wait returns the *instant* a
/// cancel arrives, and the loop layout guarantees we never emit
/// an extra event after cancellation. (The old `AtomicBool` poll
/// had a "check-then-send" race that leaked one synthetic emit
/// per hold, throwing post-release manual presses off by one.)
struct HoldTracker {
    repeat: RepeatConfig,
    active: Mutex<HashMap<u16, Sender<()>>>,
}

impl HoldTracker {
    fn new(repeat: RepeatConfig) -> Self {
        Self {
            repeat,
            active: Mutex::new(HashMap::new()),
        }
    }

    fn start(&self, code: u16, emit: Key, tx: Sender<Key>) {
        self.stop(code); // cancel any prior hold on the same key
        let (cancel_tx, cancel_rx) = mpsc::channel::<()>();
        self.active
            .lock()
            .expect("hold poisoned")
            .insert(code, cancel_tx);
        let repeat = self.repeat;
        thread::spawn(move || {
            use mpsc::RecvTimeoutError;
            // Initial delay. Returning `Ok(())` means we got the
            // cancel signal during the wait — return immediately,
            // no emit. `Disconnected` means the sender was dropped
            // (the HoldTracker forgot us) — same idea, return.
            match cancel_rx.recv_timeout(repeat.delay) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {}
            }
            // First synthetic fires at t = delay — matching the
            // compositor's first auto-repeat. Then each subsequent
            // wait+emit pair keeps step with compositor at
            // `interval`.
            if tx.send(emit).is_err() {
                return;
            }
            loop {
                // Wait first, emit second: if cancellation arrives
                // during the wait the recv returns instantly and
                // we exit without firing the next event. There's
                // still a tiny race for the FIRST synthetic above
                // (cancel between delay-end and the send), but
                // that window is sub-millisecond.
                match cancel_rx.recv_timeout(repeat.interval) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                    Err(RecvTimeoutError::Timeout) => {}
                }
                if tx.send(emit).is_err() {
                    return;
                }
            }
        });
    }

    fn stop(&self, code: u16) {
        if let Some(cancel_tx) = self.active.lock().expect("hold poisoned").remove(&code) {
            // Best effort — if the thread has already exited the
            // receiver is gone and this send just no-ops.
            let _ = cancel_tx.send(());
        }
    }
}

/// Cross-device duplicate suppressor. Users with `keyd` (or other
/// input remappers) often have several evdev devices emitting the
/// same key: the physical keyboard, the keyd-virtual-keyboard, and
/// sometimes a third passthrough device. Each carries the same
/// keycode in lockstep, so the daemon sees 2–3× the events the
/// compositor delivers to the focused app — and the daemon's
/// caret runs ahead of the TUI's by the same multiple.
///
/// `Dedupe` collapses runs of the same `(keycode, value)` event
/// arriving within a short window across any device. The first
/// event in the window is forwarded; the rest are dropped.
struct Dedupe {
    last: Option<(u16, i32, Instant)>,
}

impl Dedupe {
    fn new() -> Self {
        Self { last: None }
    }

    /// Returns `true` if this event should be processed (not a
    /// near-duplicate of the last one we saw). The window is short
    /// enough that legitimate manual repeats (typing the same key
    /// twice quickly) still come through.
    fn allow(&mut self, code: u16, value: i32) -> bool {
        const WINDOW: Duration = Duration::from_millis(8);
        let now = Instant::now();
        let is_dup = matches!(
            self.last,
            Some((last_code, last_value, last_time))
                if last_code == code && last_value == value && now - last_time < WINDOW
        );
        let allow = !is_dup;
        if allow {
            self.last = Some((code, value, now));
        }
        allow
    }
}

/// Resolve the trigger spec for the given chord. Bare modifiers (no
/// non-modifier key) are degenerate; in that case both `sym` fields
/// are 0 and `letter_match` below never fires.
fn resolve_trigger(chord: &Chord) -> TriggerSpec {
    let sym = xkb::keysym_from_name(&chord.key, xkb::KEYSYM_CASE_INSENSITIVE).raw();
    let alt_sym = match sym {
        0x61..=0x7A => sym - 0x20,
        0x41..=0x5A => sym + 0x20,
        _ => 0,
    };
    TriggerSpec {
        sym,
        alt_sym,
        needs_ctrl: chord.ctrl,
        needs_alt: chord.alt,
        needs_shift: chord.shift,
        needs_super: chord.super_,
    }
}

/// Enumerate `/dev/input` and return the devices that look like
/// keyboards (those that can emit letter keys).
fn keyboard_devices() -> Result<Vec<Device>, CaptureError> {
    let entries = match std::fs::read_dir("/dev/input") {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            return Err(CaptureError::Permission);
        }
        Err(_) => return Err(CaptureError::NoKeyboards),
    };

    let mut keyboards = Vec::new();
    let mut permission_denied = false;
    for entry in entries.flatten() {
        let path = entry.path();
        let is_event_node = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("event"));
        if !is_event_node {
            continue;
        }
        match Device::open(&path) {
            Ok(device) if is_keyboard(&device) => keyboards.push(device),
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::PermissionDenied => {
                permission_denied = true;
            }
            Err(_) => {}
        }
    }

    if !keyboards.is_empty() {
        Ok(keyboards)
    } else if permission_denied {
        Err(CaptureError::Permission)
    } else {
        Err(CaptureError::NoKeyboards)
    }
}

/// A device is treated as a keyboard if it can emit letter keys.
fn is_keyboard(device: &Device) -> bool {
    device
        .supported_keys()
        .is_some_and(|keys| keys.contains(KeyCode::KEY_A))
}

/// Enumerate `/dev/input` and return the devices that look like
/// mice — those that can emit `BTN_LEFT`. Best-effort: any error
/// (permission denied, missing /dev/input, …) yields an empty
/// list rather than failing the whole daemon, since mouse
/// listening is a UX-improvement add-on, not a hard requirement.
fn mouse_devices() -> Vec<Device> {
    let Ok(entries) = std::fs::read_dir("/dev/input") else {
        return Vec::new();
    };
    let mut mice = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_event_node = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("event"));
        if !is_event_node {
            continue;
        }
        if let Ok(device) = Device::open(&path)
            && is_mouse(&device)
        {
            mice.push(device);
        }
    }
    mice
}

/// A device is treated as a mouse if it advertises `BTN_LEFT`.
fn is_mouse(device: &Device) -> bool {
    device
        .supported_keys()
        .is_some_and(|keys| keys.contains(KeyCode::BTN_LEFT))
}

/// Read one mouse forever; set [`caret_suspect_flag`] on every
/// `BTN_LEFT` press. The daemon reads the flag in its word-fix
/// path to widen the nearby-word scan when the buffer caret may
/// have drifted from the visible cursor (the buffer doesn't see
/// mouse clicks, so without this signal it has no idea the
/// cursor moved).
fn read_mouse(mut device: Device, suspect: Arc<AtomicBool>) {
    loop {
        let Ok(events) = device.fetch_events() else {
            return;
        };
        for input in events {
            if let EventSummary::Key(_, code, value) = input.destructure()
                && code == KeyCode::BTN_LEFT
                && value == 1
            {
                suspect.store(true, Ordering::Relaxed);
            }
        }
    }
}

/// Read one device forever, translating key events into [`Key`]s and
/// sending them to `tx`. Returns — ending the thread — when the device
/// disappears or the receiver is dropped.
#[allow(clippy::too_many_arguments)]
fn read_device(
    mut device: Device,
    device_id: u32,
    keymap_text: &str,
    triggers: &[TriggerSpec],
    chord_capture: &ChordCaptureSlot,
    dedupe: &Mutex<Dedupe>,
    hold: &HoldTracker,
    mods: &ModsWatch,
    tx: &Sender<Key>,
) {
    let _device_name = device
        .name()
        .map(|s| s.to_string())
        .unwrap_or_else(|| "<unnamed>".to_string());
    // Each thread builds its own xkb state: Context/Keymap/State hold
    // raw pointers and are not Send, so they cannot cross the thread
    // boundary. The keymap text was already validated by `start`.
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let Some(keymap) = xkb::Keymap::new_from_string(
        &context,
        keymap_text.to_owned(),
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    ) else {
        return;
    };
    let mut state = xkb::State::new(&keymap);

    loop {
        let Ok(events) = device.fetch_events() else {
            return;
        };
        for input in events {
            let EventSummary::Key(_, code, value) = input.destructure() else {
                continue;
            };
            let keycode = xkb::Keycode::new(u32::from(code.0) + 8);

            // Drop near-duplicate events from sibling devices (keyd
            // virtual keyboard + physical, etc.) — see `Dedupe`.
            // Applied to BOTH press and release so xkb state doesn't
            // double-toggle modifiers either.
            if !dedupe.lock().expect("dedupe poisoned").allow(code.0, value) {
                continue;
            }

            // Drop kernel auto-repeats. We synthesize our own at the
            // compositor's rate (see `HoldTracker`) so the buffer's
            // caret tracks what the focused app actually sees,
            // not what evdev's slower kernel-driven repeat fires.
            if value == 2 {
                continue;
            }

            // value: 0 = release, 1 = press. Read the key from the
            // *current* state, before this key updates it
            // (the xkbcommon convention).
            if value == 1 {
                // Chord-record mode pre-empts normal Key handling so
                // pressing the chord doesn't reset the buffer or fire
                // any trigger while prefs is recording.
                if chord_capture.is_armed()
                    && let Some(chord) = chord_from_state(&state, keycode)
                    && chord_capture.try_emit(chord)
                {
                    // Modifier state still needs to update below.
                } else if let Some(key) = translate(&state, keycode, triggers) {
                    if tx.send(key).is_err() {
                        return; // receiver dropped
                    }
                    // Start synthetic auto-repeats for this key so
                    // holding it advances the buffer caret in sync
                    // with the compositor's repeats.
                    hold.start(code.0, key, tx.clone());
                }
            } else {
                // value == 0 (release). Stop any hold thread for
                // this key so the buffer stops auto-advancing.
                hold.stop(code.0);
            }

            // Track modifier state changes on press and release.
            let direction = if value == 0 {
                xkb::KeyDirection::Up
            } else {
                xkb::KeyDirection::Down
            };
            state.update_key(keycode, direction);

            // Publish this device's current chord-mod mask so the
            // emit path can wait for everything to clear before
            // typing — otherwise wtype's BackSpaces inherit the
            // held Ctrl/etc. and turn into delete-word.
            mods.update(device_id, mods_mask(&state));
        }
    }
}

/// Translate a pressed key into a [`Key`] for the buffer, or `None` to
/// ignore it.
fn translate(state: &xkb::State, keycode: xkb::Keycode, triggers: &[TriggerSpec]) -> Option<Key> {
    let sym = state.key_get_one_sym(keycode).raw();

    // Modifier keys themselves are never buffered and never reset —
    // they only affect xkb state, which `read_device` updates after
    // this call.
    if is_modifier_keysym(sym) {
        return None;
    }

    // Any of the daemon's bound chords match this key+modifier combo?
    // Suppress so pressing the chord doesn't ALSO reset the buffer
    // via the has_action_modifier branch below. Hyprland fires the
    // trigger separately (via SIGUSR1).
    let chord_match = triggers.iter().any(|trigger| {
        let letter_match = trigger.sym != 0
            && (sym == trigger.sym || (trigger.alt_sym != 0 && sym == trigger.alt_sym));
        letter_match && is_trigger_chord(state, *trigger)
    });
    if chord_match {
        return None;
    }

    // Ctrl+Left / Ctrl+Right (with no Alt/Super) is the universal
    // "jump by word" shortcut. Track those as word-boundary caret
    // moves rather than treating them as a reset — otherwise the
    // buffer goes blind every time the user word-jumps to fix a
    // typo. Shift may also be held (selection extension), but that
    // doesn't change where the caret ends up.
    {
        use xkb::keysyms::{KEY_Left, KEY_Right};
        let active = |m: &str| state.mod_name_is_active(m, xkb::STATE_MODS_EFFECTIVE);
        let ctrl_only =
            active(xkb::MOD_NAME_CTRL) && !active(xkb::MOD_NAME_ALT) && !active(xkb::MOD_NAME_LOGO);
        if ctrl_only {
            if sym == KEY_Left {
                return Some(Key::WordLeft);
            }
            if sym == KEY_Right {
                return Some(Key::WordRight);
            }
        }
    }

    // A non-modifier key pressed while Ctrl/Alt/Super is held is a
    // shortcut, not typed text — and it may have moved the caret or
    // edited. Reset.
    if has_action_modifier(state) {
        return Some(Key::Reset);
    }

    classify(sym, &state.key_get_utf8(keycode))
}

/// Build the chord string for a key pressed in chord-record mode:
/// the currently-held SUPER/CTRL/SHIFT/ALT modifiers, plus the
/// canonical name of the non-modifier key. Returns `None` for
/// modifier-only presses so prefs can keep recording until the user
/// hits a real key.
///
/// Format matches [`hyprcorrect_core::Chord::parse`] exactly and uses
/// the canonical modifier order, e.g.
/// `"CTRL+SHIFT+ALT+SUPER+F"` or `"CTRL+SPACE"` or bare `"F1"`.
fn chord_from_state(state: &xkb::State, keycode: xkb::Keycode) -> Option<String> {
    let sym = state.key_get_one_sym(keycode).raw();
    if is_modifier_keysym(sym) {
        return None;
    }
    let key_token = chord_key_token(sym)?;

    let active = |m: &str| state.mod_name_is_active(m, xkb::STATE_MODS_EFFECTIVE);
    // Canonical order — CTRL, SHIFT, ALT, SUPER — matching
    // `hyprcorrect_core::Chord::Display` and `hyprland_modifiers` so a
    // freshly recorded chord round-trips to the same string the rest of
    // the app renders.
    let mut parts: Vec<&str> = Vec::new();
    if active(xkb::MOD_NAME_CTRL) {
        parts.push("CTRL");
    }
    if active(xkb::MOD_NAME_SHIFT) {
        parts.push("SHIFT");
    }
    if active(xkb::MOD_NAME_ALT) {
        parts.push("ALT");
    }
    if active(xkb::MOD_NAME_LOGO) {
        parts.push("SUPER");
    }
    Some(if parts.is_empty() {
        key_token
    } else {
        format!("{}+{key_token}", parts.join("+"))
    })
}

/// The token used in chord strings for a non-modifier keysym.
/// Letters become uppercase ASCII; common named keys (Space, F-row,
/// arrows, etc.) use the same UPPERCASE tokens
/// [`hyprcorrect_core::Chord::parse`] accepts; anything else falls
/// back to xkb's canonical keysym name uppercased.
fn chord_key_token(sym: u32) -> Option<String> {
    // Canonical chord tokens. Match the form used by vernier and
    // the rest of the hyprcorrect UI so the recorded chord round-
    // trips cleanly and the chip renderer doesn't need a second
    // translation layer. Hyprland accepts the long xkb keysym
    // names (Escape, Return, BackSpace, ...) case-insensitively,
    // so the only tokens that need translation back to xkb names
    // at hyprctl-bind time are ESC and ENTER — handled in
    // `Chord::hyprland_key`.
    let named = match sym {
        0xff1b => Some("ESC"),            // Escape
        0xff0d | 0xff8d => Some("ENTER"), // Return / KP_Enter
        0xff09 => Some("TAB"),            // Tab
        0xff08 => Some("BACKSPACE"),      // BackSpace
        0xffff => Some("DELETE"),         // Delete
        0xff52 => Some("UP"),             // Up
        0xff54 => Some("DOWN"),           // Down
        0xff51 => Some("LEFT"),           // Left
        0xff53 => Some("RIGHT"),          // Right
        0x20 => Some("SPACE"),            // space
        0x2b => Some("PLUS"),             // +  (avoid colliding with the modifier separator)
        0x2d => Some("MINUS"),            // -
        0x3d => Some("EQUAL"),            // =
        _ => None,
    };
    if let Some(token) = named {
        return Some(token.to_string());
    }
    if (0x21..=0x7E).contains(&sym) {
        // Printable ASCII keysyms (letters, digits, punctuation) are
        // identical to their codepoint; lowercase letters get folded
        // up so `Chord::parse` round-trips.
        let ch = char::from_u32(sym)?.to_ascii_uppercase();
        return Some(ch.to_string());
    }
    let name = xkb::keysym_get_name(xkb::Keysym::from(sym));
    if name.is_empty() {
        return None;
    }
    Some(name.to_ascii_uppercase())
}

/// `true` when the currently-held modifier set matches the trigger
/// chord's modifier set exactly.
fn is_trigger_chord(state: &xkb::State, trigger: TriggerSpec) -> bool {
    let active = |m: &str| state.mod_name_is_active(m, xkb::STATE_MODS_EFFECTIVE);
    active(xkb::MOD_NAME_CTRL) == trigger.needs_ctrl
        && active(xkb::MOD_NAME_ALT) == trigger.needs_alt
        && active(xkb::MOD_NAME_SHIFT) == trigger.needs_shift
        && active(xkb::MOD_NAME_LOGO) == trigger.needs_super
}

/// `true` if Ctrl, Alt, or Super is currently held.
fn has_action_modifier(state: &xkb::State) -> bool {
    let active = |m: &str| state.mod_name_is_active(m, xkb::STATE_MODS_EFFECTIVE);
    active(xkb::MOD_NAME_CTRL) || active(xkb::MOD_NAME_ALT) || active(xkb::MOD_NAME_LOGO)
}

/// `true` if `sym` is one of the modifier keysyms — Shift, Control,
/// Caps Lock, Meta, Alt, Super, or Hyper (left or right). xkb assigns
/// these the contiguous range `0xffe1..=0xffee`.
fn is_modifier_keysym(sym: u32) -> bool {
    (0xffe1..=0xffee).contains(&sym)
}

/// Resolve "does this keysym reset the buffer right now" against
/// the user's prefs-driven [`ResetKeyConfig`]. Cheap (one RwLock
/// read) so `classify` can call it on every keystroke.
fn reset_for_keysym(sym: u32) -> bool {
    use xkb::keysyms::{
        KEY_Delete, KEY_Down, KEY_Escape, KEY_ISO_Left_Tab, KEY_Insert, KEY_KP_Enter, KEY_Linefeed,
        KEY_Next, KEY_Prior, KEY_Return, KEY_Tab, KEY_Up,
    };
    let cfg = reset_keys();
    matches!(
        sym,
        s if (s == KEY_Return || s == KEY_KP_Enter || s == KEY_Linefeed) && cfg.enter
    ) || matches!(sym, s if (s == KEY_Tab || s == KEY_ISO_Left_Tab) && cfg.tab)
        || matches!(sym, s if s == KEY_Escape && cfg.escape)
        || matches!(sym, s if s == KEY_Up && cfg.up)
        || matches!(sym, s if s == KEY_Down && cfg.down)
        || matches!(sym, s if s == KEY_Prior && cfg.page_up)
        || matches!(sym, s if s == KEY_Next && cfg.page_down)
        || matches!(sym, s if s == KEY_Delete && cfg.delete)
        || matches!(sym, s if s == KEY_Insert && cfg.insert)
}

/// Classify an xkb keysym and the UTF-8 it produces into a buffer
/// [`Key`]: Backspace and caret-moving keys are matched by keysym; a
/// single printable character becomes a `Char`; everything else (bare
/// modifiers, function keys) is ignored.
fn classify(sym: u32, utf8: &str) -> Option<Key> {
    use xkb::keysyms::{KEY_BackSpace, KEY_End, KEY_Home, KEY_Left, KEY_Right};
    // Left/Right arrow press translates to a buffer caret move,
    // and Home/End jump to the line edges (single-line context: a
    // safe approximation for the buffer, since we reset on
    // Return/Enter anyway). Ctrl+arrow word-jumps are detected
    // upstream in `translate`. The remaining context-changing
    // keys (Enter/Tab/Esc/Up/Down/PageUp/PageDown/Delete/Insert)
    // reset the buffer when the user has them toggled on in
    // prefs — see `ResetKeyConfig`.

    if sym == KEY_BackSpace {
        Some(Key::Backspace)
    } else if sym == KEY_Left {
        Some(Key::MoveLeft)
    } else if sym == KEY_Right {
        Some(Key::MoveRight)
    } else if sym == KEY_Home {
        Some(Key::LineStart)
    } else if sym == KEY_End {
        Some(Key::LineEnd)
    } else if reset_for_keysym(sym) {
        Some(Key::Reset)
    } else {
        let mut chars = utf8.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) if !c.is_control() => Some(Key::Char(c)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xkb::keysyms::{
        KEY_BackSpace, KEY_End, KEY_Escape, KEY_Home, KEY_Left, KEY_Return, KEY_Right, KEY_Tab,
        KEY_Up,
    };

    #[test]
    fn backspace_keysym_maps_to_backspace() {
        assert_eq!(classify(KEY_BackSpace, ""), Some(Key::Backspace));
    }

    #[test]
    fn left_right_arrows_move_the_caret() {
        assert_eq!(classify(KEY_Left, ""), Some(Key::MoveLeft));
        assert_eq!(classify(KEY_Right, ""), Some(Key::MoveRight));
    }

    #[test]
    fn home_and_end_jump_to_line_edges() {
        assert_eq!(classify(KEY_Home, ""), Some(Key::LineStart));
        assert_eq!(classify(KEY_End, ""), Some(Key::LineEnd));
    }

    // The reset-key classifier reads a process-global `RwLock`
    // (see `set_reset_keys`), so default + toggled assertions
    // share state across tests. Cargo runs tests in parallel
    // within a binary, which would race if we used three
    // separate `#[test]` fns — combine them into one and
    // explicitly set the config at every checkpoint.
    #[test]
    fn reset_key_classifier_honors_config() {
        // Defaults: Enter/Up reset, Tab/Esc ignored.
        set_reset_keys(ResetKeyConfig::default());
        assert_eq!(classify(KEY_Return, ""), Some(Key::Reset));
        assert_eq!(classify(KEY_Up, ""), Some(Key::Reset));
        assert_eq!(classify(KEY_Tab, "\t"), None);
        assert_eq!(classify(KEY_Escape, "\u{1b}"), None);

        // Flip Tab/Esc on, Enter off — classify mirrors the
        // new config.
        set_reset_keys(ResetKeyConfig {
            enter: false,
            tab: true,
            escape: true,
            ..Default::default()
        });
        assert_eq!(classify(KEY_Tab, "\t"), Some(Key::Reset));
        assert_eq!(classify(KEY_Escape, "\u{1b}"), Some(Key::Reset));
        assert_eq!(classify(KEY_Return, ""), None);

        // Restore defaults so other tests in this module that
        // don't touch the global still observe expected behavior.
        set_reset_keys(ResetKeyConfig::default());
    }

    #[test]
    fn a_printable_key_maps_to_a_char() {
        // 0x0061 / 0x0020 are the keysyms for 'a' and space.
        assert_eq!(classify(0x0061, "a"), Some(Key::Char('a')));
        assert_eq!(classify(0x0020, " "), Some(Key::Char(' ')));
    }

    #[test]
    fn a_bare_modifier_is_ignored() {
        // A modifier key (here Shift_L, keysym 0xffe1) produces no UTF-8.
        assert_eq!(classify(0xffe1, ""), None);
    }
}
