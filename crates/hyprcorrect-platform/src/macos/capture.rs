//! Observe-only keystroke capture via a listen-only `CGEventTap`.
//!
//! A session-level, head-insert, **listen-only** tap runs on its own
//! dedicated `CFRunLoop` thread (a tap only needs *a* run loop, not the
//! AppKit main one). Each key event is translated into a
//! [`hyprcorrect_core::Key`] and pushed down an mpsc channel to the
//! daemon, mirroring the Linux `evdev` path. The tap needs **Input
//! Monitoring** (System Settings → Privacy & Security).
//!
//! The tap is listen-only because the trigger chord is intercepted by
//! Carbon `RegisterEventHotKey` (see `hotkey.rs`), not by the tap — so
//! unlike on Wayland we never need to *drop* an event, and we avoid the
//! heavier Accessibility grant an active tap would require.

use std::os::raw::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use hyprcorrect_core::{Chord, Key};

use super::chord_capture::ChordCaptureSlot;
use super::ffi::*;
use super::keymap;

/// Which control keys clear the per-window typing buffer. Runtime view
/// the classifier reads on every keystroke; rebuilt from config on load
/// and reload. Mirrors the Linux `capture::ResetKeyConfig` field-for-field
/// so the daemon's `reset_key_config` helper is platform-neutral.
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
        // Enter / arrows / page / delete / insert reset by default;
        // Tab and Escape do not (they're common in-field keys).
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

/// Capture start-up failure. Variant *names* differ from Linux (no
/// `/dev/input`, no xkb) but the shape is the same three-failure set the
/// daemon prints via `Display`.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error(
        "Input Monitoring permission is not granted — open System Settings → \
         Privacy & Security → Input Monitoring, enable hyprcorrect, then restart it"
    )]
    Permission,
    #[error("could not create the CGEventTap (Input Monitoring may be denied)")]
    TapCreation,
    #[error("could not spawn the capture run-loop thread: {0}")]
    Thread(String),
}

// --- Daemon-wide shared state -----------------------------------------------

static RESET_KEY_CONFIG: OnceLock<RwLock<ResetKeyConfig>> = OnceLock::new();
static CARET_SUSPECT: OnceLock<Arc<AtomicBool>> = OnceLock::new();
/// Latest device-independent modifier flags seen by the tap. Read by
/// [`wait_mods_clear`] so emit can hold off until the trigger chord is
/// released.
static MODS_STATE: AtomicU64 = AtomicU64::new(0);
/// Set once the tap is live; [`wait_mods_clear`] returns `true`
/// immediately when capture never started (e.g. unit tests calling emit).
static CAPTURE_ACTIVE: AtomicBool = AtomicBool::new(false);

fn reset_key_config() -> &'static RwLock<ResetKeyConfig> {
    RESET_KEY_CONFIG.get_or_init(|| RwLock::new(ResetKeyConfig::default()))
}

/// Replace the daemon-wide reset-key config. Called at startup and on
/// every config reload.
pub fn set_reset_keys(cfg: ResetKeyConfig) {
    *reset_key_config().write().expect("reset-key lock") = cfg;
}

/// Shared "a recent mouse click may have moved the caret" flag. The
/// word-fix path widens its scan to the whole buffer while it's set; the
/// daemon clears it after a fix or a reset key.
pub fn caret_suspect_flag() -> Arc<AtomicBool> {
    CARET_SUSPECT
        .get_or_init(|| Arc::new(AtomicBool::new(false)))
        .clone()
}

/// Block up to `timeout` for the chord modifiers (⌘/⌃/⌥/⇧) to clear, so
/// a synthetic-text burst isn't poisoned by a still-held modifier.
/// Returns `true` if cleared (or capture never started), `false` on
/// timeout.
pub fn wait_mods_clear(timeout: Duration) -> bool {
    if !CAPTURE_ACTIVE.load(Ordering::Relaxed) {
        return true;
    }
    const CHORD_MODS: u64 = kCGEventFlagMaskCommand
        | kCGEventFlagMaskControl
        | kCGEventFlagMaskAlternate
        | kCGEventFlagMaskShift;
    let deadline = Instant::now() + timeout;
    loop {
        if MODS_STATE.load(Ordering::Relaxed) & CHORD_MODS == 0 {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

// --- Tap context (lives for the process; reached from the C callback) -------

/// A chord reduced to the fields the tap callback compares against:
/// virtual keycode plus the four modifier bools.
#[derive(Clone, Copy)]
struct ChordKey {
    vkey: u16,
    ctrl: bool,
    shift: bool,
    alt: bool,
    super_: bool,
}

struct TapContext {
    tx: Sender<Key>,
    /// Trigger / action chords whose key-press the tap must NOT buffer
    /// (Carbon already intercepts them, but this is the belt-and-braces
    /// the Linux backend also keeps).
    suppression: Vec<ChordKey>,
    chord_capture: Arc<ChordCaptureSlot>,
    caret_suspect: Arc<AtomicBool>,
    /// The tap's mach port, stored as `usize` so it can be read back in
    /// the callback to re-enable after a timeout disable.
    port: AtomicUsize,
}

pub fn start(
    chords: &[Chord],
    chord_capture: Arc<ChordCaptureSlot>,
) -> Result<Receiver<Key>, CaptureError> {
    // Input Monitoring pre-flight. If it isn't granted, request it (this
    // registers hyprcorrect in the System Settings list and prompts) and
    // ask the user to grant + restart — a freshly-granted tap doesn't
    // take effect in the already-running process.
    if !unsafe { CGPreflightListenEventAccess() } {
        unsafe {
            CGRequestListenEventAccess();
        }
        return Err(CaptureError::Permission);
    }

    let suppression: Vec<ChordKey> = chords
        .iter()
        .filter_map(|c| {
            keymap::key_token_to_vkey(&c.key).map(|vkey| ChordKey {
                vkey,
                ctrl: c.ctrl,
                shift: c.shift,
                alt: c.alt,
                super_: c.super_,
            })
        })
        .collect();

    let (tx, rx) = mpsc::channel::<Key>();
    let caret_suspect = caret_suspect_flag();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), CaptureError>>();

    let spawn = thread::Builder::new()
        .name("hyprcorrect-capture".into())
        .spawn(move || {
            // Build the context on this thread so the raw pointer never
            // crosses a thread boundary; leak it for the tap's lifetime.
            let ctx = Box::new(TapContext {
                tx,
                suppression,
                chord_capture,
                caret_suspect,
                port: AtomicUsize::new(0),
            });
            let ctx_ptr = Box::into_raw(ctx);

            let mask = event_mask_bit(kCGEventKeyDown)
                | event_mask_bit(kCGEventFlagsChanged)
                | event_mask_bit(kCGEventLeftMouseDown);
            let port = unsafe {
                CGEventTapCreate(
                    kCGSessionEventTap,
                    kCGHeadInsertEventTap,
                    kCGEventTapOptionListenOnly,
                    mask,
                    tap_callback,
                    ctx_ptr as *mut c_void,
                )
            };
            if port.is_null() {
                let _ = ready_tx.send(Err(CaptureError::TapCreation));
                drop(unsafe { Box::from_raw(ctx_ptr) });
                return;
            }
            unsafe { (*ctx_ptr).port.store(port as usize, Ordering::Relaxed) };

            let source = unsafe { CFMachPortCreateRunLoopSource(ptr::null(), port, 0) };
            if source.is_null() {
                let _ = ready_tx.send(Err(CaptureError::TapCreation));
                unsafe { CFRelease(port as *const c_void) };
                drop(unsafe { Box::from_raw(ctx_ptr) });
                return;
            }
            unsafe {
                CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
                CGEventTapEnable(port, true);
                CFRelease(source);
            }
            CAPTURE_ACTIVE.store(true, Ordering::Relaxed);
            let _ = ready_tx.send(Ok(()));
            // Block forever servicing the tap. The process exits out
            // from under this thread on daemon shutdown.
            unsafe { CFRunLoopRun() };
        });

    if let Err(e) = spawn {
        return Err(CaptureError::Thread(e.to_string()));
    }

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(rx),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(CaptureError::TapCreation),
    }
}

// --- The tap callback -------------------------------------------------------

// The `kCGEvent*` constants keep their C names by design; using them in
// match arms otherwise trips the non-upper-case-globals lint.
#[allow(non_upper_case_globals)]
unsafe extern "C" fn tap_callback(
    _proxy: CGEventTapProxy,
    etype: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef {
    let ctx = unsafe { &*(user_info as *const TapContext) };

    match etype {
        kCGEventTapDisabledByTimeout => {
            // The kernel disabled us for running long (load / App Nap).
            // Documented recovery: just re-enable in place.
            let port = ctx.port.load(Ordering::Relaxed) as *mut c_void;
            if !port.is_null() {
                unsafe { CGEventTapEnable(port, true) };
            }
            return event;
        }
        kCGEventTapDisabledByUserInput => {
            // Secure input (password field) or a permission change. We
            // deliberately do NOT re-enable: while secure input is on,
            // re-enabling would churn, and not buffering password keys is
            // the privacy-correct behaviour. Capture resumes on restart.
            log::warn!("macos capture: tap disabled (secure input or permission change)");
            return event;
        }
        kCGEventLeftMouseDown => {
            // A click may have moved the caret away from the buffer end.
            ctx.caret_suspect.store(true, Ordering::Relaxed);
            return event;
        }
        kCGEventFlagsChanged => {
            MODS_STATE.store(unsafe { CGEventGetFlags(event) }, Ordering::Relaxed);
            return event;
        }
        kCGEventKeyDown => { /* fall through */ }
        _ => return event,
    }

    // Skip events the emit/clipboard paths synthesized: a session tap sees
    // our own `CGEventPost` output, and buffering it would double-apply the
    // correction (the daemon already rewrites the buffer after an emit).
    if unsafe { CGEventGetIntegerValueField(event, kCGEventSourceUserData) } == SYNTHETIC_MARK {
        return event;
    }

    let keycode = unsafe { CGEventGetIntegerValueField(event, kCGKeyboardEventKeycode) } as u16;
    let flags = unsafe { CGEventGetFlags(event) };
    MODS_STATE.store(flags, Ordering::Relaxed);

    let m = Mods::from_flags(flags);

    // 1) If prefs is recording a chord, hand it the reconstructed chord
    //    string instead of buffering the press. This MUST come before the
    //    suppression check so a chord matching a current bind can still be
    //    recorded (the suppression list isn't updated during a recording).
    if ctx.chord_capture.is_armed()
        && let Some(token) = keymap::vkey_to_token(keycode)
    {
        let chord_string = build_chord_string(&m, &token);
        if ctx.chord_capture.try_emit(chord_string) {
            return event;
        }
    }

    // 2) Suppress the trigger/action chords' key-press.
    if ctx.suppression.iter().any(|c| {
        c.vkey == keycode
            && c.ctrl == m.ctrl
            && c.shift == m.shift
            && c.alt == m.alt
            && c.super_ == m.command
    }) {
        return event;
    }

    // 3) Classify into a buffer key.
    if let Some(key) = classify(keycode, &m) {
        let _ = ctx.tx.send(key);
        return event;
    }

    // 4) Otherwise pull the typed character (unless a command/control
    //    shortcut, which we treat as a reset since it may edit text).
    if m.command || m.ctrl {
        // ⌘V paste, ⌘Z undo, ⌘A select-all… caret/text may have moved.
        let _ = ctx.tx.send(Key::Reset);
        return event;
    }
    if let Some(c) = typed_char(event) {
        let _ = ctx.tx.send(Key::Char(c));
    }
    event
}

/// Decoded modifier state for one event.
struct Mods {
    ctrl: bool,
    shift: bool,
    alt: bool,
    command: bool,
}

impl Mods {
    fn from_flags(flags: u64) -> Self {
        Self {
            ctrl: flags & kCGEventFlagMaskControl != 0,
            shift: flags & kCGEventFlagMaskShift != 0,
            alt: flags & kCGEventFlagMaskAlternate != 0,
            command: flags & kCGEventFlagMaskCommand != 0,
        }
    }
}

/// Map a navigation / control keycode to a buffer [`Key`]. Returns
/// `None` for ordinary printable keys (handled by the unicode path).
fn classify(keycode: u16, m: &Mods) -> Option<Key> {
    let cfg = *reset_key_config().read().expect("reset-key lock");

    // macOS honours emacs-style caret navigation in Cocoa text views and
    // in readline/terminals, so ⌃A/E/F/B/N/P move the caret — they must
    // map to the same buffer Keys as the arrows, NOT wipe the buffer
    // (which the blanket `m.ctrl → Reset` in the caller would do).
    if m.ctrl && !m.command && !m.alt {
        match keycode {
            0x00 => return Some(Key::LineStart), // ⌃A
            0x0E => return Some(Key::LineEnd),   // ⌃E
            0x03 => return Some(Key::MoveRight), // ⌃F
            0x0B => return Some(Key::MoveLeft),  // ⌃B
            // ⌃N / ⌃P (next/prev line) fall through to the caller's
            // `m.ctrl → Reset`, the safe choice for a vertical move.
            _ => {}
        }
    }

    Some(match keycode {
        // ⌥⌫ / ⌘⌫ delete a whole word / to line-start — more than the
        // buffer's one-char pop can track, so reset instead of desyncing.
        0x33 if m.alt || m.command => Key::Reset,
        0x33 => Key::Backspace, // ⌫
        0x7B => {
            // Left: ⌥ = word, ⌘ = line start, else char.
            if m.alt {
                Key::WordLeft
            } else if m.command {
                Key::LineStart
            } else {
                Key::MoveLeft
            }
        }
        0x7C => {
            if m.alt {
                Key::WordRight
            } else if m.command {
                Key::LineEnd
            } else {
                Key::MoveRight
            }
        }
        0x73 => Key::LineStart, // Home
        0x77 => Key::LineEnd,   // End
        0x7E if cfg.up => Key::Reset,
        0x7D if cfg.down => Key::Reset,
        0x74 if cfg.page_up => Key::Reset,
        0x79 if cfg.page_down => Key::Reset,
        0x24 | 0x4C if cfg.enter => Key::Reset, // Return / keypad Enter
        0x30 if cfg.tab => Key::Reset,
        0x35 if cfg.escape => Key::Reset,
        0x75 if cfg.delete => Key::Reset, // forward delete
        0x72 if cfg.insert => Key::Reset, // Help/Insert
        // Bare arrows/page/enter/etc. with their reset toggle off, or any
        // other keycode: not a buffer-control key.
        0x7E | 0x7D | 0x74 | 0x79 | 0x24 | 0x4C | 0x30 | 0x35 | 0x75 | 0x72 => return None,
        _ => return None,
    })
}

/// Read the committed character(s) of a key-down event. Returns the
/// single printable `char`, or `None` for empty / control / multi-char
/// (dead-key) sequences.
fn typed_char(event: CGEventRef) -> Option<char> {
    let mut buf = [0u16; 8];
    let mut actual: usize = 0;
    unsafe {
        CGEventKeyboardGetUnicodeString(event, buf.len(), &mut actual, buf.as_mut_ptr());
    }
    if actual == 0 {
        return None;
    }
    let s = String::from_utf16_lossy(&buf[..actual]);
    let mut chars = s.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None; // dead-key / composed multi-char — out of scope (M5+)
    }
    if c.is_control() {
        return None;
    }
    Some(c)
}

/// Reconstruct a `CTRL+SHIFT+ALT+SUPER+KEY`-style chord string from the
/// decoded modifiers and key token (for the prefs chord recorder).
fn build_chord_string(m: &Mods, token: &str) -> String {
    let mut s = String::new();
    if m.ctrl {
        s.push_str("CTRL+");
    }
    if m.shift {
        s.push_str("SHIFT+");
    }
    if m.alt {
        s.push_str("ALT+");
    }
    if m.command {
        s.push_str("SUPER+");
    }
    s.push_str(token);
    s
}
