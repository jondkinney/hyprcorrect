//! Global trigger via the freedesktop `GlobalShortcuts` portal.
//!
//! Registers the trigger chord (Super+Ctrl+Shift+Alt+letter) with the
//! compositor so it intercepts the chord — the focused application
//! (terminals!) never sees it. Each activation arrives as `()` on the
//! returned channel.
//!
//! Runs a dedicated tokio current-thread runtime on a
//! `hyprcorrect-portal` thread; the synchronous `start` waits for the
//! initial bind to succeed before returning.

use std::sync::mpsc::{self, Receiver, Sender, SyncSender};

use ashpd::desktop::global_shortcuts::{BindShortcutsOptions, GlobalShortcuts, NewShortcut};
use futures_util::StreamExt;

/// An error starting the portal-registered trigger.
#[derive(Debug, thiserror::Error)]
pub enum HotkeyError {
    /// Could not connect to or talk to the portal.
    #[error("GlobalShortcuts portal: {0}")]
    Portal(String),
    /// Could not spawn the portal thread.
    #[error("could not spawn portal thread: {0}")]
    Thread(String),
}

/// Start the portal-registered trigger.
///
/// The chord is fixed to Super+Ctrl+Shift+Alt+letter; the letter is
/// taken from `$HYPRCORRECT_TRIGGER` (xkb keysym name, default `F`).
/// Each activation arrives as `()` on the returned receiver.
///
/// # Errors
///
/// See [`HotkeyError`] — portal failure or thread spawn failure.
pub fn start() -> Result<Receiver<()>, HotkeyError> {
    let letter = std::env::var("HYPRCORRECT_TRIGGER").unwrap_or_else(|_| "F".to_string());
    let trigger = format!("CTRL+ALT+SHIFT+LOGO+{}", letter.to_uppercase());

    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), HotkeyError>>(1);
    let ready_tx_for_thread = ready_tx.clone();

    std::thread::Builder::new()
        .name("hyprcorrect-portal".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(r) => r,
                Err(e) => {
                    let _ = ready_tx_for_thread
                        .send(Err(HotkeyError::Thread(format!("tokio runtime: {e}"))));
                    return;
                }
            };
            runtime.block_on(async move {
                if let Err(e) = run_portal(trigger, tx, &ready_tx_for_thread).await {
                    let _ = ready_tx_for_thread.send(Err(e));
                }
            });
        })
        .map_err(|e| HotkeyError::Thread(e.to_string()))?;

    ready_rx
        .recv()
        .map_err(|_| HotkeyError::Thread("portal init failed".into()))??;

    Ok(rx)
}

async fn run_portal(
    trigger: String,
    tx: Sender<()>,
    ready_tx: &SyncSender<Result<(), HotkeyError>>,
) -> Result<(), HotkeyError> {
    let proxy = GlobalShortcuts::new()
        .await
        .map_err(|e| HotkeyError::Portal(format!("create proxy: {e}")))?;
    let session = proxy
        .create_session(Default::default())
        .await
        .map_err(|e| HotkeyError::Portal(format!("create session: {e}")))?;

    let shortcut = NewShortcut::new("fix-word", "Correct the last typed word")
        .preferred_trigger(trigger.as_str());
    let request = proxy
        .bind_shortcuts(&session, &[shortcut], None, BindShortcutsOptions::default())
        .await
        .map_err(|e| HotkeyError::Portal(format!("bind: {e}")))?;
    request
        .response()
        .map_err(|e| HotkeyError::Portal(format!("bind response: {e}")))?;

    let _ = ready_tx.send(Ok(()));

    let mut activated = proxy
        .receive_activated()
        .await
        .map_err(|e| HotkeyError::Portal(format!("activated stream: {e}")))?;

    // Only one shortcut is registered; every activation is the trigger.
    while activated.next().await.is_some() {
        if tx.send(()).is_err() {
            break; // receiver dropped — daemon is shutting down
        }
    }
    Ok(())
}
