//! Frontmost-application tracking via `NSWorkspace`.
//!
//! macOS exposes app-level focus, not per-window focus, so for M2 the
//! buffer is keyed by **bundle identifier**: per-window buffers degrade
//! to per-app buffers, still a strict improvement over a single global
//! one. `start` seeds the current frontmost app and installs an observer
//! for `NSWorkspaceDidActivateApplicationNotification`; each activation
//! re-queries the frontmost app and pushes a [`FocusEvent::Focused`].
//!
//! `address` and `class` both carry the bundle id — the daemon keys
//! per-app buffers on `address` and matches the privacy blocklist on
//! `class`, and the macOS blocklist is a list of bundle ids.

use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, Sender};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_app_kit::{NSRunningApplication, NSWorkspace};
use objc2_foundation::NSString;

/// A focus change. `Closed` is unused on macOS for M2 (apps quitting
/// don't need to drop a per-app buffer eagerly), but the variant exists
/// to match the platform contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusEvent {
    Focused {
        address: String,
        class: String,
    },
    #[allow(dead_code)]
    Closed {
        address: String,
    },
}

/// The frontmost app at startup, used to seed the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitialFocus {
    pub address: String,
    pub class: String,
}

#[derive(Debug, thiserror::Error)]
pub enum FocusError {
    /// Could not install the activation observer.
    #[error("could not start NSWorkspace focus tracking: {0}")]
    Observer(String),
}

/// Keeps the notification observer token alive for the process lifetime
/// (dropping it would unsubscribe). Touched only on the main thread.
struct SendableObserver(#[allow(dead_code)] Retained<ProtocolObject<dyn NSObjectProtocol>>);
unsafe impl Send for SendableObserver {}
unsafe impl Sync for SendableObserver {}
static OBSERVER: OnceLock<SendableObserver> = OnceLock::new();

/// The bundle id (falling back to the localized name) of a running app.
fn app_identity(app: &NSRunningApplication) -> String {
    app.bundleIdentifier()
        .map(|s| s.to_string())
        .or_else(|| app.localizedName().map(|s| s.to_string()))
        .unwrap_or_else(|| "unknown".to_string())
}

pub fn start() -> Result<(Option<InitialFocus>, Receiver<FocusEvent>), FocusError> {
    super::app::run_on_main_sync(
        || -> Result<(Option<InitialFocus>, Receiver<FocusEvent>), FocusError> {
            let workspace = NSWorkspace::sharedWorkspace();

            let initial = workspace.frontmostApplication().map(|app| {
                let id = app_identity(&app);
                InitialFocus {
                    address: id.clone(),
                    class: id,
                }
            });

            let (tx, rx) = mpsc::channel::<FocusEvent>();
            let tx_block: Sender<FocusEvent> = tx;

            // The block fires on the main run loop each time an app
            // activates. We re-query the frontmost app rather than parse
            // the notification's userInfo — when `didActivate` fires the
            // frontmost app *is* the newly-activated one.
            let block = RcBlock::new(
                move |_notif: core::ptr::NonNull<objc2_foundation::NSNotification>| {
                    let ws = NSWorkspace::sharedWorkspace();
                    if let Some(app) = ws.frontmostApplication() {
                        let id = app_identity(&app);
                        let _ = tx_block.send(FocusEvent::Focused {
                            address: id.clone(),
                            class: id,
                        });
                    }
                },
            );

            let center = workspace.notificationCenter();
            let name = NSString::from_str("NSWorkspaceDidActivateApplicationNotification");
            let observer = unsafe {
                center.addObserverForName_object_queue_usingBlock(Some(&name), None, None, &block)
            };
            // Stash the token so the subscription outlives this call.
            let _ = OBSERVER.set(SendableObserver(observer));

            Ok((initial, rx))
        },
    )
}
