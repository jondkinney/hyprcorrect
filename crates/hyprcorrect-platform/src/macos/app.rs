//! NSApplication bootstrap on the main thread, plus the libdispatch
//! helpers every other macOS module uses to marshal AppKit work onto
//! main from the daemon worker thread.
//!
//! Adapted from the sibling `vernier` project (MIT/Apache, same
//! author), trimmed to hyprcorrect's needs: there is no single
//! `PlatformEvent` bus — capture/hotkey/focus/tray each own their
//! channel — so the reopen delegate spawns the prefs subprocess
//! directly instead of forwarding an event.

use std::os::raw::c_int;
use std::sync::{Mutex, OnceLock};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate};

use super::install_main_state;

/// `kProcessTransformToUIElementApplication`. Converting the process
/// type is required for an *unbundled* binary on Sequoia: a bare
/// `cargo run` binary is classed `kProcessNotAnApplication`, and the
/// menu-bar agent silently drops status items from such a process even
/// after `setActivationPolicy(.accessory)`. Calling `TransformProcessType`
/// first is the documented fix.
const K_PROCESS_TRANSFORM_TO_UI_ELEMENT: u32 = 4;
const K_CURRENT_PROCESS: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy)]
struct ProcessSerialNumber {
    high_long_of_psn: u32,
    low_long_of_psn: u32,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn TransformProcessType(psn: *const ProcessSerialNumber, transform_type: u32) -> c_int;
}

fn transform_to_ui_element() {
    let psn = ProcessSerialNumber {
        high_long_of_psn: 0,
        low_long_of_psn: K_CURRENT_PROCESS,
    };
    let status = unsafe { TransformProcessType(&psn, K_PROCESS_TRANSFORM_TO_UI_ELEMENT) };
    if status != 0 {
        log::warn!("macos: TransformProcessType returned {status}");
    }
}

/// Bootstrap the AppKit main thread, spawn `daemon_body` on a worker,
/// and run `NSApp.run()` forever. MUST be called from the OS main
/// thread (AppKit asserts on thread identity). When the daemon body
/// returns, NSApp is asked to stop and the process exits.
pub fn bootstrap_main<F>(daemon_body: F) -> !
where
    F: FnOnce() + Send + 'static,
{
    let mtm = MainThreadMarker::new()
        .expect("hyprcorrect_platform::macos::bootstrap_main must run on the main thread");

    install_main_state();
    // Promote the process type BEFORE constructing NSApplication.
    transform_to_ui_element();

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    // Eagerly finish launching so a status item created from the worker
    // thread doesn't race the menu-bar plumbing.
    app.finishLaunching();
    log::info!("macos: NSApp finished launching, activation=Accessory");

    // Install a delegate so re-launching the running app (Finder
    // double-click / `open Hyprcorrect.app` / Dock click) opens prefs.
    let delegate = HyprDelegate::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    APP_DELEGATE.set(SendableDelegate(delegate)).ok();

    NSAPP.set(SendableApp(app.clone())).ok();

    std::thread::Builder::new()
        .name("hyprcorrect-daemon".into())
        .spawn(move || {
            daemon_body();
            terminate_nsapp();
        })
        .expect("spawn hyprcorrect daemon worker thread");

    app.run();
    std::process::exit(0)
}

struct SendableApp(Retained<NSApplication>);
// `stop:`/`postEvent:` are safe to call cross-thread (they post a quit
// event); we never touch other NSApplication methods off-main.
unsafe impl Send for SendableApp {}
unsafe impl Sync for SendableApp {}

static NSAPP: OnceLock<SendableApp> = OnceLock::new();

fn terminate_nsapp() {
    run_on_main_async(|| {
        let Some(SendableApp(app)) = NSAPP.get() else {
            return;
        };
        app.stop(None);
        wake_main_event_loop();
    });
}

/// Post a no-op event so `NSApp.run()` blocked on `nextEvent` returns
/// and notices the `stop:` flag. Must run on main; callers dispatch.
fn wake_main_event_loop() {
    use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType};
    use objc2_foundation::NSPoint;

    let Some(SendableApp(app)) = NSAPP.get() else {
        return;
    };
    let _ = MainThreadMarker::new().expect("wake_main_event_loop off-main");
    let event = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
        NSEventType::ApplicationDefined,
        NSPoint { x: 0.0, y: 0.0 },
        NSEventModifierFlags(0),
        0.0,
        0,
        None,
        0,
        0,
        0,
    );
    if let Some(event) = event {
        app.postEvent_atStart(&event, true);
    }
}

/// Dispatch `f` to the main thread and block until it returns. Safe from
/// any thread. If the caller is *already* on the main thread (e.g. an
/// AppKit/Carbon callback that needs to touch main-thread state), run `f`
/// inline — `DispatchQueue::main().exec_sync` from main would deadlock,
/// since the main run loop is busy in our callback and can't service the
/// dispatched block.
pub(crate) fn run_on_main_sync<F, R>(f: F) -> R
where
    F: FnOnce() -> R + Send,
    R: Send,
{
    if MainThreadMarker::new().is_some() {
        return f();
    }
    let result: Mutex<Option<R>> = Mutex::new(None);
    DispatchQueue::main().exec_sync(|| {
        *result.lock().expect("dispatch sync result lock") = Some(f());
    });
    result
        .into_inner()
        .expect("dispatch sync result mutex")
        .expect("dispatch sync closure produced no result")
}

/// Fire-and-forget dispatch to the main thread.
pub(crate) fn run_on_main_async<F>(f: F)
where
    F: FnOnce() + Send + 'static,
{
    DispatchQueue::main().exec_async(f);
}

/// Best-effort: launch `hyprcorrect prefs` as a detached subprocess.
/// Used by the reopen delegate. The prefs entry's own singleton lock
/// focuses an existing window instead of opening a second one.
fn spawn_prefs_subprocess() {
    use std::process::{Command, Stdio};
    let Ok(exe) = std::env::current_exe() else {
        log::warn!("macos: cannot find own executable to launch prefs");
        return;
    };
    let _ = Command::new(exe)
        .arg("prefs")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

// --- NSApplicationDelegate ---------------------------------------------------

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HyprcorrectAppDelegate"]
    struct HyprDelegate;

    unsafe impl NSObjectProtocol for HyprDelegate {}

    unsafe impl NSApplicationDelegate for HyprDelegate {
        /// LaunchServices sends the running process a
        /// `kAEReopenApplication` event on re-launch. The daemon has no
        /// main window, so any re-launch means "open Preferences".
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn application_should_handle_reopen(
            &self,
            _sender: &NSApplication,
            _has_visible_windows: bool,
        ) -> bool {
            log::info!("macos: reopen Apple Event → open prefs");
            spawn_prefs_subprocess();
            true
        }
    }
);

impl HyprDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        unsafe { msg_send![mtm.alloc::<Self>(), init] }
    }
}

/// Strong-ref holder — NSApp keeps only a weak delegate reference.
struct SendableDelegate(#[allow(dead_code)] Retained<HyprDelegate>);
// Safety: a MainThreadOnly NSObject subclass we never touch off-main;
// this storage only keeps the retain count alive for the process life.
unsafe impl Send for SendableDelegate {}
unsafe impl Sync for SendableDelegate {}
static APP_DELEGATE: OnceLock<SendableDelegate> = OnceLock::new();
