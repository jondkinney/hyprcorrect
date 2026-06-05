//! macOS platform backend (milestone M2).
//!
//! - **Capture:** a listen-only `CGEventTap` on its own CFRunLoop
//!   thread, translating Quartz key events into [`hyprcorrect_core::Key`]
//!   (needs Input Monitoring).
//! - **Synthetic input:** `CGEvent` keyboard events
//!   (`CGEventKeyboardSetUnicodeString` for arbitrary correction text,
//!   keycode 0x33 for Backspace; needs Accessibility).
//! - **Hotkeys:** Carbon `RegisterEventHotKey` — the one global-hotkey
//!   API that needs no TCC permission and *intercepts* the chord, so
//!   terminals never see the raw key. The Carbon callback writes the
//!   action label and `raise`s `SIGUSR1`, so `signal_channel` keeps the
//!   exact same shape as Linux.
//! - **Focus:** `NSWorkspace.frontmostApplication` + the
//!   `didActivateApplication` notification. App-level addressing for M2.
//! - **Menu bar:** `NSStatusItem`.
//!
//! ## Threading model (mirrors the sibling `vernier`)
//!
//! AppKit objects (`NSStatusItem`, `NSWorkspace` observers) and Carbon
//! hotkey dispatch require the OS main thread with a running event
//! loop. The daemon, however, is a long-lived synchronous loop we don't
//! want to restructure. So [`bootstrap_main`] sets NSApp to `.accessory`,
//! spawns the daemon body on a worker thread, and runs `NSApp.run()` on
//! main. Backend functions that touch AppKit marshal onto main via
//! libdispatch ([`app::run_on_main_sync`]). The CGEventTap is the one
//! piece that runs on its *own* thread (a tap only needs *a* CFRunLoop,
//! not the main one).

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};

mod app;
pub mod apps;
pub mod capture;
// The chord-capture IPC (a Unix-domain socket + worker threads) is
// fully OS-independent — it only touches `std::os::unix::net` and
// `hyprcorrect_core::runtime`, both of which work on macOS. Rather than
// duplicate ~250 lines, reuse the Linux source file verbatim.
#[path = "../linux/chord_capture.rs"]
pub mod chord_capture;
pub mod clipboard;
pub mod emit;
mod ffi;
pub mod focus;
pub mod hotkey;
mod keymap;
pub mod tray;

pub use app::bootstrap_main;

/// Main-thread-only registry of the retained AppKit / Carbon handles
/// the backend creates. Reached via [`with_main_state`] from closures
/// dispatched onto the main queue (and from the Carbon hotkey callback,
/// which the AppKit run loop dispatches on main).
pub(crate) struct MainState {
    /// Registered Carbon hotkeys, keyed by our own id. Each entry
    /// carries the `UnregisterEventHotKey` handle and the action label
    /// (`"word"` / `"sentence"` / `"review"` / `"review-llm"`) the
    /// callback writes to the runtime action file before signalling.
    pub hotkeys: HashMap<u32, hotkey::HotkeyResources>,
    /// The single shared Carbon event handler, installed lazily on the
    /// first `install_bind`; every per-hotkey entry shares it.
    pub carbon_handler_installed: bool,
    /// The live status-item resources, if the tray was started.
    pub tray: Option<tray::TrayResources>,
}

impl MainState {
    fn new() -> Self {
        Self {
            hotkeys: HashMap::new(),
            carbon_handler_installed: false,
            tray: None,
        }
    }
}

thread_local! {
    static MAIN_STATE_TLS: RefCell<Option<MainState>> = const { RefCell::new(None) };
}

/// Run `f` with mutable access to the main-thread state. Panics if
/// called off the main thread (the TLS is initialised by
/// [`bootstrap_main`], which only runs there).
pub(crate) fn with_main_state<R>(f: impl FnOnce(&mut MainState) -> R) -> R {
    MAIN_STATE_TLS.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let state = borrow
            .as_mut()
            .expect("macOS main-thread state not initialised; was bootstrap_main called?");
        f(state)
    })
}

/// Install the empty main-thread state. Called once by
/// [`bootstrap_main`] before NSApp starts.
pub(crate) fn install_main_state() {
    MAIN_STATE_TLS.with(|cell| {
        let mut borrow = cell.borrow_mut();
        if borrow.is_none() {
            *borrow = Some(MainState::new());
        }
    });
}

static NEXT_ID: AtomicU32 = AtomicU32::new(1);

/// Monotonic id for a freshly registered Carbon hotkey.
pub(crate) fn next_id() -> u32 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Logical size of the main display in points `(width, height)`. Used by
/// the daemon to size the review popup. Returns `(0.0, 0.0)` very early in
/// startup before any screen exists.
pub fn primary_screen_size() -> (f32, f32) {
    app::run_on_main_sync(|| {
        use objc2::MainThreadMarker;
        use objc2_app_kit::NSScreen;
        let Some(mtm) = MainThreadMarker::new() else {
            return (0.0, 0.0);
        };
        let Some(screen) = NSScreen::mainScreen(mtm) else {
            return (0.0, 0.0);
        };
        let frame = screen.frame();
        (frame.size.width as f32, frame.size.height as f32)
    })
}
