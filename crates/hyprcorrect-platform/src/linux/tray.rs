//! System tray (ksni-based) for the hyprcorrect daemon.
//!
//! Publishes a StatusNotifierItem with a small menu: Pause/Resume,
//! Open Preferences…, Quit. Menu activations arrive on the returned
//! channel. The pause state is shared with the daemon via an
//! `Arc<AtomicBool>` — the tray reads it live to choose its icon,
//! label, and SNI status — and [`TrayHandle::refresh`] pushes a
//! property-change so SNI hosts pick up the new state immediately.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};

/// A menu event from the tray.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// The user picked "Pause" or "Resume" — toggle the pause flag.
    TogglePause,
    /// The user picked "Open Preferences…".
    OpenPrefs,
    /// The user picked "Quit".
    Quit,
}

/// An error starting the tray.
#[derive(Debug, thiserror::Error)]
pub enum TrayError {
    /// The ksni service (D-Bus / SNI publication) could not start.
    #[error("ksni: {0}")]
    Ksni(String),
}

/// A live handle to the running tray. Holding it keeps the SNI
/// service registered; dropping it tears it down.
pub struct TrayHandle {
    inner: ksni::blocking::Handle<HyprcorrectTray>,
}

impl TrayHandle {
    /// Re-publish the tray's properties so SNI hosts pick up changes
    /// to pause state immediately. Cheap: the closure is a no-op —
    /// state lives in the shared `Arc<AtomicBool>`.
    pub fn refresh(&self) {
        self.inner.update(|_| {});
    }
}

/// Start the tray. Returns a [`TrayHandle`] (the caller must hold
/// it for the life of the daemon) and a receiver of menu activations.
///
/// `paused` is the shared pause flag; the tray reads it live and
/// changes its icon / label / SNI status to reflect it.
///
/// # Errors
///
/// See [`TrayError`].
pub fn start(paused: Arc<AtomicBool>) -> Result<(TrayHandle, Receiver<TrayEvent>), TrayError> {
    use ksni::blocking::TrayMethods;

    let (events_tx, events_rx) = mpsc::channel();
    let tray = HyprcorrectTray { events_tx, paused };
    let inner = tray.spawn().map_err(|e| TrayError::Ksni(e.to_string()))?;
    Ok((TrayHandle { inner }, events_rx))
}

struct HyprcorrectTray {
    events_tx: Sender<TrayEvent>,
    paused: Arc<AtomicBool>,
}

impl HyprcorrectTray {
    fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
}

impl ksni::Tray for HyprcorrectTray {
    fn id(&self) -> String {
        "hyprcorrect".to_string()
    }

    fn title(&self) -> String {
        "hyprcorrect".to_string()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::ApplicationStatus
    }

    fn status(&self) -> ksni::Status {
        // Stay `Active` even while paused — many SNI hosts (Waybar,
        // for example) hide `Passive` items entirely, which would make
        // the menu unreachable. Pause is conveyed by the icon swap.
        ksni::Status::Active
    }

    fn icon_name(&self) -> String {
        // Bundling a proper hyprcorrect icon (SVG → tiny-skia,
        // vernier-style) is later polish. For now, use the theme's
        // spelling-check icon, swapped for a muted symbolic variant
        // while paused.
        if self.is_paused() {
            "tools-check-spelling-symbolic".to_string()
        } else {
            "tools-check-spelling".to_string()
        }
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let description = if self.is_paused() {
            "Paused. Click the tray icon to resume.".to_string()
        } else {
            "Press the trigger chord to fix the last word.".to_string()
        };
        ksni::ToolTip {
            title: "hyprcorrect".into(),
            description,
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::MenuItem;
        use ksni::menu::StandardItem;
        let pause_label = if self.is_paused() { "Resume" } else { "Pause" };
        vec![
            StandardItem {
                label: pause_label.into(),
                activate: Box::new(|this: &mut HyprcorrectTray| {
                    let _ = this.events_tx.send(TrayEvent::TogglePause);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Open Preferences…".into(),
                activate: Box::new(|this: &mut HyprcorrectTray| {
                    let _ = this.events_tx.send(TrayEvent::OpenPrefs);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|this: &mut HyprcorrectTray| {
                    let _ = this.events_tx.send(TrayEvent::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}
